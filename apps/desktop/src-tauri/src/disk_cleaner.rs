//! Metadata-only, bounded candidate collection from local fixed-volume roots.
//! A plan is an in-memory, expiring, single-use capability; paths are never accepted over IPC.
use serde::Serialize;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File, OpenOptions, ReadDir},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime},
};
use tauri::{Emitter, Manager, State};

mod quick;
mod quick_locations;
mod shared;
use shared::SharedInventory;
pub(crate) use shared::{
    epoch as shared_epoch, home_scan as shared_home_scan, invalidate as invalidate_shared_scan,
};

const TTL: Duration = Duration::from_secs(15 * 60);
const CANDIDATE_LIMIT: usize = 20_000;
const PER_GROUP_LIMIT: usize = 5_000;
const MAX_ACTIONS: usize = 10_000;
const RECOMMENDED_LIMIT: usize = 200_000;
const RECOMMENDED_MEMORY_LIMIT: usize = 256 * 1024 * 1024;
const RECOMMENDED_EXECUTION_TIME: Duration = Duration::from_secs(15 * 60);
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
pub struct DiskCleanerState(
    Arc<Mutex<Inner>>,
    Arc<crate::shared_scan::Coordinator<SharedInventory>>,
);
#[derive(Default)]
struct Inner {
    session: Option<DiskScanSession>,
    snapshot: Option<Snapshot>,
    snapshot_epoch: Option<u64>,
    active: Option<(String, Arc<AtomicBool>)>,
    precancelled: Vec<(String, Instant)>,
    execution: Option<CleanupExecution>,
}
#[derive(Clone)]
struct Snapshot {
    id: String,
    created: SystemTime,
    candidates: Vec<Candidate>,
    recommended: Option<Vec<Candidate>>,
}
#[derive(Clone, Serialize)]
pub struct DiskScanSession {
    scan_mode: &'static str,
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
        .snapshot_epoch
        .is_some_and(|epoch| epoch != state.1.epoch())
    {
        inner.snapshot = None;
        inner.snapshot_epoch = None;
        invalidate_session(&mut inner, "文件列表可能已变化，请重新检查");
    }
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
    force: Option<bool>,
    mode: Option<String>,
) -> Result<DiskScanSession, String> {
    crate::rule_catalog::ensure_ready()?;
    let scan_mode = match mode.as_deref() {
        None | Some("full") => "full",
        Some("quick") => "quick",
        _ => return Err("检查方式无效".into()),
    };
    if let Some(s) = session_snapshot(state.inner())?.filter(|s| s.status == "running") {
        return Ok(s);
    }
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
    let mut roots = Vec::new();
    for id in &drive_ids {
        if !available.iter().any(|d| &d.id == id) {
            return Err("所选磁盘不可用，请刷新磁盘列表".into());
        }
        if !roots.contains(&PathBuf::from(id)) {
            roots.push(PathBuf::from(id));
        }
    }
    let shared = state.inner().clone();
    let cancel = register_scan(&shared, &scan_id)?;
    let session = DiskScanSession {
        scan_mode,
        scan_id: scan_id.clone(),
        drive_ids,
        status: "running",
        progress: None,
        preview: None,
        error: None,
    };
    install_scan_session(&shared, session.clone())?;
    tauri::async_runtime::spawn(async move {
        let worker = shared.clone();
        let worker_id = scan_id.clone();
        let actor_cancel = cancel.clone();
        let result = tokio::task::spawn_blocking(move || {
            if scan_mode == "quick" {
                return quick::run(&app, &worker, &roots, &worker_id, &actor_cancel);
            }
            shared::run_disk(
                &app,
                &worker,
                &roots,
                &worker_id,
                actor_cancel,
                force.unwrap_or(false),
            )
        })
        .await
        .map_err(text_error)
        .and_then(|r| r);
        finish_scan(&shared, &scan_id, cancel.load(Ordering::Acquire), result);
    });
    Ok(session)
}

fn finish_scan(
    shared: &DiskCleanerState,
    scan_id: &str,
    cancelled: bool,
    result: Result<(Snapshot, Preview, u64), String>,
) {
    if let Ok(mut inner) = shared.0.lock() {
        if !inner.active.as_ref().is_some_and(|(id, _)| id == scan_id) {
            return;
        }
        inner.active = None;
        if cancelled {
            if let Some(s) = inner.session.as_mut().filter(|s| s.scan_id == scan_id) {
                s.status = "cancelled";
                s.preview = None;
                s.error = None;
            }
        } else {
            match result {
                Ok((snapshot, preview, epoch)) if shared.1.epoch() == epoch => {
                    inner.snapshot = Some(snapshot);
                    inner.snapshot_epoch = Some(epoch);
                    if let Some(s) = inner.session.as_mut().filter(|s| s.scan_id == scan_id) {
                        s.status = "completed";
                        s.preview = Some(Arc::new(preview));
                        s.error = None;
                    }
                }
                Ok(_) => invalidate_session(&mut inner, "文件列表可能已变化，请重新检查"),
                Err(e) => invalidate_session(&mut inner, &e),
            }
        }
    }
}

#[derive(Clone)]
struct Candidate {
    id: String,
    category: &'static str,
    path: PathBuf,
    stamp: Stamp,
    // Includes volume root and every parent; identity/creation only for directories.
    parent: Arc<Directory>,
    recommendation: Option<Recommendation>,
}
#[derive(Clone)]
enum Recommendation {
    Full {
        rule: crate::full_cleanup_rules::Match,
        context: Arc<crate::full_cleanup_rules::Context>,
    },
    Cache(Box<crate::cache_cleaner::RecommendedTarget>),
}
impl Recommendation {
    fn selection_id(&self) -> String {
        let id = match self {
            Self::Cache(target) => target.id(),
            Self::Full { rule, context } => {
                let catalog = &context.catalog;
                let profile = catalog
                    .profiles
                    .iter()
                    .find(|p| p.id == rule.rule_id.as_ref());
                catalog
                    .legacy
                    .iter()
                    .find(|legacy| {
                        legacy.id == rule.rule_id.as_ref()
                            || profile.is_some_and(|profile| {
                                profile.family == legacy.family
                                    && profile.locations.iter().any(|location| {
                                        location.root
                                            == if legacy.family == "electron" {
                                                "roaming"
                                            } else {
                                                "local"
                                            }
                                            && location
                                                .path
                                                .replace('\\', "/")
                                                .eq_ignore_ascii_case(
                                                    &legacy.prefix.replace('\\', "/"),
                                                )
                                    })
                            })
                    })
                    .map_or(rule.rule_id.as_ref(), |legacy| legacy.id.as_str())
            }
        };
        format!("software:{id}")
    }
    fn id(&self) -> &'static str {
        match self {
            Self::Full { rule, .. } => rule.kind.id(),
            Self::Cache(target) => target.id(),
        }
    }
    fn category(&self) -> crate::cache_cleaner::RecommendationCategory {
        match self {
            Self::Full { rule, .. } => rule.kind.category(),
            Self::Cache(_) => crate::cache_cleaner::RecommendationCategory::ApplicationCache,
        }
    }
    fn title(&self) -> &str {
        match self {
            Self::Full { rule, .. } => &rule.title,
            Self::Cache(target) => target.title(),
        }
    }
    fn valid(
        &self,
        path: &Path,
        modified: SystemTime,
        running: &impl Fn(&str) -> Result<bool, String>,
        held: Option<&File>,
    ) -> bool {
        match self {
            Self::Full { rule, context } => {
                context.identify(path, modified, SystemTime::now()).as_ref() == Some(rule)
                    && rule.processes.iter().all(|name| running(name) == Ok(false))
            }
            Self::Cache(target) => target.valid_with_running(running, held),
        }
    }
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
    fn scan_chain_intact(&self) -> bool {
        // Every saved ancestor is checked once. The target's stamp() also checks
        // ancestors above the scan root; execution still pins the complete chain.
        self.chain()
            .all(|d| directory_handle_stamp(&d.path).ok().as_ref() == Some(&d.stamp))
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
    rule_reuse: crate::rule_probe::ReuseStats,
}
#[derive(Serialize, Clone)]
pub struct Preview {
    scan_mode: &'static str,
    #[serde(skip)]
    learned_locations: Vec<PathBuf>,
    plan_id: String,
    scan_id: String,
    created_at: String,
    groups: Vec<Group>,
    stats: Stats,
    warnings: Vec<String>,
    recommended: Option<crate::cache_cleaner::RecommendedPreview>,
    overview: Vec<SpaceUsage>,
    unclassified_caches: Vec<SpaceUsage>,
    overview_limited: bool,
    shared_reused: bool,
}
#[derive(Serialize, Clone)]
struct Group {
    id: &'static str,
    title: &'static str,
    description: &'static str,
    risk: &'static str,
    file_count: u64,
    bytes: u64,
    entries: Vec<Entry>,
}
#[derive(Serialize, Clone)]
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
#[derive(Clone, Serialize, Default)]
pub struct CacheResult {
    deleted_files: u64,
    deleted_bytes: u64,
    skipped_files: u64,
    failed_files: u64,
    messages: Vec<String>,
    log_path: String,
    history_warning: Option<String>,
}
#[derive(Clone, Serialize)]
pub struct CleanupExecution {
    drive_ids: Vec<String>,
    plan_id: String,
    status: &'static str,
    total_files: u64,
    processed_files: u64,
    batch: u64,
    deleted_files: u64,
    deleted_bytes: u64,
    result: Option<CacheResult>,
    error: Option<String>,
}
#[tauri::command]
pub fn get_disk_cleanup_execution(
    state: State<'_, DiskCleanerState>,
) -> Result<Option<CleanupExecution>, String> {
    state
        .0
        .lock()
        .map(|inner| inner.execution.clone())
        .map_err(text_error)
}
type ExecutionProgress<'a> = &'a mut dyn FnMut(usize, usize, usize, &CacheResult);
struct ExecutionPolicy<'a> {
    batched: bool,
    notify: Option<ExecutionProgress<'a>>,
    batch_items: usize,
}
impl ExecutionPolicy<'_> {
    fn manual() -> Self {
        Self {
            batched: false,
            notify: None,
            batch_items: MAX_ACTIONS,
        }
    }
}
#[derive(Clone, Serialize)]
struct Progress {
    phase: &'static str,
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
        phase: "files",
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

fn attach_recommended_inner(
    app: &tauri::AppHandle,
    snapshot: &mut Option<Snapshot>,
    preview: &mut Preview,
    roots: &[(String, PathBuf)],
    cancel: &AtomicBool,
    publish_progress: bool,
) {
    let Some(saved) = snapshot.as_mut() else {
        return;
    };
    if cancel.load(Ordering::Acquire) {
        return;
    }
    let mut event = progress(
        &preview.scan_id,
        roots.last().map_or("", |(id, _)| id),
        &preview.stats,
    );
    event.phase = "recommendations";
    if publish_progress {
        if let Ok(mut inner) = app.state::<DiskCleanerState>().0.lock() {
            if let Some(session) = inner.session.as_mut() {
                session.progress = Some(event.clone());
            }
        }
        let _ = app.emit("disk-cleanup-progress", event);
    }
    let selected: Vec<_> = roots.iter().map(|(_, root)| root.clone()).collect();
    let (cache, mut info) = match app.path().local_data_dir() {
        Ok(base) => crate::cache_cleaner::prepare_recommended(
            &base,
            app.path().data_dir().ok().as_deref(),
            &selected,
            cancel,
        ),
        Err(_) => (
            None,
            crate::cache_cleaner::RecommendedPreview::unavailable(),
        ),
    };
    if let Some(previous) = &preview.recommended {
        info.blocked_apps.extend(previous.blocked_apps.clone());
        info.limited |= previous.limited;
    }
    let mut candidates = RecommendedCollector::new(RECOMMENDED_MEMORY_LIMIT, RECOMMENDED_LIMIT);
    for file in saved.recommended.take().unwrap_or_default() {
        candidates.push(file);
    }
    let mut directories = HashMap::new();
    for target in cache.into_iter().flat_map(|plan| plan.into_targets()) {
        if cancel.load(Ordering::Acquire) {
            return;
        }
        let cost = target
            .path
            .as_os_str()
            .len()
            .saturating_mul(8)
            .saturating_add(2048)
            .saturating_add(
                target
                    .path
                    .parent()
                    .into_iter()
                    .flat_map(Path::ancestors)
                    .map(|path| path.as_os_str().len().saturating_mul(4).saturating_add(256))
                    .sum::<usize>(),
            );
        if candidates.paths.contains(&target.path) {
            continue;
        }
        if !candidates.accepts(target.size(), cost) {
            info.limited = true;
            continue;
        }
        if let Some(candidate) = cache_candidate(target, roots, &mut directories) {
            candidates.push(candidate);
            // Candidate chains own their directories; cap auxiliary strong references.
            if directories.len() > 512 {
                directories.clear();
            }
        } else {
            info.limited = true;
        }
    }
    info.limited |= candidates.limited;
    finish_recommendations(saved, &mut info, candidates.into_files());
    saved.created = SystemTime::now();
    preview.created_at = chrono::Utc::now().to_rfc3339();
    preview.recommended = Some(info);
}

#[cfg(test)]
fn take_recommended(state: &DiskCleanerState, plan_id: &str) -> Result<Snapshot, String> {
    take_recommended_selected(state, plan_id, None)
}

fn take_recommended_selected(
    state: &DiskCleanerState,
    plan_id: &str,
    selected_groups: Option<&[String]>,
) -> Result<Snapshot, String> {
    let mut inner = state.0.lock().map_err(text_error)?;
    check_snapshot_epoch(state, &mut inner)?;
    let plan = inner.snapshot.as_ref().ok_or("预览已失效，请重新检查")?;
    if plan.id != plan_id {
        return Err("预览编号无效，请重新检查".into());
    }
    if plan.created.elapsed().map_or(true, |age| age > TTL) {
        inner.snapshot = None;
        invalidate_session(&mut inner, "预览已过期，请重新检查");
        return Err("预览已过期，请重新检查".into());
    }
    if plan.recommended.is_none() {
        return Err("没有可便捷清理的项目，请重新检查".into());
    }
    let selected = if let Some(ids) = selected_groups {
        let available: HashSet<_> = plan
            .recommended
            .as_ref()
            .unwrap()
            .iter()
            .filter_map(|file| file.recommendation.as_ref())
            .map(Recommendation::selection_id)
            .collect();
        let unique: HashSet<_> = ids.iter().cloned().collect();
        if ids.is_empty()
            || ids.len() > 512
            || unique.len() != ids.len()
            || !unique.is_subset(&available)
        {
            return Err("请选择有效的便捷清理项目".into());
        }
        Some(unique)
    } else {
        None
    };
    let mut plan = inner.snapshot.take().unwrap();
    invalidate_session(&mut inner, "预览已使用，请重新检查");
    plan.candidates = plan.recommended.take().unwrap();
    if let Some(selected) = selected {
        plan.candidates.retain(|file| {
            file.recommendation
                .as_ref()
                .is_some_and(|rule| selected.contains(&rule.selection_id()))
        });
    }
    Ok(plan)
}

#[tauri::command]
pub fn execute_recommended_cleanup(
    app: tauri::AppHandle,
    state: State<'_, DiskCleanerState>,
    plan_id: String,
    selected_group_ids: Vec<String>,
) -> Result<CleanupExecution, String> {
    let lease = reserve_operation()?;
    let drive_ids = state
        .0
        .lock()
        .map_err(text_error)?
        .session
        .as_ref()
        .map_or(vec![], |session| session.drive_ids.clone());
    let snapshot = take_recommended_selected(state.inner(), &plan_id, Some(&selected_group_ids))?;
    invalidate_shared_scan(&app);
    validate_drives(&snapshot)?;
    if snapshot.candidates.len() > RECOMMENDED_LIMIT
        || snapshot
            .candidates
            .iter()
            .any(|file| file.recommendation.is_none())
    {
        return Err("便捷清理计划无效，请重新扫描".into());
    }
    let app_data = crate::app_data_dir(&app);
    let shared = state.inner().clone();
    let session = CleanupExecution {
        drive_ids,
        plan_id: plan_id.clone(),
        status: "running",
        total_files: snapshot.candidates.len() as u64,
        processed_files: 0,
        batch: 1,
        deleted_files: 0,
        deleted_bytes: 0,
        result: None,
        error: None,
    };
    shared.0.lock().map_err(text_error)?.execution = Some(session.clone());
    tauri::async_runtime::spawn(async move {
        let _lease = lease;
        let worker = shared.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            let ids: Vec<_> = snapshot
                .candidates
                .iter()
                .map(|file| file.id.clone())
                .collect();
            let mut notify =
                |processed: usize, _total: usize, batch: usize, result: &CacheResult| {
                    if let Ok(mut inner) = worker.0.lock() {
                        if let Some(job) = inner.execution.as_mut() {
                            job.processed_files = processed as u64;
                            job.batch = batch as u64;
                            job.deleted_files = result.deleted_files;
                            job.deleted_bytes = result.deleted_bytes;
                        }
                    }
                };
            execute_recorded_policy(
                snapshot,
                &ids,
                &app_data,
                crate::direct_delete::delete,
                crate::cache_cleaner::browser_running,
                ExecutionPolicy {
                    batched: true,
                    notify: Some(&mut notify),
                    batch_items: MAX_ACTIONS,
                },
            )
        })
        .await
        .map_err(text_error)
        .and_then(|result| result);
        if let Ok(mut inner) = shared.0.lock() {
            if let Some(job) = inner
                .execution
                .as_mut()
                .filter(|job| job.plan_id == plan_id)
            {
                match outcome {
                    Ok(result) => {
                        job.processed_files =
                            result.deleted_files + result.skipped_files + result.failed_files;
                        job.deleted_files = result.deleted_files;
                        job.deleted_bytes = result.deleted_bytes;
                        job.status = "completed";
                        job.result = Some(result);
                    }
                    Err(error) => {
                        job.status = "failed";
                        job.error = Some(error);
                    }
                }
            }
        }
    });
    Ok(session)
}

