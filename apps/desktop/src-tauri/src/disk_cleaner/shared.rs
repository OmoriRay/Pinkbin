use super::*;
use crate::shared_scan::{Inventory, ResultView, SharedProgress};
use pinkbin_scaffold::CompiledScaffold;
use pinkbin_scanner::Node;

pub(super) struct SharedInventory {
    roots: Vec<PathBuf>,
    snapshot: Snapshot,
    preview: Preview,
    trees: HashMap<PathBuf, Arc<Node>>,
    per_root: HashMap<PathBuf, Stats>,
    blocked: HashMap<PathBuf, Vec<String>>,
    last_scanned: Vec<PathBuf>,
    captures: HashMap<PathBuf, String>,
}
impl Inventory for SharedInventory {
    fn roots(&self) -> &[PathBuf] {
        &self.roots
    }
    fn fresh(&self) -> bool {
        self.snapshot.created.elapsed().is_ok_and(|v| v <= TTL)
    }
}
fn within(path: &Path, roots: &[PathBuf]) -> bool {
    roots
        .iter()
        .any(|r| crate::rule_catalog::beneath(path, r).is_some())
}
fn combine_stats(left: &mut Stats, right: &Stats) {
    left.scanned_files = left.scanned_files.saturating_add(right.scanned_files);
    left.scanned_bytes = left.scanned_bytes.saturating_add(right.scanned_bytes);
    left.scanned_directories += right.scanned_directories;
    left.denied_directories += right.denied_directories;
    left.protected_directories += right.protected_directories;
    left.linked_entries += right.linked_entries;
    left.limited |= right.limited;
    left.cancelled |= right.cancelled;
    left.rule_reuse.marker_cache_hits += right.rule_reuse.marker_cache_hits;
    left.rule_reuse.directory_cache_hits += right.rule_reuse.directory_cache_hits;
    left.rule_reuse.filesystem_queries += right.rule_reuse.filesystem_queries;
    left.rule_reuse.memory_bytes = left
        .rule_reuse
        .memory_bytes
        .max(right.rule_reuse.memory_bytes);
}
fn manual_view(snapshot: &mut Snapshot, preview: &mut Preview) {
    for group in &mut preview.groups {
        group.entries.clear();
        group.file_count = 0;
        group.bytes = 0;
    }
    let mut kept = Vec::new();
    let mut memory = 0usize;
    for mut candidate in snapshot.candidates.drain(..) {
        let Some(group) = preview
            .groups
            .iter_mut()
            .find(|g| g.id == candidate.category)
        else {
            continue;
        };
        let cost = candidate_cost(&candidate.path, &candidate.parent);
        if kept.len() >= CANDIDATE_LIMIT
            || group.entries.len() >= PER_GROUP_LIMIT
            || memory.saturating_add(cost) > CANDIDATE_MEMORY_LIMIT
        {
            preview.stats.limited = true;
            continue;
        }
        memory += cost;
        candidate.id = format!("file-{}", kept.len());
        group.file_count += 1;
        group.bytes += candidate.stamp.size;
        group.entries.push(Entry {
            id: candidate.id.clone(),
            category: group.id,
            title: candidate
                .path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into(),
            risk: "review",
            size: candidate.stamp.size,
            path: candidate.path.to_string_lossy().into(),
            modified: chrono::DateTime::<chrono::Utc>::from(candidate.stamp.modified).to_rfc3339(),
            default_selected: false,
        });
        kept.push(candidate);
    }
    snapshot.candidates = kept;
}
fn merge(
    seed: Option<Arc<SharedInventory>>,
    mut snapshot: Snapshot,
    mut preview: Preview,
    mut trees: HashMap<PathBuf, Arc<Node>>,
    mut per_root: HashMap<PathBuf, Stats>,
    mut blocked: HashMap<PathBuf, Vec<String>>,
    new_roots: Vec<PathBuf>,
) -> SharedInventory {
    let mut roots = new_roots.clone();
    let mut captures = new_roots
        .iter()
        .map(|r| (r.clone(), format!("{}:{}", snapshot.id, r.display())))
        .collect::<HashMap<_, _>>();
    if let Some(seed) = seed {
        let mut candidates = seed.snapshot.candidates.clone();
        candidates.append(&mut snapshot.candidates);
        snapshot.candidates = candidates;
        snapshot.created = snapshot.created.min(seed.snapshot.created);
        let mut info = preview.recommended.take().unwrap_or_default();
        let mut recommendations = seed.snapshot.recommended.clone().unwrap_or_default();
        recommendations.extend(snapshot.recommended.take().unwrap_or_default());
        info.limited |= seed.preview.recommended.as_ref().is_some_and(|p| p.limited);
        info.blocked_apps.extend(
            seed.preview
                .recommended
                .as_ref()
                .map_or(vec![], |p| p.blocked_apps.clone()),
        );
        finish_recommendations(&mut snapshot, &mut info, recommendations);
        preview.recommended = Some(info);
        combine_stats(&mut preview.stats, &seed.preview.stats);
        preview.overview.extend(seed.preview.overview.clone());
        preview.overview.sort_by_key(|v| std::cmp::Reverse(v.bytes));
        preview.overview.truncate(15);
        preview
            .unclassified_caches
            .extend(seed.preview.unclassified_caches.clone());
        preview
            .unclassified_caches
            .sort_by_key(|v| std::cmp::Reverse(v.bytes));
        preview.unclassified_caches.truncate(8);
        preview.warnings.extend(seed.preview.warnings.clone());
        preview.warnings.sort();
        preview.warnings.dedup();
        trees.extend(seed.trees.iter().map(|(p, n)| (p.clone(), n.clone())));
        per_root.extend(seed.per_root.clone());
        blocked.extend(seed.blocked.clone());
        captures.extend(seed.captures.clone());
        roots.extend(seed.roots.clone());
        roots.sort();
        roots.dedup();
    }
    manual_view(&mut snapshot, &mut preview);
    SharedInventory {
        roots,
        snapshot,
        preview,
        trees,
        per_root,
        blocked,
        last_scanned: new_roots,
        captures,
    }
}
fn build_inventory(
    app: Option<&tauri::AppHandle>,
    roots: &[PathBuf],
    seed: Option<Arc<SharedInventory>>,
    cancel: &AtomicBool,
    compiled: Arc<Vec<CompiledScaffold>>,
    report: &mut dyn FnMut(SharedProgress),
) -> Result<SharedInventory, String> {
    crate::rule_catalog::ensure_ready()?;
    let _lease = reserve_operation()?;
    let started_at = SystemTime::now();
    let indexed = roots
        .iter()
        .map(|p| (p.to_string_lossy().into_owned(), p.clone()))
        .collect::<Vec<_>>();
    let scan_id = format!("inventory-{}", SEQUENCE.fetch_add(1, Ordering::Relaxed));
    let mut sink = crate::shared_tree::TreeSink::new(compiled);
    let mut per_root = HashMap::new();
    let mut previous = Stats::default();
    let mut root_base = Stats::default();
    let mut current = String::new();
    let (mut snapshot, mut preview) = scan_roots_observed(
        &indexed,
        &scan_id,
        cancel,
        Limits::default(),
        &crate::full_cleanup_rules::Context::current(),
        crate::cache_cleaner::browser_running,
        |root, stats| {
            if current != root {
                root_base = previous.clone();
                current = root.into();
            }
            let delta = Stats {
                scanned_files: stats.scanned_files - root_base.scanned_files,
                scanned_bytes: stats.scanned_bytes - root_base.scanned_bytes,
                scanned_directories: stats.scanned_directories - root_base.scanned_directories,
                denied_directories: stats.denied_directories - root_base.denied_directories,
                protected_directories: stats.protected_directories
                    - root_base.protected_directories,
                linked_entries: stats.linked_entries - root_base.linked_entries,
                ..Default::default()
            };
            per_root.insert(PathBuf::from(root), delta.clone());
            previous = stats.clone();
            report(SharedProgress {
                phase: "files",
                root: root.into(),
                files: delta.scanned_files,
                bytes: delta.scanned_bytes,
                directories: delta.scanned_directories,
                denied: delta.denied_directories,
                protected: delta.protected_directories,
                linked: delta.linked_entries,
            });
        },
        Some(&mut sink),
    );
    if cancel.load(Ordering::Acquire) {
        return Err("检查已取消".into());
    }
    if roots.iter().any(|r| !sink.trees.contains_key(r)) {
        return Err("部分磁盘未完整检查，请重新检查".into());
    }
    if let Some(app) = app {
        report(SharedProgress {
            phase: "recommendations",
            root: roots
                .last()
                .map_or(String::new(), |p| p.to_string_lossy().into()),
            ..Default::default()
        });
        attach_recommended_inner(app, &mut snapshot, &mut preview, &indexed, cancel, false);
    }
    if cancel.load(Ordering::Acquire) {
        return Err("检查已取消".into());
    }
    if let Some(app) = app {
        if quick_locations::save(app, &preview.learned_locations, cancel).is_err() {
            preview
                .warnings
                .push("已完成检查，但未能保存已知位置；下次可使用全盘检查重新发现。".into());
        }
    }
    let mut snapshot = snapshot.ok_or("检查没有生成有效计划")?;
    snapshot.created = snapshot.created.min(started_at);
    preview.created_at = chrono::DateTime::<chrono::Utc>::from(snapshot.created).to_rfc3339();
    let mut blocked = sink.blocked;
    if let Some(base) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
        if let Some(root) = roots
            .iter()
            .find(|r| crate::rule_catalog::beneath(&base, r).is_some())
        {
            blocked.entry(root.clone()).or_default().extend(
                preview
                    .recommended
                    .as_ref()
                    .map_or(vec![], |p| p.blocked_apps.clone()),
            );
        }
    }
    Ok(merge(
        seed,
        snapshot,
        preview,
        sink.trees,
        per_root,
        blocked,
        roots.into(),
    ))
}
fn compiled(app: &tauri::AppHandle) -> Result<Arc<Vec<CompiledScaffold>>, String> {
    let state = app.state::<crate::AppState>();
    let data = state.scaffolds.lock().map_err(text_error)?;
    Ok(Arc::new(pinkbin_scaffold::compile_all(&data)))
}
fn projection(
    view: &ResultView<SharedInventory>,
    roots: &[PathBuf],
    id: &str,
) -> (Snapshot, Preview) {
    let data = &view.data;
    let mut snapshot = data.snapshot.clone();
    snapshot.id = token();
    snapshot.candidates.retain(|c| within(&c.path, roots));
    let recommended = snapshot
        .recommended
        .take()
        .unwrap_or_default()
        .into_iter()
        .filter(|c| within(&c.path, roots))
        .collect::<Vec<_>>();
    let mut preview = data.preview.clone();
    preview.created_at = chrono::DateTime::<chrono::Utc>::from(snapshot.created).to_rfc3339();
    preview.plan_id = snapshot.id.clone();
    preview.scan_id = id.into();
    preview.shared_reused = view.reused || roots.iter().any(|r| !data.last_scanned.contains(r));
    preview.stats = Stats {
        limited: data.preview.stats.limited,
        ..Default::default()
    };
    for root in roots {
        if let Some(stats) = data.per_root.get(root) {
            combine_stats(&mut preview.stats, stats)
        }
    }
    preview
        .overview
        .retain(|v| within(Path::new(&v.path), roots));
    preview
        .unclassified_caches
        .retain(|v| within(Path::new(&v.path), roots));
    manual_view(&mut snapshot, &mut preview);
    let mut info = crate::cache_cleaner::RecommendedPreview {
        limited: data.preview.recommended.as_ref().is_some_and(|p| p.limited),
        ..Default::default()
    };
    for root in roots {
        if let Some(names) = data.blocked.get(root) {
            info.blocked_apps.extend(names.clone())
        }
    }
    finish_recommendations(&mut snapshot, &mut info, recommended);
    preview.recommended = Some(info);
    (snapshot, preview)
}
pub(super) fn run_disk(
    app: &tauri::AppHandle,
    state: &DiskCleanerState,
    roots: &[PathBuf],
    id: &str,
    cancel: Arc<AtomicBool>,
    force: bool,
) -> Result<(Snapshot, Preview, u64), String> {
    let mut partial = if force {
        HashMap::new()
    } else {
        state
            .1
            .peek()
            .map_or(HashMap::new(), |v| v.data.per_root.clone())
    };
    let view = state.1.ensure(
        roots,
        force,
        cancel,
        |p| {
            if p.phase == "files" && roots.iter().any(|r| r == Path::new(&p.root)) {
                partial.insert(
                    PathBuf::from(&p.root),
                    Stats {
                        scanned_files: p.files,
                        scanned_bytes: p.bytes,
                        scanned_directories: p.directories,
                        denied_directories: p.denied,
                        protected_directories: p.protected,
                        linked_entries: p.linked,
                        ..Default::default()
                    },
                );
            }
            let mut stats = Stats::default();
            for root in roots {
                if let Some(s) = partial.get(root) {
                    combine_stats(&mut stats, s)
                }
            }
            let mut event = progress(id, &p.root, &stats);
            event.phase = if p.phase == "recommendations" {
                "recommendations"
            } else {
                "files"
            };
            if let Ok(mut inner) = state.0.lock() {
                if let Some(s) = inner.session.as_mut().filter(|s| s.scan_id == id) {
                    s.progress = Some(event.clone())
                }
            }
            let _ = app.emit("disk-cleanup-progress", event);
        },
        |missing, seed, physical, report| {
            if force {
                let _ = app.emit("shared-scan-invalidated", state.1.epoch());
            }
            build_inventory(Some(app), missing, seed, physical, compiled(app)?, report)
        },
    )?;
    let (snapshot, preview) = projection(&view, roots, id);
    Ok((snapshot, preview, view.epoch))
}
pub(crate) struct SharedHome {
    pub node: Arc<Node>,
    pub epoch: u64,
    pub reused: bool,
    pub elapsed_ms: u64,
    pub result_key: String,
}
pub(crate) fn home_scan(
    app: &tauri::AppHandle,
    state: &DiskCleanerState,
    path: &Path,
    force: bool,
    mut report: impl FnMut(SharedProgress),
) -> Result<Option<SharedHome>, String> {
    let started = Instant::now();
    fn find(node: &Node, path: &Path) -> Option<Node> {
        if crate::rule_catalog::beneath(path, Path::new(&node.path))
            .is_some_and(|p| p.as_os_str().is_empty())
        {
            return Some(node.clone());
        }
        for child in &node.children {
            if child.is_dir {
                if let Some(found) = find(child, path) {
                    return Some(found);
                }
            }
        }
        None
    }
    if !force {
        if let Some(view) = state.1.peek() {
            for (root, node) in &view.data.trees {
                let found = if crate::rule_catalog::beneath(path, root)
                    .is_some_and(|r| r.as_os_str().is_empty())
                {
                    Some(node.clone())
                } else {
                    find(node, path).map(Arc::new)
                };
                if let Some(node) = found {
                    return Ok(Some(SharedHome {
                        node,
                        epoch: view.epoch,
                        reused: true,
                        elapsed_ms: started.elapsed().as_millis() as u64,
                        result_key: format!(
                            "{}:{}",
                            view.data
                                .captures
                                .get(root)
                                .unwrap_or(&view.data.snapshot.id),
                            path.display()
                        ),
                    }));
                }
            }
        }
    }
    let root = fixed_drives()?.into_iter().find(|d| {
        crate::rule_catalog::beneath(path, Path::new(&d.id))
            .is_some_and(|r| r.as_os_str().is_empty())
    });
    let Some(root) = root else { return Ok(None) };
    let root = PathBuf::from(root.id);
    let roots = vec![root.clone()];
    let mut last_home = SharedProgress {
        root: path.to_string_lossy().into(),
        ..Default::default()
    };
    let view = state.1.ensure(
        &roots,
        force,
        Arc::new(AtomicBool::new(false)),
        |p| {
            if Path::new(&p.root) == root {
                last_home = p;
            } else if last_home.files == 0 {
                last_home.root = "等待共享检查完成".into();
            }
            report(last_home.clone());
        },
        |missing, seed, cancel, progress| {
            if force {
                let _ = app.emit("shared-scan-invalidated", state.1.epoch());
            }
            build_inventory(Some(app), missing, seed, cancel, compiled(app)?, progress)
        },
    )?;
    let node = view
        .data
        .trees
        .get(&root)
        .cloned()
        .ok_or("共享目录结果不可用，请重新扫描")?;
    Ok(Some(SharedHome {
        node,
        epoch: view.epoch,
        reused: view.reused,
        elapsed_ms: started.elapsed().as_millis() as u64,
        result_key: format!(
            "{}:{}",
            view.data
                .captures
                .get(&root)
                .unwrap_or(&view.data.snapshot.id),
            path.display()
        ),
    }))
}
pub(crate) fn epoch(state: &DiskCleanerState) -> u64 {
    state.1.epoch()
}
pub(crate) fn invalidate(app: &tauri::AppHandle) {
    let state = app.state::<DiskCleanerState>();
    state.1.invalidate();
    if let Ok(mut inner) = state.0.lock() {
        inner.snapshot = None;
        inner.snapshot_epoch = None;
        if inner
            .session
            .as_ref()
            .is_some_and(|s| s.status == "completed")
        {
            invalidate_session(&mut inner, "文件列表可能已变化，请重新检查")
        }
    }
    let _ = app.emit("shared-scan-invalidated", state.1.epoch());
}

