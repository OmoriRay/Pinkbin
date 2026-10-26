//! Startup-only portable storage. Call before Tauri initializes plugins or WebView2.
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const IDENTIFIER: &str = "dev.pinkbin.app";
const MARKER: &str = ".portable-ready-v1";
const MARKER_CONTENT: &[u8] = b"Pinkbin portable data v1\n";
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
pub struct PortableData {
    pub root: PathBuf,
}

pub fn initialize() -> Result<PortableData, String> {
    let exe = std::env::current_exe().map_err(|e| format!("无法确定程序所在目录：{e}"))?;
    let roaming = dirs::data_dir().ok_or("无法定位原应用数据目录，已停止启动以保护已有配置")?;
    let local =
        dirs::data_local_dir().ok_or("无法定位原 WebView 数据目录，已停止启动以保护已有配置")?;
    initialize_at(
        &exe,
        &roaming.join(IDENTIFIER),
        &local.join(IDENTIFIER),
        || old_instance_running(&exe),
    )
}

// Injectable paths and process check keep tests independent of real AppData and running apps.
fn initialize_at(
    exe: &Path,
    old_roaming: &Path,
    old_local: &Path,
    old_running: impl FnOnce() -> Result<bool, String>,
) -> Result<PortableData, String> {
    if !exe.is_absolute() {
        return Err("程序路径必须是绝对路径，无法创建便携数据目录".into());
    }
    let parent = exe.parent().ok_or("无法确定程序所在目录")?;
    let root = parent.join("PinkbinData");
    let result = (|| -> Result<(), String> {
        let _lock = StartupLock::acquire(parent)?;
        if exists(&root)? {
            require_directory(&root)?;
            let marker = root.join(MARKER);
            if exists(&marker)? {
                require_regular_file(&marker)?;
                let mut expected = [0u8; MARKER_CONTENT.len() + 1];
                use std::io::Read;
                let count = File::open(&marker)
                    .and_then(|mut f| f.read(&mut expected))
                    .map_err(|e| format!("无法读取便携数据就绪标记：{e}"))?;
                if &expected[..count] != MARKER_CONTENT {
                    return Err("便携数据就绪标记无效，已保留现有数据；请恢复原目录后重试".into());
                }
                probe_writable(&root)?;
                return Ok(());
            }
            if fs::read_dir(&root)
                .map_err(|e| format!("无法读取已有便携数据目录：{e}"))?
                .next()
                .is_some()
            {
                return Err("便携数据目录已有内容但缺少有效就绪标记，已停止导入且不会覆盖；请先备份并检查该目录".into());
            }
        }
        probe_writable(parent)?;
        let has_roaming = exists(old_roaming)?;
        let has_local = exists(old_local)?;
        if (has_roaming || has_local) && old_running()? {
            return Err(
                "检测到其他 Pinkbin 正在运行，请退出旧版及其 WebView2 进程后重试迁移".into(),
            );
        }
        let staging = Staging::create(parent)?;
        if has_roaming {
            copy_tree(old_roaming, &staging.path, false)?;
        }
        if has_local {
            let webview = staging.path.join("WebView");
            // Never combine two profiles or overwrite an application-owned directory.
            if exists(&webview)? {
                return Err(
                    "原应用数据中已有 WebView 目录，无法安全合并；请先备份并检查原数据".into(),
                );
            }
            fs::create_dir(&webview).map_err(|e| format!("无法创建 WebView 迁移目录：{e}"))?;
            copy_tree(old_local, &webview, true)?;
        }
        // All copies must finish before marking or publishing. The source is never modified.
        let mut marker = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(staging.path.join(MARKER))
            .map_err(|e| format!("无法创建便携数据就绪标记：{e}"))?;
        marker
            .write_all(MARKER_CONTENT)
            .and_then(|_| marker.sync_all())
            .map_err(|e| format!("无法保存便携数据就绪标记：{e}"))?;
        drop(marker);
        probe_writable(&staging.path)?;
        // Only an empty, unmarked directory may be replaced, under the startup lock.
        if exists(&root)? {
            require_directory(&root)?;
            fs::remove_dir(&root)
                .map_err(|e| format!("便携数据目录已有内容或无法准备，未覆盖任何文件：{e}"))?;
        }
        fs::rename(&staging.path, &root)
            .map_err(|e| format!("无法发布便携数据，原数据已保留，请重试：{e}"))?;
        Ok(())
    })();
    result.map_err(|e| {
        format!(
            "{e}\n目标目录：{}\n请确认目录可写；程序不会改用 AppData 保存。",
            root.display()
        )
    })?;
    Ok(PortableData { root })
}