fn validate_drives(snapshot: &Snapshot) -> Result<(), String> {
    let drives = fixed_drives()?;
    if snapshot.candidates.iter().any(|c| {
        !drives.iter().any(|d| {
            c.parent
                .chain()
                .last()
                .is_some_and(|root| root.path == Path::new(&d.id))
        })
    }) {
        return Err("磁盘列表已改变，请重新扫描".into());
    }
    Ok(())
}

fn cache_candidate(
    target: crate::cache_cleaner::RecommendedTarget,
    roots: &[(String, PathBuf)],
    directories: &mut HashMap<PathBuf, Arc<Directory>>,
) -> Option<Candidate> {
    let root = roots
        .iter()
        .find(|(_, root)| target.path.starts_with(root))?
        .1
        .as_path();
    let parent_path = target.path.parent()?;
    let mut chain = parent_path
        .ancestors()
        .take_while(|path| path.starts_with(root))
        .collect::<Vec<_>>();
    if chain.len() > MAX_DEPTH {
        return None;
    }
    chain.reverse();
    let mut parent: Option<Arc<Directory>> = None;
    for path in chain {
        let directory = if let Some(saved) = directories.get(path) {
            saved.clone()
        } else {
            let stamp = stamp(path, true).ok()?;
            let volume = parent.as_ref().map_or(stamp.identity.0, |p| p.volume);
            if stamp.identity.0 != volume {
                return None;
            }
            let directory = Arc::new(Directory {
                path: path.into(),
                stamp,
                depth: parent.as_ref().map_or(1, |p| p.depth + 1),
                parent: parent.clone(),
                volume,
            });
            directories.insert(path.into(), directory.clone());
            directory
        };
        parent = Some(directory);
    }
    let parent = parent?;
    let stamp = stamp(&target.path, false).ok()?;
    if stamp.identity.0 != parent.volume {
        return None;
    }
    Some(Candidate {
        id: String::new(),
        category: target.id(),
        path: target.path.clone(),
        stamp,
        parent,
        recommendation: Some(Recommendation::Cache(Box::new(target))),
    })
}

