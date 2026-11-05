//! A fresh local projection that never enters the full-disk shared inventory.
use super::*;
pub(super) fn run(
    app: &tauri::AppHandle,
    state: &DiskCleanerState,
    roots: &[PathBuf],
    id: &str,
    cancel: &AtomicBool,
) -> Result<(Snapshot, Preview, u64), String> {
    let _lease = reserve_operation()?;
    let epoch = state.1.epoch();
    let context = crate::full_cleanup_rules::Context::current();
    let (routes, invalid) = quick_locations::routes(app, &context, roots);
    let indexed = roots
        .iter()
        .map(|p| (p.to_string_lossy().into_owned(), p.clone()))
        .collect::<Vec<_>>();
    let (snapshot, mut preview) = scan_roots_routed(
        &indexed,
        id,
        cancel,
        Limits::default(),
        &context,
        crate::cache_cleaner::browser_running,
        |root, stats| {
            let event = progress(id, root, stats);
            if let Ok(mut inner) = state.0.lock() {
                if let Some(s) = inner.session.as_mut().filter(|s| s.scan_id == id) {
                    s.progress = Some(event.clone());
                }
            }
            let _ = app.emit("disk-cleanup-progress", event);
        },
        None,
        Some(&routes),
    );
    if cancel.load(Ordering::Acquire) {
        return Err("检查已取消".into());
    }
    let mut snapshot = snapshot.ok_or("快速复查没有生成有效计划")?;
    snapshot.candidates.clear();
    preview.groups.clear();
    preview.overview.clear();
    preview.unclassified_caches.clear();
    preview.overview_limited = false;
    preview.learned_locations.clear();
    preview
        .warnings
        .push("仅检查已知位置；全盘检查可发现新安装或搬移的软件位置。".into());
    if invalid {
        preview.warnings.push(
            "已知位置记录无效，已仅检查当前可验证的标准位置；可通过全盘检查重新发现。".into(),
        );
    }
    Ok((snapshot, preview, epoch))
}
