//! Privacy-preserving summaries. A durable begin record is required before cleanup.
//! Begin and finish are immutable, atomically published files: a missing finish
//! always means incomplete, including after process termination.
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
};

const VERSION: u32 = 1;
const MAX_RECORDS: usize = 100;
const MAX_FILE_BYTES: u64 = 16 * 1024;
const MAX_DIRECTORY_ENTRIES: usize = 1_000;
const DIRECTORY: &str = "cleanup-history-v1";
static SEQUENCE: AtomicU64 = AtomicU64::new(0);
static HISTORY_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HistoryRule {
    pub id: String,
    pub title: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct HistorySummary {
    #[serde(alias = "recycled_files")]
    pub deleted_files: u64,
    #[serde(alias = "recycled_bytes")]
    pub deleted_bytes: u64,
    pub skipped_files: u64,
    pub failed_files: u64,
}

#[derive(Debug, Deserialize, Serialize)]
struct Begin {
    #[serde(default)]
    permanent_delete: bool,
    version: u32,
    id: String,
    plan_id: String,
    started_at: String,
    selected_rules: Vec<HistoryRule>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Finish {
    version: u32,
    finished_at: String,
    summary: HistorySummary,
    failed: bool,
}

pub struct RecordToken {
    directory: PathBuf,
}

#[derive(Debug, Serialize)]
pub struct CleanupHistory {
    pub records: Vec<HistoryRecord>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct HistoryRecord {
    pub permanent_delete: bool,
    pub id: String,
    pub plan_id: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub selected_rules: Vec<HistoryRule>,
    pub status: String,
    pub deleted_files: u64,
    pub deleted_bytes: u64,
    pub skipped_files: u64,
    pub failed_files: u64,
    pub message: Option<String>,
}

fn safe_path(path: &Path) -> Result<(), String> {
    for ancestor in path.ancestors() {
        let meta = match fs::symlink_metadata(ancestor) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err("无法访问清理记录目录".into()),
        };
        if meta.file_type().is_symlink() {
            return Err("清理记录目录包含不安全的链接".into());
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if meta.file_attributes() & 0x400 != 0 {
                return Err("清理记录目录包含不安全的链接".into());
            }
        }
    }
    Ok(())
}

fn record_name(name: &str) -> bool {
    name.starts_with("record-")
        && name.len() <= 100
        && name
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b'-' || b.is_ascii_lowercase())
}

fn directories(root: &Path) -> Result<Vec<PathBuf>, String> {
    safe_path(root)?;
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err("无法读取清理记录，请检查应用数据目录权限".into()),
    };
    let mut result = Vec::new();
    for (index, entry) in entries.enumerate() {
        if index >= MAX_DIRECTORY_ENTRIES {
            return Err("清理记录目录内容异常，已停止读取以保护应用".into());
        }
        let entry = entry.map_err(|_| "无法读取部分清理记录".to_string())?;
        if record_name(&entry.file_name().to_string_lossy()) {
            safe_path(&entry.path())?;
            if entry
                .file_type()
                .map_err(|_| "无法读取清理记录".to_string())?
                .is_dir()
            {
                result.push(entry.path());
            }
        }
    }
    result.sort_unstable_by(|a, b| b.file_name().cmp(&a.file_name()));
    Ok(result)
}

