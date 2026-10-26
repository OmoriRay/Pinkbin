use serde::Serialize;
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};
use tauri::{Manager, State};

const IDS: [&str; 14] = [
    "temp",
    "chrome",
    "edge",
    "brave",
    "firefox",
    "pip-cache",
    "npm-cache",
    "nvidia-shader",
    "crash-dumps",
    "cherry-studio-cache",
    "chatbox-cache",
    "lx-music-cache",
    "docker-desktop-cache",
    "opencode-cache",
];
const CHROMIUM_PROFILE_CACHES: [&str; 7] = [
    "Cache/Cache_Data",
    "Code Cache",
    "GPUCache",
    "DawnCache",
    "DawnGraphiteCache",
    "DawnWebGPUCache",
    "Media Cache",
];
const CHROMIUM_SHARED_CACHES: [&str; 4] = [
    "ShaderCache",
    "GrShaderCache",
    "GraphiteDawnCache",
    "GPUPersistentCache",
];
const ELECTRON_CACHE_ROOTS: [&str; 5] = [
    "Cache/Cache_Data",
    "Code Cache",
    "GPUCache",
    "DawnGraphiteCache",
    "DawnWebGPUCache",
];
const MAX_ENTRIES_PER_RULE: usize = 50_000;
const MAX_CANDIDATES_PER_RULE: usize = 20_000;
const MAX_ACTIONS: usize = 10_000;
const PREVIEW_TTL: Duration = Duration::from_secs(15 * 60);

#[derive(Default, Clone)]
pub struct CacheCleanerState(Arc<Mutex<Inner>>);
#[derive(Default)]
struct Inner {
    session: Option<CacheScanSession>,
    snapshot: Option<Snapshot>,
    executing: bool,
    generation: u64,
}

#[derive(Clone, Serialize)]
pub struct CacheScanSession {
    scan_id: String,
    status: &'static str,
    #[serde(serialize_with = "crate::serialize_shared_option")]
    preview: Option<Arc<Preview>>,
    error: Option<String>,
}

fn invalidate_session(inner: &mut Inner, message: &str) {
    if let Some(session) = inner.session.as_mut() {
        session.status = "failed";
        session.preview = None;
        session.error = Some(message.into());
    }
}

fn session_snapshot(state: &CacheCleanerState) -> Result<Option<CacheScanSession>, String> {
    let mut inner = state.0.lock().map_err(err)?;
    if inner
        .session
        .as_ref()
        .is_some_and(|s| s.status == "completed")
    {
        let valid = inner.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot
                .created
                .elapsed()
                .is_ok_and(|age| age <= PREVIEW_TTL)
                && inner
                    .session
                    .as_ref()
                    .and_then(|s| s.preview.as_ref())
                    .is_some_and(|preview| preview.plan_id == snapshot.id)
        });
        if !valid {
            inner.snapshot = None;
            invalidate_session(&mut inner, "预览已过期或已使用，请重新扫描");
        }
    }
    Ok(inner.session.clone())
}

#[tauri::command]
pub fn get_cache_cleanup_scan(
    state: State<'_, CacheCleanerState>,
) -> Result<Option<CacheScanSession>, String> {
    session_snapshot(state.inner())
}

#[tauri::command]
pub fn start_cache_cleanup_scan(
    app: tauri::AppHandle,
    state: State<'_, CacheCleanerState>,
) -> Result<CacheScanSession, String> {
    if let Some(session) = session_snapshot(state.inner())?.filter(|s| s.status == "running") {
        return Ok(session);
    }
    let operation = crate::disk_cleaner::reserve_operation()?;
    let base = app.path().local_data_dir().map_err(err)?;
    let roaming = app.path().data_dir().map_err(err)?;
    let shared = state.inner().clone();
    let generation = begin_scan(&shared)?;
    let session = CacheScanSession {
        scan_id: format!("cache-{generation}"),
        status: "running",
        preview: None,
        error: None,
    };
    shared.0.lock().map_err(err)?.session = Some(session.clone());
    tauri::async_runtime::spawn(async move {
        // Keep the shared operation lease through publication, even on worker panic.
        let _operation = operation;
        let outcome = tokio::task::spawn_blocking(move || {
            let (snapshot, preview) = scan_with_roaming(&base, Some(&roaming), browser_running);
            Ok::<_, String>((snapshot?, preview))
        })
        .await
        .map_err(|e| format!("扫描任务异常：{e}"))
        .and_then(|r| r);
        if let Ok(mut inner) = shared.0.lock() {
            if inner.generation != generation || inner.executing {
                return;
            }
            match outcome {
                Ok((snapshot, preview)) => {
                    inner.snapshot = Some(snapshot);
                    if let Some(session) = inner.session.as_mut() {
                        session.status = "completed";
                        session.preview = Some(Arc::new(preview));
                    }
                }
                Err(error) => {
                    inner.snapshot = None;
                    invalidate_session(&mut inner, &error);
                }
            }
        }
    });
    Ok(session)
}
struct Snapshot {
    id: String,
    created: SystemTime,
    base: PathBuf,
    base_stamp: Stamp,
    roaming: Option<(PathBuf, Stamp)>,
    candidates: Vec<Candidate>,
}
#[derive(Clone)]
struct Candidate {
    rule: String,
    root: PathBuf,
    root_stamp: Stamp,
    path: PathBuf,
    stamp: Stamp,
}
#[derive(Clone, PartialEq, Eq, Debug)]
struct Stamp {
    len: u64,
    modified: SystemTime,
    created: Option<SystemTime>,
    identity: (u64, u64),
}

#[derive(Serialize)]
pub struct Preview {
    plan_id: String,
    created_at: String,
    rules: Vec<Rule>,
    warnings: Vec<String>,
}
#[derive(Serialize)]
pub struct Rule {
    id: String,
    title: String,
    description: String,
    consequence: String,
    retention_days: u64,
    file_count: u64,
    bytes: u64,
    samples: Vec<String>,
    status: String,
    message: Option<String>,
}
#[derive(Serialize, Default)]
pub struct CleanupResult {
    recycled_files: u64,
    recycled_bytes: u64,
    skipped_files: u64,
    failed_files: u64,
    messages: Vec<String>,
    log_path: String,
    history_warning: Option<String>,
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

#[tauri::command]
pub async fn preview_cache_cleanup(
    app: tauri::AppHandle,
    state: State<'_, CacheCleanerState>,
) -> Result<Preview, String> {
    let operation = crate::disk_cleaner::reserve_operation()?;
    let base = app.path().local_data_dir().map_err(err)?;
    let roaming = app.path().data_dir().map_err(err)?;
    let shared = state.inner().clone();
    let generation = begin_scan(&shared)?;
    tokio::task::spawn_blocking(move || {
        let _operation = operation;
        let (snapshot, preview) = scan_with_roaming(&base, Some(&roaming), browser_running);
        let snapshot = snapshot?;
        finish_scan(&shared, generation, snapshot)?;
        Ok(preview)
    })
    .await
    .map_err(err)?
}

fn begin_scan(state: &CacheCleanerState) -> Result<u64, String> {
    let mut inner = state.0.lock().map_err(err)?;
    if inner.executing {
        return Err("正在清理，请稍后再扫描".into());
    }
    inner.generation = inner.generation.wrapping_add(1);
    inner.snapshot = None;
    inner.session = None;
    Ok(inner.generation)
}

fn finish_scan(
    state: &CacheCleanerState,
    generation: u64,
    snapshot: Snapshot,
) -> Result<(), String> {
    let mut inner = state.0.lock().map_err(err)?;
    if inner.generation != generation || inner.executing {
        return Err("此扫描已被更新的请求替代，请使用最新预览".into());
    }
    inner.snapshot = Some(snapshot);
    Ok(())
}

#[tauri::command]
pub async fn execute_cache_cleanup(
    app: tauri::AppHandle,
    state: State<'_, CacheCleanerState>,
    plan_id: String,
    rule_ids: Vec<String>,
) -> Result<CleanupResult, String> {
    let operation = crate::disk_cleaner::reserve_operation()?;
    let base = app.path().local_data_dir().map_err(err)?;
    let roaming = app.path().data_dir().map_err(err)?;
    let app_data = crate::app_data_dir(&app);
    let log = app_data.join("cache-cleanup.jsonl");
    let shared = state.inner().clone();
    let snapshot = take_snapshot(&shared, &plan_id, &rule_ids)?;
    tokio::task::spawn_blocking(move || {
        let _operation = operation;
        struct Release(CacheCleanerState);
        impl Drop for Release {
            fn drop(&mut self) {
                if let Ok(mut x) = self.0 .0.lock() {
                    x.executing = false;
                }
            }
        }
        let _release = Release(shared);
        execute_recorded_snapshot_with_roaming(
            snapshot,
            &base,
            Some(&roaming),
            &rule_ids,
            &log,
            &app_data,
            browser_running,
            crate::recycle_only::recycle,
        )
    })
    .await
    .map_err(err)?
}

#[cfg(test)]
fn execute_recorded_snapshot(
    snapshot: Snapshot,
    base: &Path,
    ids: &[String],
    log: &Path,
    app_data: &Path,
    running: impl Fn(&str) -> Result<bool, String>,
    recycle: impl FnMut(&Path) -> Result<(), String>,
) -> Result<CleanupResult, String> {
    execute_recorded_snapshot_with_roaming(
        snapshot, base, None, ids, log, app_data, running, recycle,
    )
}

// Storage roots and injected I/O callbacks stay explicit at this boundary.
#[allow(clippy::too_many_arguments)]
fn execute_recorded_snapshot_with_roaming(
    snapshot: Snapshot,
    base: &Path,
    roaming: Option<&Path>,
    ids: &[String],
    log: &Path,
    app_data: &Path,
    running: impl Fn(&str) -> Result<bool, String>,
    recycle: impl FnMut(&Path) -> Result<(), String>,
) -> Result<CleanupResult, String> {
    let plan_id = snapshot.id.clone();
    let mut seen = HashSet::new();
    let rules = ids
        .iter()
        .filter(|id| seen.insert(id.as_str()))
        .map(|id| crate::cleanup_history::HistoryRule {
            id: id.clone(),
            title: rule_title(id).to_owned(),
        })
        .collect();
    let record = crate::cleanup_history::start_record(app_data, &plan_id, rules)
        .map_err(|e| format!("未能保存清理记录，本次未执行清理，请重新扫描。{e}"))?;
    let mut outcome =
        execute_snapshot_with_roaming(snapshot, base, roaming, ids, log, running, recycle);
    let (summary, failure) = match &outcome {
        Ok(result) => (
            crate::cleanup_history::HistorySummary {
                recycled_files: result.recycled_files,
                recycled_bytes: result.recycled_bytes,
                skipped_files: result.skipped_files,
                failed_files: result.failed_files,
            },
            result
                .messages
                .iter()
                .any(|message| message.contains("日志") && message.contains("失败"))
                .then_some("清理日志未能完整保存"),
        ),
        Err(_) => (
            crate::cleanup_history::HistorySummary::default(),
            Some("清理未能完成"),
        ),
    };
    if crate::cleanup_history::finish_record(&record, summary, failure).is_err() {
        let warning = "本次清理结果未能保存到清理记录，请以当前结果为准。";
        match &mut outcome {
            Ok(result) => result.history_warning = Some(warning.into()),
            Err(error) => error.push_str(&format!("；{warning}")),
        }
    }
    outcome
}

fn take_snapshot(state: &CacheCleanerState, id: &str, ids: &[String]) -> Result<Snapshot, String> {
    if ids.is_empty()
        || ids
            .iter()
            .any(|id| !IDS.contains(&id.as_str()) || id == "nvidia-shader")
    {
        return Err("请选择有效的清理项目".into());
    }
    let mut inner = state.0.lock().map_err(err)?;
    if inner.executing {
        return Err("已有清理正在执行".into());
    }
    if inner.snapshot.as_ref().map(|p| p.id.as_str()) != Some(id) {
        return Err("预览已失效，请重新扫描".into());
    }
    if inner
        .snapshot
        .as_ref()
        .is_some_and(|s| s.created.elapsed().map_or(true, |age| age > PREVIEW_TTL))
    {
        inner.snapshot = None;
        invalidate_session(&mut inner, "预览已超过 15 分钟，请重新扫描");
        return Err("预览已超过 15 分钟，请重新扫描".into());
    }
    inner.executing = true;
    invalidate_session(&mut inner, "预览已使用，请重新扫描");
    Ok(inner.snapshot.take().unwrap())
}

fn no_links(path: &Path) -> io::Result<()> {
    for ancestor in path.ancestors() {
        let meta = fs::symlink_metadata(ancestor)?;
        if meta.file_type().is_symlink() {
            return Err(io::Error::other("跳过符号链接"));
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if meta.file_attributes() & 0x400 != 0 {
                return Err(io::Error::other("跳过重解析点"));
            }
        }
    }
    Ok(())
}

fn stamp(path: &Path) -> io::Result<Stamp> {
    no_links(path)?;
    let file = File::open(path)?;
    let m = file.metadata()?;
    if !m.is_file() && !m.is_dir() {
        return Err(io::Error::other("不是普通文件或目录"));
    }
    #[cfg(windows)]
    let identity = {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        (
            info.dwVolumeSerialNumber as u64,
            ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
        )
    };
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        (m.dev(), m.ino())
    };
    #[cfg(not(any(unix, windows)))]
    let identity = (0, 0);
    Ok(Stamp {
        len: m.len(),
        modified: m.modified()?,
        created: m.created().ok(),
        identity,
    })
}