fn exists(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("无法检查数据路径 {}：{e}", path.display())),
    }
}

fn is_link(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0 // FILE_ATTRIBUTE_REPARSE_POINT
    }
    #[cfg(not(windows))]
    false
}

fn checked_metadata(path: &Path) -> Result<fs::Metadata, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|e| format!("无法读取数据路径 {}：{e}", path.display()))?;
    if is_link(&metadata) {
        return Err(format!(
            "数据目录包含链接或重解析点，已停止迁移以避免越界：{}",
            path.display()
        ));
    }
    Ok(metadata)
}

fn require_directory(path: &Path) -> Result<(), String> {
    if !checked_metadata(path)?.is_dir() {
        return Err(format!("数据路径不是文件夹：{}", path.display()));
    }
    Ok(())
}

fn require_regular_file(path: &Path) -> Result<(), String> {
    if !checked_metadata(path)?.is_file() {
        return Err(format!("数据路径不是普通文件：{}", path.display()));
    }
    Ok(())
}

fn unique_path(parent: &Path, purpose: &str) -> PathBuf {
    parent.join(format!(
        ".PinkbinData-{purpose}-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ))
}

struct Staging {
    path: PathBuf,
}

impl Staging {
    fn create(parent: &Path) -> Result<Self, String> {
        let path = unique_path(parent, "staging");
        fs::create_dir(&path).map_err(|e| format!("无法创建迁移暂存目录：{e}"))?;
        Ok(Self { path })
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        // This exact, newly created directory only contains our own regular copies.
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct StartupLock {
    file: Option<File>,
    path: PathBuf,
}

impl StartupLock {
    fn acquire(parent: &Path) -> Result<Self, String> {
        let path = parent.join(".PinkbinData-startup.lock");
        if exists(&path)? {
            require_regular_file(&path)?;
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.create(true).share_mode(0);
        }
        #[cfg(not(windows))]
        options.create_new(true);
        let file = options.open(&path).map_err(|e| {
            format!("无法取得便携数据启动锁；请关闭其他 Pinkbin，并确认程序目录可写：{e}")
        })?;
        Ok(Self {
            file: Some(file),
            path,
        })
    }
}

impl Drop for StartupLock {
    fn drop(&mut self) {
        drop(self.file.take());
        let _ = fs::remove_file(&self.path);
    }
}

fn probe_writable(directory: &Path) -> Result<(), String> {
    require_directory(directory)?;
    let source = unique_path(directory, "write-probe");
    let target = unique_path(directory, "rename-probe");
    let result = (|| -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&source)?;
        file.write_all(b"Pinkbin writable check\n")?;
        file.sync_all()?;
        drop(file);
        fs::rename(&source, &target)?;
        fs::remove_file(&target)?;
        Ok(())
    })();
    let _ = fs::remove_file(&source);
    let _ = fs::remove_file(&target);
    result.map_err(|e| {
        format!(
            "无法在目录 {} 创建、写入或重命名文件：{e}",
            directory.display()
        )
    })
}

fn skipped_browser_cache(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy().to_ascii_lowercase();
    matches!(
        name.as_str(),
        "cache"
            | "code cache"
            | "gpucache"
            | "dawncache"
            | "dawngraphitecache"
            | "dawnwebgpucache"
            | "shadercache"
            | "grshadercache"
            | "crashpad"
            | "browsermetrics"
    )
}

fn open_exclusive_read(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(0);
    }
    options.open(path).map_err(|e| {
        format!(
            "无法读取原数据或文件正在使用中，请关闭旧 Pinkbin 及其 WebView2 后重试：{}：{e}",
            path.display()
        )
    })
}

fn copy_tree(source: &Path, target: &Path, webview: bool) -> Result<(), String> {
    require_directory(source)?;
    for entry in
        fs::read_dir(source).map_err(|e| format!("无法读取原数据目录 {}：{e}", source.display()))?
    {
        let entry = entry.map_err(|e| format!("原数据目录读取不完整，迁移已停止：{e}"))?;
        let source = entry.path();
        let metadata = checked_metadata(&source)?;
        let name = entry.file_name();
        let destination = target.join(&name);
        if metadata.is_dir() {
            if webview && skipped_browser_cache(&name) {
                continue;
            }
            fs::create_dir(&destination)
                .map_err(|e| format!("无法创建迁移子目录 {}：{e}", destination.display()))?;
            copy_tree(&source, &destination, webview)?;
        } else if metadata.is_file() {
            // Even skipped LOCK files must be obtainable exclusively: a held lock means
            // that the profile is live and cannot be copied consistently.
            let mut input = open_exclusive_read(&source)?;
            if webview && name.to_string_lossy().eq_ignore_ascii_case("LOCK") {
                continue;
            }
            let before = input
                .metadata()
                .map_err(|e| format!("无法检查原数据文件：{e}"))?;
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&destination)
                .map_err(|e| format!("无法复制原数据到 {}：{e}", destination.display()))?;
            let copied = io::copy(&mut input, &mut output)
                .and_then(|count| {
                    output.sync_all()?;
                    Ok(count)
                })
                .map_err(|e| {
                    format!("原数据复制不完整，源数据已保留：{}：{e}", source.display())
                })?;
            let after = input
                .metadata()
                .map_err(|e| format!("无法复查原数据文件：{e}"))?;
            if copied != before.len()
                || before.len() != after.len()
                || before.modified().ok() != after.modified().ok()
            {
                return Err(format!(
                    "原数据在复制时发生变化，请关闭旧 Pinkbin 后重试：{}",
                    source.display()
                ));
            }
        } else {
            return Err(format!(
                "原数据中包含不支持的文件类型，迁移已停止：{}",
                source.display()
            ));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn old_instance_running(exe: &Path) -> Result<bool, String> {
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
            return Err("无法检查旧 Pinkbin 进程，已停止迁移".into());
        }
        struct Snapshot(windows_sys::Win32::Foundation::HANDLE);
        impl Drop for Snapshot {
            fn drop(&mut self) {
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
        let _snapshot = Snapshot(handle);
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(handle, &mut entry) == 0 {
            return Err("无法读取旧 Pinkbin 进程信息，已停止迁移".into());
        }
        let current_name = exe.file_name().unwrap_or_default().to_string_lossy();
        loop {
            let end = entry
                .szExeFile
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..end]);
            if entry.th32ProcessID != std::process::id()
                && (pinkbin_executable_name(&name) || name.eq_ignore_ascii_case(&current_name))
            {
                return Ok(true);
            }
            if Process32NextW(handle, &mut entry) == 0 {
                return if GetLastError() == ERROR_NO_MORE_FILES {
                    Ok(false)
                } else {
                    Err("旧 Pinkbin 进程检查不完整，已停止迁移".into())
                };
            }
        }
    }
}

fn pinkbin_executable_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.starts_with("pinkbin") && name.ends_with(".exe")
}

