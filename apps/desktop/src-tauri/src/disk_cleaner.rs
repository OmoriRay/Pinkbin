//! Metadata-only, bounded candidate collection from local fixed-volume roots.
//! A plan is an in-memory, expiring, single-use capability; paths are never accepted over IPC.
use serde::Serialize;
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions, ReadDir},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime},
};
use tauri::{Emitter, State};

const TTL: Duration = Duration::from_secs(15 * 60);
const CANDIDATE_LIMIT: usize = 20_000;
const PER_GROUP_LIMIT: usize = 5_000;
const MAX_ACTIONS: usize = 10_000;
const MAX_DEPTH: usize = 256;
const CANDIDATE_MEMORY_LIMIT: usize = 64 * 1024 * 1024;
const CATEGORIES: [&str; 5] = ["temp", "crash", "logs", "installers", "large"];
static BUSY: AtomicBool = AtomicBool::new(false);
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Shared with the older cache commands so scans and cleanup never overlap.
pub struct OperationLease;
pub fn reserve_operation() -> Result<OperationLease, String> {
    BUSY.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| OperationLease)
        .map_err(|_| "已有扫描或清理正在进行，请等待完成或取消扫描".into())
}
impl Drop for OperationLease {
    fn drop(&mut self) {
        BUSY.store(false, Ordering::Release);
    }
}

#[derive(Default, Clone)]
pub struct DiskCleanerState(Arc<Mutex<Inner>>);
#[derive(Default)]
struct Inner {
    session: Option<DiskScanSession>,
    snapshot: Option<Snapshot>,
    active: Option<(String, Arc<AtomicBool>)>,
    precancelled: Vec<(String, Instant)>,
}
struct Snapshot {
    id: String,
    created: SystemTime,
    candidates: Vec<Candidate>,
}
#[derive(Clone, Serialize)]
pub struct DiskScanSession {
    scan_id: String,
    drive_ids: Vec<String>,
    status: &'static str,
    progress: Option<Progress>,
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

fn session_snapshot(state: &DiskCleanerState) -> Result<Option<DiskScanSession>, String> {
    let mut inner = state.0.lock().map_err(text_error)?;
    if inner
        .session
        .as_ref()
        .is_some_and(|s| s.status == "completed")
    {
        let valid = inner.snapshot.as_ref().is_some_and(|p| {
            p.created.elapsed().is_ok_and(|age| age <= TTL)
                && inner
                    .session
                    .as_ref()
                    .and_then(|s| s.preview.as_ref())
                    .is_some_and(|v| v.plan_id == p.id)
        });
        if !valid {
            inner.snapshot = None;
            invalidate_session(&mut inner, "预览已过期或已使用，请重新扫描");
        }
    }
    Ok(inner.session.clone())
}

#[tauri::command]
pub fn get_disk_cleanup_scan(
    state: State<'_, DiskCleanerState>,
) -> Result<Option<DiskScanSession>, String> {
    session_snapshot(state.inner())
}

#[tauri::command]
pub fn start_disk_cleanup_scan(
    app: tauri::AppHandle,
    state: State<'_, DiskCleanerState>,
    drive_ids: Vec<String>,
    scan_id: String,
) -> Result<DiskScanSession, String> {
    if let Some(s) = session_snapshot(state.inner())?.filter(|s| s.status == "running") {
        return Ok(s);
    }
    let lease = reserve_operation()?;
    if scan_id.is_empty()
        || scan_id.len() > 128
        || !scan_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("扫描编号无效".into());
    }
    let available = fixed_drives()?;
    if drive_ids.is_empty() || drive_ids.len() > 26 {
        return Err("请选择本地磁盘".into());
    }
    let mut seen = HashSet::new();
    let mut roots = Vec::new();
    for id in &drive_ids {
        if !available.iter().any(|d| &d.id == id) {
            return Err("所选磁盘不可用，请刷新磁盘列表".into());
        }
        if seen.insert(id.clone()) {
            roots.push((id.clone(), PathBuf::from(id)));
        }
    }
    let shared = state.inner().clone();
    let cancel = register_scan(&shared, &scan_id)?;
    let session = DiskScanSession {
        scan_id: scan_id.clone(),
        drive_ids,
        status: "running",
        progress: None,
        preview: None,
        error: None,
    };
    shared.0.lock().map_err(text_error)?.session = Some(session.clone());
    tauri::async_runtime::spawn(async move {
        let worker = shared.clone();
        let worker_id = scan_id.clone();
        // Lease lives in the supervisor until the result has been published,
        // including when the blocking worker panics.
        let _lease = lease;
        let outcome = tokio::task::spawn_blocking(move || {
            let (snapshot, mut preview) = scan_roots(
                &roots,
                &worker_id,
                &cancel,
                Limits::default(),
                |id, stats| {
                    let event = progress(&worker_id, id, stats);
                    if let Ok(mut inner) = worker.0.lock() {
                        if let Some(s) = inner.session.as_mut() {
                            s.progress = Some(event.clone());
                        }
                    }
                    let _ = app.emit("disk-cleanup-progress", event);
                },
            );
            let mut inner = worker.0.lock().map_err(text_error)?;
            if cancel.load(Ordering::Acquire) {
                preview.stats.cancelled = true;
                preview.plan_id.clear();
                preview.groups.clear();
            } else {
                inner.snapshot = snapshot;
            }
            if let Some(s) = inner.session.as_mut() {
                s.status = if preview.stats.cancelled {
                    "cancelled"
                } else {
                    "completed"
                };
                s.preview = if preview.stats.cancelled {
                    None
                } else {
                    Some(Arc::new(preview))
                };
            }
            Ok::<_, String>(())
        })
        .await
        .map_err(|e| format!("扫描任务异常：{e}"))
        .and_then(|r| r);
        if let Ok(mut inner) = shared.0.lock() {
            inner.active = None;
            if let Err(error) = outcome {
                inner.snapshot = None;
                invalidate_session(&mut inner, &error);
            }
        }
    });
    Ok(session)
}
#[derive(Clone)]
struct Candidate {
    id: String,
    category: &'static str,
    path: PathBuf,
    stamp: Stamp,
    // Includes volume root and every parent; identity/creation only for directories.
    parent: Arc<Directory>,
}
struct Directory {
    path: PathBuf,
    stamp: Stamp,
    parent: Option<Arc<Directory>>,
    depth: usize,
    volume: u64,
}
impl Directory {
    fn chain(&self) -> impl Iterator<Item = &Directory> {
        std::iter::successors(Some(self), |dir| dir.parent.as_deref())
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    identity: (u64, u128),
    size: u64,
    modified: SystemTime,
    created: Option<SystemTime>,
}
#[derive(Serialize)]
pub struct Drive {
    id: String,
    label: String,
    total_bytes: u64,
    available_bytes: u64,
}
#[derive(Serialize, Default, Clone, Debug)]
pub struct Stats {
    scanned_files: u64,
    scanned_bytes: u64,
    scanned_directories: u64,
    denied_directories: u64,
    protected_directories: u64,
    linked_entries: u64,
    limited: bool,
    cancelled: bool,
}
#[derive(Serialize)]
pub struct Preview {
    plan_id: String,
    scan_id: String,
    created_at: String,
    groups: Vec<Group>,
    stats: Stats,
    warnings: Vec<String>,
}
#[derive(Serialize)]
struct Group {
    id: &'static str,
    title: &'static str,
    description: &'static str,
    risk: &'static str,
    file_count: u64,
    bytes: u64,
    entries: Vec<Entry>,
}
#[derive(Serialize)]
struct Entry {
    id: String,
    category: &'static str,
    title: String,
    risk: &'static str,
    size: u64,
    path: String,
    modified: String,
    default_selected: bool,
}
#[derive(Serialize, Default)]
pub struct CacheResult {
    recycled_files: u64,
    recycled_bytes: u64,
    skipped_files: u64,
    failed_files: u64,
    messages: Vec<String>,
    log_path: String,
    history_warning: Option<String>,
}
#[derive(Clone, Serialize)]
struct Progress {
    scan_id: String,
    drive_id: String,
    scanned_files: u64,
    scanned_bytes: u64,
    scanned_directories: u64,
    denied_directories: u64,
    protected_directories: u64,
    linked_entries: u64,
}
fn progress(scan_id: &str, drive_id: &str, s: &Stats) -> Progress {
    Progress {
        scan_id: scan_id.into(),
        drive_id: drive_id.into(),
        scanned_files: s.scanned_files,
        scanned_bytes: s.scanned_bytes,
        scanned_directories: s.scanned_directories,
        denied_directories: s.denied_directories,
        protected_directories: s.protected_directories,
        linked_entries: s.linked_entries,
    }
}
fn text_error(e: impl std::fmt::Display) -> String {
    e.to_string()
}
fn token() -> String {
    format!(
        "disk-{}-{}-{}",
        chrono::Utc::now().timestamp_micros(),
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}
fn title(category: &str) -> &'static str {
    match category {
        "temp" => "较旧的临时文件",
        "crash" => "较旧的崩溃转储",
        "logs" => "较旧的日志",
        "installers" => "较旧的安装介质",
        _ => "大文件人工检查",
    }
}
fn description(category: &str) -> &'static str {
    match category {
        "temp" => "至少 7 天未修改的临时或未完成下载文件；可能仍被程序使用，请逐项确认。",
        "crash" => "至少 30 天未修改的转储；删除后将失去对应故障诊断资料。",
        "logs" => "至少 30 天未修改的日志或 .old 文件；可能用于审计、恢复或排障。",
        "installers" => "至少 30 天未修改的安装包或镜像；可能仍需用于安装、修复或离线恢复。",
        _ => "至少 500 MiB 的普通文件；大文件并不代表垃圾，仅供人工判断是否保留。",
    }
}