fn directory_stamp(path: &Path) -> io::Result<Stamp> {
    #[cfg(windows)]
    {
        use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };
        no_links(path)?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(0x02000000 | 0x00200000)
            .open(path)?;
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let m = file.metadata()?;
        if !m.is_dir() {
            return Err(io::Error::other("不是目录"));
        }
        Ok(Stamp {
            len: 0,
            modified: SystemTime::UNIX_EPOCH,
            created: m.created().ok(),
            identity: (
                info.dwVolumeSerialNumber as u64,
                ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
            ),
        })
    }
    #[cfg(not(windows))]
    {
        if !fs::symlink_metadata(path)?.is_dir() {
            return Err(io::Error::other("不是目录"));
        }
        let mut s = stamp(path)?;
        s.len = 0;
        s.modified = SystemTime::UNIX_EPOCH;
        Ok(s)
    }
}

#[cfg(windows)]
fn browser_running(name: &str) -> Result<bool, String> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, GetLastError, ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE},
        System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        },
    };
    unsafe {
        let handle = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if handle == INVALID_HANDLE_VALUE {
            return Err("无法检查浏览器进程，本项已阻止".into());
        }
        struct Handle(windows_sys::Win32::Foundation::HANDLE);
        impl Drop for Handle {
            fn drop(&mut self) {
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
        let _handle = Handle(handle);
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(handle, &mut entry) == 0 {
            return Err("无法读取浏览器进程，本项已阻止".into());
        }
        loop {
            let end = entry
                .szExeFile
                .iter()
                .position(|x| *x == 0)
                .unwrap_or(entry.szExeFile.len());
            if String::from_utf16_lossy(&entry.szExeFile[..end]).eq_ignore_ascii_case(name) {
                return Ok(true);
            }
            if Process32NextW(handle, &mut entry) == 0 {
                return if GetLastError() == ERROR_NO_MORE_FILES {
                    Ok(false)
                } else {
                    Err("进程检查不完整，本项已阻止".into())
                };
            }
        }
    }
}
#[cfg(not(windows))]
fn browser_running(_: &str) -> Result<bool, String> {
    Err("当前仅支持 Windows 缓存清理".into())
}

fn profile_name(name: &str) -> bool {
    name == "Default"
        || name
            .strip_prefix("Profile ")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}
fn browser_root(base: &Path, id: &str) -> PathBuf {
    base.join(match id {
        "chrome" => "Google/Chrome/User Data",
        "edge" => "Microsoft/Edge/User Data",
        "brave" => "BraveSoftware/Brave-Browser/User Data",
        "firefox" => "Mozilla/Firefox/Profiles",
        _ => unreachable!("only browser rules have profile roots"),
    })
}
fn electron_app(id: &str) -> Option<&'static str> {
    match id {
        "cherry-studio-cache" => Some("CherryStudio"),
        "chatbox-cache" => Some("xyz.chatboxapp.app"),
        "lx-music-cache" => Some("lx-music-desktop"),
        "docker-desktop-cache" => Some("Docker Desktop"),
        "opencode-cache" => Some("ai.opencode.desktop"),
        _ => None,
    }
}
fn roots(base: &Path, id: &str) -> io::Result<Vec<PathBuf>> {
    if let Some(app) = electron_app(id) {
        return Ok(ELECTRON_CACHE_ROOTS
            .iter()
            .map(|sub| base.join(app).join(sub))
            .collect());
    }
    if id == "temp" {
        return Ok(vec![base.join("Temp")]);
    }
    if id == "crash-dumps" {
        return Ok(vec![base.join("CrashDumps")]);
    }
    if id == "pip-cache" {
        return Ok(["http-v2", "http", "wheels"]
            .iter()
            .map(|sub| base.join("pip/Cache").join(sub))
            .collect());
    }
    if id == "npm-cache" {
        return Ok(["content-v2", "index-v5"]
            .iter()
            .map(|sub| base.join("npm-cache/_cacache").join(sub))
            .collect());
    }
    if id == "nvidia-shader" {
        return Ok(["DXCache", "GLCache"]
            .iter()
            .map(|sub| base.join("NVIDIA").join(sub))
            .collect());
    }
    if !["chrome", "edge", "brave", "firefox"].contains(&id) {
        return Err(io::Error::other("未知清理规则"));
    }
    let browser = browser_root(base, id);
    match fs::symlink_metadata(&browser) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e),
    }
    no_links(&browser)?;
    let mut result = if id == "firefox" {
        Vec::new()
    } else {
        CHROMIUM_SHARED_CACHES
            .iter()
            .map(|sub| browser.join(sub))
            .collect()
    };
    for (index, entry) in fs::read_dir(browser)?.enumerate() {
        if index >= 1000 {
            return Err(io::Error::other("浏览器目录条目超过安全上限"));
        }
        let entry = entry?;
        if id == "firefox" || profile_name(&entry.file_name().to_string_lossy()) {
            no_links(&entry.path())?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let subdirs: &[&str] = if id == "firefox" {
                &["cache2/entries"]
            } else {
                &CHROMIUM_PROFILE_CACHES
            };
            for sub in subdirs {
                result.push(entry.path().join(sub));
            }
        }
    }
    result.sort();
    Ok(result)
}

fn retention_days(id: &str) -> Option<u64> {
    match id {
        "chrome"
        | "edge"
        | "brave"
        | "firefox"
        | "nvidia-shader"
        | "cherry-studio-cache"
        | "chatbox-cache"
        | "lx-music-cache"
        | "docker-desktop-cache"
        | "opencode-cache" => Some(0),
        "temp" => Some(7),
        "pip-cache" | "npm-cache" | "crash-dumps" => Some(30),
        _ => None,
    }
}

pub(super) fn rule_title(id: &str) -> &str {
    match id {
        "temp" => "用户临时文件",
        "cherry-studio-cache" => "Cherry Studio 缓存",
        "chatbox-cache" => "Chatbox 缓存",
        "lx-music-cache" => "洛雪音乐缓存",
        "docker-desktop-cache" => "Docker Desktop 缓存",
        "opencode-cache" => "OpenCode 缓存",
        "chrome" => "Chrome 缓存",
        "edge" => "Edge 缓存",
        "brave" => "Brave 缓存",
        "firefox" => "Firefox 缓存",
        "pip-cache" => "Python 下载缓存",
        "npm-cache" => "npm 下载缓存",
        "nvidia-shader" => "NVIDIA 缓存空间参考",
        "crash-dumps" => "崩溃转储",
        _ => "未知清理项目",
    }
}

fn processes(id: &str) -> (&'static str, &'static [&'static str]) {
    match id {
        "cherry-studio-cache" => ("Cherry Studio", &["Cherry Studio.exe", "CherryStudio.exe"]),
        "docker-desktop-cache" => ("Docker Desktop", &["Docker Desktop.exe"]),
        "opencode-cache" => ("OpenCode", &["OpenCode.exe"]),
        "lx-music-cache" => ("洛雪音乐", &["lx-music-desktop.exe"]),
        "chatbox-cache" => ("Chatbox", &["Chatbox.exe", "Chatbox CE.exe"]),
        "chrome" => ("Chrome", &["chrome.exe"]),
        "edge" => ("Edge", &["msedge.exe"]),
        "brave" => ("Brave", &["brave.exe"]),
        "firefox" => ("Firefox", &["firefox.exe"]),
        "pip-cache" => (
            "Python 和 pip",
            &[
                "python.exe",
                "pythonw.exe",
                "python3.exe",
                "py.exe",
                "pip.exe",
                "pip3.exe",
            ],
        ),
        _ => ("", &[]),
    }
}

fn processes_closed(
    id: &str,
    running: &impl Fn(&str) -> Result<bool, String>,
) -> Result<(), String> {
    let (name, executables) = processes(id);
    for executable in executables {
        if running(executable)? {
            return Err(format!("请先关闭 {name} 后重新扫描"));
        }
    }
    Ok(())
}

fn eligible(path: &Path, id: &str, modified: SystemTime, now: SystemTime) -> bool {
    let Some(days) = retention_days(id) else {
        return false;
    };
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if ["db", "db-wal", "db-shm", "sqlite", "sqlite3"]
        .iter()
        .any(|x| ext.eq_ignore_ascii_case(x))
    {
        return false;
    }
    if id == "crash-dumps" && !ext.eq_ignore_ascii_case("dmp") {
        return false;
    }
    if id == "firefox"
        && !path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| {
                name.len() == 40
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b))
            })
    {
        return false;
    }
    now.duration_since(modified)
        .is_ok_and(|age| age >= Duration::from_secs(days * 86400))
}