fn finish_recommendations(
    snapshot: &mut Snapshot,
    info: &mut crate::cache_cleaner::RecommendedPreview,
    mut candidates: Vec<Candidate>,
) {
    candidates.sort_by_key(|file| std::cmp::Reverse(file.stamp.size));
    let mut seen = HashSet::new();
    let mut identities = HashSet::new();
    candidates
        .retain(|file| seen.insert(file.path.clone()) && identities.insert(file.stamp.identity));
    info.limited |= candidates.len() > RECOMMENDED_LIMIT;
    candidates.truncate(RECOMMENDED_LIMIT);
    info.groups.clear();
    info.file_count = 0;
    info.bytes = 0;
    let mut memory = 0usize;
    candidates.retain(|file| {
        memory = memory.saturating_add(candidate_cost(&file.path, &file.parent));
        if memory > RECOMMENDED_MEMORY_LIMIT {
            info.limited = true;
            return false;
        }
        true
    });
    for (index, file) in candidates.iter_mut().enumerate() {
        file.id = format!("quick-{index}");
        let rule = file.recommendation.as_ref().unwrap();
        let selection_id = rule.selection_id();
        file.category = rule.id();
        if let Some(group) = info.groups.iter_mut().find(|group| {
            group.id == selection_id
                && group.title == rule.title()
                && group.category == rule.category()
        }) {
            group.file_count += 1;
            group.bytes = group.bytes.saturating_add(file.stamp.size);
        } else {
            info.groups.push(crate::cache_cleaner::RecommendedGroup {
                id: selection_id,
                category: rule.category(),
                title: rule.title().into(),
                file_count: 1,
                bytes: file.stamp.size,
            });
        }
        info.file_count += 1;
        info.bytes = info.bytes.saturating_add(file.stamp.size);
    }
    info.groups
        .sort_by_key(|group| std::cmp::Reverse(group.bytes));
    info.blocked_apps.sort();
    info.blocked_apps.dedup();
    snapshot.recommended = (!candidates.is_empty()).then_some(candidates);
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
    let shared = state.inner().clone();
    start_disk_cleanup_scan(app, state, drive_ids, scan_id, None, None)?;
    loop {
        let session = session_snapshot(&shared)?.ok_or("检查任务不存在")?;
        match session.status {
            "completed" => {
                return session
                    .preview
                    .as_deref()
                    .cloned()
                    .ok_or("检查结果不可用".into())
            }
            "running" => tokio::time::sleep(Duration::from_millis(200)).await,
            _ => return Err(session.error.unwrap_or_else(|| "检查已取消".into())),
        }
    }
}
#[tauri::command]
pub fn cancel_disk_cleanup_scan(
    state: State<'_, DiskCleanerState>,
    scan_id: String,
) -> Result<bool, String> {
    let result = request_cancel(state.inner(), &scan_id);
    state.1.cancel_changed();
    result
}
fn install_scan_session(state: &DiskCleanerState, session: DiskScanSession) -> Result<(), String> {
    state.0.lock().map_err(text_error)?.session = Some(session);
    Ok(())
}
fn register_scan(state: &DiskCleanerState, scan_id: &str) -> Result<Arc<AtomicBool>, String> {
    let cancel = Arc::new(AtomicBool::new(false));
    let mut inner = state.0.lock().map_err(text_error)?;
    if inner
        .execution
        .as_ref()
        .is_some_and(|job| job.status == "running")
    {
        return Err("清理正在进行，请等待完成后再检查".into());
    }
    if inner
        .active
        .as_ref()
        .is_some_and(|(_, cancelled)| !cancelled.load(Ordering::Acquire))
    {
        return Err("已有检查正在进行，请等待完成或取消检查".into());
    }
    inner
        .precancelled
        .retain(|(_, created)| created.elapsed() < TTL);
    if let Some(index) = inner.precancelled.iter().position(|(id, _)| id == scan_id) {
        inner.precancelled.remove(index);
        cancel.store(true, Ordering::Release);
    }
    // Clear only a terminal execution while holding the same lock as the running guard.
    // A cleanup may publish its running job between registration and session installation.
    inner.execution = None;
    inner.snapshot = None;
    inner.snapshot_epoch = None;
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
            if let Some(session) = inner.session.as_mut().filter(|s| s.scan_id == scan_id) {
                session.status = "cancelled";
                session.preview = None;
                session.error = None;
            }
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
// Keep the most useful files across the whole walk, rather than allowing early
// small files to starve larger caches found on later volumes. Memory stays bounded.
struct RecommendedCollector {
    files: BTreeMap<(u64, u64), Candidate>,
    paths: HashSet<PathBuf>,
    identities: HashSet<(u64, u128)>,
    memory: usize,
    budget: usize,
    count: usize,
    sequence: u64,
    limited: bool,
}
impl RecommendedCollector {
    fn new(budget: usize, count: usize) -> Self {
        Self {
            files: BTreeMap::new(),
            paths: HashSet::new(),
            identities: HashSet::new(),
            memory: 0,
            budget,
            count,
            sequence: 0,
            limited: false,
        }
    }
    fn accepts(&self, size: u64, cost: usize) -> bool {
        cost <= self.budget
            && ((self.files.len() < self.count && self.memory.saturating_add(cost) <= self.budget)
                || self
                    .files
                    .first_key_value()
                    .is_some_and(|((small, _), _)| *small < size))
    }
    fn push(&mut self, file: Candidate) {
        if self.paths.contains(&file.path) || self.identities.contains(&file.stamp.identity) {
            return;
        }
        let cost = candidate_cost(&file.path, &file.parent);
        let mut memory = self.memory;
        let mut count = self.files.len();
        let mut remove = Vec::new();
        if cost > self.budget || self.count == 0 {
            self.limited = true;
            return;
        }
        for (key, small) in &self.files {
            if count < self.count && memory.saturating_add(cost) <= self.budget {
                break;
            }
            if key.0 >= file.stamp.size {
                self.limited = true;
                return;
            }
            remove.push(*key);
            memory = memory.saturating_sub(candidate_cost(&small.path, &small.parent));
            count -= 1;
        }
        if count >= self.count || memory.saturating_add(cost) > self.budget {
            self.limited = true;
            return;
        }
        self.limited |= !remove.is_empty();
        for key in remove {
            let small = self.files.remove(&key).unwrap();
            self.paths.remove(&small.path);
            self.identities.remove(&small.stamp.identity);
        }
        self.memory = memory.saturating_add(cost);
        self.paths.insert(file.path.clone());
        self.identities.insert(file.stamp.identity);
        self.files.insert((file.stamp.size, self.sequence), file);
        self.sequence += 1;
    }
    fn into_files(self) -> Vec<Candidate> {
        self.files.into_values().collect()
    }
}
#[derive(Serialize, Default, Clone)]
struct SpaceUsage {
    path: String,
    title: String,
    file_count: u64,
    bytes: u64,
}
struct SpaceCollector {
    entries: HashMap<PathBuf, (u64, u64)>,
    limit: usize,
    limited: bool,
}
impl SpaceCollector {
    fn new(limit: usize) -> Self {
        Self {
            entries: HashMap::new(),
            limit,
            limited: false,
        }
    }
    fn add(&mut self, root: &Path, bytes: u64) {
        if !self.entries.contains_key(root)
            && (self.entries.len() >= self.limit || root.as_os_str().len() > 4096)
        {
            self.limited = true;
            return;
        }
        let item = self.entries.entry(root.to_owned()).or_default();
        item.0 += 1;
        item.1 = item.1.saturating_add(bytes);
    }
    fn finish(self, count: usize) -> Vec<SpaceUsage> {
        let mut entries: Vec<_> = self
            .entries
            .into_iter()
            .map(|(path, (file_count, bytes))| {
                let title = if path
                    .file_name()
                    .is_some_and(|s| s.eq_ignore_ascii_case("$RECYCLE.BIN"))
                {
                    format!("{} 回收站", path.parent().unwrap_or(&path).display())
                } else {
                    path.display().to_string()
                };
                SpaceUsage {
                    path: path.to_string_lossy().into(),
                    title,
                    file_count,
                    bytes,
                }
            })
            .collect();
        entries.sort_by_key(|item| std::cmp::Reverse(item.bytes));
        entries.truncate(count);
        entries
    }
}
fn cache_shaped_directory(path: &Path) -> bool {
    path.file_name().is_some_and(|name| {
        matches!(
            name.to_string_lossy().to_ascii_lowercase().as_str(),
            "cache"
                | ".cache"
                | "cache_data"
                | "code cache"
                | "gpucache"
                | "shadercache"
                | "cache2"
        )
    })
}
struct Frame {
    entries: ReadDir,
    directory: Arc<Directory>,
    allowed_children: Option<Vec<PathBuf>>,
    read_only: bool,
    space_root: Option<Arc<PathBuf>>,
    cache_root: Option<Arc<PathBuf>>,
    summary: Option<crate::shared_tree::Summary>,
}

/// Developer-only read-only audit; it never creates an executable cleanup capability or writes user data.
pub fn readonly_audit() -> Result<serde_json::Value, String> {
    shared::audit()
}

fn directory_handle_stamp(path: &Path) -> io::Result<Stamp> {
    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt;
        OpenOptions::new()
            .access_mode(0x80)
            .share_mode(7)
            .custom_flags(0x02000000 | 0x00200000)
            .open(path)?
    };
    #[cfg(not(windows))]
    let file = File::open(path)?;
    stamp_handle(&file, true)
}