#[tauri::command]
pub fn list_cleanup_drives() -> Result<Vec<Drive>, String> {
    fixed_drives()
}
#[cfg(windows)]
fn fixed_drives() -> Result<Vec<Drive>, String> {
    use windows_sys::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives,
    };
    let bits = unsafe { GetLogicalDrives() };
    if bits == 0 {
        return Err("无法枚举本地磁盘".into());
    }
    let mut result = Vec::new();
    for index in 0..26 {
        if bits & (1 << index) == 0 {
            continue;
        }
        let id = format!("{}:\\", (b'A' + index as u8) as char);
        let wide: Vec<u16> = id.encode_utf16().chain(Some(0)).collect();
        if unsafe { GetDriveTypeW(wide.as_ptr()) } != 3 {
            continue;
        }
        let (mut available, mut total, mut free) = (0u64, 0u64, 0u64);
        if unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, &mut total, &mut free) } == 0
        {
            continue;
        }
        result.push(Drive {
            label: format!("本地磁盘 ({})", &id[..2]),
            id,
            total_bytes: total,
            available_bytes: available,
        });
    }
    Ok(result)
}
#[cfg(not(windows))]
fn fixed_drives() -> Result<Vec<Drive>, String> {
    Ok(Vec::new())
}

#[tauri::command]
pub async fn preview_disk_cleanup(
    app: tauri::AppHandle,
    state: State<'_, DiskCleanerState>,
    drive_ids: Vec<String>,
    scan_id: String,
) -> Result<Preview, String> {
    let lease = reserve_operation()?;
    if scan_id.is_empty()
        || scan_id.len() > 128
        || !scan_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("扫描编号无效".into());
    }
    let available = fixed_drives()?;
    if drive_ids.is_empty() || drive_ids.len() > 26 {
        return Err("请选择本地磁盘".into());
    }
    let mut seen = HashSet::new();
    let mut roots = Vec::new();
    for id in drive_ids {
        if !available.iter().any(|d| d.id == id) {
            return Err("所选磁盘不可用，请刷新磁盘列表".into());
        }
        if seen.insert(id.clone()) {
            roots.push((id.clone(), PathBuf::from(id)));
        }
    }
    let shared = state.inner().clone();
    let cancel = register_scan(&shared, &scan_id)?;
    tokio::task::spawn_blocking(move || {
        let _lease = lease;
        struct ClearActive(DiskCleanerState);
        impl Drop for ClearActive {
            fn drop(&mut self) {
                if let Ok(mut inner) = self.0 .0.lock() {
                    inner.active = None;
                }
            }
        }
        let _clear = ClearActive(shared.clone());
        let (snapshot, mut preview) =
            scan_roots(&roots, &scan_id, &cancel, Limits::default(), |id, stats| {
                let _ = app.emit("disk-cleanup-progress", progress(&scan_id, id, stats));
            });
        let mut inner = shared.0.lock().map_err(text_error)?;
        // Closing cancellation race: cancel and publishing the capability use the same mutex.
        if cancel.load(Ordering::Acquire) {
            preview.stats.cancelled = true;
            preview.plan_id.clear();
            preview.groups.clear();
        } else {
            inner.snapshot = snapshot;
        }
        inner.active = None;
        Ok(preview)
    })
    .await
    .map_err(text_error)?
}
#[tauri::command]
pub fn cancel_disk_cleanup_scan(
    state: State<'_, DiskCleanerState>,
    scan_id: String,
) -> Result<bool, String> {
    request_cancel(state.inner(), &scan_id)
}
fn register_scan(state: &DiskCleanerState, scan_id: &str) -> Result<Arc<AtomicBool>, String> {
    let cancel = Arc::new(AtomicBool::new(false));
    let mut inner = state.0.lock().map_err(text_error)?;
    inner
        .precancelled
        .retain(|(_, created)| created.elapsed() < TTL);
    if let Some(index) = inner.precancelled.iter().position(|(id, _)| id == scan_id) {
        inner.precancelled.remove(index);
        cancel.store(true, Ordering::Release);
    }
    inner.snapshot = None;
    inner.session = None;
    inner.active = Some((scan_id.into(), cancel.clone()));
    Ok(cancel)
}
fn request_cancel(state: &DiskCleanerState, scan_id: &str) -> Result<bool, String> {
    if scan_id.is_empty()
        || scan_id.len() > 128
        || !scan_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("扫描编号无效".into());
    }
    let mut inner = state.0.lock().map_err(text_error)?;
    if let Some((active, cancel)) = &inner.active {
        if active == scan_id {
            cancel.store(true, Ordering::Release);
            return Ok(true);
        }
    }
    inner
        .precancelled
        .retain(|(_, created)| created.elapsed() < TTL);
    if !inner.precancelled.iter().any(|(id, _)| id == scan_id) {
        if inner.precancelled.len() >= 32 {
            inner.precancelled.remove(0);
        }
        inner.precancelled.push((scan_id.into(), Instant::now()));
    }
    Ok(true)
}