#[cfg(test)]
fn scan(
    base: &Path,
    running: impl Fn(&str) -> Result<bool, String>,
) -> (Result<Snapshot, String>, Preview) {
    scan_with_roaming(base, None, running)
}

fn scan_with_roaming(
    base: &Path,
    roaming: Option<&Path>,
    running: impl Fn(&str) -> Result<bool, String>,
) -> (Result<Snapshot, String>, Preview) {
    scan_with_limits_with_roaming(
        base,
        roaming,
        running,
        MAX_ENTRIES_PER_RULE,
        MAX_CANDIDATES_PER_RULE,
        Duration::from_secs(5),
    )
}

#[cfg(test)]
fn scan_with_limits(
    base: &Path,
    running: impl Fn(&str) -> Result<bool, String>,
    entry_limit: usize,
    candidate_limit: usize,
    time_limit: Duration,
) -> (Result<Snapshot, String>, Preview) {
    scan_with_limits_with_roaming(
        base,
        None,
        running,
        entry_limit,
        candidate_limit,
        time_limit,
    )
}

fn scan_with_limits_with_roaming(
    base: &Path,
    roaming: Option<&Path>,
    running: impl Fn(&str) -> Result<bool, String>,
    entry_limit: usize,
    candidate_limit: usize,
    time_limit: Duration,
) -> (Result<Snapshot, String>, Preview) {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let now = SystemTime::now();
    let id = format!(
        "{}-{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let mut preview = Preview {
        plan_id: id.clone(),
        created_at: chrono::Utc::now().to_rfc3339(),
        rules: vec![],
        warnings: vec![],
    };
    let base_stamp = match directory_stamp(base) {
        Ok(s) => s,
        Err(e) => return (Err(format!("无法验证本地数据目录：{e}")), preview),
    };
    let roaming_anchor = roaming.and_then(|p| {
        directory_stamp(p)
            .ok()
            .map(|stamp| (p.to_path_buf(), stamp))
    });
    let mut candidates = Vec::new();
    for id in IDS {
        let start = Instant::now();
        let mut visited = 0;
        let mut limited = false;
        let title = rule_title(id);
        let mut rule = Rule {
            id: id.into(),
            title: title.into(),
            description: match id {
                "temp" => "仅清理超过 7 天的用户临时文件",
                "crash-dumps" => "仅清理超过 30 天的 .dmp 文件",
                "pip-cache" => "仅清理超过 30 天的默认 Python 下载和构建缓存",
                "npm-cache" => "仅清理超过 30 天的 npm 下载缓存，不涉及已安装的依赖",
                "nvidia-shader" => "仅查看 NVIDIA DirectX 和 OpenGL 着色器缓存占用",
                "lx-music-cache" => "应用关闭后清理默认网页、代码和 GPU 缓存，不涉及歌单或下载目录",
                "docker-desktop-cache" => {
                    "应用关闭后清理默认界面缓存，不涉及镜像、容器、卷或虚拟磁盘"
                }
                "cherry-studio-cache" | "chatbox-cache" | "opencode-cache" => {
                    "应用关闭后清理默认目录中的网页、代码和 GPU 缓存，不涉及聊天或账号数据"
                }
                _ => "浏览器关闭后清理可重新生成的网页缓存",
            }
            .into(),
            consequence: match id {
                "lx-music-cache" => "移入回收站；页面与音频缓存可能需要重新下载，首次打开可能稍慢",
                "crash-dumps" => "旧崩溃诊断资料将移入回收站",
                "pip-cache" => "以后安装可能需要重新下载或构建；依赖这些缓存的离线安装可能无法进行",
                "npm-cache" => "以后安装可能需要重新下载；依赖缓存的离线安装可能无法进行",
                "nvidia-shader" => {
                    "清除后游戏首次运行可能重新编译着色器并出现短暂卡顿；需遵循显卡官方清理流程"
                }
                _ => "移入回收站；应用可能重新生成缓存，首次打开可能稍慢",
            }
            .into(),
            retention_days: retention_days(id).unwrap(),
            file_count: 0,
            bytes: 0,
            samples: vec![],
            status: "empty".into(),
            message: None,
        };
        let blocked = if id == "nvidia-shader" {
            Some("请按 NVIDIA 官方指引清理；当前仅查看占用".to_owned())
        } else {
            processes_closed(id, &running).err()
        };
        let anchor = if electron_app(id).is_some() {
            match roaming_anchor.as_ref() {
                Some((path, _)) => path.as_path(),
                None => {
                    rule.message = Some("未能验证系统漫游数据目录".into());
                    rule.status = if roaming.is_some() {
                        "blocked"
                    } else {
                        "empty"
                    }
                    .into();
                    preview.rules.push(rule);
                    continue;
                }
            }
        } else {
            base
        };
        let roots = match roots(anchor, id) {
            Ok(r) => r,
            Err(e) => {
                rule.status = "blocked".into();
                rule.message = Some(format!("目录不可安全读取：{e}"));
                preview.rules.push(rule);
                continue;
            }
        };
        let mut found_root = false;
        let mut incomplete = false;
        for root in roots {
            match fs::symlink_metadata(&root) {
                Ok(_) => {
                    found_root = true;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => {
                    incomplete = true;
                    preview.warnings.push(format!("{title}：无法检查目录：{e}"));
                    continue;
                }
            }
            let root_stamp = match directory_stamp(&root) {
                Ok(s) => s,
                Err(e) => {
                    incomplete = true;
                    preview
                        .warnings
                        .push(format!("{title}：跳过不可验证或链接目录：{e}"));
                    continue;
                }
            };
            let mut stack = vec![root.clone()];
            while let Some(dir) = stack.pop() {
                if visited >= entry_limit
                    || rule.file_count >= candidate_limit as u64
                    || start.elapsed() > time_limit
                {
                    incomplete = true;
                    limited = true;
                    break;
                }
                if no_links(&dir).is_err() {
                    incomplete = true;
                    preview
                        .warnings
                        .push(format!("{title}：链接或不可验证的目录已跳过"));
                    continue;
                }
                let entries = match fs::read_dir(&dir) {
                    Ok(e) => e,
                    Err(e) => {
                        incomplete = true;
                        preview
                            .warnings
                            .push(format!("{title}：部分目录无法访问：{e}"));
                        continue;
                    }
                };
                for entry in entries {
                    visited += 1;
                    if visited > entry_limit
                        || rule.file_count >= candidate_limit as u64
                        || start.elapsed() > time_limit
                    {
                        incomplete = true;
                        limited = true;
                        break;
                    }
                    let entry = match entry {
                        Ok(e) => e,
                        Err(e) => {
                            incomplete = true;
                            preview
                                .warnings
                                .push(format!("{title}：无法读取部分目录条目：{e}"));
                            continue;
                        }
                    };
                    let path = entry.path();
                    if no_links(&path).is_err() {
                        incomplete = true;
                        preview
                            .warnings
                            .push(format!("{title}：部分链接或不可验证的条目已跳过"));
                        continue;
                    }
                    let kind = match entry.file_type() {
                        Ok(k) => k,
                        Err(e) => {
                            incomplete = true;
                            preview
                                .warnings
                                .push(format!("{title}：无法读取文件类型：{e}"));
                            continue;
                        }
                    };
                    if kind.is_dir() {
                        if id != "crash-dumps" && id != "firefox" {
                            stack.push(path);
                        }
                        continue;
                    }
                    if !kind.is_file() {
                        continue;
                    }
                    // 运行中的程序可能独占文件；只读测量无需打开文件或创建执行身份。
                    let metadata = if blocked.is_some() {
                        fs::symlink_metadata(&path).and_then(|m| Ok((m.modified()?, m.len(), None)))
                    } else {
                        stamp(&path).map(|s| (s.modified, s.len, Some(s)))
                    };
                    let Ok((modified, len, file_stamp)) = metadata else {
                        incomplete = true;
                        preview
                            .warnings
                            .push(format!("{title}：部分文件无法读取元数据，已跳过"));
                        continue;
                    };
                    if !eligible(&path, id, modified, now) {
                        continue;
                    }
                    rule.file_count += 1;
                    rule.bytes = rule.bytes.saturating_add(len);
                    if rule.samples.len() < 5 {
                        rule.samples.push(path.to_string_lossy().into());
                    }
                    if let Some(stamp) = file_stamp {
                        candidates.push(Candidate {
                            rule: id.into(),
                            root: root.clone(),
                            root_stamp: root_stamp.clone(),
                            path,
                            stamp,
                        });
                    }
                }
            }
        }
        if rule.file_count > 0 {
            rule.status = "ready".into();
        }
        if incomplete {
            rule.message = Some("部分目录或文件未能扫描；仅列出已安全验证的文件".into());
            if rule.file_count == 0 {
                rule.status = "blocked".into();
            }
        } else if !found_root {
            rule.message = Some("未找到此缓存目录".into());
        }
        if limited {
            preview.warnings.push(format!(
                "{title}：达到本项扫描上限，仅显示部分结果；清理后可再次扫描"
            ));
        }
        if let Some(message) = blocked {
            rule.status = "blocked".into();
            rule.message = Some(message);
        }
        preview.rules.push(rule);
    }
    preview.warnings.sort();
    preview.warnings.dedup();
    (
        Ok(Snapshot {
            id,
            created: now,
            base: base.into(),
            base_stamp,
            roaming: roaming_anchor,
            candidates,
        }),
        preview,
    )
}

fn log_event(
    file: &mut File,
    plan: &str,
    path: &Path,
    status: &str,
    detail: &str,
) -> Result<(), String> {
    let event = serde_json::json!({"at":chrono::Utc::now().to_rfc3339(),"plan_id":plan,"path":path,"status":status,"detail":detail});
    serde_json::to_writer(&mut *file, &event).map_err(err)?;
    file.write_all(b"\n").map_err(err)?;
    file.flush().map_err(err)?;
    file.sync_data().map_err(err)
}

#[cfg(test)]
fn execute_snapshot(
    snapshot: Snapshot,
    current_base: &Path,
    ids: &[String],
    log: &Path,
    running: impl Fn(&str) -> Result<bool, String>,
    recycle: impl FnMut(&Path) -> Result<(), String>,
) -> Result<CleanupResult, String> {
    execute_snapshot_with_roaming(snapshot, current_base, None, ids, log, running, recycle)
}

fn execute_snapshot_with_roaming(
    snapshot: Snapshot,
    current_base: &Path,
    roaming: Option<&Path>,
    ids: &[String],
    log: &Path,
    running: impl Fn(&str) -> Result<bool, String>,
    mut recycle: impl FnMut(&Path) -> Result<(), String>,
) -> Result<CleanupResult, String> {
    execute_snapshot_with_logger_with_roaming(
        snapshot,
        current_base,
        roaming,
        ids,
        log,
        running,
        &mut recycle,
        log_event,
    )
}

#[cfg(test)]
fn execute_snapshot_with_logger(
    snapshot: Snapshot,
    current_base: &Path,
    ids: &[String],
    log: &Path,
    running: impl Fn(&str) -> Result<bool, String>,
    recycle: impl FnMut(&Path) -> Result<(), String>,
    logger: impl FnMut(&mut File, &str, &Path, &str, &str) -> Result<(), String>,
) -> Result<CleanupResult, String> {
    execute_snapshot_with_logger_with_roaming(
        snapshot,
        current_base,
        None,
        ids,
        log,
        running,
        recycle,
        logger,
    )
}

// Storage roots and injected I/O callbacks stay explicit at this boundary.
#[allow(clippy::too_many_arguments)]
fn execute_snapshot_with_logger_with_roaming(
    snapshot: Snapshot,
    current_base: &Path,
    roaming: Option<&Path>,
    ids: &[String],
    log: &Path,
    running: impl Fn(&str) -> Result<bool, String>,
    mut recycle: impl FnMut(&Path) -> Result<(), String>,
    mut logger: impl FnMut(&mut File, &str, &Path, &str, &str) -> Result<(), String>,
) -> Result<CleanupResult, String> {
    if snapshot.base != current_base
        || directory_stamp(current_base).ok().as_ref() != Some(&snapshot.base_stamp)
    {
        return Err("本地数据目录已改变，请重新扫描".into());
    }
    if snapshot.roaming.as_ref().is_some_and(|(path, saved)| {
        roaming != Some(path.as_path()) || directory_stamp(path).ok().as_ref() != Some(saved)
    }) {
        return Err("漫游数据目录已改变，请重新扫描".into());
    }
    let parent = log.parent().ok_or("日志路径无效")?;
    fs::create_dir_all(parent).map_err(|e| format!("无法创建日志目录，未执行清理：{e}"))?;
    no_links(parent).map_err(err)?;
    if log.exists() {
        no_links(log).map_err(err)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .map_err(|e| format!("无法打开日志，未执行清理：{e}"))?;
    let mut result = CleanupResult {
        log_path: log.to_string_lossy().into(),
        ..Default::default()
    };
    let selected: HashSet<&str> = ids.iter().map(String::as_str).collect();
    let start = Instant::now();
    let mut actions = 0;
    let mut stopped = false;
    for c in snapshot
        .candidates
        .iter()
        .filter(|c| selected.contains(c.rule.as_str()))
    {
        if stopped {
            result.skipped_files += 1;
            continue;
        }
        if actions >= MAX_ACTIONS || start.elapsed() > Duration::from_secs(60) {
            result
                .messages
                .push("清理达到数量或时间上限，其余文件已跳过，请重新扫描".into());
            stopped = true;
            result.skipped_files += 1;
            continue;
        }
        let browser_ok = processes_closed(&c.rule, &running).is_ok();
        let anchor = if electron_app(&c.rule).is_some() {
            match snapshot.roaming.as_ref() {
                Some((path, _)) => path.as_path(),
                None => {
                    result.skipped_files += 1;
                    continue;
                }
            }
        } else {
            current_base
        };
        let valid_root = roots(anchor, &c.rule).is_ok_and(|r| r.contains(&c.root));
        if !browser_ok
            || !valid_root
            || !c.path.starts_with(&c.root)
            || directory_stamp(&c.root).ok().as_ref() != Some(&c.root_stamp)
            || stamp(&c.path).ok().as_ref() != Some(&c.stamp)
            || !eligible(&c.path, &c.rule, c.stamp.modified, SystemTime::now())
        {
            result.skipped_files += 1;
            continue;
        }
        if let Err(e) = logger(
            &mut file,
            &snapshot.id,
            &c.path,
            "pending",
            "准备移入回收站",
        ) {
            result
                .messages
                .push(format!("日志写入失败，已停止清理：{e}"));
            result.skipped_files += 1;
            stopped = true;
            continue;
        }
        actions += 1;
        // 回收 API 依路径操作，因此尽量缩短最终验证和调用之间的窗口。
        let browser_still_closed = processes_closed(&c.rule, &running).is_ok();
        if !browser_still_closed
            || snapshot.roaming.as_ref().is_some_and(|(path, saved)| {
                roaming != Some(path.as_path())
                    || directory_stamp(path).ok().as_ref() != Some(saved)
            })
            || directory_stamp(current_base).ok().as_ref() != Some(&snapshot.base_stamp)
            || directory_stamp(&c.root).ok().as_ref() != Some(&c.root_stamp)
            || stamp(&c.path).ok().as_ref() != Some(&c.stamp)
        {
            result.skipped_files += 1;
            if let Err(e) = logger(
                &mut file,
                &snapshot.id,
                &c.path,
                "skipped",
                "文件或目录已变化",
            ) {
                result.messages.push(e);
                stopped = true;
            }
            continue;
        }
        let (status, detail) = match recycle(&c.path) {
            Ok(()) => {
                result.recycled_files += 1;
                result.recycled_bytes = result.recycled_bytes.saturating_add(c.stamp.len);
                ("success", "已移入回收站".into())
            }
            Err(e) => {
                result.failed_files += 1;
                ("error", e)
            }
        };
        if let Err(e) = logger(&mut file, &snapshot.id, &c.path, status, &detail) {
            result
                .messages
                .push(format!("结果日志写入失败，已停止后续清理：{e}"));
            stopped = true;
        }
    }
    if result.skipped_files > 0 {
        result
            .messages
            .push("已跳过运行中的浏览器、发生变化或无法安全验证的文件".into());
    }
    if result.failed_files > 0 {
        result
            .messages
            .push("部分文件无法移入回收站，详情见日志".into());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::FileTimes;

    const ELECTRON_IDS: [&str; 5] = [
        "cherry-studio-cache",
        "chatbox-cache",
        "lx-music-cache",
        "docker-desktop-cache",
        "opencode-cache",
    ];

    #[test]
    fn cache_cleaner_roaming_only_known_caches_and_recorded_execution() {
        let t = tempfile::tempdir().unwrap();
        let local = t.path().join("local");
        let roaming = t.path().join("roaming");
        fs::create_dir_all(&local).unwrap();
        fs::create_dir_all(&roaming).unwrap();
        let mut protected = Vec::new();
        for id in ELECTRON_IDS {
            let app = electron_app(id).unwrap();
            for root in roots(&roaming, id).unwrap() {
                old_file(&root, "fresh-cache", 0);
                protected.push(old_file(&root, "keep.sqlite", 30));
            }
            for sub in [
                "compile-cache/data",
                "sessions/session.json",
                "opencode.settings",
                "opencode.db",
                "persisted-state.json",
                "Docker.raw",
                "Docker.vhdx",
                "wsl/data/ext4.vhdx",
                "volumes/project/data",
                "containers/work/data",
                "DawnCache/unsupported",
                "DawnGraphiteCache-other/data",
                "Data/chat",
                "LxDatas/list",
                "chatbox-blobs/blob",
                "chatbox-databases/chat.db",
                "IndexedDB/data",
                "Local Storage/data",
                "Network/Cookies",
                "Session Storage/data",
                "Service Worker/CacheStorage/data",
                "Partitions/default/Cache/Cache_Data/data",
                "Cache/outside",
                "Downloads/music.mp3",
                "config.json",
            ] {
                protected.push(old_file(&roaming.join(app), sub, 30));
            }
            protected.push(old_file(&local.join(app), "GPUCache/wrong-anchor", 30));
            protected.push(old_file(
                &roaming.join(format!("{app}-other")),
                "GPUCache/neighbor",
                30,
            ));
        }
        let (snapshot, p) = scan_with_roaming(&local, Some(&roaming), |_| Ok(false));
        let snapshot = snapshot.unwrap();
        assert_eq!(
            snapshot.candidates.len(),
            ELECTRON_IDS.len() * ELECTRON_CACHE_ROOTS.len()
        );
        for id in ELECTRON_IDS {
            let r = p.rules.iter().find(|r| r.id == id).unwrap();
            assert_eq!(r.status, "ready");
            assert_eq!(r.file_count, 5);
        }
        let result = execute_recorded_snapshot_with_roaming(
            snapshot,
            &local,
            Some(&roaming),
            &ELECTRON_IDS.map(String::from),
            &t.path().join("audit/actions.jsonl"),
            &t.path().join("history"),
            |_| Ok(false),
            |path| fs::remove_file(path).map_err(err),
        )
        .unwrap();
        assert_eq!(result.recycled_files, 25);
        assert!(protected.iter().all(|p| p.exists()));
    }

    #[test]
    fn cache_cleaner_roaming_running_and_detection_failure_measure_without_candidates() {
        let local = tempfile::tempdir().unwrap();
        let roaming = tempfile::tempdir().unwrap();
        for id in ELECTRON_IDS {
            old_file(&roots(roaming.path(), id).unwrap()[0], "cache", 0);
        }
        for fail in [false, true] {
            let (s, p) = scan_with_roaming(local.path(), Some(roaming.path()), |_| {
                if fail {
                    Err("进程检查失败".into())
                } else {
                    Ok(true)
                }
            });
            assert!(s.unwrap().candidates.is_empty());
            for id in ELECTRON_IDS {
                let r = p.rules.iter().find(|r| r.id == id).unwrap();
                assert_eq!(r.status, "blocked");
                assert_eq!(r.file_count, 1);
                assert!(r.bytes > 0);
            }
        }
        for id in ELECTRON_IDS {
            let (s, _) = scan_with_roaming(local.path(), Some(roaming.path()), |_| Ok(false));
            let result = execute_snapshot_with_roaming(
                s.unwrap(),
                local.path(),
                Some(roaming.path()),
                &[id.into()],
                &local.path().join(format!("{id}.jsonl")),
                |_| Ok(true),
                |_| panic!("running application must not recycle"),
            )
            .unwrap();
            assert_eq!(result.recycled_files, 0);
            assert_eq!(result.skipped_files, 1);
        }
    }

    #[test]
    fn cache_cleaner_v5_each_ui_process_blocks_its_rule_and_final_recycle() {
        for (id, executable) in [
            ("docker-desktop-cache", "Docker Desktop.exe"),
            ("opencode-cache", "OpenCode.exe"),
        ] {
            let local = tempfile::tempdir().unwrap();
            let roaming = tempfile::tempdir().unwrap();
            old_file(&roots(roaming.path(), id).unwrap()[0], "fresh", 0);
            let (s, p) = scan_with_roaming(local.path(), Some(roaming.path()), |name| {
                Ok(name.eq_ignore_ascii_case(executable))
            });
            let rule = p.rules.iter().find(|r| r.id == id).unwrap();
            assert_eq!(rule.status, "blocked");
            assert_eq!(rule.file_count, 1);
            assert!(s.unwrap().candidates.iter().all(|c| c.rule != id));
            let (s, _) = scan_with_roaming(local.path(), Some(roaming.path()), |_| Ok(false));
            let started = std::cell::Cell::new(false);
            let result = execute_snapshot_with_logger_with_roaming(
                s.unwrap(),
                local.path(),
                Some(roaming.path()),
                &[id.into()],
                &local.path().join("audit"),
                |name| Ok(started.get() && name.eq_ignore_ascii_case(executable)),
                |_| panic!("application started after pending log"),
                |file, plan, path, status, detail| {
                    log_event(file, plan, path, status, detail)?;
                    if status == "pending" {
                        started.set(true);
                    }
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(result.recycled_files, 0);
            assert_eq!(result.skipped_files, 1);
        }
    }

    #[test]
    fn cache_cleaner_roaming_anchor_replacement_and_known_folder_change_rejected() {
        let t = tempfile::tempdir().unwrap();
        let local = t.path().join("local");
        let roaming = t.path().join("roaming");
        fs::create_dir_all(&local).unwrap();
        for id in ELECTRON_IDS {
            old_file(&roots(&roaming, id).unwrap()[0], "cache", 0);
        }
        let (s, _) = scan_with_roaming(&local, Some(&roaming), |_| Ok(false));
        let other = t.path().join("other");
        fs::create_dir(&other).unwrap();
        assert!(execute_snapshot_with_roaming(
            s.unwrap(),
            &local,
            Some(&other),
            &ELECTRON_IDS.map(String::from),
            &t.path().join("audit1"),
            |_| Ok(false),
            |_| panic!("changed KnownFolder")
        )
        .is_err());
        let (s, _) = scan_with_roaming(&local, Some(&roaming), |_| Ok(false));
        fs::rename(&roaming, t.path().join("saved")).unwrap();
        fs::create_dir(&roaming).unwrap();
        assert!(execute_snapshot_with_roaming(
            s.unwrap(),
            &local,
            Some(&roaming),
            &ELECTRON_IDS.map(String::from),
            &t.path().join("audit2"),
            |_| Ok(false),
            |_| panic!("replaced anchor")
        )
        .is_err());
    }

    #[test]
    fn cache_cleaner_roaming_root_replacement_and_final_anchor_recheck() {
        for id in ELECTRON_IDS {
            let t = tempfile::tempdir().unwrap();
            let local = t.path().join("local");
            fs::create_dir(&local).unwrap();
            let roaming = t.path().join("roaming");
            let root = roots(&roaming, id).unwrap()[0].clone();
            old_file(&root, "cache", 0);
            let (s, _) = scan_with_roaming(&local, Some(&roaming), |_| Ok(false));
            fs::rename(&root, root.with_extension("saved")).unwrap();
            fs::create_dir(&root).unwrap();
            old_file(&root, "cache", 0);
            let r = execute_snapshot_with_roaming(
                s.unwrap(),
                &local,
                Some(&roaming),
                &[id.into()],
                &t.path().join("audit1"),
                |_| Ok(false),
                |_| panic!("replaced cache root"),
            )
            .unwrap();
            assert_eq!(r.skipped_files, 1);
            let (s, _) = scan_with_roaming(&local, Some(&roaming), |_| Ok(false));
            let r = execute_snapshot_with_logger_with_roaming(
                s.unwrap(),
                &local,
                Some(&roaming),
                &[id.into()],
                &t.path().join("audit2"),
                |_| Ok(false),
                |_| panic!("anchor changed after pending"),
                |file, plan, path, status, detail| {
                    log_event(file, plan, path, status, detail)?;
                    if status == "pending" {
                        fs::rename(&roaming, t.path().join("saved")).map_err(err)?;
                        fs::create_dir(&roaming).map_err(err)?;
                    }
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(r.recycled_files, 0);
            assert_eq!(r.skipped_files, 1);
        }
    }

    #[test]
    fn cache_cleaner_roaming_scan_limits_and_invalid_anchor() {
        let local = tempfile::tempdir().unwrap();
        let roaming = tempfile::tempdir().unwrap();
        for id in ELECTRON_IDS {
            for n in 0..4 {
                old_file(
                    &roots(roaming.path(), id).unwrap()[0],
                    &format!("cache{n}"),
                    0,
                );
            }
        }
        let (s, p) = scan_with_limits_with_roaming(
            local.path(),
            Some(roaming.path()),
            |_| Ok(false),
            10,
            2,
            Duration::from_secs(5),
        );
        assert_eq!(s.unwrap().candidates.len(), ELECTRON_IDS.len() * 2);
        for id in ELECTRON_IDS {
            assert_eq!(p.rules.iter().find(|r| r.id == id).unwrap().file_count, 2);
        }
        let (_, p) = scan_with_roaming(local.path(), Some(&roaming.path().join("missing")), |_| {
            Ok(false)
        });
        for id in ELECTRON_IDS {
            assert_eq!(
                p.rules.iter().find(|r| r.id == id).unwrap().status,
                "blocked"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn cache_cleaner_roaming_each_root_rejects_junction() {
        use std::os::windows::process::CommandExt;
        use std::process::Command;
        for id in ELECTRON_IDS {
            for sub in ELECTRON_CACHE_ROOTS.into_iter().chain([""]) {
                let local = tempfile::tempdir().unwrap();
                let roaming = tempfile::tempdir().unwrap();
                let outside = tempfile::tempdir().unwrap();
                old_file(outside.path(), "protected", 30);
                let root = sub.split('/').filter(|part| !part.is_empty()).fold(
                    roaming.path().join(electron_app(id).unwrap()),
                    |path, part| path.join(part),
                );
                fs::create_dir_all(root.parent().unwrap()).unwrap();
                assert!(Command::new("cmd")
                    .args(["/c", "mklink", "/J"])
                    .arg(&root)
                    .arg(outside.path())
                    .creation_flags(0x08000000)
                    .output()
                    .unwrap()
                    .status
                    .success());
                let (s, _) = scan_with_roaming(local.path(), Some(roaming.path()), |_| Ok(false));
                assert!(!s.unwrap().candidates.iter().any(|c| c.rule == id));
                fs::remove_dir(&root).unwrap();
            }
        }
        let t = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        for id in ELECTRON_IDS {
            old_file(&roots(outside.path(), id).unwrap()[0], "protected", 0);
        }
        let anchor = t.path().join("roaming-link");
        assert!(Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&anchor)
            .arg(outside.path())
            .creation_flags(0x08000000)
            .output()
            .unwrap()
            .status
            .success());
        let (s, p) = scan_with_roaming(t.path(), Some(&anchor), |_| Ok(false));
        assert!(s.unwrap().candidates.is_empty());
        for id in ELECTRON_IDS {
            assert_eq!(
                p.rules.iter().find(|r| r.id == id).unwrap().status,
                "blocked"
            );
        }
        fs::remove_dir(&anchor).unwrap();
    }

    fn old_file(base: &Path, relative: &str, days: u64) -> PathBuf {
        let path = base.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"cached test bytes").unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(
                FileTimes::new()
                    .set_modified(SystemTime::now() - Duration::from_secs(days * 86400)),
            )
            .unwrap();
        path
    }

    fn preview(base: &Path) -> (Snapshot, Preview) {
        let (snapshot, preview) = scan(base, |_| Ok(false));
        (snapshot.unwrap(), preview)
    }

    fn ids() -> Vec<String> {
        IDS.iter()
            .filter(|id| **id != "nvidia-shader")
            .map(|s| s.to_string())
            .collect()
    }

    const FIREFOX_ENTRY: &str = "0123456789ABCDEF0123456789ABCDEF01234567";

    #[test]
    fn cache_cleaner_history_begin_failure_prevents_recycle() {
        let t = tempfile::tempdir().unwrap();
        let target = old_file(t.path(), "Temp/old.tmp", 31);
        let app_data = t.path().join("not-a-directory");
        fs::write(&app_data, b"fixture").unwrap();
        let (snapshot, _) = preview(t.path());
        let result = execute_recorded_snapshot(
            snapshot,
            t.path(),
            &["temp".into()],
            &t.path().join("log/a.jsonl"),
            &app_data,
            |_| Ok(false),
            |_| panic!("history must be saved first"),
        );
        assert!(result.err().unwrap().contains("本次未执行清理"));
        assert!(target.is_file());
    }

    #[test]
    fn cache_cleaner_history_records_real_results_and_save_failure_warning() {
        for break_finish in [false, true] {
            let t = tempfile::tempdir().unwrap();
            let target = old_file(t.path(), "Temp/old.tmp", 31);
            let (snapshot, _) = preview(t.path());
            let app_data = t.path().join("app-data");
            let result = execute_recorded_snapshot(
                snapshot,
                t.path(),
                &["temp".into()],
                &t.path().join("log/a.jsonl"),
                &app_data,
                |_| Ok(false),
                |path| {
                    fs::remove_file(path).map_err(err)?;
                    if break_finish {
                        let root = app_data.join("cleanup-history-v1");
                        let record = fs::read_dir(root).unwrap().next().unwrap().unwrap().path();
                        fs::write(record.join("finish.json.pending"), b"fixture").unwrap();
                    }
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(result.recycled_files, 1);
            assert_eq!(result.recycled_bytes, 17);
            assert!(!target.exists());
            assert_eq!(result.history_warning.is_some(), break_finish);
            let history = crate::cleanup_history::read_history(&app_data).unwrap();
            assert_eq!(history.records.len(), 1);
            assert_eq!(
                history.records[0].status,
                if break_finish {
                    "incomplete"
                } else {
                    "completed"
                }
            );
            assert_eq!(history.records[0].selected_rules[0].title, "用户临时文件");
            if !break_finish {
                assert_eq!(history.records[0].recycled_files, 1);
                assert_eq!(history.records[0].recycled_bytes, result.recycled_bytes);
            }
        }
    }

    #[test]
    fn cache_cleaner_running_application_is_measured_without_candidates() {
        let t = tempfile::tempdir().unwrap();
        old_file(
            t.path(),
            "Google/Chrome/User Data/Default/Cache/Cache_Data/data",
            0,
        );
        old_file(
            t.path(),
            "Microsoft/Edge/User Data/Default/DawnWebGPUCache/data",
            0,
        );
        for running in [false, true] {
            let (s, p) = scan(t.path(), |_| {
                if running {
                    Ok(true)
                } else {
                    Err("cannot enumerate processes".into())
                }
            });
            assert!(s.unwrap().candidates.is_empty());
            for id in ["chrome", "edge"] {
                let rule = p.rules.iter().find(|r| r.id == id).unwrap();
                assert_eq!(rule.status, "blocked");
                assert_eq!(rule.file_count, 1);
                assert_eq!(rule.bytes, 17);
                assert!(rule.message.is_some());
            }
        }
    }

    #[test]
    fn cache_cleaner_npm_known_buckets_age_and_neighbors() {
        let t = tempfile::tempdir().unwrap();
        let expected: HashSet<_> = ["content-v2", "index-v5"]
            .into_iter()
            .map(|bucket| {
                old_file(
                    t.path(),
                    &format!("npm-cache/_cacache/{bucket}/aa/bb/cache"),
                    31,
                )
            })
            .collect();
        for path in [
            "npm-cache/_cacache/content-v2/aa/bb/recent",
            "npm-cache/_cacache/index-v5/aa/bb/recent",
        ] {
            old_file(t.path(), path, 1);
        }
        for path in [
            "npm-cache/_cacache/tmp/active-download",
            "npm-cache/_npx/app/node_modules/module.js",
            "npm-cache/_logs/npm.log",
            "npm-cache/custom.txt",
            "npm/node_modules/package/index.js",
            "npm/.npmrc",
            "npm-cache/_cacache/content-v3/unknown",
            "npm-cache/_cacache/content-v2/account.db",
        ] {
            old_file(t.path(), path, 31);
        }
        let (s, p) = scan(t.path(), |_| Ok(true));
        let s = s.unwrap();
        let rule = p.rules.iter().find(|r| r.id == "npm-cache").unwrap();
        assert_eq!(rule.status, "ready");
        assert_eq!(rule.retention_days, 30);
        assert_eq!(
            s.candidates
                .iter()
                .map(|c| c.path.clone())
                .collect::<HashSet<_>>(),
            expected
        );
        let r = execute_snapshot(
            s,
            t.path(),
            &["npm-cache".into()],
            &t.path().join("log/a.jsonl"),
            |_| Ok(true),
            |path| {
                assert!(expected.contains(path));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(r.recycled_files, 2);
    }

    #[test]
    fn cache_cleaner_nvidia_reference_is_never_executable() {
        let t = tempfile::tempdir().unwrap();
        old_file(t.path(), "NVIDIA/DXCache/shader.bin", 0);
        old_file(t.path(), "NVIDIA/GLCache/app/shader.bin", 0);
        old_file(t.path(), "NVIDIA/settings.json", 31);
        old_file(t.path(), "NVIDIA Corporation/userdata.dat", 31);
        let (s, p) = preview(t.path());
        let rule = p.rules.iter().find(|r| r.id == "nvidia-shader").unwrap();
        assert_eq!(rule.status, "blocked");
        assert_eq!(rule.file_count, 2);
        assert_eq!(rule.bytes, 34);
        assert_eq!(rule.title, rule_title("nvidia-shader"));
        assert!(rule.message.as_ref().unwrap().contains("仅查看"));
        assert!(s.candidates.is_empty());
        let r = execute_snapshot(
            s,
            t.path(),
            &["nvidia-shader".into()],
            &t.path().join("log/a.jsonl"),
            |_| Ok(false),
            |_| panic!("read-only reference must not act"),
        )
        .unwrap();
        assert_eq!(r.recycled_files, 0);
    }

    #[test]
    fn cache_cleaner_expanded_chromium_roots_and_profile_boundaries() {
        let t = tempfile::tempdir().unwrap();
        let mut expected = HashSet::new();
        for browser in [
            "Google/Chrome",
            "Microsoft/Edge",
            "BraveSoftware/Brave-Browser",
        ] {
            for sub in CHROMIUM_PROFILE_CACHES {
                expected.insert(old_file(
                    t.path(),
                    &format!("{browser}/User Data/Default/{sub}/entry"),
                    0,
                ));
            }
            for sub in CHROMIUM_SHARED_CACHES {
                expected.insert(old_file(
                    t.path(),
                    &format!("{browser}/User Data/{sub}/entry"),
                    0,
                ));
            }
            for sub in [
                "Default/IndexedDB",
                "Default/Service Worker/CacheStorage",
                "Default/Local Storage",
                "Default/Session Storage",
                "Default/Extensions",
                "Default/Network",
                "OptGuideOnDeviceModel",
                "component_crx_cache",
                "extensions_crx_cache",
                "Guest Profile/DawnGraphiteCache",
                "Profile not-a-number/DawnWebGPUCache",
            ] {
                old_file(t.path(), &format!("{browser}/User Data/{sub}/entry"), 31);
            }
        }
        let (s, _) = preview(t.path());
        assert_eq!(
            s.candidates
                .iter()
                .map(|c| c.path.clone())
                .collect::<HashSet<_>>(),
            expected
        );
        assert_eq!(s.candidates.len(), 33);
    }

    #[cfg(windows)]
    #[test]
    fn cache_cleaner_v3_roots_reject_junctions() {
        use std::os::windows::process::CommandExt;
        let mut roots: Vec<String> = CHROMIUM_PROFILE_CACHES
            .iter()
            .skip(3)
            .map(|sub| format!("Google/Chrome/User Data/Default/{sub}"))
            .collect();
        roots.extend(
            CHROMIUM_SHARED_CACHES
                .iter()
                .map(|sub| format!("Google/Chrome/User Data/{sub}")),
        );
        roots.extend(
            [
                "npm-cache/_cacache/content-v2",
                "npm-cache/_cacache/index-v5",
                "NVIDIA/DXCache",
                "NVIDIA/GLCache",
            ]
            .into_iter()
            .map(str::to_owned),
        );
        for relative in roots {
            let t = tempfile::tempdir().unwrap();
            let target = t.path().join("outside");
            old_file(&target, "entry", 31);
            let link = relative
                .split('/')
                .fold(t.path().to_owned(), |path, part| path.join(part));
            fs::create_dir_all(link.parent().unwrap()).unwrap();
            let output = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(&link)
                .arg(&target)
                .creation_flags(0x08000000)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{relative}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let (s, p) = preview(t.path());
            assert!(s.candidates.is_empty(), "{relative}");
            assert!(p.rules.iter().all(|r| r.file_count == 0), "{relative}");
            assert!(!p.warnings.is_empty(), "{relative}");
            fs::remove_dir(link).unwrap();
        }
    }

    #[test]
    fn cache_cleaner_new_roots_cover_only_known_caches_and_age_limits() {
        let t = tempfile::tempdir().unwrap();
        let mut expected = HashSet::new();
        for sub in ["Cache/Cache_Data", "Code Cache", "GPUCache"] {
            expected.insert(old_file(
                t.path(),
                &format!("BraveSoftware/Brave-Browser/User Data/Default/{sub}/data"),
                0,
            ));
        }
        expected.insert(old_file(
            t.path(),
            &format!(
                "Mozilla/Firefox/Profiles/test.default-release/cache2/entries/{FIREFOX_ENTRY}"
            ),
            0,
        ));
        for sub in ["http-v2", "http", "wheels"] {
            expected.insert(old_file(
                t.path(),
                &format!("pip/Cache/{sub}/a/b/cache-file"),
                31,
            ));
            old_file(t.path(), &format!("pip/Cache/{sub}/a/b/recent-file"), 1);
        }
        for path in [
            "BraveSoftware/Brave-Browser/User Data/Default/Bookmarks",
            "BraveSoftware/Brave-Browser/User Data/Default/IndexedDB/data",
            "BraveSoftware/Brave-Browser/User Data/Default/Service Worker/CacheStorage/data",
            "BraveSoftware/Brave-Browser/User Data/Guest Profile/GPUCache/data",
            "Mozilla/Firefox/Profiles/test.default-release/cache2/index",
            "Mozilla/Firefox/Profiles/test.default-release/cache2/entries/notes.txt",
            "Mozilla/Firefox/Profiles/test.default-release/cache2/entries/0123456789abcdef0123456789abcdef01234567",
            "Mozilla/Firefox/Profiles/test.default-release/places.sqlite",
            "Mozilla/Firefox/Profiles/test.default-release/storage/default/site/data",
            "Roaming/Mozilla/Firefox/Profiles/test.default-release/cache2/entries/data",
            "pip/Cache/selfcheck/data",
            "pip/pip.ini",
            "Programs/Python/Python313/Lib/site-packages/package.py",
        ] { old_file(t.path(), path, 31); }
        old_file(t.path(), &format!("Mozilla/Firefox/Profiles/test.default-release/cache2/entries/nested/{FIREFOX_ENTRY}"), 31);
        let (s, p) = preview(t.path());
        let actual: HashSet<_> = s.candidates.iter().map(|c| c.path.clone()).collect();
        assert_eq!(actual, expected);
        for rule in p
            .rules
            .iter()
            .filter(|r| ["brave", "firefox"].contains(&r.id.as_str()))
        {
            assert_eq!(rule.retention_days, 0);
            assert_eq!(rule.status, "ready");
        }
        let mut recycled = HashSet::new();
        let r = execute_snapshot(
            s,
            t.path(),
            &ids(),
            &t.path().join("log/a.jsonl"),
            |_| Ok(false),
            |path| {
                recycled.insert(path.to_owned());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(r.recycled_files, expected.len() as u64);
        assert_eq!(recycled, expected);
        assert!(!eligible(
            Path::new("cache"),
            "unknown",
            SystemTime::now(),
            SystemTime::now()
        ));
        assert!(roots(t.path(), "unknown").is_err());
    }

    #[test]
    fn cache_cleaner_all_browser_caches_include_fresh_files() {
        let t = tempfile::tempdir().unwrap();
        for browser in [
            "Google/Chrome",
            "Microsoft/Edge",
            "BraveSoftware/Brave-Browser",
        ] {
            old_file(
                t.path(),
                &format!("{browser}/User Data/Default/Cache/Cache_Data/fresh"),
                0,
            );
        }
        let (s, _) = preview(t.path());
        assert_eq!(s.candidates.len(), 3);
        assert!(s
            .candidates
            .iter()
            .all(|c| retention_days(&c.rule) == Some(0)));
    }

    #[test]
    fn cache_cleaner_rule_budgets_do_not_starve_later_rules() {
        let t = tempfile::tempdir().unwrap();
        for i in 0..8 {
            old_file(t.path(), &format!("Temp/{i}.tmp"), 8);
        }
        old_file(t.path(), "Google/Chrome/User Data/Default/GPUCache/data", 0);
        old_file(t.path(), "pip/Cache/http-v2/data", 31);
        for (entries, candidates) in [(3, 20), (20, 3)] {
            let (s, p) = scan_with_limits(
                t.path(),
                |_| Ok(false),
                entries,
                candidates,
                Duration::from_secs(30),
            );
            let s = s.unwrap();
            assert_eq!(s.candidates.iter().filter(|c| c.rule == "temp").count(), 3);
            assert_eq!(
                s.candidates.iter().filter(|c| c.rule == "chrome").count(),
                1
            );
            assert_eq!(
                s.candidates
                    .iter()
                    .filter(|c| c.rule == "pip-cache")
                    .count(),
                1
            );
            assert!(p
                .warnings
                .iter()
                .any(|w| w.contains("用户临时文件") && w.contains("上限")));
        }
    }

    #[test]
    fn cache_cleaner_new_rule_processes_block_scan_and_execute() {
        let t = tempfile::tempdir().unwrap();
        old_file(
            t.path(),
            "BraveSoftware/Brave-Browser/User Data/Default/GPUCache/data",
            0,
        );
        old_file(
            t.path(),
            &format!("Mozilla/Firefox/Profiles/test.default/cache2/entries/{FIREFOX_ENTRY}"),
            0,
        );
        old_file(t.path(), "pip/Cache/wheels/data", 31);
        for (id, executable) in [
            ("brave", "brave.exe"),
            ("firefox", "firefox.exe"),
            ("pip-cache", "python.exe"),
        ] {
            let (s, p) = scan(t.path(), |name| Ok(name == executable));
            assert!(!s.unwrap().candidates.iter().any(|c| c.rule == id));
            let rule = p.rules.iter().find(|r| r.id == id).unwrap();
            assert_eq!(rule.status, "blocked");
            assert!(rule.message.as_ref().unwrap().contains("请先关闭"));
            let (s, _) = preview(t.path());
            let r = execute_snapshot(
                s,
                t.path(),
                &[id.into()],
                &t.path().join("log/a.jsonl"),
                |name| Ok(name == executable),
                |_| panic!("running application"),
            )
            .unwrap();
            assert_eq!(r.skipped_files, 1);
            let (s, _) = preview(t.path());
            let checks = std::cell::Cell::new(0);
            let r = execute_snapshot(
                s,
                t.path(),
                &[id.into()],
                &t.path().join("log/a.jsonl"),
                |name| {
                    if name != executable {
                        return Ok(false);
                    }
                    checks.set(checks.get() + 1);
                    Ok(checks.get() > 1)
                },
                |_| panic!("application started after log write"),
            )
            .unwrap();
            assert_eq!(r.skipped_files, 1);
        }
    }

    #[cfg(windows)]
    #[test]
    fn cache_cleaner_each_new_cache_root_rejects_junctions() {
        use std::os::windows::process::CommandExt;
        for relative in [
            "BraveSoftware/Brave-Browser/User Data/Default/Cache/Cache_Data",
            "BraveSoftware/Brave-Browser/User Data/Default/Code Cache",
            "BraveSoftware/Brave-Browser/User Data/Default/GPUCache",
            "Mozilla/Firefox/Profiles/test.default/cache2/entries",
            "pip/Cache/http-v2",
            "pip/Cache/http",
            "pip/Cache/wheels",
        ] {
            let t = tempfile::tempdir().unwrap();
            let target = t.path().join("outside");
            old_file(&target, FIREFOX_ENTRY, 31);
            let link = relative
                .split('/')
                .fold(t.path().to_owned(), |path, part| path.join(part));
            fs::create_dir_all(link.parent().unwrap()).unwrap();
            let output = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(&link)
                .arg(&target)
                .creation_flags(0x08000000)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{relative}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let (s, p) = preview(t.path());
            assert!(s.candidates.is_empty(), "{relative}");
            assert!(!p.warnings.is_empty(), "{relative}");
            fs::remove_dir(link).unwrap();
        }
    }

    #[test]
    fn cache_cleaner_eligibility_age_and_database_exclusions() {
        let now = SystemTime::now();
        let old = now - Duration::from_secs(8 * 86400);
        for name in [
            "account.db",
            "a.DB-WAL",
            "a.db-shm",
            "a.sqlite",
            "a.SQLITE3",
        ] {
            assert!(!eligible(Path::new(name), "temp", old, now), "{name}");
        }
        assert!(eligible(Path::new("cache.bin"), "temp", old, now));
        assert!(!eligible(
            Path::new("cache.bin"),
            "temp",
            now - Duration::from_secs(86400),
            now
        ));
        assert!(!eligible(
            Path::new("cache.bin"),
            "temp",
            now + Duration::from_secs(10),
            now
        ));
        assert!(!eligible(Path::new("old.dmp"), "crash-dumps", old, now));
        let ancient = now - Duration::from_secs(31 * 86400);
        assert!(eligible(Path::new("old.DMP"), "crash-dumps", ancient, now));
        assert!(!eligible(Path::new("old.txt"), "crash-dumps", ancient, now));
    }

    #[test]
    fn cache_cleaner_scans_only_explicit_roots_and_profiles() {
        let t = tempfile::tempdir().unwrap();
        let base = t.path();
        for path in [
            "Temp/old.tmp",
            "Google/Chrome/User Data/Default/Cache/Cache_Data/data",
            "Google/Chrome/User Data/Profile 2/Code Cache/js/data",
            "Microsoft/Edge/User Data/Default/GPUCache/data",
            "CrashDumps/old.dmp",
        ] {
            old_file(base, path, 31);
        }
        for path in [
            "Temp/account.DB",
            "Google/Chrome/User Data/Default/Cache/other",
            "Google/Chrome/User Data/Default/History",
            "Google/Chrome/User Data/Guest Profile/Cache/Cache_Data/data",
            "Google/Chrome/User Data/Profile bad/GPUCache/data",
            "CrashDumps/nested/old.dmp",
            "CrashDumps/old.log",
            "Other/old.tmp",
        ] {
            old_file(base, path, 31);
        }
        old_file(base, "Temp/fresh.tmp", 1);
        let (snapshot, p) = preview(base);
        assert_eq!(snapshot.candidates.len(), 5);
        assert_eq!(p.rules.iter().map(|r| r.file_count).sum::<u64>(), 5);
        assert_eq!(
            p.rules
                .iter()
                .find(|r| r.id == "chrome")
                .unwrap()
                .file_count,
            2
        );
        assert!(!profile_name("Profile "));
        assert!(!profile_name("Profile 1/.."));
    }

    #[test]
    fn cache_cleaner_missing_and_failed_directories_are_distinguished() {
        let t = tempfile::tempdir().unwrap();
        let (_, p) = preview(t.path());
        assert!(p
            .rules
            .iter()
            .all(|r| (r.status == "empty" || r.id == "nvidia-shader") && r.message.is_some()));
        fs::write(t.path().join("Temp"), b"not a directory").unwrap();
        let (_, p) = preview(t.path());
        assert_eq!(p.rules[0].status, "blocked");
        assert!(!p.warnings.is_empty());
    }

    #[test]
    fn cache_cleaner_browser_detection_fails_closed_at_scan_and_execute() {
        let t = tempfile::tempdir().unwrap();
        old_file(t.path(), "Google/Chrome/User Data/Default/GPUCache/data", 8);
        let (s, p) = scan(t.path(), |_| Err("process enumeration failed".into()));
        assert!(s.unwrap().candidates.is_empty());
        assert_eq!(p.rules[1].status, "blocked");
        let (s, _) = preview(t.path());
        let mut calls = 0;
        let result = execute_snapshot(
            s,
            t.path(),
            &ids(),
            &t.path().join("logs/a.jsonl"),
            |_| Ok(true),
            |_| {
                calls += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(calls, 0);
        assert_eq!(result.skipped_files, 1);
        let (s, _) = preview(t.path());
        let result = execute_snapshot(
            s,
            t.path(),
            &ids(),
            &t.path().join("logs/a.jsonl"),
            |_| Err("cannot check".into()),
            |_| panic!("must fail closed"),
        )
        .unwrap();
        assert_eq!(result.skipped_files, 1);
    }

    #[test]
    fn cache_cleaner_changed_file_is_skipped() {
        let t = tempfile::tempdir().unwrap();
        let path = old_file(t.path(), "Temp/a.tmp", 8);
        let (s, _) = preview(t.path());
        fs::write(&path, b"new content").unwrap();
        let r = execute_snapshot(
            s,
            t.path(),
            &ids(),
            &t.path().join("log/a.jsonl"),
            |_| Ok(false),
            |_| panic!("changed file must not be recycled"),
        )
        .unwrap();
        assert_eq!(r.skipped_files, 1);
    }

    #[test]
    fn cache_cleaner_replaced_root_is_skipped() {
        let t = tempfile::tempdir().unwrap();
        old_file(t.path(), "Temp/a.tmp", 8);
        let (s, _) = preview(t.path());
        fs::rename(t.path().join("Temp"), t.path().join("saved")).unwrap();
        old_file(t.path(), "Temp/a.tmp", 8);
        let r = execute_snapshot(
            s,
            t.path(),
            &ids(),
            &t.path().join("log/a.jsonl"),
            |_| Ok(false),
            |_| panic!("replaced root must not be recycled"),
        )
        .unwrap();
        assert_eq!(r.skipped_files, 1);
    }

    #[test]
    fn cache_cleaner_replaced_base_is_rejected() {
        let t = tempfile::tempdir().unwrap();
        let base = t.path().join("base");
        old_file(&base, "Temp/a.tmp", 8);
        let (s, _) = preview(&base);
        fs::rename(&base, t.path().join("saved")).unwrap();
        fs::create_dir(&base).unwrap();
        assert!(execute_snapshot(
            s,
            &base,
            &ids(),
            &t.path().join("log/a.jsonl"),
            |_| Ok(false),
            |_| panic!("replaced base")
        )
        .is_err());
    }

    #[test]
    fn cache_session_restores_shared_preview_and_new_generation_clears_it() {
        let t = tempfile::tempdir().unwrap();
        let (snapshot, preview) = preview(t.path());
        let state = CacheCleanerState::default();
        {
            let mut inner = state.0.lock().unwrap();
            inner.snapshot = Some(snapshot);
            inner.session = Some(CacheScanSession {
                scan_id: "cache-1".into(),
                status: "completed",
                preview: Some(Arc::new(preview)),
                error: None,
            });
        }
        let first = session_snapshot(&state).unwrap().unwrap();
        let second = session_snapshot(&state.clone()).unwrap().unwrap();
        assert_eq!(second.status, "completed");
        assert!(Arc::ptr_eq(
            first.preview.as_ref().unwrap(),
            second.preview.as_ref().unwrap()
        ));
        assert!(serde_json::to_value(second).unwrap()["preview"]["plan_id"].is_string());
        begin_scan(&state).unwrap();
        assert!(session_snapshot(&state).unwrap().is_none());
    }

    #[test]
    fn cache_session_cannot_restore_consumed_or_expired_plan() {
        let t = tempfile::tempdir().unwrap();
        for expired in [false, true] {
            let (mut snapshot, preview) = preview(t.path());
            let id = snapshot.id.clone();
            if expired {
                snapshot.created = SystemTime::now() - PREVIEW_TTL - Duration::from_secs(1);
            }
            let state = CacheCleanerState::default();
            {
                let mut inner = state.0.lock().unwrap();
                inner.snapshot = Some(snapshot);
                inner.session = Some(CacheScanSession {
                    scan_id: "cache-1".into(),
                    status: "completed",
                    preview: Some(Arc::new(preview)),
                    error: None,
                });
            }
            if !expired {
                assert!(take_snapshot(&state, &id, &ids()).is_ok());
            }
            let restored = session_snapshot(&state).unwrap().unwrap();
            assert_eq!(restored.status, "failed");
            assert!(restored.preview.is_none());
            assert!(restored.error.is_some());
            assert!(take_snapshot(&state, &id, &ids()).is_err());
        }
    }

    #[test]
    fn cache_cleaner_tokens_reject_invalid_replayed_expired_and_concurrent() {
        let t = tempfile::tempdir().unwrap();
        let state = CacheCleanerState::default();
        let (s, _) = preview(t.path());
        let id = s.id.clone();
        state.0.lock().unwrap().snapshot = Some(s);
        assert!(take_snapshot(&state, &id, &[]).is_err());
        assert!(take_snapshot(&state, &id, &["arbitrary/path".into()]).is_err());
        assert!(take_snapshot(&state, "wrong", &ids()).is_err());
        let _s = take_snapshot(&state, &id, &ids()).unwrap();
        assert!(take_snapshot(&state, &id, &ids()).is_err());
        state.0.lock().unwrap().executing = false;
        assert!(take_snapshot(&state, &id, &ids()).is_err());
        let (mut s, _) = preview(t.path());
        s.created = SystemTime::now() - Duration::from_secs(901);
        let id = s.id.clone();
        state.0.lock().unwrap().snapshot = Some(s);
        assert!(take_snapshot(&state, &id, &ids()).is_err());
        assert!(state.0.lock().unwrap().snapshot.is_none());
    }

    #[test]
    fn cache_cleaner_only_one_thread_can_consume_token() {
        let t = tempfile::tempdir().unwrap();
        let (s, _) = preview(t.path());
        let id = s.id.clone();
        let state = CacheCleanerState::default();
        state.0.lock().unwrap().snapshot = Some(s);
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let state = state.clone();
                let id = id.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    take_snapshot(&state, &id, &ids()).is_ok()
                })
            })
            .collect();
        barrier.wait();
        assert_eq!(
            handles
                .into_iter()
                .filter(|h| h.thread().id() != std::thread::current().id())
                .map(|h| h.join().unwrap() as usize)
                .sum::<usize>(),
            1
        );
    }

    #[test]
    fn cache_cleaner_new_scan_invalidates_previous_and_rejects_late_results() {
        let t = tempfile::tempdir().unwrap();
        let state = CacheCleanerState::default();
        let first = begin_scan(&state).unwrap();
        let (s, _) = preview(t.path());
        let old_id = s.id.clone();
        finish_scan(&state, first, s).unwrap();
        let second = begin_scan(&state).unwrap();
        assert!(take_snapshot(&state, &old_id, &ids()).is_err());
        let third = begin_scan(&state).unwrap();
        let (s, _) = preview(t.path());
        let latest = s.id.clone();
        finish_scan(&state, third, s).unwrap();
        let (s, _) = preview(t.path());
        assert!(finish_scan(&state, second, s).is_err());
        assert_eq!(
            state.0.lock().unwrap().snapshot.as_ref().unwrap().id,
            latest
        );
        take_snapshot(&state, &latest, &ids()).unwrap();
        assert!(begin_scan(&state).is_err());
    }

    #[test]
    fn cache_cleaner_log_failure_prevents_all_actions() {
        let t = tempfile::tempdir().unwrap();
        old_file(t.path(), "Temp/a.tmp", 8);
        old_file(t.path(), "Temp/b.tmp", 8);
        let (s, _) = preview(t.path());
        let r = execute_snapshot_with_logger(
            s,
            t.path(),
            &ids(),
            &t.path().join("log/a.jsonl"),
            |_| Ok(false),
            |_| panic!("unlogged action"),
            |_, _, _, _, _| Err("disk full".into()),
        )
        .unwrap();
        assert_eq!(r.skipped_files, 2);
        assert_eq!(r.recycled_files, 0);
        assert!(r.messages.iter().any(|m| m.contains("disk full")));
    }

    #[test]
    fn cache_cleaner_log_open_failure_prevents_actions() {
        let t = tempfile::tempdir().unwrap();
        old_file(t.path(), "Temp/a.tmp", 8);
        let (s, _) = preview(t.path());
        assert!(execute_snapshot(
            s,
            t.path(),
            &ids(),
            t.path(),
            |_| Ok(false),
            |_| panic!("unlogged action")
        )
        .is_err());
    }

    #[test]
    fn cache_cleaner_partial_recycle_failure_continues_and_logs() {
        let t = tempfile::tempdir().unwrap();
        old_file(t.path(), "Temp/a.tmp", 8);
        old_file(t.path(), "Temp/b.tmp", 8);
        let (s, _) = preview(t.path());
        let log = t.path().join("log/a.jsonl");
        let mut calls = 0;
        let r = execute_snapshot(
            s,
            t.path(),
            &ids(),
            &log,
            |_| Ok(false),
            |_| {
                calls += 1;
                if calls == 1 {
                    Err("test locked file".into())
                } else {
                    Ok(())
                }
            },
        )
        .unwrap();
        assert_eq!(calls, 2);
        assert_eq!((r.recycled_files, r.failed_files), (1, 1));
        let events: Vec<serde_json::Value> = fs::read_to_string(log)
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        assert_eq!(events.len(), 4);
        assert_eq!(events[0]["status"], "pending");
        assert_eq!(events[1]["status"], "error");
        assert_eq!(events[2]["status"], "pending");
        assert_eq!(events[3]["status"], "success");
    }

    #[cfg(windows)]
    #[test]
    fn cache_cleaner_windows_junction_is_not_followed() {
        use std::os::windows::process::CommandExt;
        let t = tempfile::tempdir().unwrap();
        let target = t.path().join("outside");
        old_file(&target, "protected.tmp", 8);
        fs::create_dir(t.path().join("Temp")).unwrap();
        let link = t.path().join("Temp").join("link");
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "{}",
            String::from_utf8_lossy(&status.stderr)
        );
        let (s, p) = preview(t.path());
        assert!(s.candidates.is_empty());
        assert!(!p.warnings.is_empty());
        assert!(no_links(&link.join("protected.tmp")).is_err());
        fs::remove_dir(&link).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn cache_cleaner_symlink_is_not_followed() {
        let t = tempfile::tempdir().unwrap();
        let target = old_file(t.path(), "outside/a.tmp", 8);
        fs::create_dir(t.path().join("Temp")).unwrap();
        std::os::unix::fs::symlink(target, t.path().join("Temp/link")).unwrap();
        let (s, p) = preview(t.path());
        assert!(s.candidates.is_empty());
        assert!(!p.warnings.is_empty());
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "仅在显式验证时将自建临时夹具移入 Windows 回收站"]
    fn cache_cleaner_windows_real_recycle_smoke() {
        let t = tempfile::Builder::new()
            .prefix("pinkbin-recycle-smoke-")
            .tempdir()
            .unwrap();
        // Windows 回收站返回长路径，TEMP 则可能使用 ADMINI~1 这样的短路径。
        let canonical = fs::canonicalize(t.path()).unwrap();
        let base = PathBuf::from(canonical.to_str().unwrap().trim_start_matches(r"\\?\"));
        let fixtures = [
            "Temp/pinkbin-smoke.tmp".to_owned(),
            "Google/Chrome/User Data/Default/Cache/Cache_Data/pinkbin-smoke".to_owned(),
            "Microsoft/Edge/User Data/Default/GPUCache/pinkbin-smoke".to_owned(),
            "BraveSoftware/Brave-Browser/User Data/Default/Code Cache/pinkbin-smoke".to_owned(),
            format!("Mozilla/Firefox/Profiles/smoke.default/cache2/entries/{FIREFOX_ENTRY}"),
            "pip/Cache/wheels/pinkbin-smoke.whl".to_owned(),
            "npm-cache/_cacache/content-v2/aa/bb/pinkbin-smoke".to_owned(),
            "CrashDumps/pinkbin-smoke.dmp".to_owned(),
        ];
        let mut paths: Vec<_> = fixtures
            .iter()
            .map(|relative| old_file(&base, relative, 31))
            .collect();
        let roaming = base.join("roaming");
        for id in ELECTRON_IDS {
            paths.push(old_file(
                &roots(&roaming, id).unwrap()[0],
                "pinkbin-smoke",
                0,
            ));
        }
        let protected = old_file(&base, "Google/Chrome/User Data/Default/History", 31);
        let fresh = old_file(&base, "Temp/pinkbin-fresh.tmp", 0);
        let state = CacheCleanerState::default();
        let generation = begin_scan(&state).unwrap();
        let (snapshot, preview) = scan_with_roaming(&base, Some(&roaming), |_| Ok(false));
        let snapshot = snapshot.unwrap();
        assert_eq!(snapshot.candidates.len(), paths.len());
        let expected_bytes: u64 = snapshot.candidates.iter().map(|c| c.stamp.len).sum();
        finish_scan(&state, generation, snapshot).unwrap();
        let selected = ids();
        let snapshot = take_snapshot(&state, &preview.plan_id, &selected).unwrap();
        let log = base.join("logs/recycle-smoke.jsonl");
        let app_data = base.join("app-data");
        let result = execute_recorded_snapshot_with_roaming(
            snapshot,
            &base,
            Some(&roaming),
            &selected,
            &log,
            &app_data,
            |_| Ok(false),
            |path| {
                assert!(paths.iter().any(|fixture| fixture == path));
                crate::recycle_only::recycle(path)
            },
        )
        .unwrap();
        assert_eq!(result.recycled_files, paths.len() as u64);
        assert_eq!(result.recycled_bytes, expected_bytes);
        assert_eq!(result.skipped_files, 0);
        assert_eq!(result.failed_files, 0);
        assert!(result.history_warning.is_none());
        let history = crate::cleanup_history::read_history(&app_data).unwrap();
        assert_eq!(history.records.len(), 1);
        assert_eq!(history.records[0].status, "completed");
        assert_eq!(history.records[0].recycled_files, paths.len() as u64);
        assert_eq!(history.records[0].recycled_bytes, expected_bytes);
        assert!(paths.iter().all(|path| !path.exists()));
        assert!(protected.is_file());
        assert!(fresh.is_file());
        let events: Vec<serde_json::Value> = fs::read_to_string(log)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events.len(), paths.len() * 2);
        for event in events.as_chunks::<2>().0 {
            assert_eq!(event[0]["status"], "pending");
            assert_eq!(event[1]["status"], "success");
        }
        let recycled = trash::os_limited::list()
            .unwrap()
            .into_iter()
            .filter(|item| paths.contains(&item.original_path()))
            .collect::<Vec<_>>();
        assert_eq!(recycled.len(), paths.len());
        trash::os_limited::restore_all(recycled).unwrap();
        assert!(paths.iter().all(|path| path.is_file()));
        assert!(paths
            .iter()
            .all(|path| fs::read(path).unwrap() == b"cached test bytes"));
    }

    #[cfg(windows)]
    #[test]
    fn cache_cleaner_windows_process_enumeration_smoke() {
        assert_eq!(
            browser_running("pinkbin-test-nonexistent-browser.exe"),
            Ok(false)
        );
    }
}