#[cfg(test)]
fn scan_roots(
    roots: &[(String, PathBuf)],
    scan_id: &str,
    cancel: &AtomicBool,
    limits: Limits,
    notify: impl FnMut(&str, &Stats),
) -> (Option<Snapshot>, Preview) {
    scan_roots_with_rules(
        roots,
        scan_id,
        cancel,
        limits,
        &crate::full_cleanup_rules::Context::current(),
        crate::cache_cleaner::browser_running,
        notify,
    )
}
#[cfg(test)]
fn scan_roots_with_rules(
    roots: &[(String, PathBuf)],
    scan_id: &str,
    cancel: &AtomicBool,
    limits: Limits,
    rules: &crate::full_cleanup_rules::Context,
    process_status: impl Fn(&str) -> Result<bool, String>,
    notify: impl FnMut(&str, &Stats),
) -> (Option<Snapshot>, Preview) {
    scan_roots_observed(
        roots,
        scan_id,
        cancel,
        limits,
        rules,
        process_status,
        notify,
        None,
    )
}
#[allow(clippy::too_many_arguments)]
fn scan_roots_observed(
    roots: &[(String, PathBuf)],
    scan_id: &str,
    cancel: &AtomicBool,
    limits: Limits,
    rules: &crate::full_cleanup_rules::Context,
    process_status: impl Fn(&str) -> Result<bool, String>,
    notify: impl FnMut(&str, &Stats),
    trees: Option<&mut crate::shared_tree::TreeSink>,
) -> (Option<Snapshot>, Preview) {
    scan_roots_routed(
        roots,
        scan_id,
        cancel,
        limits,
        rules,
        process_status,
        notify,
        trees,
        None,
    )
}
#[allow(clippy::too_many_arguments)]
fn scan_roots_routed(
    roots: &[(String, PathBuf)],
    scan_id: &str,
    cancel: &AtomicBool,
    limits: Limits,
    rules: &crate::full_cleanup_rules::Context,
    process_status: impl Fn(&str) -> Result<bool, String>,
    mut notify: impl FnMut(&str, &Stats),
    mut trees: Option<&mut crate::shared_tree::TreeSink>,
    routes: Option<&quick_locations::Routes>,
) -> (Option<Snapshot>, Preview) {
    let context = Arc::new(rules.clone());
    let mut recommended = RecommendedCollector::new(
        if limits.memory == CANDIDATE_MEMORY_LIMIT {
            RECOMMENDED_MEMORY_LIMIT
        } else {
            limits.memory
        },
        RECOMMENDED_LIMIT,
    );
    let mut running = HashMap::new();
    let mut blocked_apps = HashSet::new();
    let mut learned = quick_locations::Hints::new(roots.iter().map(|(_, root)| root.clone()));
    let mut walk_limited = false;
    let started = Instant::now();
    let now = SystemTime::now();
    let mut last_progress = Instant::now();
    let mut preview = Preview {
        scan_mode: if routes.is_some() { "quick" } else { "full" },
        learned_locations: Vec::new(),
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
        recommended: None,
        overview: Vec::new(),
        unclassified_caches: Vec::new(),
        overview_limited: false,
        shared_reused: false,
    };
    let matcher = crate::rule_probe::ScanMatcher::new(rules);
    let mut spaces = SpaceCollector::new(4096);
    let mut unknown_caches = SpaceCollector::new(4096);
    let mut candidates = Vec::new();
    let mut retained_bytes = 0usize;
    'volumes: for (drive_id, root) in roots {
        if let Some(sink) = trees.as_deref_mut() {
            sink.begin_root(root);
        }
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
            allowed_children: rules.restricted_children(root),
            read_only: protected_directory(root) && rules.restricted_children(root).is_none(),
            space_root: None,
            cache_root: None,
            summary: trees
                .as_ref()
                .map(|_| crate::shared_tree::Summary::default()),
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
                walk_limited = true;
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
                    let done = frames.pop().unwrap();
                    if let Some(mut summary) = done.summary {
                        if let Some(sink) = trees.as_deref_mut() {
                            sink.directory(&done.directory.path, &mut summary);
                            if let Some(parent) = frames.last_mut() {
                                if let Some(parent_summary) = parent.summary.as_mut() {
                                    parent_summary.child(&summary);
                                }
                            } else {
                                sink.finish_root(&done.directory.path, &summary);
                            }
                        }
                    }
                    continue;
                }
                Some(Err(_)) => {
                    preview.stats.denied_directories += 1;
                    continue;
                }
                Some(Ok(v)) => v,
            };
            let path = entry.path();
            if routes.is_some_and(|routes| !routes.allows(&path)) {
                continue;
            }
            // Wider metadata inventory never grants deletion authority to a protected branch.
            let route_denied = frame
                .allowed_children
                .as_ref()
                .is_some_and(|allowed| !allowed.contains(&path))
                || rules.readonly_maintenance_entry(&path);
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
                let allowed_children = rules.restricted_children(&path);
                let read_only = frame.read_only
                    || route_denied
                    || (protected_directory(&path) && allowed_children.is_none());
                if read_only && !frame.read_only {
                    preview.stats.protected_directories += 1;
                }
                if frame.directory.depth >= limits.depth {
                    walk_limited = true;
                    preview.stats.limited = true;
                    preview.stats.protected_directories += 1;
                    continue;
                }
                let identity = match if read_only {
                    directory_handle_stamp(&path)
                } else {
                    stamp(&path, true)
                } {
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
                let space_root = frame
                    .space_root
                    .clone()
                    .or_else(|| Some(Arc::new(path.clone())));
                let cache_root = if cache_shaped_directory(&path) {
                    Some(Arc::new(path.clone()))
                } else {
                    frame.cache_root.clone()
                };
                let directory = Arc::new(Directory {
                    path,
                    stamp: identity,
                    parent: Some(frame.directory.clone()),
                    depth: frame.directory.depth + 1,
                    volume: frame.directory.volume,
                });
                preview.stats.scanned_directories += 1;
                let summary = (frame.summary.is_some()
                    && !crate::shared_tree::excluded(&directory.path))
                .then(crate::shared_tree::Summary::default);
                frames.push(Frame {
                    summary,
                    entries,
                    directory,
                    allowed_children: if read_only { None } else { allowed_children },
                    read_only,
                    space_root,
                    cache_root,
                });
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            if let Some(summary) = frame.summary.as_mut() {
                summary.file(&path, meta.len());
                if let Some(sink) = trees.as_deref_mut() {
                    sink.file(&path, meta.len());
                }
            }
            preview.stats.scanned_files += 1;
            preview.stats.scanned_bytes = preview.stats.scanned_bytes.saturating_add(meta.len());
            spaces.add(
                frame
                    .space_root
                    .as_deref()
                    .map_or(root.as_path(), PathBuf::as_path),
                meta.len(),
            );
            if frame.read_only || route_denied {
                if let Some(cache) = &frame.cache_root {
                    unknown_caches.add(cache, meta.len());
                }
                continue;
            }
            let Ok(modified) = meta.modified() else {
                continue;
            };
            let matched_rule = matcher.identify(&path, modified, now);
            if matched_rule.is_none() {
                if let Some(cache) = &frame.cache_root {
                    unknown_caches.add(cache, meta.len());
                }
            }
            if let Some(rule) = matched_rule {
                if let Some(boundary) = quick_locations::boundary(&path, root) {
                    learned.add(root, boundary);
                }
                let closed = rule.processes.iter().all(|name| {
                    *running
                        .entry(name.clone())
                        .or_insert_with(|| process_status(name) == Ok(false))
                });
                if !closed {
                    if !rule.app.is_empty() {
                        blocked_apps.insert(rule.app.to_string());
                        if let Some(sink) = trees.as_deref_mut() {
                            sink.blocked(&rule.app);
                        }
                    }
                } else {
                    let cost = candidate_cost(&path, &frame.directory);
                    if !recommended.accepts(meta.len(), cost) {
                        recommended.limited = true;
                    } else if let Ok(saved) = stamp(&path, false) {
                        // Reject linked/replaced files before the more expensive complete parent check.
                        if saved.identity.0 == frame.directory.volume
                            && saved.size == meta.len()
                            && saved.modified == modified
                            && frame.directory.scan_chain_intact()
                        {
                            recommended.push(Candidate {
                                id: String::new(),
                                category: rule.kind.id(),
                                path: path.clone(),
                                stamp: saved,
                                parent: frame.directory.clone(),
                                recommendation: Some(Recommendation::Full {
                                    rule,
                                    context: context.clone(),
                                }),
                            });
                        }
                    }
                }
            }
            // Quick mode prepares only rule recommendations, never manual file candidates.
            if routes.is_some() {
                continue;
            }
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
            let cost = candidate_cost(&path, &frame.directory);
            if candidates.len() >= limits.candidates
                || group.entries.len() >= limits.per_group
                || retained_bytes.saturating_add(cost) > limits.memory
            {
                preview.stats.limited = true;
                continue;
            }
            // Validate the traversal's saved chain before saving an actionable candidate.
            if !frame.directory.scan_chain_intact() {
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
                recommendation: None,
            });
        }
        notify(drive_id, &preview.stats);
    }
    preview.learned_locations = learned.finish();
    preview.overview_limited = spaces.limited || unknown_caches.limited;
    preview.overview = spaces.finish(15);
    preview.unclassified_caches = unknown_caches.finish(8);
    preview.stats.limited |= recommended.limited;
    preview.stats.cancelled = cancel.load(Ordering::Acquire);
    if preview.stats.protected_directories > 0 {
        preview
            .warnings
            .push("系统关键目录和软件环境仅统计空间；旧临时文件、归档报告和维护日志按明确规则检查。过深目录仍会跳过。".into());
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
        preview
            .warnings
            .push("达到本次检查上限，仅列出部分文件；可重新检查。".into());
    }
    preview.stats.rule_reuse = matcher.stats();
    if preview.stats.cancelled {
        preview.plan_id.clear();
        preview.groups.clear();
        preview.recommended = None;
        preview
            .warnings
            .push("扫描已取消，未生成可执行清理预览。".into());
        return (None, preview);
    }
    let mut snapshot = Snapshot {
        id: preview.plan_id.clone(),
        created: SystemTime::now(),
        candidates,
        recommended: None,
    };
    let mut info = crate::cache_cleaner::RecommendedPreview {
        blocked_apps: blocked_apps.into_iter().collect(),
        limited: walk_limited || recommended.limited,
        ..Default::default()
    };
    finish_recommendations(&mut snapshot, &mut info, recommended.into_files());
    preview.recommended = Some(info);
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
    check_snapshot_epoch(state, &mut inner)?;
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
fn check_snapshot_epoch(state: &DiskCleanerState, inner: &mut Inner) -> Result<(), String> {
    if inner
        .snapshot_epoch
        .is_some_and(|epoch| epoch != state.1.epoch())
    {
        inner.snapshot = None;
        inner.snapshot_epoch = None;
        invalidate_session(inner, "文件列表可能已变化，请重新检查");
        return Err("文件列表可能已变化，请重新检查".into());
    }
    Ok(())
}
fn candidate_cost(path: &Path, parent: &Directory) -> usize {
    path.as_os_str()
        .len()
        .saturating_mul(8)
        .saturating_add(2048)
        .saturating_add(
            parent
                .chain()
                .map(|d| {
                    d.path
                        .as_os_str()
                        .len()
                        .saturating_mul(4)
                        .saturating_add(256)
                })
                .sum::<usize>(),
        )
}
#[cfg(test)]
fn valid_candidate(c: &Candidate) -> bool {
    valid_candidate_with_running(c, &crate::cache_cleaner::browser_running, None)
}
fn valid_candidate_with_running(
    c: &Candidate,
    running: &impl Fn(&str) -> Result<bool, String>,
    held: Option<&File>,
) -> bool {
    c.path.parent() == Some(c.parent.path.as_path())
        && c.parent
            .chain()
            .all(|d| stamp(&d.path, true).ok().as_ref() == Some(&d.stamp))
        && stamp(&c.path, false).ok().as_ref() == Some(&c.stamp)
        && match &c.recommendation {
            Some(rule) => rule.valid(&c.path, c.stamp.modified, running, held),
            None => {
                classify(&c.path, c.stamp.size, c.stamp.modified, SystemTime::now())
                    == Some(c.category)
            }
        }
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
        // Exclusive DELETE access binds the action to this verified file handle.
        OpenOptions::new()
            .access_mode(0x10000 | 0x80 | 1)
            .share_mode(0)
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
    invalidate_shared_scan(&app);
    let drives = fixed_drives()?;
    if snapshot.candidates.iter().any(|c| {
        !drives.iter().any(|d| {
            c.parent
                .chain()
                .last()
                .is_some_and(|root| root.path == Path::new(&d.id))
        })
    }) {
        return Err("磁盘列表已改变，请重新扫描".into());
    }
    tokio::task::spawn_blocking(move || {
        let _lease = lease;
        execute_recorded(snapshot, &file_ids, &app_data, |p, _handle| {
            crate::direct_delete::delete(p, _handle)
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
    recycle: impl FnMut(&Path, &File) -> Result<(), String>,
) -> Result<CacheResult, String> {
    execute_recorded_with_running(
        snapshot,
        ids,
        app_data,
        recycle,
        crate::cache_cleaner::browser_running,
    )
}
fn execute_recorded_with_running(
    snapshot: Snapshot,
    ids: &[String],
    app_data: &Path,
    recycle: impl FnMut(&Path, &File) -> Result<(), String>,
    running: impl Fn(&str) -> Result<bool, String>,
) -> Result<CacheResult, String> {
    execute_recorded_policy(
        snapshot,
        ids,
        app_data,
        recycle,
        running,
        ExecutionPolicy::manual(),
    )
}
fn execute_recorded_policy(
    snapshot: Snapshot,
    ids: &[String],
    app_data: &Path,
    recycle: impl FnMut(&Path, &File) -> Result<(), String>,
    running: impl Fn(&str) -> Result<bool, String>,
    policy: ExecutionPolicy<'_>,
) -> Result<CacheResult, String> {
    if policy.batched
        && (snapshot.candidates.len() > RECOMMENDED_LIMIT
            || snapshot
                .candidates
                .iter()
                .any(|file| file.recommendation.is_none())
            || ids.len() != snapshot.candidates.len())
    {
        return Err("便捷清理计划无效，请重新扫描".into());
    }
    let selected: HashSet<_> = ids.iter().map(String::as_str).collect();
    let mut categories = HashSet::new();
    let rules = snapshot
        .candidates
        .iter()
        .filter(|c| {
            selected.contains(c.id.as_str())
                && categories.insert((
                    c.category,
                    c.recommendation.as_ref().map(Recommendation::selection_id),
                    c.recommendation
                        .as_ref()
                        .map(|rule| rule.title().to_owned()),
                ))
        })
        .enumerate()
        .map(|(index, c)| crate::cleanup_history::HistoryRule {
            id: format!("disk-{}-{index}", c.category),
            title: c
                .recommendation
                .as_ref()
                .map_or_else(|| title(c.category), Recommendation::title)
                .into(),
        })
        .collect();
    let history = crate::cleanup_history::start_record(app_data, &snapshot.id, rules)
        .map_err(|e| format!("未能保存清理记录，本次未执行清理，请重新扫描。{e}"))?;
    let outcome = execute_files_with_policy(
        &snapshot, &selected, app_data, recycle, log_line, running, policy,
    );
    let (summary, failure) = match &outcome {
        Ok(r) => (
            crate::cleanup_history::HistorySummary {
                deleted_files: r.deleted_files,
                deleted_bytes: r.deleted_bytes,
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
#[cfg(test)]
fn execute_files(
    snapshot: &Snapshot,
    selected: &HashSet<&str>,
    app_data: &Path,
    mut recycle: impl FnMut(&Path, &File) -> Result<(), String>,
) -> Result<CacheResult, String> {
    execute_files_with_logger(snapshot, selected, app_data, &mut recycle, log_line)
}
#[cfg(test)]
fn execute_files_with_logger(
    snapshot: &Snapshot,
    selected: &HashSet<&str>,
    app_data: &Path,
    mut recycle: impl FnMut(&Path, &File) -> Result<(), String>,
    mut logger: impl FnMut(&mut File, &str, &Path, &str) -> Result<(), String>,
) -> Result<CacheResult, String> {
    execute_files_with_logger_and_running(
        snapshot,
        selected,
        app_data,
        &mut recycle,
        &mut logger,
        crate::cache_cleaner::browser_running,
    )
}
#[cfg(test)]
fn execute_files_with_logger_and_running(
    snapshot: &Snapshot,
    selected: &HashSet<&str>,
    app_data: &Path,
    mut recycle: impl FnMut(&Path, &File) -> Result<(), String>,
    mut logger: impl FnMut(&mut File, &str, &Path, &str) -> Result<(), String>,
    running: impl Fn(&str) -> Result<bool, String>,
) -> Result<CacheResult, String> {
    execute_files_with_policy(
        snapshot,
        selected,
        app_data,
        &mut recycle,
        &mut logger,
        running,
        ExecutionPolicy::manual(),
    )
}
fn execute_files_with_policy(
    snapshot: &Snapshot,
    selected: &HashSet<&str>,
    app_data: &Path,
    mut recycle: impl FnMut(&Path, &File) -> Result<(), String>,
    mut logger: impl FnMut(&mut File, &str, &Path, &str) -> Result<(), String>,
    running: impl Fn(&str) -> Result<bool, String>,
    mut policy: ExecutionPolicy<'_>,
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
    let mut batch_started = Instant::now();
    let mut batch_first = 0usize;
    let mut batch = 1usize;
    let total = snapshot
        .candidates
        .iter()
        .filter(|c| selected.contains(c.id.as_str()))
        .count();
    let mut last_progress = Instant::now();
    let mut stopped = false;
    for (index, c) in snapshot
        .candidates
        .iter()
        .filter(|c| selected.contains(c.id.as_str()))
        .enumerate()
    {
        if policy.batched
            && (index - batch_first >= policy.batch_items.clamp(1, MAX_ACTIONS)
                || batch_started.elapsed() >= Duration::from_secs(60))
        {
            batch += 1;
            batch_first = index;
            batch_started = Instant::now();
        }
        if index == 0 || last_progress.elapsed() >= Duration::from_millis(250) {
            if let Some(notify) = policy.notify.as_mut() {
                notify(index, total, batch, &result);
            }
            last_progress = Instant::now();
        }
        let limit_reached = if policy.batched {
            started.elapsed() >= RECOMMENDED_EXECUTION_TIME
        } else {
            index >= MAX_ACTIONS || started.elapsed() >= Duration::from_secs(60)
        };
        if stopped || limit_reached {
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
            || !valid_candidate_with_running(c, &running, Some(&target_handle))
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
        // Revalidate after durable pending, then delete the same held file.
        if stamp_handle(&target_handle, false).ok().as_ref() != Some(&c.stamp)
            || !valid_candidate_with_running(c, &running, Some(&target_handle))
        {
            result.skipped_files += 1;
            continue;
        }
        let action = recycle(&c.path, &target_handle);
        drop(target_handle);
        let status = match action {
            Ok(()) if matches!(fs::symlink_metadata(&c.path), Err(e) if e.kind() == io::ErrorKind::NotFound) =>
            {
                result.deleted_files += 1;
                result.deleted_bytes = result.deleted_bytes.saturating_add(c.stamp.size);
                "success"
            }
            Ok(()) => {
                result.failed_files += 1;
                if !result.messages.iter().any(|m| m.contains("未能确认")) {
                    result
                        .messages
                        .push("部分文件未能确认直接删除，未计入成功数量。".into());
                }
                "uncertain"
            }
            Err(error) => {
                result.failed_files += 1;
                if result.messages.len() < 8 {
                    result.messages.push(format!("文件未能确认删除：{error}"));
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
    if let Some(notify) = policy.notify.as_mut() {
        notify(total, total, batch, &result);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::FileTimes;

    #[test]
    fn cancelling_shared_session_finishes_waiting_and_old_worker_cannot_replace_new_session() {
        let state = DiskCleanerState::default();
        let set_session = |id: &str| {
            register_scan(&state, id).unwrap();
            state.0.lock().unwrap().session = Some(DiskScanSession {
                scan_mode: "full",
                scan_id: id.into(),
                drive_ids: vec!["fixture".into()],
                status: "running",
                progress: None,
                preview: None,
                error: None,
            });
        };
        set_session("old");
        request_cancel(&state, "old").unwrap();
        assert_eq!(
            session_snapshot(&state).unwrap().unwrap().status,
            "cancelled"
        );
        set_session("new");
        finish_scan(&state, "old", true, Err("old worker finished".into()));
        finish_scan(&state, "old", false, Err("old worker failed".into()));
        let inner = state.0.lock().unwrap();
        assert_eq!(inner.active.as_ref().unwrap().0, "new");
        assert_eq!(inner.session.as_ref().unwrap().status, "running");
        assert_eq!(inner.session.as_ref().unwrap().scan_id, "new");
        drop(inner);
        finish_scan(&state, "new", true, Err("cancelled".into()));
        assert!(state.0.lock().unwrap().active.is_none());
    }

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
            scan_mode: "full",
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

    fn rule_context(root: &Path) -> crate::full_cleanup_rules::Context {
        crate::full_cleanup_rules::Context {
            catalog: crate::rule_catalog::builtin(),
            local: Some(root.join("user/AppData/Local")),
            roaming: Some(root.join("user/AppData/Roaming")),
            windows: Some(root.join("Windows")),
            temp: vec![
                root.join("user/AppData/Local/Temp"),
                root.join("Windows/Temp"),
                root.join("D/Temp"),
            ],
            pip: vec![
                root.join("user/AppData/Local/pip/Cache"),
                root.join("D/pip-cache"),
            ],
            npm: vec![
                root.join("user/AppData/Local/npm-cache"),
                root.join("D/npm-cache"),
            ],
            nuget: vec![
                root.join("user/AppData/Local/NuGet/v3-cache"),
                root.join("D/nuget-http"),
            ],
            cargo: vec![root.join("user/.cargo"), root.join("D/cargo-home")],
            huggingface_xet: vec![],
            program_data: Some(root.join("ProgramData")),
        }
    }
    fn scan_rules(
        root: &Path,
        context: &crate::full_cleanup_rules::Context,
        running: impl Fn(&str) -> Result<bool, String>,
    ) -> (Snapshot, Preview) {
        let (plan, preview) = scan_roots_with_rules(
            &[("owned-volume".into(), root.into())],
            "full-rules",
            &AtomicBool::new(false),
            Limits::default(),
            context,
            running,
            |_, _| {},
        );
        (plan.unwrap(), preview)
    }
    fn rule_file(root: &Path, name: &str, days: u64) -> PathBuf {
        let path = root.join(name);
        old_file(&path, days);
        path
    }
    #[test]
    fn selected_software_excludes_unselected_cache_and_diagnostics_and_records_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let context = rule_context(root);
        let chrome = rule_file(
            root,
            "user/AppData/Local/Google/Chrome/User Data/Default/Cache/Cache_Data/one",
            60,
        );
        rule_file(
            root,
            "user/AppData/Local/Google/Chrome/User Data/Local State",
            60,
        );
        let cherry = rule_file(
            root,
            "user/AppData/Roaming/CherryStudio/Cache/Cache_Data/two",
            60,
        );
        rule_file(root, "user/AppData/Roaming/CherryStudio/Local State", 60);
        rule_file(
            root,
            "user/AppData/Roaming/CherryStudio/Crashpad/settings.dat",
            60,
        );
        let dump = rule_file(root, "user/AppData/Roaming/CherryStudio/Crashpad/reports/11111111-2222-3333-4444-555555555555.dmp", 60);
        let (plan, preview) = scan_rules(root, &context, |_| Ok(false));
        let files = plan.recommended.as_ref().unwrap();
        assert_eq!(
            files.len(),
            3,
            "{:?}",
            files.iter().map(|file| &file.path).collect::<Vec<_>>()
        );
        let chrome_id = files
            .iter()
            .find(|file| file.path == chrome)
            .unwrap()
            .recommendation
            .as_ref()
            .unwrap()
            .selection_id();
        let cherry_ids: HashSet<_> = files
            .iter()
            .filter(|file| file.path == cherry || file.path == dump)
            .map(|file| file.recommendation.as_ref().unwrap().selection_id())
            .collect();
        assert_eq!(
            cherry_ids.len(),
            1,
            "one software selection covers cache and crash reports"
        );
        assert!(!cherry_ids.contains(&chrome_id));
        let id = plan.id.clone();
        let state = completed_session(plan, preview);
        for invalid in [
            vec![],
            vec![chrome_id.clone(), chrome_id.clone()],
            vec!["software:unknown".into()],
        ] {
            assert!(take_recommended_selected(&state, &id, Some(&invalid)).is_err());
            assert!(
                state.0.lock().unwrap().snapshot.is_some(),
                "invalid selection must not consume the plan"
            );
        }
        let plan = take_recommended_selected(&state, &id, Some(&[chrome_id])).unwrap();
        assert_eq!(plan.candidates.len(), 1);
        assert_eq!(plan.candidates[0].path, chrome);
        let ids = plan
            .candidates
            .iter()
            .map(|file| file.id.clone())
            .collect::<Vec<_>>();
        let data = root.join("data");
        fs::create_dir(&data).unwrap();
        let result = execute_recorded_policy(
            plan,
            &ids,
            &data,
            crate::direct_delete::delete,
            |_| Ok(false),
            ExecutionPolicy {
                batched: true,
                notify: None,
                batch_items: MAX_ACTIONS,
            },
        )
        .unwrap();
        assert_eq!(result.deleted_files, 1);
        assert!(!chrome.exists());
        assert!(cherry.exists() && dump.exists());
        let history = crate::cleanup_history::read_history(&data).unwrap();
        assert_eq!(history.records.len(), 1);
        assert_eq!(history.records[0].deleted_files, 1);
        assert!(history.records[0]
            .selected_rules
            .iter()
            .all(|rule| !rule.title.contains("Cherry")));
        assert!(take_recommended_selected(&state, &id, None).is_err());
    }
    #[test]
    fn multiple_selected_software_projects_keep_distinct_history_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let context = rule_context(root);
        for app in ["CherryStudio", "xyz.chatboxapp.app"] {
            rule_file(root, &format!("user/AppData/Roaming/{app}/Local State"), 60);
            rule_file(
                root,
                &format!("user/AppData/Roaming/{app}/Cache/Cache_Data/file"),
                60,
            );
        }
        let (plan, preview) = scan_rules(root, &context, |_| Ok(false));
        assert_eq!(plan.recommended.as_ref().unwrap().len(), 2);
        let ids = preview
            .recommended
            .as_ref()
            .unwrap()
            .groups
            .iter()
            .map(|group| group.id.clone())
            .collect::<Vec<_>>();
        let id = plan.id.clone();
        let state = completed_session(plan, preview);
        let plan = take_recommended_selected(&state, &id, Some(&ids)).unwrap();
        let files = plan
            .candidates
            .iter()
            .map(|file| file.id.clone())
            .collect::<Vec<_>>();
        let data = root.join("data");
        fs::create_dir(&data).unwrap();
        let result = execute_recorded_policy(
            plan,
            &files,
            &data,
            crate::direct_delete::delete,
            |_| Ok(false),
            ExecutionPolicy {
                batched: true,
                notify: None,
                batch_items: MAX_ACTIONS,
            },
        )
        .unwrap();
        assert_eq!(result.deleted_files, 2);
        let history = crate::cleanup_history::read_history(&data).unwrap();
        assert!(history.warnings.is_empty());
        let rules = &history.records[0].selected_rules;
        assert_eq!(rules.len(), 2);
        assert_ne!(rules[0].id, rules[1].id);
        assert!(rules.iter().any(|rule| rule.title.contains("Cherry")));
        assert!(rules.iter().any(|rule| rule.title.contains("Chatbox")));
    }
    #[test]
    fn wider_space_inventory_never_makes_protected_files_actionable() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for path in [
            "Program Files/Product/large.iso",
            "Windows/System32/old.log",
            "$RECYCLE.BIN/old.log",
            "project/venv/old.log",
        ] {
            old_file(&root.join(path), 100);
        }
        old_file(&root.join("personal.log"), 100);
        let context = rule_context(root);
        let (plan, preview) = scan_rules(root, &context, |_| Ok(false));
        assert_eq!(preview.stats.scanned_files, 5);
        assert_eq!(plan.candidates.len(), 1);
        assert!(plan.candidates[0].path.ends_with("personal.log"));
        assert!(preview
            .overview
            .iter()
            .any(|row| row.title.contains("回收站")));
        assert!(plan
            .recommended
            .as_ref()
            .is_none_or(|files| files.is_empty()));
        assert!(root.join("Windows/System32/old.log").is_file());
    }
    #[test]
    fn noncache_archives_execute_while_current_logs_databases_and_system_files_stay() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let context = rule_context(root);
        let expected=[
            "user/AppData/Roaming/CherryStudio/Crashpad/reports/1448c0e1-689e-43c0-a47c-32eb1d6e5f7e.dmp",
            "user/AppData/Roaming/CherryStudio/logs/main.old.log",
            "user/AppData/Roaming/CherryStudio/logs/app-error.2020-01-01.log",
            "user/AppData/Roaming/QQ/log/app_launcher-20200101.log",
            "Windows/Logs/CBS/CbsPersist_20200101120000.cab",
            "Windows/Logs/DISM/dism.log.bak",
            "D/npm-cache/_logs/2020-01-01T12_00_00_000Z-debug-0.log",
        ].map(|p|rule_file(root,p,120));
        let kept = [
            "user/AppData/Roaming/CherryStudio/Local State",
            "user/AppData/Roaming/CherryStudio/Crashpad/settings.dat",
            "user/AppData/Roaming/CherryStudio/Crashpad/metadata",
            "user/AppData/Roaming/CherryStudio/logs/main.log",
            "user/AppData/Roaming/CherryStudio/logs/app-error.2021-01-01.log",
            "user/AppData/Roaming/CherryStudio/IndexedDB/000003.log",
            "user/AppData/Roaming/CherryStudio/logs/000003.log",
            "user/AppData/Roaming/QQ/Local State",
            "user/AppData/Roaming/QQ/log/app_launcher-20210101.log",
            "Windows/Logs/CBS/CBS.log",
            "Windows/Logs/DISM/dism.log",
            "Windows/Logs/CBS/installer.cab",
            "Windows/Logs/CBS/Other/old.log",
            "Windows/Installer/repair.tmp",
        ]
        .map(|p| rule_file(root, p, 120));
        let (plan, preview) = scan_rules(root, &context, |_| Ok(false));
        assert_eq!(
            preview.recommended.as_ref().unwrap().file_count,
            expected.len() as u64
        );
        assert!(plan
            .candidates
            .iter()
            .all(|c| !c.path.ends_with("CBS.log") && !c.path.ends_with("dism.log")));
        let state = completed_session(plan, preview);
        let id = state
            .0
            .lock()
            .unwrap()
            .snapshot
            .as_ref()
            .unwrap()
            .id
            .clone();
        let plan = take_recommended(&state, &id).unwrap();
        let logs = tempfile::tempdir().unwrap();
        let result = execute_files_with_logger_and_running(
            &plan,
            &selected(&plan),
            logs.path(),
            crate::direct_delete::delete,
            log_line,
            |_| Ok(false),
        )
        .unwrap();
        assert_eq!(result.deleted_files, expected.len() as u64);
        assert!(expected.iter().all(|p| !p.exists()));
        assert!(kept.iter().all(|p| p.exists()));
    }
    #[test]
    fn noncache_processes_and_changed_report_markers_prevent_deletion() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let context = rule_context(root);
        let report=rule_file(root,"user/AppData/Roaming/CherryStudio/Crashpad/reports/1448c0e1-689e-43c0-a47c-32eb1d6e5f7e.dmp",120);
        rule_file(root, "user/AppData/Roaming/CherryStudio/Local State", 0);
        let marker = rule_file(
            root,
            "user/AppData/Roaming/CherryStudio/Crashpad/settings.dat",
            0,
        );
        let (_, blocked) = scan_rules(root, &context, |name| {
            Ok(name.eq_ignore_ascii_case("Cherry Studio.exe"))
        });
        assert_eq!(blocked.recommended.unwrap().file_count, 0);
        let (plan, preview) = scan_rules(root, &context, |_| Ok(false));
        assert_eq!(preview.recommended.as_ref().unwrap().file_count, 1);
        let state = completed_session(plan, preview);
        let id = state
            .0
            .lock()
            .unwrap()
            .snapshot
            .as_ref()
            .unwrap()
            .id
            .clone();
        let plan = take_recommended(&state, &id).unwrap();
        let logs = tempfile::tempdir().unwrap();
        let result = execute_files_with_logger_and_running(
            &plan,
            &selected(&plan),
            logs.path(),
            crate::direct_delete::delete,
            log_line,
            |name| Ok(name.eq_ignore_ascii_case("Cherry Studio.exe")),
        )
        .unwrap();
        assert_eq!(result.deleted_files, 0);
        assert!(report.exists());
        let (plan, preview) = scan_rules(root, &context, |_| Ok(false));
        let state = completed_session(plan, preview);
        let id = state
            .0
            .lock()
            .unwrap()
            .snapshot
            .as_ref()
            .unwrap()
            .id
            .clone();
        let plan = take_recommended(&state, &id).unwrap();
        fs::remove_file(marker).unwrap();
        let result = execute_files_with_logger_and_running(
            &plan,
            &selected(&plan),
            logs.path(),
            crate::direct_delete::delete,
            log_line,
            |_| Ok(false),
        )
        .unwrap();
        assert_eq!(result.deleted_files, 0);
        assert!(report.exists());
    }
    #[test]
    fn full_walk_recommends_verified_rules_on_another_volume_without_manual_selection() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let context = rule_context(root);
        let expected = [
            ("D/Temp/stale.tmp", 40),
            ("user/AppData/Local/CrashDumps/app.dmp", 40),
            ("D/pip-cache/http-v2/a/download.body", 40),
            ("D/npm-cache/_cacache/content-v2/a/hash", 40),
            ("D/project/__pycache__/module.cpython-313.pyc", 8),
            ("user/AppData/Roaming/Code/Cache/Cache_Data/hash", 1),
            (
                "user/AppData/Roaming/Code/logs/20260101T120000/window1/renderer.log",
                40,
            ),
            ("D/VSCode/data/user-data/GPUCache/hash", 1),
            ("Windows/Temp/old.etl", 40),
            ("Windows/Minidump/old.dmp", 40),
        ]
        .map(|(name, days)| rule_file(root, name, days));
        for name in [
            "D/project/module.py",
            "D/VSCode/Code.exe",
            "D/VSCode/resources/app/product.json",
        ] {
            rule_file(root, name, 0);
        }
        let kept = [
            "D/project/important.tmp",
            "D/project/important.log",
            "D/Downloads/installer.iso",
            "D/Downloads/unfinished.part",
            "D/project/__pycache__/orphan.cpython-313.pyc",
            "user/AppData/Roaming/Code/User/History/original.log",
            "user/AppData/Roaming/Code/Backups/unsaved.tmp",
            "D/Temp/recent.tmp",
            "D/Temp/UnsavedFiles/unsaved.tmp",
            "D/project/model.safetensors",
            "Windows/Installer/repair.tmp",
            "Windows/WinSxS/repair.tmp",
            "D/unverified/data/user-data/GPUCache/hash",
        ]
        .map(|name| {
            rule_file(
                root,
                name,
                if name.ends_with("recent.tmp") { 1 } else { 40 },
            )
        });
        let (plan, preview) = scan_rules(root, &context, |_| Ok(false));
        let recommendation = preview.recommended.unwrap();
        assert_eq!(recommendation.file_count, expected.len() as u64);
        let state = completed_session(
            plan,
            Preview {
                recommended: Some(recommendation),
                ..preview
            },
        );
        let id = state
            .0
            .lock()
            .unwrap()
            .snapshot
            .as_ref()
            .unwrap()
            .id
            .clone();
        let plan = take_recommended(&state, &id).unwrap();
        assert!(plan.candidates.iter().all(|c| c.recommendation.is_some()));
        let ids = selected(&plan);
        let data = tempfile::tempdir().unwrap();
        let result = execute_files_with_logger_and_running(
            &plan,
            &ids,
            data.path(),
            crate::direct_delete::delete,
            log_line,
            |_| Ok(false),
        )
        .unwrap();
        assert_eq!(result.deleted_files, expected.len() as u64);
        assert!(expected.iter().all(|p| !p.exists()));
        assert!(kept.iter().all(|p| p.exists()));
        assert!(take_recommended(&state, &id).is_err());
    }
    #[test]
    fn mixed_full_walk_and_browser_cache_use_one_history_and_leave_manual_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let context = rule_context(root);
        let ordinary = rule_file(root, "D/Temp/old.tmp", 40);
        let cache = rule_file(
            root,
            "user/AppData/Local/Google/Chrome/User Data/Default/GPUCache/owned",
            1,
        );
        let manual = rule_file(root, "D/work/keep.tmp", 40);
        let (mut plan, mut preview) = scan_rules(root, &context, |_| Ok(false));
        let local = context.local.as_ref().unwrap();
        let (cache_plan, _) = crate::cache_cleaner::prepare_recommended_with_running(
            local,
            None,
            &[root.into()],
            &AtomicBool::new(false),
            |_| Ok(false),
        );
        let mut files = plan.recommended.take().unwrap();
        let mut directories = HashMap::new();
        files.extend(
            cache_plan
                .unwrap()
                .into_targets()
                .into_iter()
                .map(|target| {
                    cache_candidate(target, &[("owned".into(), root.into())], &mut directories)
                        .unwrap()
                }),
        );
        finish_recommendations(&mut plan, preview.recommended.as_mut().unwrap(), files);
        assert_eq!(preview.recommended.as_ref().unwrap().file_count, 2);
        let id = plan.id.clone();
        let state = completed_session(plan, preview);
        let plan = take_recommended(&state, &id).unwrap();
        let ids = plan
            .candidates
            .iter()
            .map(|c| c.id.clone())
            .collect::<Vec<_>>();
        let data = tempfile::tempdir().unwrap();
        let result = execute_recorded_with_running(
            plan,
            &ids,
            data.path(),
            crate::direct_delete::delete,
            |_| Ok(false),
        )
        .unwrap();
        assert_eq!(result.deleted_files, 2);
        assert_eq!(result.failed_files, 0);
        assert!(!cache.exists() && !ordinary.exists());
        assert!(manual.exists());
        let history = crate::cleanup_history::read_history(data.path()).unwrap();
        assert_eq!(history.records.len(), 1);
        assert_eq!(history.records[0].deleted_files, 2);
        assert_eq!(history.records[0].selected_rules.len(), 2);
        assert!(take_recommended(&state, &id).is_err());
    }

    #[test]
    fn recommended_batches_continue_and_share_one_history_record() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let context = rule_context(root);
        let expected: Vec<_> = (0..7)
            .map(|index| rule_file(root, &format!("D/Temp/owned-{index}.tmp"), 40))
            .collect();
        let keep = rule_file(root, "D/documents/keep.tmp", 40);
        let (plan, preview) = scan_rules(root, &context, |_| Ok(false));
        let state = completed_session(plan, preview);
        let id = state
            .0
            .lock()
            .unwrap()
            .snapshot
            .as_ref()
            .unwrap()
            .id
            .clone();
        let plan = take_recommended(&state, &id).unwrap();
        let ids: Vec<_> = plan.candidates.iter().map(|c| c.id.clone()).collect();
        let data = tempfile::tempdir().unwrap();
        let mut final_progress = (0, 0, 0);
        let mut progress = |processed: usize, total: usize, batch: usize, _result: &CacheResult| {
            final_progress = (processed, total, batch);
        };
        let result = execute_recorded_policy(
            plan,
            &ids,
            data.path(),
            crate::direct_delete::delete,
            |_| Ok(false),
            ExecutionPolicy {
                batched: true,
                notify: Some(&mut progress),
                batch_items: 3,
            },
        )
        .unwrap();
        assert_eq!(result.deleted_files, 7);
        assert_eq!(result.failed_files, 0);
        assert_eq!(final_progress, (7, 7, 3));
        assert!(expected.iter().all(|p| !p.exists()));
        assert!(keep.exists());
        let history = crate::cleanup_history::read_history(data.path()).unwrap();
        assert_eq!(history.records.len(), 1);
        assert_eq!(history.records[0].deleted_files, 7);
        assert!(take_recommended(&state, &id).is_err());
    }
    fn next_rule_files(root: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let expected=[
            "Windows/MEMORY.DMP", "Windows/LiveKernelReports/WATCHDOG/old.dmp",
            "ProgramData/Microsoft/Windows/WER/ReportArchive/AppCrash_example/Report.wer",
            "user/AppData/Local/Microsoft/Windows/WER/ReportArchive/Kernel_example/trace.etl",
            "D/nuget-http/feed/nupkg_sample.1.0.0.dat",
            "D/project/target/debug/incremental/module/s-old/dep-graph.bin",
            "D/project/target/x86_64-pc-windows-msvc/release/incremental/module/work.o",
            "user/AppData/Local/EpicGamesLauncher/Saved/webcache_4430/Default/Cache/Cache_Data/hash",
            "user/AppData/Local/JetBrains/PyCharm2026.2/caches/index.dat",
            "user/AppData/Local/JetBrains/IntelliJIdea2026.2/index/keys",
            "user/AppData/Local/Vivaldi/User Data/Profile 2/Code Cache/js/data",
            "D/Vivaldi/User Data/Default/GPUCache/hash",
        ].map(|name|rule_file(root,name,40)).to_vec();
        for name in [
            "D/project/Cargo.toml",
            "D/Vivaldi/Application/vivaldi.exe",
            "D/Vivaldi/User Data/Local State",
        ] {
            rule_file(root, name, 0);
        }
        let kept=[
            "Windows/system.ini", "Windows/Installer/keep.msi", "Windows/WinSxS/keep.log",
            "Windows/LiveKernelReports/WATCHDOG/recent.dmp",
            "ProgramData/Microsoft/Windows/WER/ReportQueue/AppCrash_waiting/Report.wer",
            "ProgramData/Microsoft/Windows/WER/ReportArchive/MyDocuments/notes.txt",
            "ProgramData/Microsoft/Windows/WER/ReportArchive/Config.ini",
            "ProgramData/Microsoft/Windows/WER/ReportArchive/AppCrash_example/program.exe",
            "ProgramData/Microsoft/Windows/Other/keep.log", "ProgramData/Microsoft/Office/keep.log",
            "ProgramData/Unknown/Caches/keep.tmp",
            "D/nuget-http/feed/working.dat", "D/nuget-http/config.xml", "D/nuget-http/feed/nupkg_recent.dat",
            "D/project/target/release/program.exe", "D/project/target/debug/deps/library.rlib",
            "D/project/target/debug/incremental/module/session.lock",
            "D/no-project/target/debug/incremental/module/cache.bin",
            "user/AppData/Local/EpicGamesLauncher/Saved/webcache_backup/Cache/Cache_Data/copy",
            "user/AppData/Local/EpicGamesLauncher/Saved/webcache_4430/Default/Network/Cookies",
            "user/AppData/Local/EpicGamesLauncher/Saved/webcache_4430/Default/Local Storage/leveldb/chat.log",
            "user/AppData/Local/EpicGamesLauncher/Saved/Saves/game.sav",
            "user/AppData/Local/JetBrains/PyCharm2026.2/LocalHistory/recover.dat",
            "user/AppData/Local/JetBrains/PyCharm2026.2/plugins/installed.jar",
            "user/AppData/Local/JetBrains/PyCharmUnknown/caches/keep.dat",
            "user/AppData/Local/JetBrains/PyCharm2026.2/caches/recent.dat",
            "user/AppData/Local/Vivaldi/User Data/Default/Bookmarks",
            "user/AppData/Local/Vivaldi/User Data/Default/Media Cache/offline",
            "D/Vivaldi/User Data/Default/Login Data",
            "D/fake-browser/User Data/Default/GPUCache/hash",
        ].map(|name|rule_file(root,name,if name.contains("recent") {1}else{40})).to_vec();
        (expected, kept)
    }
    #[test]
    fn expanded_rules_reclaim_only_verified_files_and_preserve_user_data() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let context = rule_context(root);
        let (expected, kept) = next_rule_files(root);
        let (mut plan, preview) = scan_rules(root, &context, |_| Ok(false));
        assert_eq!(
            preview.recommended.as_ref().unwrap().file_count,
            expected.len() as u64
        );
        plan.candidates = plan.recommended.take().unwrap();
        let ids = plan
            .candidates
            .iter()
            .map(|c| c.id.clone())
            .collect::<Vec<_>>();
        let data = tempfile::tempdir().unwrap();
        let result = execute_recorded_with_running(
            plan,
            &ids,
            data.path(),
            crate::direct_delete::delete,
            |_| Ok(false),
        )
        .unwrap();
        assert_eq!(result.deleted_files, expected.len() as u64);
        assert_eq!(result.failed_files, 0);
        assert!(expected.iter().all(|p| !p.exists()));
        assert!(kept.iter().all(|p| p.exists()));
        assert_eq!(
            crate::cleanup_history::read_history(data.path())
                .unwrap()
                .records
                .len(),
            1
        );
    }
    #[test]
    fn expanded_rules_block_processes_and_revalidate_project_and_browser_markers() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let context = rule_context(root);
        let (expected, _) = next_rule_files(root);
        for running in [true, false] {
            let (plan, preview) = scan_rules(root, &context, |_| {
                if running {
                    Ok(true)
                } else {
                    Err("unknown".into())
                }
            });
            let files = plan.recommended.unwrap();
            assert!(files.iter().all(|c| c.category == "quick-dumps"));
            assert!(preview.recommended.unwrap().blocked_apps.len() >= 6);
        }
        let (mut plan, _) = scan_rules(root, &context, |_| Ok(false));
        plan.candidates = plan.recommended.take().unwrap();
        std::fs::remove_file(root.join("D/project/Cargo.toml")).unwrap();
        std::fs::remove_file(root.join("D/Vivaldi/User Data/Local State")).unwrap();
        let data = tempfile::tempdir().unwrap();
        plan.candidates
            .retain(|c| matches!(c.category, "quick-rust" | "quick-portable-browser"));
        let ids = selected(&plan);
        let result = execute_files_with_logger_and_running(
            &plan,
            &ids,
            data.path(),
            crate::direct_delete::delete,
            log_line,
            |_| Ok(false),
        )
        .unwrap();
        assert_eq!(result.deleted_files, 0);
        assert_eq!(result.skipped_files, 3);
        assert!(root
            .join("D/project/target/debug/incremental/module/s-old/dep-graph.bin")
            .exists());
        assert!(root
            .join("D/Vivaldi/User Data/Default/GPUCache/hash")
            .exists());
        assert!(expected.iter().all(|path| path.exists()));
    }
    #[test]
    fn full_scan_collector_retains_larger_late_candidates_with_bounded_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let context = rule_context(root);
        rule_file(root, "D/Temp/cache.tmp", 40);
        let (mut plan, _) = scan_rules(root, &context, |_| Ok(false));
        let original = plan.recommended.take().unwrap().pop().unwrap();
        let mut collector = RecommendedCollector::new(CANDIDATE_MEMORY_LIMIT, 2);
        for (name, size) in [
            ("early-1", 1),
            ("early-2", 2),
            ("later-big", 1024 * 1024),
            ("last-small", 1),
        ] {
            let mut file = original.clone();
            file.path = root.join(name);
            file.stamp.identity.1 = size as u128 + u128::from(name == "last-small") * 100;
            file.stamp.size = size;
            collector.push(file);
        }
        assert_eq!(collector.files.len(), 2);
        assert!(collector.limited);
        assert!(collector.paths.contains(&root.join("later-big")));
        assert!(collector.memory <= collector.budget);
        let cost = candidate_cost(&original.path, &original.parent);
        let mut collector = RecommendedCollector::new(cost * 2, 2);
        let mut small = original.clone();
        small.path = root.join("small.tmp");
        small.stamp.size = 1;
        small.stamp.identity.1 = 1;
        collector.push(small);
        let mut big = original.clone();
        big.path = root.join("big.tmp");
        big.stamp.size = 1000;
        big.stamp.identity.1 = 2;
        collector.push(big);
        let mut intermediate = original;
        intermediate.stamp.identity.1 = 3;
        intermediate.stamp.size = 500;
        // This entry needs both old entries' memory, but the larger old entry is preferable.
        let mut attempted = false;
        for len in 1..2000 {
            intermediate.path = root.join("x".repeat(len));
            let needed = candidate_cost(&intermediate.path, &intermediate.parent);
            if needed <= collector.budget
                && collector.memory + needed > collector.budget
                && needed
                    + candidate_cost(
                        &collector.files.last_key_value().unwrap().1.path,
                        &intermediate.parent,
                    )
                    > collector.budget
            {
                attempted = true;
                collector.push(intermediate);
                break;
            }
        }
        assert!(attempted);
        assert_eq!(collector.files.len(), 2);
        assert!(
            collector.paths.contains(&root.join("small.tmp"))
                && collector.paths.contains(&root.join("big.tmp"))
        );
    }

    #[test]
    fn full_walk_still_blocks_running_apps_and_source_disappearance() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let context = rule_context(root);
        let cache = rule_file(root, "user/AppData/Roaming/Code/GPUCache/cache", 2);
        let source = rule_file(root, "D/project/module.py", 0);
        let bytecode = rule_file(root, "D/project/__pycache__/module.cpython-313.pyc", 8);
        let (plan, preview) = scan_rules(root, &context, |_| Ok(true));
        assert!(plan.recommended.is_none());
        assert!(preview
            .recommended
            .unwrap()
            .blocked_apps
            .contains(&"Visual Studio Code".into()));
        let (mut plan, _) = scan_rules(root, &context, |_| Ok(false));
        plan.candidates = plan.recommended.take().unwrap();
        std::fs::remove_file(source).unwrap();
        let data = tempfile::tempdir().unwrap();
        let ids = selected(&plan);
        let result = execute_files_with_logger_and_running(
            &plan,
            &ids,
            data.path(),
            crate::direct_delete::delete,
            log_line,
            |name| Ok(name == "Code.exe"),
        )
        .unwrap();
        assert_eq!(result.deleted_files, 0);
        assert_eq!(result.skipped_files, 2);
        assert!(cache.exists() && bytecode.exists());
        let (plan, _) = scan_rules(root, &context, |_| Err("process enumeration failed".into()));
        assert!(plan.recommended.is_none());
    }
    #[test]
    fn full_rules_limits_do_not_stop_walk_or_include_unselected_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let context = rule_context(root);
        rule_file(root, "D/Temp/old.tmp", 40);
        rule_file(root, "E/Temp/old.tmp", 40);
        let (plan, preview) = scan_roots_with_rules(
            &[("selected-D".into(), root.join("D"))],
            "scope",
            &AtomicBool::new(false),
            Limits {
                memory: 1,
                ..Limits::default()
            },
            &context,
            |_| Ok(false),
            |_, _| {},
        );
        assert!(plan.unwrap().recommended.is_none());
        assert!(preview.stats.limited);
        assert_eq!(preview.stats.scanned_files, 1);
        let (plan, _) = scan_roots_with_rules(
            &[("selected-D".into(), root.join("D"))],
            "scope",
            &AtomicBool::new(false),
            Limits::default(),
            &context,
            |_| Ok(false),
            |_, _| {},
        );
        let files = plan.unwrap().recommended.unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].path.starts_with(root.join("D")));
    }
    #[test]
    fn unified_recommendations_deduplicate_identity_and_keep_multiple_batches() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let context = rule_context(root);
        rule_file(root, "D/Temp/old.tmp", 40);
        let (mut plan, _) = scan_rules(root, &context, |_| Ok(false));
        let file = plan.recommended.take().unwrap().pop().unwrap();
        let mut files = vec![file.clone(); MAX_ACTIONS + 2];
        for (index, file) in files.iter_mut().enumerate() {
            file.path = root.join(format!("D/Temp/file-{index}.tmp"));
            file.stamp.identity.1 = index as u128 + 1;
        }
        let mut alias = files[0].clone();
        alias.path = root.join("D/Temp/same-file-other-case.tmp");
        files.push(alias);
        files.push(files[0].clone());
        let mut info = crate::cache_cleaner::RecommendedPreview::default();
        finish_recommendations(&mut plan, &mut info, files);
        assert_eq!(info.file_count, (MAX_ACTIONS + 2) as u64);
        assert!(!info.limited);
        assert_eq!(plan.recommended.unwrap().len(), MAX_ACTIONS + 2);
    }

    #[test]
    fn recommended_capability_is_bound_to_scan_and_consumed_exactly_once() {
        let tmp = tempfile::tempdir().unwrap();
        old_file(
            &tmp.path()
                .join("Google/Chrome/User Data/Default/GPUCache/owned"),
            8,
        );
        let (mut plan, mut preview) = scan(tmp.path());
        let roots = [tmp.path().to_path_buf()];
        let cancel = AtomicBool::new(false);
        let (quick, info) = crate::cache_cleaner::prepare_recommended_with_running(
            tmp.path(),
            None,
            &roots,
            &cancel,
            |_| Ok(false),
        );
        assert!(info.file_count > 0);
        plan.recommended = quick.map(|cache| {
            cache
                .into_targets()
                .into_iter()
                .filter_map(|target| {
                    cache_candidate(
                        target,
                        &[("fixture".into(), tmp.path().into())],
                        &mut HashMap::new(),
                    )
                })
                .collect()
        });
        preview.recommended = Some(info);
        let id = plan.id.clone();
        let state = DiskCleanerState::default();
        {
            let mut inner = state.0.lock().unwrap();
            inner.snapshot = Some(plan);
            inner.session = Some(DiskScanSession {
                scan_mode: "full",
                scan_id: "owned-scan".into(),
                drive_ids: vec![],
                status: "completed",
                progress: None,
                preview: Some(Arc::new(preview)),
                error: None,
            });
        }
        let restored = session_snapshot(&state).unwrap().unwrap();
        assert!(
            restored
                .preview
                .unwrap()
                .recommended
                .as_ref()
                .unwrap()
                .file_count
                > 0
        );
        assert!(take_recommended(&state, "wrong-plan").is_err());
        assert!(state.0.lock().unwrap().snapshot.is_some());
        assert!(take_recommended(&state, &id).is_ok());
        assert!(take_recommended(&state, &id).is_err());
        assert!(state.0.lock().unwrap().snapshot.is_none());
        assert_eq!(session_snapshot(&state).unwrap().unwrap().status, "failed");
    }

    #[test]
    fn expired_recommended_capability_is_rejected_without_deleting_files() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp
            .path()
            .join("Google/Chrome/User Data/Default/GPUCache/owned");
        old_file(&file, 8);
        let (mut plan, _) = scan(tmp.path());
        plan.recommended = crate::cache_cleaner::prepare_recommended_with_running(
            tmp.path(),
            None,
            &[tmp.path().to_path_buf()],
            &AtomicBool::new(false),
            |_| Ok(false),
        )
        .0
        .map(|cache| {
            cache
                .into_targets()
                .into_iter()
                .filter_map(|target| {
                    cache_candidate(
                        target,
                        &[("fixture".into(), tmp.path().into())],
                        &mut HashMap::new(),
                    )
                })
                .collect()
        });
        plan.created = SystemTime::now() - Duration::from_secs(901);
        let id = plan.id.clone();
        let state = DiskCleanerState::default();
        state.0.lock().unwrap().snapshot = Some(plan);
        assert!(take_recommended(&state, &id).is_err());
        assert!(file.exists());
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
    fn protected_environments_are_counted_readonly_and_user_subtrees_remain_actionable() {
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
        assert_eq!(preview.stats.scanned_files, 12);
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
        assert!(take_snapshot(&state, &id, std::slice::from_ref(&file)).is_err());
        let (plan, _) = scan(tmp.path());
        let id = plan.id.clone();
        state.0.lock().unwrap().snapshot = Some(plan);
        assert!(take_snapshot(&state, "corrupt", std::slice::from_ref(&file)).is_err());
        assert!(take_snapshot(&state, &id, &["other-file".into()]).is_err());
        assert!(take_snapshot(&state, &id, &[file.clone(), file.clone()]).is_err());
        assert!(take_snapshot(&state, &id, std::slice::from_ref(&file)).is_ok());
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
        assert!(plan.candidates[0].parent.scan_chain_intact());
        fs::rename(&nested, tmp.path().join("old-folder")).unwrap();
        old_file(&nested.join("a.tmp"), 8);
        assert!(!plan.candidates[0].parent.scan_chain_intact());
        assert!(!valid_candidate(&plan.candidates[0]));
    }
    #[test]
    fn durable_pending_precedes_delete_and_exclusive_handle_blocks_mutation() {
        let tmp = tempfile::tempdir().unwrap();
        let logs = tempfile::tempdir().unwrap();
        old_file(&tmp.path().join("a.tmp"), 8);
        let (plan, _) = scan(tmp.path());
        let outcome = execute_files_with_logger(
            &plan,
            &selected(&plan),
            logs.path(),
            crate::direct_delete::delete,
            |file, id, path, status| {
                log_line(file, id, path, status)?;
                if status == "pending" {
                    assert!(fs::remove_file(path).is_err());
                    assert!(fs::write(path, b"changed during pending").is_err());
                }
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(outcome.deleted_files, 1);
        assert_eq!(outcome.skipped_files, 0);
        let log = fs::read_to_string(&outcome.log_path).unwrap();
        assert!(log.contains("pending") && log.contains("success"));
        assert!(!tmp.path().join("a.tmp").exists());
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
            |_, _| panic!("must not recycle"),
            |_, _, _, _| Err("injected".into()),
        )
        .unwrap();
        assert_eq!(outcome.deleted_files, 0);
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
        assert!(
            execute_recorded(plan, &ids, logs.path(), |_, _| panic!("must not recycle")).is_err()
        );
    }
    #[test]
    fn history_finish_failure_preserves_real_action_statistics() {
        let tmp = tempfile::tempdir().unwrap();
        let logs = tempfile::tempdir().unwrap();
        old_file(&tmp.path().join("a.tmp"), 8);
        let (plan, _) = scan(tmp.path());
        let ids: Vec<_> = plan.candidates.iter().map(|c| c.id.clone()).collect();
        let outcome = execute_recorded(plan, &ids, logs.path(), |path, _handle| {
            let record = fs::read_dir(logs.path().join("cleanup-history-v1"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            fs::write(record.join("finish.json"), b"injected conflict").unwrap();
            crate::direct_delete::delete(path, _handle).unwrap();
            Ok(())
        })
        .unwrap();
        assert_eq!(outcome.deleted_files, 1);
        assert_eq!(outcome.deleted_bytes, 12);
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
        let outcome = execute_files(&plan, &selected(&plan), logs.path(), |path, _handle| {
            count += 1;
            if count == 1 {
                Err("in use".into())
            } else {
                crate::direct_delete::delete(path, _handle).unwrap();
                Ok(())
            }
        })
        .unwrap();
        assert_eq!(outcome.failed_files, 1);
        assert_eq!(outcome.deleted_files, 1);
    }
    #[test]
    fn recycler_ok_without_source_disappearance_is_uncertain_not_success() {
        let tmp = tempfile::tempdir().unwrap();
        let logs = tempfile::tempdir().unwrap();
        old_file(&tmp.path().join("a.tmp"), 8);
        let (plan, _) = scan(tmp.path());
        let outcome = execute_files(&plan, &selected(&plan), logs.path(), |_, _| Ok(())).unwrap();
        assert_eq!(outcome.deleted_files, 0);
        assert_eq!(outcome.failed_files, 1);
        assert!(fs::read_to_string(outcome.log_path)
            .unwrap()
            .contains("uncertain"));
    }
    #[cfg(windows)]
    #[test]
    fn real_windows_direct_delete_fixture_smoke() {
        let tmp = tempfile::tempdir().unwrap();
        let logs = tempfile::tempdir().unwrap();
        let path = tmp.path().join("Pinkbin owned fixture 🌸.tmp");
        let neighbor = tmp.path().join("keep.txt");
        old_file(&path, 8);
        fs::write(&neighbor, b"keep").unwrap();
        let (plan, _) = scan(tmp.path());
        let ids: Vec<_> = plan.candidates.iter().map(|c| c.id.clone()).collect();
        let outcome =
            execute_recorded(plan, &ids, logs.path(), crate::direct_delete::delete).unwrap();
        assert_eq!(outcome.deleted_files, 1, "{:?}", outcome.messages);
        assert!(!path.exists());
        assert_eq!(fs::read(&neighbor).unwrap(), b"keep");
        let history = crate::cleanup_history::read_history(logs.path()).unwrap();
        assert!(history.records[0].permanent_delete);
        assert_eq!(history.records[0].deleted_files, 1);
        assert!(!trash::os_limited::list()
            .unwrap()
            .iter()
            .any(|item| item.original_path() == path));
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
            execute_recorded(plan, &ids, logs.path(), crate::direct_delete::delete).unwrap();
        assert_eq!(
            outcome.deleted_files + outcome.failed_files + outcome.skipped_files,
            1
        );
        if outcome.deleted_files == 1 {
            assert!(!path.exists());
        } else {
            assert_eq!(outcome.deleted_files, 0);
        }
        assert!(!trash::os_limited::list()
            .unwrap()
            .iter()
            .any(|item| item.original_path() == path));
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
    #[test]
    #[ignore = "只读检查本机归档日志，需 PINKBIN_DIAGNOSTICS_AUDIT_OUTPUT"]
    fn audit_current_diagnostics_readonly() {
        let output = PathBuf::from(
            std::env::var_os("PINKBIN_DIAGNOSTICS_AUDIT_OUTPUT")
                .expect("explicit read-only audit output required"),
        );
        assert!(output.is_absolute());
        let context = crate::full_cleanup_rules::Context::current();
        let mut roots = vec![];
        for profile in context
            .catalog
            .profiles
            .iter()
            .filter(|p| p.family == "diagnostics")
        {
            for location in &profile.locations {
                let base = if location.root == "local" {
                    &context.local
                } else {
                    &context.roaming
                };
                if let Some(base) = base {
                    let root = base.join(&location.path);
                    for part in if profile.id == "unreal-crash-logs" {
                        vec!["Saved/Logs"]
                    } else if profile.id == "obs-logs" {
                        vec!["logs", "crashes"]
                    } else {
                        vec!["Logs"]
                    } {
                        let path = root.join(part);
                        if path.is_dir() {
                            roots.push((profile.id.clone(), path));
                        }
                    }
                }
            }
        }
        let (_, preview) = scan_roots_with_rules(
            &roots,
            "diagnostics-audit",
            &AtomicBool::new(false),
            Limits::default(),
            &context,
            crate::cache_cleaner::browser_running,
            |_, _| {},
        );
        let value = serde_json::json!({"mode":"read_only_targeted", "deleted_files":0, "roots":roots, "stats":preview.stats, "recommended":preview.recommended, "warnings":preview.warnings});
        let mut file = File::options()
            .create_new(true)
            .write(true)
            .open(output)
            .unwrap();
        use std::io::Write;
        file.write_all(serde_json::to_string_pretty(&value).unwrap().as_bytes())
            .unwrap();
    }
}