pub(super) fn audit() -> Result<serde_json::Value, String> {
    crate::rule_catalog::ensure_ready()?;
    let drives = fixed_drives()?;
    let roots = drives
        .iter()
        .map(|d| PathBuf::from(&d.id))
        .collect::<Vec<_>>();
    if roots.is_empty() {
        return Err("没有可扫描的磁盘".into());
    }
    let coordinator = crate::shared_scan::Coordinator::default();
    let started = Instant::now();
    let cancel = Arc::new(AtomicBool::new(false));
    let parsed = crate::EMBEDDED_SCAFFOLDS
        .files()
        .filter_map(|f| f.contents_utf8())
        .filter_map(|s| pinkbin_scaffold::parse_toml(s).ok())
        .collect::<Vec<_>>();
    let compiled = Arc::new(pinkbin_scaffold::compile_all(&parsed));
    let mut last = Instant::now();
    let mut build = |missing: &[PathBuf],
                     seed,
                     physical: &AtomicBool,
                     report: &mut dyn FnMut(SharedProgress)| {
        build_inventory(None, missing, seed, physical, compiled.clone(), report)
    };
    let first = coordinator.ensure(
        &roots[..1],
        false,
        cancel.clone(),
        |p| {
            if last.elapsed() >= Duration::from_secs(5) {
                eprintln!("{} files={} bytes={}", p.root, p.files, p.bytes);
                last = Instant::now();
            }
        },
        &mut build,
    )?;
    let first_runs = coordinator.runs();
    let home = coordinator.ensure(&roots[..1], false, cancel.clone(), |_| {}, &mut build)?;
    if coordinator.runs() != first_runs || !home.reused {
        return Err("主页共享复核失败".into());
    }
    let all = coordinator.ensure(
        &roots,
        false,
        cancel.clone(),
        |p| {
            if last.elapsed() >= Duration::from_secs(5) {
                eprintln!("{} files={} bytes={}", p.root, p.files, p.bytes);
                last = Instant::now();
            }
        },
        &mut build,
    )?;
    let completed_runs = coordinator.runs();
    let again = coordinator.ensure(&roots, false, cancel, |_| {}, &mut build)?;
    let (scope, preview) = projection(&again, &roots[..1], "audit-scope");
    if scope
        .candidates
        .iter()
        .chain(scope.recommended.as_deref().unwrap_or_default())
        .any(|c| !within(&c.path, &roots[..1]))
    {
        return Err("共享范围复核失败".into());
    }
    let data = &all.data;
    Ok(
        serde_json::json!({"mode":"read_only","deleted_files":0,"elapsed_seconds":started.elapsed().as_secs_f64(),"drives":drives,"stats":data.preview.stats,"recommended":data.preview.recommended,"overview":data.preview.overview,"unclassified_caches":data.preview.unclassified_caches,"overview_limited":data.preview.overview_limited,"warnings":data.preview.warnings,
    "sharing":{"first_scope":roots[0],"first_scope_runs":first_runs,"all_scope_runs":completed_runs,"after_reuse_runs":coordinator.runs(),"home_reused":home.reused,"cleanup_reused":again.reused,"scope_filter_files":scope.candidates.len(),"scope_preview_files":preview.stats.scanned_files,"root_trees":data.trees.iter().map(|(p,n)|serde_json::json!({"path":p,"files":n.file_count,"bytes":n.size})).collect::<Vec<_>>(),"first_tree_bytes":first.data.trees.get(&roots[0]).map(|n|n.size)}}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared_scan::Coordinator;

    #[test]
    fn reused_scan_request_id_never_reissues_the_same_cleanup_capability() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        old_file(&root.join("Temp/owned.tmp"));
        let view = ResultView {
            data: Arc::new(inventory(std::slice::from_ref(&root), None)),
            epoch: 0,
            reused: true,
        };
        let (first, _) = projection(&view, std::slice::from_ref(&root), "same-request");
        let (second, preview) = projection(&view, std::slice::from_ref(&root), "same-request");
        assert_ne!(first.id, second.id);
        assert_eq!(preview.scan_id, "same-request");
        assert_eq!(preview.plan_id, second.id);
        let state = DiskCleanerState::default();
        state.0.lock().unwrap().snapshot = Some(second);
        assert!(take_recommended(&state, &first.id).is_err());
    }
    fn old_file(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"owned fixture").unwrap();
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(
                fs::FileTimes::new()
                    .set_modified(SystemTime::now() - Duration::from_secs(60 * 86400)),
            )
            .unwrap();
    }
    fn inventory(roots: &[PathBuf], seed: Option<Arc<SharedInventory>>) -> SharedInventory {
        let mut rules = crate::full_cleanup_rules::Context::current();
        rules.temp = roots.iter().map(|r| r.join("Temp")).collect();
        let indexed = roots
            .iter()
            .map(|r| (r.to_string_lossy().into_owned(), r.clone()))
            .collect::<Vec<_>>();
        let mut sink = crate::shared_tree::TreeSink::new(Arc::new(vec![]));
        let mut per_root = HashMap::new();
        let mut previous = Stats::default();
        let (snapshot, preview) = scan_roots_observed(
            &indexed,
            "fixture",
            &AtomicBool::new(false),
            Limits::default(),
            &rules,
            |_| Ok(false),
            |root, stats| {
                per_root.insert(
                    PathBuf::from(root),
                    Stats {
                        scanned_files: stats.scanned_files - previous.scanned_files,
                        scanned_bytes: stats.scanned_bytes - previous.scanned_bytes,
                        ..Default::default()
                    },
                );
                previous = stats.clone();
            },
            Some(&mut sink),
        );
        merge(
            seed,
            snapshot.unwrap(),
            preview,
            sink.trees,
            per_root,
            sink.blocked,
            roots.into(),
        )
    }
    #[test]
    fn real_metadata_walk_builds_tree_and_plan_together_and_only_supplements_missing_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = [
            tmp.path().join("C"),
            tmp.path().join("D"),
            tmp.path().join("E"),
        ];
        for root in &paths {
            old_file(&root.join("Temp/owned.tmp"));
            old_file(&root.join("Docs/keep.txt"));
        }
        let c = Coordinator::default();
        let mut traversed = vec![];
        let mut build =
            |roots: &[PathBuf], seed, _: &AtomicBool, _: &mut dyn FnMut(SharedProgress)| {
                traversed.extend_from_slice(roots);
                Ok(inventory(roots, seed))
            };
        let first = c
            .ensure(
                &paths[..1],
                false,
                Arc::new(AtomicBool::new(false)),
                |_| {},
                &mut build,
            )
            .unwrap();
        assert_eq!(first.data.trees[&paths[0]].file_count, 2);
        assert_eq!(first.data.trees[&paths[0]].children.len(), 2);
        let reused = c
            .ensure(
                &paths[..1],
                false,
                Arc::new(AtomicBool::new(false)),
                |_| {},
                &mut build,
            )
            .unwrap();
        assert!(Arc::ptr_eq(
            &first.data.trees[&paths[0]],
            &reused.data.trees[&paths[0]]
        ));
        let all = c
            .ensure(
                &paths,
                false,
                Arc::new(AtomicBool::new(false)),
                |_| {},
                &mut build,
            )
            .unwrap();
        assert_eq!(all.data.trees.len(), 3);
        assert_eq!(c.runs(), 2);
        assert_eq!(traversed, paths);
        let (snapshot, preview) = projection(&all, &paths[..1], "scope-C");
        assert!(snapshot
            .candidates
            .iter()
            .chain(snapshot.recommended.as_deref().unwrap_or_default())
            .all(|p| within(&p.path, &paths[..1])));
        assert_eq!(preview.stats.scanned_files, 2);
        assert_eq!(preview.recommended.unwrap().file_count, 1);
        assert_eq!(snapshot.candidates.len(), 1);
        assert!(preview
            .groups
            .iter()
            .flat_map(|g| &g.entries)
            .all(|e| !e.default_selected));
    }
    #[test]
    fn reused_candidate_still_rejects_changed_file_and_old_epoch_without_polling() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let file = root.join("Temp/owned.tmp");
        old_file(&file);
        let state = DiskCleanerState::default();
        let view = state
            .1
            .ensure(
                std::slice::from_ref(&root),
                false,
                Arc::new(AtomicBool::new(false)),
                |_| {},
                |r, s, _, _| Ok(inventory(r, s)),
            )
            .unwrap();
        let (snapshot, preview) = projection(&view, std::slice::from_ref(&root), "scope");
        assert_eq!(snapshot.candidates.len(), 1);
        assert!(valid_candidate(&snapshot.candidates[0]));
        fs::write(&file, b"replacement changed size and timestamp").unwrap();
        assert!(!valid_candidate(&snapshot.candidates[0]));
        let id = snapshot.candidates[0].id.clone();
        {
            let mut inner = state.0.lock().unwrap();
            inner.snapshot = Some(snapshot);
            inner.snapshot_epoch = Some(view.epoch);
        }
        state.1.invalidate();
        assert!(take_snapshot(&state, &preview.plan_id, &[id]).is_err());
        assert!(file.exists());
    }
    #[test]
    fn recommended_plan_is_single_use_and_rejected_after_epoch_change() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        old_file(&root.join("Temp/owned.tmp"));
        let state = DiskCleanerState::default();
        let view = state
            .1
            .ensure(
                std::slice::from_ref(&root),
                false,
                Arc::new(AtomicBool::new(false)),
                |_| {},
                |r, s, _, _| Ok(inventory(r, s)),
            )
            .unwrap();
        let (snapshot, _) = projection(&view, std::slice::from_ref(&root), "single-use");
        let first_id = snapshot.id.clone();
        {
            let mut inner = state.0.lock().unwrap();
            inner.snapshot = Some(snapshot);
            inner.snapshot_epoch = Some(view.epoch);
        }
        assert!(take_recommended(&state, &first_id).is_ok());
        assert!(take_recommended(&state, &first_id).is_err());
        let (snapshot, _) = projection(&view, std::slice::from_ref(&root), "old-epoch");
        let second_id = snapshot.id.clone();
        {
            let mut inner = state.0.lock().unwrap();
            inner.snapshot = Some(snapshot);
            inner.snapshot_epoch = Some(view.epoch);
        }
        state.1.invalidate();
        assert!(take_recommended(&state, &second_id).is_err());
        assert!(state.1.peek().is_none());
    }
    #[test]
    fn supplement_and_projection_keep_original_expiry_time() {
        let tmp = tempfile::tempdir().unwrap();
        let c = tmp.path().join("C");
        let d = tmp.path().join("D");
        old_file(&c.join("Temp/a.tmp"));
        old_file(&d.join("Temp/b.tmp"));
        let mut first = inventory(std::slice::from_ref(&c), None);
        first.snapshot.created = SystemTime::now() - Duration::from_secs(100);
        let created = first.snapshot.created;
        let both = inventory(std::slice::from_ref(&d), Some(Arc::new(first)));
        assert_eq!(both.snapshot.created, created);
        let view = ResultView {
            data: Arc::new(both),
            epoch: 0,
            reused: true,
        };
        let (snapshot, preview) = projection(&view, &[c, d], "same-expiry");
        assert_eq!(snapshot.created, created);
        assert_eq!(
            preview.created_at,
            chrono::DateTime::<chrono::Utc>::from(created).to_rfc3339()
        );
    }
    #[test]
    fn system_space_tree_does_not_grant_deletion_rights_and_recycle_is_not_in_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        old_file(&root.join("$RECYCLE.BIN/owned.tmp"));
        old_file(&root.join("Docs/keep.txt"));
        let data = inventory(std::slice::from_ref(&root), None);
        let tree = &data.trees[&root];
        assert_eq!(tree.file_count, 1);
        assert_eq!(data.preview.stats.scanned_files, 2);
        assert!(data
            .snapshot
            .candidates
            .iter()
            .all(|c| !c.path.to_string_lossy().contains("$RECYCLE.BIN")));
    }
}