fn publish<T: Serialize>(directory: &Path, name: &str, value: &T) -> Result<(), String> {
    safe_path(directory)?;
    let bytes = serde_json::to_vec(value).map_err(|_| "无法生成清理记录".to_string())?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err("清理记录内容超过安全大小限制".into());
    }
    let target = directory.join(name);
    let temporary = directory.join(format!("{name}.pending"));
    safe_path(&target)?;
    safe_path(&temporary)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|_| "无法保存清理记录，请检查应用数据目录权限".to_string())?;
    let result = (|| {
        file.write_all(&bytes)
            .map_err(|_| "清理记录写入失败".to_string())?;
        file.sync_all()
            .map_err(|_| "清理记录写入失败".to_string())?;
        drop(file);
        if target.exists() {
            return Err("清理记录已存在，无法重复保存".into());
        }
        fs::rename(&temporary, &target).map_err(|_| "无法完成清理记录保存".to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T, String> {
    safe_path(path)?;
    let file = File::open(path).map_err(|_| "无法读取记录文件".to_string())?;
    let metadata = file
        .metadata()
        .map_err(|_| "无法读取记录文件".to_string())?;
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
        return Err("记录文件超过安全大小限制".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "无法读取记录文件".to_string())?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err("记录文件超过安全大小限制".into());
    }
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| "记录文件不完整或已损坏".to_string())?;
    if value.get("version").and_then(|v| v.as_u64()) != Some(VERSION as u64) {
        return Err("记录版本暂不支持".into());
    }
    serde_json::from_value(value).map_err(|_| "记录文件不完整或已损坏".into())
}

/// This internal API accepts the app-resolved data directory, never a frontend path.
/// Failure MUST prevent the caller from beginning any cleanup actions.
pub fn start_record(
    app_data: &Path,
    plan_id: &str,
    selected_rules: Vec<HistoryRule>,
) -> Result<RecordToken, String> {
    let _guard = HISTORY_LOCK
        .lock()
        .map_err(|_| "清理记录暂时不可用".to_string())?;
    if plan_id.is_empty()
        || plan_id.len() > 128
        || !plan_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        || selected_rules.is_empty()
        || selected_rules.len() > 32
        || selected_rules.iter().any(|r| {
            r.id.is_empty()
                || r.id.len() > 64
                || !r
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                || r.title.is_empty()
                || r.title.len() > 256
                || r.title.contains(['/', '\\', '\n', '\r'])
        })
    {
        return Err("清理项目摘要无效，未执行清理".into());
    }
    let root = app_data.join(DIRECTORY);
    safe_path(&root)?;
    fs::create_dir_all(&root).map_err(|_| "无法创建清理记录，未执行清理".to_string())?;
    safe_path(&root)?;
    let existing = directories(&root)?;
    for obsolete in existing.iter().skip(MAX_RECORDS - 1) {
        // Never recurse: unexpected files or links stop pruning instead of crossing boundaries.
        for entry in
            fs::read_dir(obsolete).map_err(|_| "无法整理旧清理记录，未执行清理".to_string())?
        {
            let entry = entry.map_err(|_| "无法整理旧清理记录，未执行清理".to_string())?;
            safe_path(&entry.path())?;
            if !matches!(
                entry.file_name().to_str(),
                Some("begin.json" | "finish.json" | "begin.json.pending" | "finish.json.pending")
            ) || !entry
                .file_type()
                .map_err(|_| "无法整理旧清理记录，未执行清理".to_string())?
                .is_file()
            {
                return Err("旧清理记录包含无法识别的内容，未执行清理".into());
            }
        }
        for name in [
            "begin.json",
            "finish.json",
            "begin.json.pending",
            "finish.json.pending",
        ] {
            let file = obsolete.join(name);
            safe_path(&file)?;
            match fs::remove_file(file) {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(_) => return Err("无法整理旧清理记录，未执行清理".into()),
            }
        }
        fs::remove_dir(obsolete).map_err(|_| "无法整理旧清理记录，未执行清理".to_string())?;
    }
    let now = chrono::Utc::now();
    let id = format!(
        "record-{:020}-{:010}-{:016}",
        now.timestamp_millis(),
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let directory = root.join(&id);
    fs::create_dir(&directory).map_err(|_| "无法创建清理记录，未执行清理".to_string())?;
    let begin = Begin {
        permanent_delete: true,
        version: VERSION,
        id,
        plan_id: plan_id.into(),
        started_at: now.to_rfc3339(),
        selected_rules,
    };
    if let Err(error) = publish(&directory, "begin.json", &begin) {
        let _ = fs::remove_dir(&directory);
        return Err(format!("{error}；未执行清理"));
    }
    Ok(RecordToken { directory })
}

/// Raw failure text is deliberately never persisted; it may contain user paths.
pub fn finish_record(
    token: &RecordToken,
    summary: HistorySummary,
    failure: Option<&str>,
) -> Result<(), String> {
    let _guard = HISTORY_LOCK
        .lock()
        .map_err(|_| "清理记录暂时不可用".to_string())?;
    read_json::<Begin>(&token.directory.join("begin.json")).map_err(|_| {
        "清理已执行，但结果记录未能保存；原始记录无法读取，请勿据此重复清理".to_string()
    })?;
    let finish = Finish {
        version: VERSION,
        finished_at: chrono::Utc::now().to_rfc3339(),
        summary,
        failed: failure.is_some(),
    };
    publish(&token.directory, "finish.json", &finish)
        .map_err(|_| "清理已执行，但结果记录未能保存；记录中会显示未完成，请勿据此重复清理".into())
}

pub fn read_history(app_data: &Path) -> Result<CleanupHistory, String> {
    let _guard = HISTORY_LOCK
        .lock()
        .map_err(|_| "清理记录暂时不可用".to_string())?;
    let directories = directories(&app_data.join(DIRECTORY))?;
    let mut history = CleanupHistory {
        records: Vec::new(),
        warnings: Vec::new(),
    };
    if directories.len() > MAX_RECORDS {
        history.warnings.push("仅显示最近 100 次清理记录".into());
    }
    for directory in directories.into_iter().take(MAX_RECORDS) {
        let begin = match read_json::<Begin>(&directory.join("begin.json")) {
            Ok(begin) => begin,
            Err(reason) => {
                history
                    .warnings
                    .push(format!("有一条清理记录无法显示：{reason}"));
                continue;
            }
        };
        let mut record = HistoryRecord {
            permanent_delete: begin.permanent_delete,
            id: begin.id,
            plan_id: begin.plan_id,
            started_at: begin.started_at,
            finished_at: None,
            selected_rules: begin.selected_rules,
            status: "incomplete".into(),
            deleted_files: 0,
            deleted_bytes: 0,
            skipped_files: 0,
            failed_files: 0,
            message: Some("这次清理尚未完成，实际清理量可能未记录。".into()),
        };
        let finish_path = directory.join("finish.json");
        if finish_path
            .try_exists()
            .map_err(|_| "无法读取清理结果记录".to_string())?
        {
            match read_json::<Finish>(&finish_path) {
                Ok(finish) => {
                    let summary = finish.summary;
                    let failed = finish.failed || summary.failed_files > 0;
                    record.status = if failed && summary.deleted_files == 0 {
                        "failed"
                    } else if failed || summary.skipped_files > 0 {
                        "partial"
                    } else {
                        "completed"
                    }
                    .into();
                    record.message = match record.status.as_str() {
                        "failed" => Some("本次清理未能完成，请重新扫描后查看可清理项目。".into()),
                        "partial" => {
                            Some("已处理部分文件；运行中、发生变化或无法处理的文件已保留。".into())
                        }
                        _ => None,
                    };
                    record.finished_at = Some(finish.finished_at);
                    record.deleted_files = summary.deleted_files;
                    record.deleted_bytes = summary.deleted_bytes;
                    record.skipped_files = summary.skipped_files;
                    record.failed_files = summary.failed_files;
                }
                Err(reason) => history
                    .warnings
                    .push(format!("有一次清理的结果无法确认：{reason}")),
            }
        }
        history.records.push(record);
    }
    Ok(history)
}

#[tauri::command]
pub async fn list_cleanup_history(app: tauri::AppHandle) -> Result<CleanupHistory, String> {
    let app_data = crate::app_data_dir(&app);
    tokio::task::spawn_blocking(move || read_history(&app_data))
        .await
        .map_err(|_| "清理记录读取任务未完成，请重试".to_string())?
}

#[cfg(test)]
mod tests {
    #[test]
    fn legacy_recycle_records_keep_their_original_method_and_counts() {
        let begin: super::Begin = serde_json::from_value(serde_json::json!({
            "version": 1, "id": "record-old", "plan_id": "old",
            "started_at": "2026-10-25T00:00:00Z", "selected_rules": []
        }))
        .unwrap();
        let summary: super::HistorySummary = serde_json::from_value(serde_json::json!({
            "recycled_files": 3, "recycled_bytes": 2048,
            "skipped_files": 1, "failed_files": 0
        }))
        .unwrap();
        assert!(!begin.permanent_delete);
        assert_eq!(summary.deleted_files, 3);
        assert_eq!(summary.deleted_bytes, 2048);
        assert_eq!(summary.skipped_files, 1);
    }
    use super::*;
    fn rules() -> Vec<HistoryRule> {
        vec![HistoryRule {
            id: "temp".into(),
            title: "临时文件".into(),
        }]
    }
    #[test]
    fn begin_finish_and_interruption_are_distinct() {
        let temp = tempfile::tempdir().unwrap();
        let token = start_record(temp.path(), "plan-1", rules()).unwrap();
        let records = read_history(temp.path()).unwrap();
        assert_eq!(records.records[0].status, "incomplete");
        assert!(records.records[0].finished_at.is_none());
        finish_record(
            &token,
            HistorySummary {
                deleted_files: 2,
                deleted_bytes: 42,
                ..Default::default()
            },
            None,
        )
        .unwrap();
        let records = read_history(temp.path()).unwrap();
        assert_eq!(records.records[0].status, "completed");
        assert_eq!(records.records[0].deleted_bytes, 42);
        assert!(records.records[0].finished_at.is_some());
        assert!(finish_record(&token, HistorySummary::default(), None).is_err());
    }
    #[test]
    fn failures_and_partial_results_never_show_completed_or_leak_error() {
        for (files, skips, failures, failed, expected) in [
            (0, 0, 0, true, "failed"),
            (1, 0, 0, true, "partial"),
            (0, 0, 1, false, "failed"),
            (1, 0, 1, false, "partial"),
            (0, 1, 0, false, "partial"),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let token = start_record(temp.path(), "plan", rules()).unwrap();
            finish_record(
                &token,
                HistorySummary {
                    deleted_files: files,
                    skipped_files: skips,
                    failed_files: failures,
                    ..Default::default()
                },
                failed.then_some("C:\\secret\\key.txt"),
            )
            .unwrap();
            let records = read_history(temp.path()).unwrap();
            assert_eq!(records.records[0].status, expected);
            assert!(!serde_json::to_string(&records).unwrap().contains("secret"));
        }
    }
    #[test]
    fn corrupt_truncated_oversized_and_unknown_version_are_reported() {
        for content in [
            b"{\"version\":1,".to_vec(),
            b"not json".to_vec(),
            vec![b' '; MAX_FILE_BYTES as usize + 1],
            b"{\"version\":999}".to_vec(),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let token = start_record(temp.path(), "plan", rules()).unwrap();
            fs::write(token.directory.join("finish.json"), &content).unwrap();
            let records = read_history(temp.path()).unwrap();
            assert_eq!(records.records[0].status, "incomplete");
            assert_eq!(records.warnings.len(), 1);
            fs::write(token.directory.join("begin.json"), &content).unwrap();
            let records = read_history(temp.path()).unwrap();
            assert!(records.records.is_empty());
            assert_eq!(records.warnings.len(), 1);
        }
    }
    #[test]
    fn begin_read_and_finish_io_failures_are_explicit() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join(DIRECTORY), "file blocks directory").unwrap();
        assert!(start_record(temp.path(), "plan", rules()).is_err());
        assert!(read_history(temp.path()).is_err());
        fs::remove_file(temp.path().join(DIRECTORY)).unwrap();
        let token = start_record(temp.path(), "plan", rules()).unwrap();
        fs::create_dir(token.directory.join("finish.json.pending")).unwrap();
        assert!(finish_record(&token, HistorySummary::default(), None)
            .unwrap_err()
            .contains("清理已执行"));
        assert_eq!(
            read_history(temp.path()).unwrap().records[0].status,
            "incomplete"
        );
    }
    #[test]
    fn history_is_bounded_and_does_not_delete_unexpected_files() {
        let temp = tempfile::tempdir().unwrap();
        for _ in 0..105 {
            start_record(temp.path(), "plan", rules()).unwrap();
        }
        let history = read_history(temp.path()).unwrap();
        assert_eq!(history.records.len(), MAX_RECORDS);
        let dirs = directories(&temp.path().join(DIRECTORY)).unwrap();
        assert_eq!(dirs.len(), MAX_RECORDS);
        let oldest = dirs.last().unwrap();
        fs::write(oldest.join("keep.txt"), "untouched").unwrap();
        assert!(start_record(temp.path(), "plan", rules()).is_err());
        assert_eq!(
            fs::read_to_string(oldest.join("keep.txt")).unwrap(),
            "untouched"
        );
        assert!(oldest.join("begin.json").exists());
    }
    #[test]
    fn rejects_oversized_or_path_like_metadata_before_writing() {
        let temp = tempfile::tempdir().unwrap();
        assert!(start_record(temp.path(), "C:\\secret", rules()).is_err());
        assert!(start_record(
            temp.path(),
            "plan",
            vec![HistoryRule {
                id: "temp".into(),
                title: "x".repeat(257)
            }]
        )
        .is_err());
        assert!(read_history(temp.path()).unwrap().records.is_empty());
        let token = start_record(temp.path(), "plan", rules()).unwrap();
        assert!(publish(
            &token.directory,
            "large.json",
            &"x".repeat(MAX_FILE_BYTES as usize)
        )
        .is_err());
        assert!(!token.directory.join("large.json.pending").exists());
    }
    #[test]
    fn excessive_directory_entries_stop_bounded_reading() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(DIRECTORY);
        fs::create_dir(&root).unwrap();
        for index in 0..=MAX_DIRECTORY_ENTRIES {
            File::create(root.join(format!("extra-{index}"))).unwrap();
        }
        assert!(read_history(temp.path()).unwrap_err().contains("内容异常"));
        assert!(start_record(temp.path(), "plan", rules()).is_err());
    }
    #[cfg(windows)]
    #[test]
    fn rejects_junctioned_history_root_and_record_directory() {
        fn junction(link: &Path, target: &Path) {
            let output = std::process::Command::new("cmd")
                .args(["/c", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = temp.path().join(DIRECTORY);
        junction(&root, outside.path());
        assert!(start_record(temp.path(), "plan", rules()).is_err());
        assert!(read_history(temp.path()).is_err());
        fs::remove_dir(&root).unwrap();
        let token = start_record(temp.path(), "plan", rules()).unwrap();
        fs::remove_file(token.directory.join("begin.json")).unwrap();
        fs::remove_dir(&token.directory).unwrap();
        junction(&token.directory, outside.path());
        assert!(read_history(temp.path()).is_err());
        assert!(finish_record(&token, HistorySummary::default(), None).is_err());
        assert!(outside.path().read_dir().unwrap().next().is_none());
        fs::remove_dir(&token.directory).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_directory_and_record() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), temp.path().join(DIRECTORY)).unwrap();
        assert!(start_record(temp.path(), "plan", rules()).is_err());
        assert!(read_history(temp.path()).is_err());
    }
}