fn unsafe_metadata(m: &fs::Metadata) -> bool {
    if m.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Reparse, offline, recall-on-open and recall-on-data-access: do not hydrate cloud files.
        if m.file_attributes() & (0x400 | 0x1000 | 0x40000 | 0x400000) != 0 {
            return true;
        }
    }
    false
}
fn safe_ancestors(path: &Path) -> io::Result<()> {
    for p in path.ancestors() {
        if unsafe_metadata(&fs::symlink_metadata(p)?) {
            return Err(io::Error::other("链接或云占位条目"));
        }
    }
    Ok(())
}
fn stamp(path: &Path, directory: bool) -> io::Result<Stamp> {
    safe_ancestors(path)?;
    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_READ_ATTRIBUTES only: no business file content is opened/read.
        OpenOptions::new()
            .access_mode(0x80)
            .share_mode(7)
            .custom_flags(0x00200000 | if directory { 0x02000000 } else { 0 })
            .open(path)?
    };
    #[cfg(not(windows))]
    let file = File::open(path)?;
    stamp_handle(&file, directory)
}
fn stamp_handle(file: &File, directory: bool) -> io::Result<Stamp> {
    let m = file.metadata()?;
    if unsafe_metadata(&m) || (directory && !m.is_dir()) || (!directory && !m.is_file()) {
        return Err(io::Error::other("文件类型不安全"));
    }
    #[cfg(windows)]
    let identity = {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FileIdInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
            BY_HANDLE_FILE_INFORMATION, FILE_ID_INFO,
        };
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if !directory && info.nNumberOfLinks != 1 {
            return Err(io::Error::other("多硬链接文件"));
        }
        let mut id: FILE_ID_INFO = unsafe { std::mem::zeroed() };
        if unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle() as _,
                FileIdInfo,
                &mut id as *mut _ as _,
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        (
            id.VolumeSerialNumber,
            u128::from_le_bytes(id.FileId.Identifier),
        )
    };
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        if !directory && m.nlink() != 1 {
            return Err(io::Error::other("多硬链接文件"));
        }
        (m.dev(), m.ino() as u128)
    };
    #[cfg(not(any(windows, unix)))]
    let identity = (0, 0);
    Ok(Stamp {
        identity,
        size: if directory { 0 } else { m.len() },
        modified: if directory {
            SystemTime::UNIX_EPOCH
        } else {
            m.modified()?
        },
        created: m.created().ok(),
    })
}
fn protected_directory(path: &Path) -> bool {
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();
    matches!(
        name.as_str(),
        "windows"
            | "windows.old"
            | "program files"
            | "program files (x86)"
            | "programdata"
            | "installer"
            | "winsxs"
            | "driverstore"
            | "system volume information"
            | "$recycle.bin"
            | "recovery"
            | "$windows.~bt"
            | "$windows.~ws"
            | "config.msi"
            | "msocache"
            | "boot"
            | "efi"
            | ".git"
            | ".svn"
            | ".hg"
            | "node_modules"
            | "venv"
            | ".venv"
            | "envs"
            | "site-packages"
            | "windowsapps"
            | "wpsystem"
            | "wudownloadcache"
            | "packages"
    )
}
fn important_file(path: &Path) -> bool {
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();
    matches!(
        name.as_str(),
        "pagefile.sys"
            | "hiberfil.sys"
            | "swapfile.sys"
            | "bootmgr"
            | "bootnxt"
            | "ntldr"
            | "ntdetect.com"
            | "boot.ini"
            | "ntuser.dat"
            | "usrclass.dat"
    ) || name.starts_with("ntuser.dat.")
        || name.starts_with("usrclass.dat.")
}
fn classify(path: &Path, size: u64, modified: SystemTime, now: SystemTime) -> Option<&'static str> {
    if important_file(path) {
        return None;
    }
    let age = now.duration_since(modified).unwrap_or_default().as_secs();
    let ext = path
        .extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();
    if age >= 7 * 86400 && matches!(ext.as_str(), "tmp" | "temp" | "part" | "crdownload") {
        Some("temp")
    } else if age >= 30 * 86400 && matches!(ext.as_str(), "dmp" | "mdmp") {
        Some("crash")
    } else if age >= 30 * 86400 && matches!(ext.as_str(), "log" | "old") {
        Some("logs")
    } else if age >= 30 * 86400 && matches!(ext.as_str(), "msi" | "msix" | "appx" | "iso" | "cab") {
        Some("installers")
    } else if size >= 500 * 1024 * 1024 {
        Some("large")
    } else {
        None
    }
}
#[derive(Clone, Copy)]
struct Limits {
    candidates: usize,
    per_group: usize,
    memory: usize,
    duration: Duration,
    depth: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            candidates: CANDIDATE_LIMIT,
            per_group: PER_GROUP_LIMIT,
            memory: CANDIDATE_MEMORY_LIMIT,
            duration: TTL,
            depth: MAX_DEPTH,
        }
    }
}
struct Frame {
    entries: ReadDir,
    directory: Arc<Directory>,
}