#[cfg(not(windows))]
fn old_instance_running(_: &Path) -> Result<bool, String> {
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("便携 软件 & 工具");
        fs::create_dir(&directory).unwrap();
        let exe = directory.join("Pinkbin.exe");
        let roaming = temp.path().join("old-roaming");
        let local = temp.path().join("old-local");
        (temp, exe, roaming, local)
    }

    fn write(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn recognizes_versioned_and_custom_old_executable_names() {
        for name in ["pinkbin.exe", "Pinkbin_0.3.9.exe", "PINKBIN_Custom.EXE"] {
            assert!(pinkbin_executable_name(name));
        }
        assert!(!pinkbin_executable_name("unrelated.exe"));
        assert!(!pinkbin_executable_name("pinkbin.dll"));
    }

    #[test]
    fn migration_uses_exe_directory_and_preserves_sources_and_storage() {
        let (_temp, exe, roaming, local) = fixture();
        write(&roaming.join(".window-state.json"), b"window");
        write(
            &roaming.join("cleanup-history-v1/record/begin.json"),
            b"history",
        );
        write(
            &local.join("EBWebView/Default/Local Storage/leveldb/000001.ldb"),
            b"configuration",
        );
        write(
            &local.join("EBWebView/Default/Local Storage/leveldb/LOCK"),
            b"",
        );
        write(&local.join("EBWebView/Default/Cache/data"), b"cache");
        let data = initialize_at(&exe, &roaming, &local, || Ok(false)).unwrap();
        assert_eq!(data.root, exe.parent().unwrap().join("PinkbinData"));
        assert_ne!(
            data.root.parent().unwrap(),
            std::env::current_dir().unwrap()
        );
        assert_eq!(
            fs::read(data.root.join(".window-state.json")).unwrap(),
            b"window"
        );
        assert_eq!(
            fs::read(data.root.join("cleanup-history-v1/record/begin.json")).unwrap(),
            b"history"
        );
        let database = "EBWebView/Default/Local Storage/leveldb/000001.ldb";
        assert_eq!(
            fs::read(data.root.join("WebView").join(database)).unwrap(),
            b"configuration"
        );
        assert_eq!(fs::read(local.join(database)).unwrap(), b"configuration");
        assert_eq!(
            fs::read(roaming.join(".window-state.json")).unwrap(),
            b"window"
        );
        assert!(local.join("EBWebView/Default/Cache/data").exists());
        assert!(!data.root.join("WebView/EBWebView/Default/Cache").exists());
        assert!(!data
            .root
            .join("WebView/EBWebView/Default/Local Storage/leveldb/LOCK")
            .exists());
        assert_eq!(fs::read(data.root.join(MARKER)).unwrap(), MARKER_CONTENT);
    }

    #[test]
    fn ready_data_is_not_overwritten_and_unmarked_data_is_not_imported_over() {
        let (_temp, exe, roaming, local) = fixture();
        let data = initialize_at(&exe, &roaming, &local, || Ok(false)).unwrap();
        write(&data.root.join("setting"), b"portable");
        write(&roaming.join("setting"), b"old");
        initialize_at(&exe, &roaming, &local, || {
            panic!("ready data must not import")
        })
        .unwrap();
        assert_eq!(fs::read(data.root.join("setting")).unwrap(), b"portable");
        fs::remove_file(data.root.join(MARKER)).unwrap();
        assert!(initialize_at(&exe, &roaming, &local, || Ok(false)).is_err());
        assert_eq!(fs::read(data.root.join("setting")).unwrap(), b"portable");
    }

    #[test]
    fn copy_failure_rolls_back_and_allows_retry_without_touching_source() {
        let (_temp, exe, roaming, local) = fixture();
        write(&roaming.join("setting"), b"original");
        // Reserved marker collision fails after actual file copies, before publication.
        write(&roaming.join(MARKER), b"unexpected");
        let root = exe.parent().unwrap().join("PinkbinData");
        assert!(initialize_at(&exe, &roaming, &local, || Ok(false)).is_err());
        assert!(!root.exists());
        assert_eq!(fs::read(roaming.join("setting")).unwrap(), b"original");
        assert_eq!(fs::read(roaming.join(MARKER)).unwrap(), b"unexpected");
        assert_eq!(fs::read_dir(exe.parent().unwrap()).unwrap().count(), 0);
        fs::remove_file(roaming.join(MARKER)).unwrap();
        initialize_at(&exe, &roaming, &local, || Ok(false)).unwrap();
        assert_eq!(fs::read(root.join("setting")).unwrap(), b"original");
    }

    #[test]
    fn active_old_instance_blocks_import_and_missing_parent_never_falls_back() {
        let (_temp, exe, roaming, local) = fixture();
        write(&roaming.join("setting"), b"old");
        assert!(initialize_at(&exe, &roaming, &local, || Ok(true))
            .unwrap_err_string()
            .contains("其他 Pinkbin"));
        assert!(!exe.parent().unwrap().join("PinkbinData").exists());
        let impossible = exe.parent().unwrap().join("missing/Pinkbin.exe");
        let error = initialize_at(&impossible, &roaming, &local, || Ok(false)).unwrap_err_string();
        assert!(error.contains(
            &impossible
                .parent()
                .unwrap()
                .join("PinkbinData")
                .display()
                .to_string()
        ));
        assert!(error.contains("不会改用 AppData"));
        assert_eq!(fs::read(roaming.join("setting")).unwrap(), b"old");
    }

    // Avoid Debug on the public data type solely to satisfy Result::unwrap_err in tests.
    trait ErrorText {
        fn unwrap_err_string(self) -> String;
    }
    impl ErrorText for Result<PortableData, String> {
        fn unwrap_err_string(self) -> String {
            match self {
                Err(e) => e,
                Ok(_) => panic!("expected failure"),
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn held_database_file_prevents_partial_profile_and_retry_succeeds() {
        use std::os::windows::fs::OpenOptionsExt;
        let (_temp, exe, roaming, local) = fixture();
        let database = local.join("EBWebView/Default/Local Storage/leveldb/000001.ldb");
        write(&database, b"configuration");
        let held = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&database)
            .unwrap();
        assert!(initialize_at(&exe, &roaming, &local, || Ok(false)).is_err());
        assert!(!exe.parent().unwrap().join("PinkbinData").exists());
        drop(held);
        let data = initialize_at(&exe, &roaming, &local, || Ok(false)).unwrap();
        assert_eq!(
            fs::read(
                data.root
                    .join("WebView/EBWebView/Default/Local Storage/leveldb/000001.ldb")
            )
            .unwrap(),
            b"configuration"
        );
    }

    #[cfg(windows)]
    #[test]
    fn startup_lock_is_exclusive_and_stale_unheld_file_is_reusable() {
        let (_temp, exe, _, _) = fixture();
        let parent = exe.parent().unwrap();
        fs::write(parent.join(".PinkbinData-startup.lock"), b"").unwrap();
        let lock = StartupLock::acquire(parent).unwrap();
        assert!(StartupLock::acquire(parent).is_err());
        drop(lock);
        assert!(StartupLock::acquire(parent).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn source_junction_is_rejected_without_copying_outside_data() {
        let (temp, exe, roaming, local) = fixture();
        let outside = temp.path().join("outside");
        write(&outside.join("keep"), b"untouched");
        fs::create_dir(&roaming).unwrap();
        let link = roaming.join("linked");
        let output = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&link)
            .arg(&outside)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result = initialize_at(&exe, &roaming, &local, || Ok(false));
        // Remove the junction itself, never its target, before fixture cleanup.
        fs::remove_dir(&link).unwrap();
        assert!(result.unwrap_err_string().contains("重解析点"));
        assert!(!exe.parent().unwrap().join("PinkbinData").exists());
        assert_eq!(fs::read(outside.join("keep")).unwrap(), b"untouched");
    }
}