fn scan_roots(
    roots: &[(String, PathBuf)],
    scan_id: &str,
    cancel: &AtomicBool,
    limits: Limits,
    mut notify: impl FnMut(&str, &Stats),
) -> (Option<Snapshot>, Preview) {
    let started = Instant::now();
    let now = SystemTime::now();
    let mut last_progress = Instant::now();
    let mut preview = Preview {
        plan_id: token(),
        scan_id: scan_id.into(),
        created_at: chrono::Utc::now().to_rfc3339(),
        groups: CATEGORIES
            .into_iter()
            .map(|id| Group {
                id,
                title: title(id),
                description: description(id),
                risk: "review",
                file_count: 0,
                bytes: 0,
                entries: Vec::new(),
            })
            .collect(),
        stats: Stats::default(),
        warnings: Vec::new(),
    };
    let mut candidates = Vec::new();
    let mut retained_bytes = 0usize;
    'volumes: for (drive_id, root) in roots {
        if cancel.load(Ordering::Acquire) {
            break;
        }
        let root_stamp = match stamp(root, true) {
            Ok(v) => v,
            Err(_) => {
                preview.stats.denied_directories += 1;
                continue;
            }
        };
        let entries = match fs::read_dir(root) {
            Ok(v) => v,
            Err(_) => {
                preview.stats.denied_directories += 1;
                continue;
            }
        };
        preview.stats.scanned_directories += 1;
        let volume = root_stamp.identity.0;
        let mut frames = vec![Frame {
            entries,
            directory: Arc::new(Directory {
                path: root.clone(),
                stamp: root_stamp,
                parent: None,
                depth: 1,
                volume,
            }),
        }];
        while !frames.is_empty() {
            if cancel.load(Ordering::Acquire) {
                break 'volumes;
            }
            if started.elapsed() >= limits.duration {
                preview.stats.limited = true;
                break 'volumes;
            }
            if last_progress.elapsed() >= Duration::from_millis(150) {
                notify(drive_id, &preview.stats);
                last_progress = Instant::now();
            }
            let frame = frames.last_mut().unwrap();
            let entry = match frame.entries.next() {
                None => {
                    frames.pop();
                    continue;
                }
                Some(Err(_)) => {
                    preview.stats.denied_directories += 1;
                    continue;
                }
                Some(Ok(v)) => v,
            };
            let path = entry.path();
            let meta = match fs::symlink_metadata(&path) {
                Ok(v) => v,
                Err(_) => {
                    preview.stats.denied_directories += 1;
                    continue;
                }
            };
            if unsafe_metadata(&meta) {
                preview.stats.linked_entries += 1;
                continue;
            }
            if meta.is_dir() {
                if protected_directory(&path) {
                    preview.stats.protected_directories += 1;
                    continue;
                }
                if frame.directory.depth >= limits.depth {
                    preview.stats.limited = true;
                    preview.stats.protected_directories += 1;
                    continue;
                }
                let identity = match stamp(&path, true) {
                    Ok(v) => v,
                    Err(_) => {
                        preview.stats.denied_directories += 1;
                        continue;
                    }
                };
                // No traversal across a mounted volume, even when its root is not tagged as a reparse point.
                if identity.identity.0 != frame.directory.volume {
                    preview.stats.linked_entries += 1;
                    continue;
                }
                let entries = match fs::read_dir(&path) {
                    Ok(v) => v,
                    Err(_) => {
                        preview.stats.denied_directories += 1;
                        continue;
                    }
                };
                let directory = Arc::new(Directory {
                    path,
                    stamp: identity,
                    parent: Some(frame.directory.clone()),
                    depth: frame.directory.depth + 1,
                    volume: frame.directory.volume,
                });
                preview.stats.scanned_directories += 1;
                frames.push(Frame { entries, directory });
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            preview.stats.scanned_files += 1;
            preview.stats.scanned_bytes = preview.stats.scanned_bytes.saturating_add(meta.len());
            let Ok(modified) = meta.modified() else {
                continue;
            };
            let Some(category) = classify(&path, meta.len(), modified, now) else {
                continue;
            };
            let group = preview
                .groups
                .iter_mut()
                .find(|g| g.id == category)
                .unwrap();
            // Conservative accounting intentionally charges a complete shared parent chain
            // per candidate. This bounds adversarial deep/long-path result payloads too.
            let cost = path
                .as_os_str()
                .len()
                .saturating_mul(8)
                .saturating_add(1024)
                .saturating_add(
                    frame
                        .directory
                        .chain()
                        .map(|d| {
                            d.path
                                .as_os_str()
                                .len()
                                .saturating_mul(4)
                                .saturating_add(256)
                        })
                        .sum::<usize>(),
                );
            if candidates.len() >= limits.candidates
                || group.entries.len() >= limits.per_group
                || retained_bytes.saturating_add(cost) > limits.memory
            {
                preview.stats.limited = true;
                continue;
            }
            // Validate the traversal's saved chain before saving an actionable candidate.
            if frame
                .directory
                .chain()
                .any(|d| stamp(&d.path, true).ok().as_ref() != Some(&d.stamp))
            {
                preview.stats.linked_entries += 1;
                continue;
            }
            let file_stamp = match stamp(&path, false) {
                Ok(v)
                    if v.identity.0 == frame.directory.volume
                        && v.size == meta.len()
                        && v.modified == modified =>
                {
                    v
                }
                _ => {
                    preview.stats.linked_entries += 1;
                    continue;
                }
            };
            let id = format!("file-{}", candidates.len());
            retained_bytes = retained_bytes.saturating_add(cost);
            group.file_count += 1;
            group.bytes = group.bytes.saturating_add(file_stamp.size);
            group.entries.push(Entry {
                id: id.clone(),
                category,
                title: path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into(),
                risk: "review",
                size: file_stamp.size,
                path: path.to_string_lossy().into(),
                modified: chrono::DateTime::<chrono::Utc>::from(file_stamp.modified).to_rfc3339(),
                default_selected: false,
            });
            candidates.push(Candidate {
                id,
                category,
                path,
                stamp: file_stamp,
                parent: frame.directory.clone(),
            });
        }
        notify(drive_id, &preview.stats);
    }
    preview.stats.cancelled = cancel.load(Ordering::Acquire);
    if preview.stats.protected_directories > 0 {
        preview
            .warnings
            .push("系统目录、软件环境及过深目录已保护跳过；扫描范围不包含这些目录。".into());
    }
    if preview.stats.denied_directories > 0 {
        preview
            .warnings
            .push("部分目录或条目无法访问，已跳过；结果不代表已检查所有文件。".into());
    }
    if preview.stats.linked_entries > 0 {
        preview
            .warnings
            .push("链接、多硬链接、云占位或身份无法验证的条目已跳过。".into());
    }
    if preview.stats.limited {
        preview.warnings.push("达到候选数量、64 MiB 候选内存预算、目录深度或 15 分钟时间上限；仅显示部分候选。候选数量与内存预算不会停止后续遍历计数。".into());
    }
    if preview.stats.cancelled {
        preview.plan_id.clear();
        preview.groups.clear();
        preview
            .warnings
            .push("扫描已取消，未生成可执行清理预览。".into());
        return (None, preview);
    }
    let snapshot = Snapshot {
        id: preview.plan_id.clone(),
        created: SystemTime::now(),
        candidates,
    };
    (Some(snapshot), preview)
}

fn take_snapshot(
    state: &DiskCleanerState,
    plan_id: &str,
    ids: &[String],
) -> Result<Snapshot, String> {
    if ids.is_empty() || ids.len() > MAX_ACTIONS {
        return Err("请选择 1 至 10000 个文件".into());
    }
    let mut inner = state.0.lock().map_err(text_error)?;
    let plan = inner.snapshot.as_ref().ok_or("预览已失效，请重新扫描")?;
    if plan.id != plan_id {
        return Err("预览编号无效，请重新扫描".into());
    }
    if plan.created.elapsed().map_or(true, |age| age > TTL) {
        inner.snapshot = None;
        invalidate_session(&mut inner, "预览已超过 15 分钟，请重新扫描");
        return Err("预览已超过 15 分钟，请重新扫描".into());
    }
    let available: HashSet<_> = plan.candidates.iter().map(|c| c.id.as_str()).collect();
    let mut unique = HashSet::new();
    if ids
        .iter()
        .any(|id| !available.contains(id.as_str()) || !unique.insert(id.as_str()))
    {
        return Err("所选文件编号无效或重复".into());
    }
    let snapshot = inner.snapshot.take().unwrap();
    invalidate_session(&mut inner, "预览已使用，请重新扫描");
    Ok(snapshot)
}
fn valid_candidate(c: &Candidate) -> bool {
    c.path.parent() == Some(c.parent.path.as_path())
        && c.parent
            .chain()
            .all(|d| stamp(&d.path, true).ok().as_ref() == Some(&d.stamp))
        && stamp(&c.path, false).ok().as_ref() == Some(&c.stamp)
        && classify(&c.path, c.stamp.size, c.stamp.modified, SystemTime::now()) == Some(c.category)
}
/// Keep parent directories open without FILE_SHARE_DELETE throughout each action.
/// Once every handle has been acquired, validating the complete chain catches any
/// replacement during acquisition and Windows prevents subsequent ancestor rename.
fn hold_parents(c: &Candidate) -> io::Result<Vec<File>> {
    c.parent
        .chain()
        .map(|directory| {
            let path = &directory.path;
            safe_ancestors(path)?;
            #[cfg(windows)]
            let file = {
                use std::os::windows::fs::OpenOptionsExt;
                OpenOptions::new()
                    .access_mode(0x80 | 1)
                    .share_mode(3)
                    .custom_flags(0x02000000 | 0x00200000)
                    .open(path)?
            };
            #[cfg(not(windows))]
            let file = File::open(path)?;
            if stamp_handle(&file, true)? != directory.stamp {
                return Err(io::Error::other("父目录句柄身份已改变"));
            }
            Ok(file)
        })
        .collect()
}
fn hold_file(path: &Path) -> io::Result<File> {
    safe_ancestors(path)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Include FILE_READ_DATA to participate in Windows share checking; attributes-only
        // opens do not enforce sharing. No read is issued against this handle.
        // Allow Shell's DELETE access but exclude writers for the duration of the action.
        OpenOptions::new()
            .access_mode(0x80 | 1)
            .share_mode(1 | 4)
            .custom_flags(0x00200000)
            .open(path)
    }
    #[cfg(not(windows))]
    {
        File::open(path)
    }
}
#[tauri::command]
pub async fn execute_disk_cleanup(
    app: tauri::AppHandle,
    state: State<'_, DiskCleanerState>,
    plan_id: String,
    file_ids: Vec<String>,
) -> Result<CacheResult, String> {
    let lease = reserve_operation()?;
    let app_data = crate::app_data_dir(&app);
    let snapshot = take_snapshot(state.inner(), &plan_id, &file_ids)?;
    let drives = fixed_drives()?;
    if snapshot.candidates.iter().any(|c| {
        !drives.iter().any(|d| {
            c.parent
                .chain()
                .last()
                .is_some_and(|root| root.path == PathBuf::from(&d.id))
        })
    }) {
        return Err("磁盘列表已改变，请重新扫描".into());
    }
    tokio::task::spawn_blocking(move || {
        let _lease = lease;
        execute_recorded(snapshot, &file_ids, &app_data, |p| {
            crate::recycle_only::recycle(p)
        })
    })
    .await
    .map_err(text_error)?
}
fn log_line(log: &mut File, plan: &str, path: &Path, status: &str) -> Result<(), String> {
    serde_json::to_writer(&mut *log, &serde_json::json!({"plan_id": plan, "path": path, "status": status, "timestamp": chrono::Utc::now().to_rfc3339()})).map_err(text_error)?;
    log.write_all(b"\n")
        .and_then(|_| log.sync_all())
        .map_err(text_error)
}
fn execute_recorded(
    snapshot: Snapshot,
    ids: &[String],
    app_data: &Path,
    recycle: impl FnMut(&Path) -> Result<(), String>,
) -> Result<CacheResult, String> {
    let selected: HashSet<_> = ids.iter().map(String::as_str).collect();
    let mut categories = HashSet::new();
    let rules = snapshot
        .candidates
        .iter()
        .filter(|c| selected.contains(c.id.as_str()) && categories.insert(c.category))
        .map(|c| crate::cleanup_history::HistoryRule {
            id: format!("disk-{}", c.category),
            title: title(c.category).into(),
        })
        .collect();
    let history = crate::cleanup_history::start_record(app_data, &snapshot.id, rules)
        .map_err(|e| format!("未能保存清理记录，本次未执行清理，请重新扫描。{e}"))?;
    let outcome = execute_files(&snapshot, &selected, app_data, recycle);
    let (summary, failure) = match &outcome {
        Ok(r) => (
            crate::cleanup_history::HistorySummary {
                recycled_files: r.recycled_files,
                recycled_bytes: r.recycled_bytes,
                skipped_files: r.skipped_files,
                failed_files: r.failed_files,
            },
            (!r.messages.is_empty() || r.failed_files > 0)
                .then_some("部分项目未完成，请查看本次结果"),
        ),
        Err(_) => (
            crate::cleanup_history::HistorySummary::default(),
            Some("清理未能完成"),
        ),
    };
    let finish = crate::cleanup_history::finish_record(&history, summary, failure);
    match outcome {
        Ok(mut result) => {
            if finish.is_err() {
                result.history_warning =
                    Some("清理结果未能保存到历史，请以本次真实统计为准。".into());
            }
            Ok(result)
        }
        Err(e) => Err(if finish.is_err() {
            format!("{e}；结果历史保存失败")
        } else {
            e
        }),
    }
}
fn execute_files(
    snapshot: &Snapshot,
    selected: &HashSet<&str>,
    app_data: &Path,
    mut recycle: impl FnMut(&Path) -> Result<(), String>,
) -> Result<CacheResult, String> {
    execute_files_with_logger(snapshot, selected, app_data, &mut recycle, log_line)
}
fn execute_files_with_logger(
    snapshot: &Snapshot,
    selected: &HashSet<&str>,
    app_data: &Path,
    mut recycle: impl FnMut(&Path) -> Result<(), String>,
    mut logger: impl FnMut(&mut File, &str, &Path, &str) -> Result<(), String>,
) -> Result<CacheResult, String> {
    safe_ancestors(app_data).map_err(text_error)?;
    let log_path = app_data.join(format!("{}-actions.jsonl", snapshot.id));
    let mut log = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&log_path)
        .map_err(|e| format!("无法创建日志，未执行清理：{e}"))?;
    let mut result = CacheResult {
        log_path: log_path.to_string_lossy().into(),
        ..Default::default()
    };
    let started = Instant::now();
    let mut stopped = false;
    for (index, c) in snapshot
        .candidates
        .iter()
        .filter(|c| selected.contains(c.id.as_str()))
        .enumerate()
    {
        if stopped || index >= MAX_ACTIONS || started.elapsed() >= Duration::from_secs(60) {
            if !stopped {
                result
                    .messages
                    .push("清理达到时间或数量上限，其余文件已跳过，请重新扫描。".into());
                stopped = true;
            }
            result.skipped_files += 1;
            continue;
        }
        let Ok(_parent_handles) = hold_parents(c) else {
            result.skipped_files += 1;
            continue;
        };
        let Ok(target_handle) = hold_file(&c.path) else {
            result.skipped_files += 1;
            continue;
        };
        if stamp_handle(&target_handle, false).ok().as_ref() != Some(&c.stamp)
            || !valid_candidate(c)
        {
            result.skipped_files += 1;
            continue;
        }
        if logger(&mut log, &snapshot.id, &c.path, "pending").is_err() {
            result
                .messages
                .push("准备日志写入失败，已停止清理。".into());
            stopped = true;
            result.skipped_files += 1;
            continue;
        }
        // trash operates on a path. Revalidate after durable pending and immediately before calling it.
        if stamp_handle(&target_handle, false).ok().as_ref() != Some(&c.stamp)
            || !valid_candidate(c)
        {
            result.skipped_files += 1;
            continue;
        }
        let status = match recycle(&c.path) {
            Ok(()) if matches!(fs::symlink_metadata(&c.path), Err(e) if e.kind() == io::ErrorKind::NotFound) =>
            {
                result.recycled_files += 1;
                result.recycled_bytes = result.recycled_bytes.saturating_add(c.stamp.size);
                "success"
            }
            Ok(()) => {
                result.failed_files += 1;
                if !result.messages.iter().any(|m| m.contains("未能确认")) {
                    result
                        .messages
                        .push("部分文件未能确认移入回收站，未计入成功数量。".into());
                }
                "uncertain"
            }
            Err(error) => {
                result.failed_files += 1;
                if result.messages.len() < 8 {
                    result.messages.push(format!("文件未能确认回收：{error}"));
                }
                "failed"
            }
        };
        if logger(&mut log, &snapshot.id, &c.path, status).is_err() {
            result
                .messages
                .push("结果日志写入失败，已停止后续清理；已执行统计仍然有效。".into());
            stopped = true;
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::{Cell, RefCell},
        fs::FileTimes,
    };

    fn old_file(path: &Path, days: u64) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"fixture only").unwrap();
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(
                FileTimes::new()
                    .set_modified(SystemTime::now() - Duration::from_secs(days * 86400)),
            )
            .unwrap();
    }
    fn scan(root: &Path) -> (Snapshot, Preview) {
        let (plan, preview) = scan_roots(
            &[("fixture".into(), root.into())],
            "scan-fixture",
            &AtomicBool::new(false),
            Limits::default(),
            |_, _| {},
        );
        (plan.unwrap(), preview)
    }
    fn selected(plan: &Snapshot) -> HashSet<&str> {
        plan.candidates.iter().map(|c| c.id.as_str()).collect()
    }

    fn completed_session(plan: Snapshot, preview: Preview) -> DiskCleanerState {
        let state = DiskCleanerState::default();
        let mut inner = state.0.lock().unwrap();
        inner.session = Some(DiskScanSession {
            scan_id: preview.scan_id.clone(),
            drive_ids: vec!["fixture".into()],
            status: "completed",
            progress: None,
            preview: Some(Arc::new(preview)),
            error: None,
        });
        inner.snapshot = Some(plan);
        drop(inner);
        state
    }

    #[test]
    fn completed_session_survives_readers_without_cloning_or_consuming_plan() {
        let tmp = tempfile::tempdir().unwrap();
        old_file(&tmp.path().join("old.tmp"), 8);
        let (plan, preview) = scan(tmp.path());
        let state = completed_session(plan, preview);
        let first = session_snapshot(&state).unwrap().unwrap();
        let second = session_snapshot(&state.clone()).unwrap().unwrap();
        assert_eq!(second.status, "completed");
        assert!(Arc::ptr_eq(
            first.preview.as_ref().unwrap(),
            second.preview.as_ref().unwrap()
        ));
        assert!(state.0.lock().unwrap().snapshot.is_some());
        let json = serde_json::to_value(second).unwrap();
        assert!(json["preview"]["plan_id"].is_string());
    }

    #[test]
    fn completed_session_cannot_restore_consumed_or_expired_capability() {
        let tmp = tempfile::tempdir().unwrap();
        old_file(&tmp.path().join("old.tmp"), 8);
        for expired in [false, true] {
            let (mut plan, preview) = scan(tmp.path());
            let id = plan.id.clone();
            let ids = vec![plan.candidates[0].id.clone()];
            if expired {
                plan.created = SystemTime::now() - TTL - Duration::from_secs(1);
            }
            let state = completed_session(plan, preview);
            if !expired {
                assert!(take_snapshot(&state, &id, &ids).is_ok());
            }
            let restored = session_snapshot(&state).unwrap().unwrap();
            assert_eq!(restored.status, "failed");
            assert!(restored.preview.is_none());
            assert!(restored.error.is_some());
            assert!(take_snapshot(&state, &id, &ids).is_err());
        }
    }

    #[test]
    fn multi_root_production_walk_classifies_and_defaults_to_no_selection() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        old_file(&a.path().join("ordinary/downloads/a.tmp"), 8);
        old_file(&a.path().join("ordinary/dump.dmp"), 31);
        old_file(&b.path().join("photos/archive.log"), 31);
        old_file(&b.path().join("downloads/setup.msi"), 31);
        old_file(&b.path().join("downloads/setup.exe"), 90);
        old_file(&b.path().join("backup.bak"), 90);
        let large = File::create(b.path().join("family-video.bin")).unwrap();
        large.set_len(500 * 1024 * 1024).unwrap();
        let (plan, preview) = scan_roots(
            &[("A".into(), a.path().into()), ("B".into(), b.path().into())],
            "multi",
            &AtomicBool::new(false),
            Limits::default(),
            |_, _| {},
        );
        assert_eq!(plan.unwrap().candidates.len(), 5);
        assert_eq!(preview.stats.scanned_files, 7);
        assert!(preview.stats.scanned_bytes >= 500 * 1024 * 1024);
        for group in preview.groups {
            assert_eq!(group.file_count, 1);
            assert!(!group.description.is_empty());
            assert!(group
                .entries
                .iter()
                .all(|e| !e.default_selected && e.risk == "review"));
        }
    }
    #[test]
    fn retention_boundaries_and_future_time_are_conservative() {
        let now = SystemTime::now();
        let at = |days: u64| now - Duration::from_secs(days * 86400);
        assert_eq!(classify(Path::new("a.tmp"), 1, at(7), now), Some("temp"));
        assert_eq!(classify(Path::new("a.tmp"), 1, at(6), now), None);
        for name in [
            "a.dmp", "a.mdmp", "a.log", "a.old", "a.msi", "a.iso", "a.cab", "a.msix", "a.appx",
        ] {
            assert!(classify(Path::new(name), 1, at(30), now).is_some());
            assert_eq!(classify(Path::new(name), 1, at(29), now), None);
        }
        assert_eq!(classify(Path::new("my.exe"), 1, at(99), now), None);
        assert_eq!(classify(Path::new("my.bak"), 1, at(99), now), None);
        assert_eq!(
            classify(Path::new("a.tmp"), 1, now + Duration::from_secs(20), now),
            None
        );
        assert_eq!(
            classify(Path::new("pagefile.sys"), u64::MAX, at(99), now),
            None
        );
        assert_eq!(
            classify(Path::new("a.bin"), 500 * 1024 * 1024 - 1, now, now),
            None
        );
    }
    #[test]
    fn protected_environments_are_pruned_but_user_subtrees_are_walked() {
        let tmp = tempfile::tempdir().unwrap();
        for dir in [
            "Windows",
            "Program Files",
            "ProgramData",
            "Installer",
            "WinSxS",
            "DriverStore",
            "$RECYCLE.BIN",
            "Recovery",
            ".git",
            "node_modules",
            ".venv",
        ] {
            old_file(&tmp.path().join(dir).join("hidden.tmp"), 99);
        }
        old_file(&tmp.path().join("Documents/project/ordinary.tmp"), 8);
        let (plan, preview) = scan(tmp.path());
        assert_eq!(plan.candidates.len(), 1);
        assert_eq!(preview.stats.protected_directories, 11);
        assert_eq!(preview.stats.scanned_files, 1);
    }
    #[test]
    fn hardlinks_are_never_candidates() {
        let tmp = tempfile::tempdir().unwrap();
        old_file(&tmp.path().join("first.tmp"), 9);
        fs::hard_link(tmp.path().join("first.tmp"), tmp.path().join("second.tmp")).unwrap();
        let (plan, preview) = scan(tmp.path());
        assert!(plan.candidates.is_empty());
        assert_eq!(preview.stats.linked_entries, 2);
        assert_eq!(preview.stats.scanned_files, 2);
    }
    #[cfg(windows)]
    #[test]
    fn junction_is_skipped_without_following_target() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        old_file(&target.path().join("outside.tmp"), 8);
        let link = tmp.path().join("junction");
        let status = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&link)
            .arg(target.path())
            .output()
            .unwrap();
        assert!(status.status.success(), "junction fixture creation failed");
        let (plan, preview) = scan(tmp.path());
        assert!(plan.candidates.is_empty());
        assert_eq!(preview.stats.linked_entries, 1);
        fs::remove_dir(link).unwrap();
    }
    #[test]
    fn candidate_limit_continues_full_count_and_saved_candidates_are_actionable() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..12 {
            old_file(&tmp.path().join(format!("{i}.tmp")), 8);
        }
        let (plan, preview) = scan_roots(
            &[("fixture".into(), tmp.path().into())],
            "limited",
            &AtomicBool::new(false),
            Limits {
                candidates: 3,
                per_group: 2,
                ..Limits::default()
            },
            |_, _| {},
        );
        let plan = plan.unwrap();
        assert_eq!(plan.candidates.len(), 2);
        assert!(plan.candidates.iter().all(valid_candidate));
        assert_eq!(preview.stats.scanned_files, 12);
        assert!(preview.stats.limited);
        assert!(!preview.plan_id.is_empty());
    }
    #[test]
    fn candidate_memory_and_depth_limits_are_transparent_without_stopping_counts() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..4 {
            old_file(&tmp.path().join(format!("{i}.tmp")), 8);
        }
        old_file(&tmp.path().join("nested/a.tmp"), 8);
        let (_, preview) = scan_roots(
            &[("fixture".into(), tmp.path().into())],
            "memory",
            &AtomicBool::new(false),
            Limits {
                memory: 1,
                ..Limits::default()
            },
            |_, _| {},
        );
        assert!(preview.stats.limited);
        assert_eq!(preview.stats.scanned_files, 5);
        assert!(preview.groups.iter().all(|g| g.entries.is_empty()));
        let (_, preview) = scan_roots(
            &[("fixture".into(), tmp.path().into())],
            "depth",
            &AtomicBool::new(false),
            Limits {
                depth: 1,
                ..Limits::default()
            },
            |_, _| {},
        );
        assert!(preview.stats.limited);
        assert_eq!(preview.stats.scanned_files, 4);
        assert_eq!(preview.stats.protected_directories, 1);
    }
    #[test]
    fn cancelled_scan_has_no_executable_capability() {
        let tmp = tempfile::tempdir().unwrap();
        old_file(&tmp.path().join("a.tmp"), 8);
        let (plan, preview) = scan_roots(
            &[("fixture".into(), tmp.path().into())],
            "cancel",
            &AtomicBool::new(true),
            Limits::default(),
            |_, _| {},
        );
        assert!(plan.is_none());
        assert!(preview.stats.cancelled);
        assert!(preview.plan_id.is_empty() && preview.groups.is_empty());
    }
    #[test]
    fn precancel_closes_startup_race_and_new_scan_invalidates_old_plan() {
        let state = DiskCleanerState::default();
        let tmp = tempfile::tempdir().unwrap();
        old_file(&tmp.path().join("a.tmp"), 8);
        let (plan, _) = scan(tmp.path());
        state.0.lock().unwrap().snapshot = Some(plan);
        request_cancel(&state, "before-start").unwrap();
        let flag = register_scan(&state, "before-start").unwrap();
        assert!(flag.load(Ordering::Acquire));
        assert!(state.0.lock().unwrap().snapshot.is_none());
        request_cancel(&state, "before-start").unwrap();
        for i in 0..50 {
            request_cancel(&state, &format!("future-{i}")).unwrap();
        }
        assert_eq!(state.0.lock().unwrap().precancelled.len(), 32);
    }
    #[test]
    fn cancellation_after_first_volume_discards_collected_candidates() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        old_file(&a.path().join("a.tmp"), 8);
        old_file(&b.path().join("b.tmp"), 8);
        let cancel = AtomicBool::new(false);
        let (plan, preview) = scan_roots(
            &[("A".into(), a.path().into()), ("B".into(), b.path().into())],
            "mid-cancel",
            &cancel,
            Limits::default(),
            |_, stats| {
                if stats.scanned_files >= 1 {
                    cancel.store(true, Ordering::Release);
                }
            },
        );
        assert!(plan.is_none());
        assert!(preview.stats.cancelled);
        assert_eq!(preview.stats.scanned_files, 1);
        assert!(preview.groups.is_empty());
    }
    #[test]
    fn zero_time_budget_returns_transparent_partial_plan() {
        let tmp = tempfile::tempdir().unwrap();
        old_file(&tmp.path().join("a.tmp"), 8);
        let (_, preview) = scan_roots(
            &[("fixture".into(), tmp.path().into())],
            "timeout",
            &AtomicBool::new(false),
            Limits {
                duration: Duration::ZERO,
                ..Limits::default()
            },
            |_, _| {},
        );
        assert!(preview.stats.limited);
        assert_eq!(preview.stats.scanned_files, 0);
        assert!(!preview.warnings.is_empty());
    }
    #[test]
    fn token_validation_rejects_corruption_expiry_duplicates_and_replay() {
        let tmp = tempfile::tempdir().unwrap();
        old_file(&tmp.path().join("a.tmp"), 8);
        let (mut plan, _) = scan(tmp.path());
        let id = plan.id.clone();
        let file = plan.candidates[0].id.clone();
        let state = DiskCleanerState::default();
        plan.created = SystemTime::now() - TTL - Duration::from_secs(1);
        state.0.lock().unwrap().snapshot = Some(plan);
        assert!(take_snapshot(&state, &id, &[file.clone()]).is_err());
        let (plan, _) = scan(tmp.path());
        let id = plan.id.clone();
        state.0.lock().unwrap().snapshot = Some(plan);
        assert!(take_snapshot(&state, "corrupt", &[file.clone()]).is_err());
        assert!(take_snapshot(&state, &id, &["other-file".into()]).is_err());
        assert!(take_snapshot(&state, &id, &[file.clone(), file.clone()]).is_err());
        assert!(take_snapshot(&state, &id, &[file.clone()]).is_ok());
        assert!(take_snapshot(&state, &id, &[file]).is_err());
    }
    #[test]
    fn changed_file_and_replaced_parent_are_not_actionable() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("folder");
        old_file(&nested.join("a.tmp"), 8);
        let (plan, _) = scan(tmp.path());
        fs::write(nested.join("a.tmp"), b"changed").unwrap();
        assert!(!valid_candidate(&plan.candidates[0]));
        let (plan, _) = scan(tmp.path());
        // Fresh write is no longer eligible. Restore old metadata and scan again.
        assert!(plan.candidates.is_empty());
        old_file(&nested.join("a.tmp"), 8);
        let (plan, _) = scan(tmp.path());
        fs::rename(&nested, tmp.path().join("old-folder")).unwrap();
        old_file(&nested.join("a.tmp"), 8);
        assert!(!valid_candidate(&plan.candidates[0]));
    }
    #[test]
    fn durable_pending_precedes_action_and_mutation_after_pending_is_rechecked() {
        let tmp = tempfile::tempdir().unwrap();
        let logs = tempfile::tempdir().unwrap();
        old_file(&tmp.path().join("a.tmp"), 8);
        let (plan, _) = scan(tmp.path());
        let calls = Cell::new(0);
        let outcome = execute_files_with_logger(
            &plan,
            &selected(&plan),
            logs.path(),
            |_| {
                calls.set(calls.get() + 1);
                Ok(())
            },
            |file, id, path, status| {
                log_line(file, id, path, status)?;
                if status == "pending" {
                    fs::remove_file(path).unwrap();
                    fs::write(path, b"changed during pending").unwrap();
                }
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(calls.get(), 0);
        assert_eq!(outcome.skipped_files, 1);
        assert!(fs::read_to_string(&outcome.log_path)
            .unwrap()
            .contains("pending"));
    }
    #[test]
    fn failed_pending_log_never_calls_recycle() {
        let tmp = tempfile::tempdir().unwrap();
        let logs = tempfile::tempdir().unwrap();
        old_file(&tmp.path().join("a.tmp"), 8);
        let (plan, _) = scan(tmp.path());
        let outcome = execute_files_with_logger(
            &plan,
            &selected(&plan),
            logs.path(),
            |_| panic!("must not recycle"),
            |_, _, _, _| Err("injected".into()),
        )
        .unwrap();
        assert_eq!(outcome.recycled_files, 0);
        assert_eq!(outcome.skipped_files, 1);
    }
    #[test]
    fn history_begin_failure_prevents_actions() {
        let tmp = tempfile::tempdir().unwrap();
        let logs = tempfile::tempdir().unwrap();
        old_file(&tmp.path().join("a.tmp"), 8);
        fs::write(logs.path().join("cleanup-history-v1"), b"blocked").unwrap();
        let (plan, _) = scan(tmp.path());
        let ids: Vec<_> = plan.candidates.iter().map(|c| c.id.clone()).collect();
        assert!(execute_recorded(plan, &ids, logs.path(), |_| panic!("must not recycle")).is_err());
    }
    #[test]
    fn history_finish_failure_preserves_real_action_statistics() {
        let tmp = tempfile::tempdir().unwrap();
        let logs = tempfile::tempdir().unwrap();
        old_file(&tmp.path().join("a.tmp"), 8);
        let (plan, _) = scan(tmp.path());
        let ids: Vec<_> = plan.candidates.iter().map(|c| c.id.clone()).collect();
        let outcome = execute_recorded(plan, &ids, logs.path(), |path| {
            let record = fs::read_dir(logs.path().join("cleanup-history-v1"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            fs::write(record.join("finish.json"), b"injected conflict").unwrap();
            fs::remove_file(path).unwrap();
            Ok(())
        })
        .unwrap();
        assert_eq!(outcome.recycled_files, 1);
        assert_eq!(outcome.recycled_bytes, 12);
        assert!(outcome.history_warning.is_some());
    }
    #[test]
    fn recycle_failure_is_per_file_and_does_not_fake_success() {
        let tmp = tempfile::tempdir().unwrap();
        let logs = tempfile::tempdir().unwrap();
        old_file(&tmp.path().join("a.tmp"), 8);
        old_file(&tmp.path().join("b.tmp"), 8);
        let (plan, _) = scan(tmp.path());
        let mut count = 0;
        let outcome = execute_files(&plan, &selected(&plan), logs.path(), |path| {
            count += 1;
            if count == 1 {
                Err("in use".into())
            } else {
                fs::remove_file(path).unwrap();
                Ok(())
            }
        })
        .unwrap();
        assert_eq!(outcome.failed_files, 1);
        assert_eq!(outcome.recycled_files, 1);
    }
    #[test]
    fn recycler_ok_without_source_disappearance_is_uncertain_not_success() {
        let tmp = tempfile::tempdir().unwrap();
        let logs = tempfile::tempdir().unwrap();
        old_file(&tmp.path().join("a.tmp"), 8);
        let (plan, _) = scan(tmp.path());
        let outcome = execute_files(&plan, &selected(&plan), logs.path(), |_| Ok(())).unwrap();
        assert_eq!(outcome.recycled_files, 0);
        assert_eq!(outcome.failed_files, 1);
        assert!(fs::read_to_string(outcome.log_path)
            .unwrap()
            .contains("uncertain"));
    }
    #[cfg(windows)]
    #[test]
    fn real_windows_recycle_only_fixture_smoke() {
        let tmp = tempfile::tempdir().unwrap();
        let logs = tempfile::tempdir().unwrap();
        let canonical = fs::canonicalize(tmp.path()).unwrap();
        let root = PathBuf::from(canonical.to_str().unwrap().trim_start_matches(r"\\?\"));
        let path = root.join("Pinkbin 测试 🌸 owned smoke.tmp");
        old_file(&path, 8);
        let (plan, _) = scan(&root);
        let ids: Vec<_> = plan.candidates.iter().map(|c| c.id.clone()).collect();
        let errors = RefCell::new(Vec::new());
        let outcome = execute_recorded(plan, &ids, logs.path(), |p| {
            crate::recycle_only::recycle(p).map_err(|error| {
                errors.borrow_mut().push(error.clone());
                error
            })
        })
        .unwrap();
        let own_recycled_count = trash::os_limited::list()
            .unwrap()
            .into_iter()
            .filter(|item| item.original_path() == path)
            .count();
        assert_eq!(
            outcome.recycled_files,
            1,
            "messages={:?}, errors={:?}, failed={}, skipped={}, own-bin={}",
            outcome.messages,
            errors.borrow(),
            outcome.failed_files,
            outcome.skipped_files,
            own_recycled_count
        );
        assert_eq!(outcome.failed_files + outcome.skipped_files, 0);
        assert!(!path.exists());
        let actions = fs::read_to_string(outcome.log_path).unwrap();
        assert!(actions.contains("pending") && actions.contains("success"));
        let record = fs::read_dir(logs.path().join("cleanup-history-v1"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let finish: serde_json::Value =
            serde_json::from_slice(&fs::read(record.join("finish.json")).unwrap()).unwrap();
        assert_eq!(finish["summary"]["recycled_files"], 1);
        assert!(outcome.history_warning.is_none());
        let recycled: Vec<_> = trash::os_limited::list()
            .unwrap()
            .into_iter()
            .filter(|item| item.original_path() == path)
            .collect();
        assert_eq!(recycled.len(), 1);
        trash::os_limited::restore_all(recycled).unwrap();
        assert!(path.is_file());
    }
    #[cfg(windows)]
    #[test]
    fn real_long_path_fixture_is_confirmed_or_reports_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let logs = tempfile::tempdir().unwrap();
        let canonical = fs::canonicalize(tmp.path()).unwrap();
        let root = PathBuf::from(canonical.to_str().unwrap().trim_start_matches(r"\\?\"));
        let mut parent = root.clone();
        for i in 0..5 {
            parent.push(format!("long-path-owned-fixture-{i}-{}", "x".repeat(40)));
        }
        let path = parent.join("Pinkbin 长路径 🌸.tmp");
        old_file(&path, 8);
        assert!(path.as_os_str().len() > 260);
        let (plan, _) = scan(&root);
        assert_eq!(plan.candidates.len(), 1);
        let ids: Vec<_> = plan.candidates.iter().map(|c| c.id.clone()).collect();
        let outcome =
            execute_recorded(plan, &ids, logs.path(), crate::recycle_only::recycle).unwrap();
        assert_eq!(
            outcome.recycled_files + outcome.failed_files + outcome.skipped_files,
            1
        );
        if outcome.recycled_files == 1 {
            assert!(!path.exists());
        } else {
            assert_eq!(outcome.recycled_files, 0);
        }
        // Restore only the exact owned fixture if Shell recycled it, even if a
        // confirmation could not be obtained. Never act on unrelated bin items.
        let recycled: Vec<_> = trash::os_limited::list()
            .unwrap()
            .into_iter()
            .filter(|item| item.original_path() == path)
            .collect();
        eprintln!(
            "owned long-path fixture: recycled={}, failed={}, skipped={}, exact restore matches={}",
            outcome.recycled_files,
            outcome.failed_files,
            outcome.skipped_files,
            recycled.len()
        );
        if !recycled.is_empty() {
            trash::os_limited::restore_all(recycled).unwrap();
            assert!(path.is_file());
        }
    }
    #[cfg(windows)]
    #[test]
    fn held_parent_handles_block_directory_replacement() {
        let tmp = tempfile::tempdir().unwrap();
        old_file(&tmp.path().join("nested/a.tmp"), 8);
        let (plan, _) = scan(tmp.path());
        let held = hold_parents(&plan.candidates[0]).unwrap();
        assert!(fs::rename(tmp.path().join("nested"), tmp.path().join("moved")).is_err());
        drop(held);
        assert!(fs::rename(tmp.path().join("nested"), tmp.path().join("moved")).is_ok());
    }
    #[cfg(windows)]
    #[test]
    fn held_target_denies_writers_and_offline_attributes_skip_scan() {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::SetFileAttributesW;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("offline.tmp");
        old_file(&path, 8);
        let held = hold_file(&path).unwrap();
        assert!(File::options().write(true).open(&path).is_err());
        drop(held);
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        assert_ne!(unsafe { SetFileAttributesW(wide.as_ptr(), 0x1000) }, 0);
        let (plan, preview) = scan(tmp.path());
        assert!(plan.candidates.is_empty());
        assert_eq!(preview.stats.linked_entries, 1);
        assert_ne!(unsafe { SetFileAttributesW(wide.as_ptr(), 0x80) }, 0);
    }
    #[test]
    fn fixed_drive_inventory_is_readonly_smoke() {
        let drives = fixed_drives().unwrap();
        #[cfg(windows)]
        assert!(!drives.is_empty());
        for drive in drives {
            assert!(drive.id.ends_with(":\\"));
            assert!(drive.available_bytes <= drive.total_bytes);
        }
    }
}
