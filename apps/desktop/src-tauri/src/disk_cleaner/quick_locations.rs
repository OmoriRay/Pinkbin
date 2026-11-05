//! Bounded path hints only. A hint never grants eligibility or deletion authority.
use super::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    io::Read,
    path::Component,
};
pub(super) const MAX_LOCATIONS: usize = 2048;
const MAX_BYTES: u64 = 512 * 1024;
const MAX_PATH: usize = 4096;
const FILE_NAME: &str = "known-cleanup-locations-v1.json";
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    version: u32,
    saved_at: u64,
    locations: Vec<PathBuf>,
}
fn timestamp() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn narrow_name(path: &Path) -> bool {
    path.file_name().and_then(|s| s.to_str()).is_some_and(|s| {
        matches!(
            s.to_ascii_lowercase().as_str(),
            "cache"
                | "cache_data"
                | "code cache"
                | "gpucache"
                | "shadercache"
                | "grshadercache"
                | "dawncache"
                | "dawngraphitecache"
                | "dawnwebgpucache"
                | "__pycache__"
                | "incremental"
                | "temp"
                | "tmp"
                | "crashdumps"
                | "reportarchive"
                | "_cacache"
                | "npm-cache"
                | "v3-cache"
                | "d3dscache"
                | "dxcache"
                | "glcache"
                | "shard-cache"
                | "cache2"
                | "logs"
                | "crashes"
                | "log"
                | "cacheddata"
                | "cachedextensions"
                | "cachedextensionvsixs"
                | "cachestorage"
                | "minidump"
                | "livekernelreports"
                | "cbs"
                | "dism"
                | "uicache"
                | "http-v2"
                | "http"
                | "wheels"
        ) || [
            "chromium_profile",
            "chromium_shared",
            "electron",
            "electron_legacy",
        ]
        .into_iter()
        .filter_map(crate::rule_catalog::allowed_template)
        .flatten()
        .filter(|relative| **relative != "Media Cache")
        .any(|relative| {
            Path::new(relative)
                .file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case(s))
        })
    })
}
fn valid_path(path: &Path) -> bool {
    let text = path.to_string_lossy();
    path.is_absolute()
        && text.len() <= MAX_PATH
        && !text.chars().any(char::is_control)
        && path.components().count() <= MAX_DEPTH
        && !text.starts_with(r"\\")
        && !path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        && !text.split(['/', '\\']).any(|s| s == "." || s == "..")
        && path
            .components()
            .filter(|c| matches!(c, Component::Normal(_)))
            .count()
            >= 2
        && (narrow_name(path)
            // Only engine-curated updater roots; eligibility remains a single old installer.exe.
            || crate::rule_catalog::builtin().updater_caches.iter().any(|rule| {
                path.file_name().is_some_and(|name| name.eq_ignore_ascii_case(&rule.directory))
            })
            // Only the reports child of Crashpad is a route, never arbitrary reports or pending.
            || (path.file_name().is_some_and(|p| p.eq_ignore_ascii_case("reports"))
                && path.parent().and_then(Path::file_name)
                    .is_some_and(|p| p.eq_ignore_ascii_case("Crashpad")))
            || (path.file_name().is_some_and(|p| {
                p.to_str()
                    .is_some_and(|s| s.eq_ignore_ascii_case("download"))
            }) && path.parent().and_then(Path::file_name).is_some_and(|p| {
                p.to_str()
                    .is_some_and(|s| s.eq_ignore_ascii_case("huggingface"))
            }) && path
                .parent()
                .and_then(Path::parent)
                .and_then(Path::file_name)
                .is_some_and(|p| p.to_str().is_some_and(|s| s.eq_ignore_ascii_case(".cache")))))
}
pub(super) fn boundary(file: &Path, root: &Path) -> Option<PathBuf> {
    file.parent()?
        .ancestors()
        .take_while(|p| *p != root && crate::rule_catalog::beneath(p, root).is_some())
        .find(|p| valid_path(p))
        .map(Path::to_path_buf)
}
// Bound discovery per volume, so a Python environment cannot crowd out other disks.
pub(super) struct Hints {
    roots: BTreeMap<PathBuf, BTreeSet<(u8, PathBuf)>>,
    count_budget: usize,
    byte_budget: usize,
}
fn hint_cost(path: &Path) -> usize {
    path.to_string_lossy()
        .len()
        .saturating_mul(2)
        .saturating_add(128)
}
fn priority(path: &Path) -> u8 {
    match path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase()
        .as_str()
    {
        "__pycache__" | "incremental" => 2,
        "temp" | "tmp" | "logs" | "log" => 1,
        _ => 0,
    }
}
fn volume_key(path: &Path) -> PathBuf {
    path.components().take(2).collect()
}
impl Hints {
    pub(super) fn new(roots: impl IntoIterator<Item = PathBuf>) -> Self {
        let roots: BTreeMap<_, _> = roots
            .into_iter()
            .map(|p| (route_key(&p), BTreeSet::new()))
            .collect();
        let count = roots.len().max(1);
        Self {
            roots,
            count_budget: MAX_LOCATIONS / count,
            byte_budget: (MAX_BYTES as usize - 128) / count,
        }
    }
    pub(super) fn add(&mut self, root: &Path, path: PathBuf) {
        if !valid_path(&path) {
            return;
        }
        let Some(bucket) = self.roots.get_mut(&route_key(root)) else {
            return;
        };
        let item = (priority(&path), path);
        if bucket.len() >= self.count_budget && bucket.last().is_some_and(|last| &item >= last) {
            return;
        }
        if !bucket.insert(item) {
            return;
        }
        while bucket.len() > self.count_budget {
            bucket.pop_last();
        }
        let mut bytes: usize = bucket.iter().map(|(_, p)| hint_cost(p)).sum();
        while bytes > self.byte_budget {
            let Some((_, removed)) = bucket.pop_last() else {
                break;
            };
            bytes = bytes.saturating_sub(hint_cost(&removed));
        }
    }
    pub(super) fn finish(self) -> Vec<PathBuf> {
        self.roots
            .into_values()
            .flat_map(|b| b.into_iter().map(|(_, p)| p))
            .collect()
    }
    fn select(paths: Vec<PathBuf>, roots: &[PathBuf]) -> Vec<PathBuf> {
        let mut hints = Self::new(roots.iter().cloned());
        for path in paths {
            if let Some(root) = roots
                .iter()
                .find(|r| crate::rule_catalog::beneath(&path, r).is_some())
            {
                hints.add(root, path);
            }
        }
        hints.finish()
    }
}
fn checked_file(path: &Path) -> io::Result<File> {
    safe_ancestors(path)?;
    let before = stamp(path, false)?;
    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt;
        OpenOptions::new()
            .read(true)
            .share_mode(1)
            .custom_flags(0x00200000)
            .open(path)?
    };
    #[cfg(not(windows))]
    let file = File::open(path)?;
    if stamp_handle(&file, false)? != before || before.size > MAX_BYTES {
        return Err(io::Error::other("位置记录无效或过大"));
    }
    Ok(file)
}
fn load_at(path: &Path) -> io::Result<Vec<PathBuf>> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e),
        Ok(m) if unsafe_metadata(&m) || !m.is_file() => {
            return Err(io::Error::other("位置记录不是普通文件"))
        }
        Ok(_) => {}
    }
    let mut bytes = vec![];
    checked_file(path)?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(io::Error::other("位置记录过大"));
    }
    let stored: Stored = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if stored.version != 1
        || stored.saved_at > timestamp().saturating_add(86400)
        || timestamp().saturating_sub(stored.saved_at) > 180 * 86400
        || stored.locations.len() > MAX_LOCATIONS
        || stored.locations.iter().any(|p| !valid_path(p))
    {
        return Err(io::Error::other("位置记录格式无效"));
    }
    // Reject links on present hints; removed software locations can remain harmless hints.
    for p in &stored.locations {
        for ancestor in p.ancestors() {
            match fs::symlink_metadata(ancestor) {
                Ok(m) if unsafe_metadata(&m) || !m.is_dir() => {
                    return Err(io::Error::other("位置包含链接或非目录"))
                }
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
    }
    Ok(stored.locations)
}
#[cfg(windows)]
fn replace(source: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
#[cfg(not(windows))]
fn replace(source: &Path, target: &Path) -> io::Result<()> {
    fs::rename(source, target)
}
fn save_at(path: &Path, locations: &[PathBuf], cancel: &AtomicBool) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("没有位置记录目录"))?;
    safe_ancestors(parent)?;
    let mut merged = load_at(path).unwrap_or_default();
    merged.extend(locations.iter().filter(|p| valid_path(p)).cloned());
    let roots: Vec<_> = merged
        .iter()
        .map(|p| volume_key(p))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut merged = Hints::select(merged, &roots);
    merged.sort();
    let mut bytes = serde_json::to_vec(&Stored {
        version: 1,
        saved_at: timestamp(),
        locations: merged.clone(),
    })
    .map_err(io::Error::other)?;
    while bytes.len() as u64 > MAX_BYTES {
        merged.pop();
        bytes = serde_json::to_vec(&Stored {
            version: 1,
            saved_at: timestamp(),
            locations: merged.clone(),
        })
        .map_err(io::Error::other)?;
    }
    if cancel.load(Ordering::Acquire) {
        return Ok(());
    }
    let temp = parent.join(format!(
        ".known-cleanup-{}-{}.tmp",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let outcome = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        if cancel.load(Ordering::Acquire) {
            return Ok(());
        }
        safe_ancestors(parent)?;
        if let Ok(m) = fs::symlink_metadata(path) {
            if unsafe_metadata(&m) || !m.is_file() {
                return Err(io::Error::other("位置记录不是普通文件"));
            }
        }
        replace(&temp, path)
    })();
    let _ = fs::remove_file(&temp);
    outcome
}
pub(super) fn save(
    app: &tauri::AppHandle,
    locations: &[PathBuf],
    cancel: &AtomicBool,
) -> io::Result<()> {
    save_at(&crate::app_data_dir(app).join(FILE_NAME), locations, cancel)
}
fn route_key(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(path.to_string_lossy().replace('/', "\\").to_lowercase())
    }
    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
}
pub(super) struct Routes {
    ancestors: HashSet<PathBuf>,
    boundaries: HashSet<PathBuf>,
}
impl Routes {
    pub(super) fn new(locations: impl IntoIterator<Item = PathBuf>, roots: &[PathBuf]) -> Self {
        let locations = locations
            .into_iter()
            .filter(|p| {
                valid_path(p)
                    && roots
                        .iter()
                        .any(|r| crate::rule_catalog::beneath(p, r).is_some())
            })
            .collect();
        let mut boundaries = HashSet::new();
        let mut ancestors = HashSet::new();
        let mut bytes = 0usize;
        for path in Hints::select(locations, roots) {
            let path = route_key(&path);
            let needed: Vec<_> = path
                .ancestors()
                .filter(|p| !ancestors.contains(*p))
                .map(Path::to_path_buf)
                .collect();
            let cost: usize = needed.iter().map(|p| hint_cost(p)).sum();
            if bytes.saturating_add(cost) > 8 * 1024 * 1024 {
                continue;
            }
            bytes += cost;
            ancestors.extend(needed);
            boundaries.insert(route_key(&path));
        }
        Self {
            ancestors,
            boundaries,
        }
    }
    pub(super) fn allows(&self, path: &Path) -> bool {
        self.ancestors.contains(&route_key(path))
            || path
                .ancestors()
                .any(|p| self.boundaries.contains(&route_key(p)))
    }
}
fn template_hints(
    catalog: &crate::rule_catalog::Catalog,
    parent: &Path,
    template: &str,
    push: &mut impl FnMut(PathBuf),
) {
    for path in catalog
        .template(template)
        .iter()
        .filter(|p| p.as_str() != "Media Cache")
    {
        push(parent.join(path));
    }
}
fn seed(context: &crate::full_cleanup_rules::Context, selected: &[PathBuf]) -> Vec<PathBuf> {
    let selected_path = |path: &Path| {
        selected
            .iter()
            .any(|root| crate::rule_catalog::beneath(path, root).is_some())
    };
    let mut hints = Hints::new(selected.iter().cloned());
    let mut push = |path: PathBuf| {
        if let Some(root) = selected
            .iter()
            .find(|r| crate::rule_catalog::beneath(&path, r).is_some())
        {
            hints.add(root, path);
        }
    };
    for path in context
        .temp
        .iter()
        .chain(&context.pip)
        .chain(&context.npm)
        .chain(&context.nuget)
    {
        push(path.clone());
    }
    for root in &context.huggingface_xet {
        if selected_path(root) && safe_ancestors(root).is_ok() {
            for entry in fs::read_dir(root).into_iter().flatten().take(256).flatten() {
                push(entry.path().join("shard-cache"));
            }
        }
    }
    if let Some(local) = &context.local {
        for rule in context
            .catalog
            .updater_caches
            .iter()
            .filter(|r| r.enabled && r.reviewed)
        {
            push(local.join(&rule.directory));
        }
        for path in [
            "CrashDumps",
            "D3DSCache",
            "NVIDIA/DXCache",
            "NVIDIA/GLCache",
            "Microsoft/Windows/WER/ReportArchive",
        ] {
            push(local.join(path));
        }
    }
    for legacy in &context.catalog.legacy {
        let base = if legacy.family == "electron" {
            &context.roaming
        } else {
            &context.local
        };
        let Some(base) = base else { continue };
        let parent = base.join(&legacy.prefix);
        if !selected_path(&parent) || safe_ancestors(&parent).is_err() {
            continue;
        }
        if legacy.family == "chromium" || legacy.family == "firefox" {
            if legacy.family == "chromium" {
                template_hints(&context.catalog, &parent, "chromium_shared", &mut push);
            }
            for entry in fs::read_dir(&parent)
                .into_iter()
                .flatten()
                .take(256)
                .flatten()
            {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name == "Default" || name.starts_with("Profile ") || legacy.family == "firefox" {
                    if legacy.family == "firefox" {
                        push(entry.path().join("cache2"));
                    } else {
                        template_hints(
                            &context.catalog,
                            &entry.path(),
                            "chromium_profile",
                            &mut push,
                        );
                    }
                }
            }
        } else if legacy.family == "electron" {
            template_hints(&context.catalog, &parent, "electron_legacy", &mut push);
            template_hints(&context.catalog, &parent, "chromium_shared", &mut push);
        }
    }
    for profile in context
        .catalog
        .profiles
        .iter()
        .filter(|p| p.enabled && p.reviewed)
    {
        for location in &profile.locations {
            let base = match location.root.as_str() {
                "local" => &context.local,
                "roaming" => &context.roaming,
                _ => &None,
            };
            if let Some(base) = base {
                let parent = base.join(&location.path);
                if !selected_path(&parent) || safe_ancestors(&parent).is_err() {
                    continue;
                }
                match profile.family.as_str() {
                    "diagnostics" => {
                        for format in &profile.maintenance {
                            if format == "obs-session" {
                                push(parent.join("crashes"));
                            }
                            push(parent.join(if format == "unreal-backup" {
                                "Saved/Logs"
                            } else {
                                "Logs"
                            }));
                        }
                    }
                    "electron" => {
                        template_hints(&context.catalog, &parent, "electron", &mut push);
                        template_hints(&context.catalog, &parent, "chromium_shared", &mut push);
                    }
                    "chromium" => {
                        template_hints(&context.catalog, &parent, "chromium_shared", &mut push)
                    }
                    _ => {
                        for leaf in ["Cache", "Code Cache", "GPUCache", "CachedData"] {
                            push(parent.join(leaf));
                        }
                    }
                }
                if profile.family != "diagnostics"
                    && (profile.logs || !profile.maintenance.is_empty())
                {
                    push(parent.join("logs"));
                }
                if profile
                    .maintenance
                    .iter()
                    .any(|format| format == "crashpad")
                {
                    push(parent.join("Crashpad/reports"));
                }
                // Enumerate only immediate browser profiles, never recurse through user data.
                for entry in fs::read_dir(&parent)
                    .into_iter()
                    .flatten()
                    .take(256)
                    .flatten()
                {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name == "Default"
                        || name.starts_with("Profile ")
                        || profile.family == "firefox"
                    {
                        if profile.family == "firefox" {
                            push(entry.path().join("cache2"));
                        } else if profile.family == "chromium" {
                            template_hints(
                                &context.catalog,
                                &entry.path(),
                                "chromium_profile",
                                &mut push,
                            );
                        }
                    }
                }
            }
        }
    }
    hints.finish()
}
pub(super) fn routes(
    app: &tauri::AppHandle,
    context: &crate::full_cleanup_rules::Context,
    roots: &[PathBuf],
) -> (Routes, bool) {
    let loaded = load_at(&crate::app_data_dir(app).join(FILE_NAME));
    let invalid = loaded.is_err();
    let mut hints = seed(context, roots);
    hints.extend(loaded.unwrap_or_default());
    (Routes::new(hints, roots), invalid)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diagnostics_full_standard_and_saved_routes_agree_and_respect_processes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("Drive");
        let local = root.join("Local");
        let roaming = root.join("Roaming");
        let obs = roaming.join("obs-studio");
        let stability = roaming.join("StabilityMatrix");
        let unreal = local.join("CrashReportClient");
        let candidates = [
            obs.join("logs/2020-01-01 00-00-00.txt"),
            obs.join("crashes/Crash 2020-01-01 00-00-00.txt"),
            stability.join("Logs/app.2020-01-01 00_00_00.log"),
            unreal.join("Saved/Logs/CrashReportClient-backup-2020.01.01-00.00.00.log"),
        ];
        for p in &candidates {
            old_file(p);
        }
        for p in [
            obs.join("global.ini"),
            obs.join("logs/2025-01-01 00-00-00.txt"),
            obs.join("crashes/Crash 2025-01-01 00-00-00.txt"),
            obs.join("basic/scenes/keep.txt"),
            stability.join("global.json"),
            stability.join("Logs/app.log"),
            stability.join("user-secrets.data"),
            unreal.join("Saved/Logs/CrashReportClient.log"),
            unreal.join("Saved/Config/WindowsEditor/Engine.ini"),
        ] {
            old_file(&p);
        }
        let context = crate::full_cleanup_rules::Context {
            local: Some(local),
            roaming: Some(roaming),
            ..Default::default()
        };
        let roots = vec![root.clone()];
        let indexed = vec![("fixture".into(), root.clone())];
        let scan = |routes: Option<&Routes>, running| {
            scan_roots_routed(
                &indexed,
                "diagnostics",
                &AtomicBool::new(false),
                Limits::default(),
                &context,
                |_| Ok(running),
                |_, _| {},
                None,
                routes,
            )
        };
        let paths = |snapshot: Option<Snapshot>| {
            snapshot
                .unwrap()
                .recommended
                .unwrap_or_default()
                .into_iter()
                .map(|c| c.path)
                .collect::<BTreeSet<_>>()
        };
        let (full, preview) = scan(None, false);
        let full = paths(full);
        assert_eq!(full, BTreeSet::from(candidates));
        let standard = Routes::new(seed(&context, &roots), &roots);
        assert_eq!(paths(scan(Some(&standard), false).0), full);
        let store = tmp.path().join(FILE_NAME);
        save_at(&store, &preview.learned_locations, &AtomicBool::new(false)).unwrap();
        let restored = Routes::new(load_at(&store).unwrap(), &roots);
        assert_eq!(paths(scan(Some(&restored), false).0), full);
        assert!(paths(scan(Some(&restored), true).0).is_empty());
    }
    #[test]
    fn updater_and_steam_routes_match_full_walk_and_reload() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("Drive");
        let local = root.join("Local");
        let roaming = root.join("Roaming");
        let installer = local.join("cherrystudio-updater/installer.exe");
        let pending = local.join("cherrystudio-updater/pending/installer.exe");
        let steam = local.join("Steam/htmlcache/Default/Cache/Cache_Data/item");
        for file in [
            &installer,
            &pending,
            &steam,
            &roaming.join("CherryStudio/Local State"),
            &local.join("Steam/htmlcache/Local State"),
        ] {
            old_file(file);
        }
        let context = crate::full_cleanup_rules::Context {
            local: Some(local),
            roaming: Some(roaming),
            ..Default::default()
        };
        let roots = vec![root.clone()];
        let indexed = vec![("fixture".into(), root.clone())];
        let scan = |routes: Option<&Routes>, running| {
            scan_roots_routed(
                &indexed,
                "new-rules",
                &AtomicBool::new(false),
                Limits::default(),
                &context,
                |_| Ok(running),
                |_, _| {},
                None,
                routes,
            )
        };
        let paths = |snapshot: Option<Snapshot>| {
            snapshot
                .unwrap()
                .recommended
                .unwrap_or_default()
                .into_iter()
                .map(|p| p.path)
                .collect::<BTreeSet<_>>()
        };
        let (full, preview) = scan(None, false);
        let full = paths(full);
        assert_eq!(full, BTreeSet::from([installer.clone(), steam]));
        let standard = Routes::new(seed(&context, &roots), &roots);
        assert_eq!(paths(scan(Some(&standard), false).0), full);
        let store = tmp.path().join(FILE_NAME);
        save_at(&store, &preview.learned_locations, &AtomicBool::new(false)).unwrap();
        let loaded = load_at(&store).unwrap();
        assert!(loaded.contains(&installer.parent().unwrap().to_path_buf()));
        let restored = Routes::new(loaded, &roots);
        assert_eq!(paths(scan(Some(&restored), false).0), full);
        assert!(paths(scan(Some(&restored), true).0).is_empty());
        assert!(!valid_path(&root.join("Documents/unknown-updater")));
    }
    #[test]
    fn regression_template_directories_and_crashpad_are_narrow_reloadable_boundaries() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for paths in crate::rule_catalog::builtin().templates.values() {
            for relative in paths.iter().filter(|p| p.as_str() != "Media Cache") {
                let directory = root.join("App").join(relative);
                assert!(
                    valid_path(&directory),
                    "missing template boundary: {relative}"
                );
            }
        }
        let reports = root.join("App/Crashpad/reports");
        assert_eq!(
            boundary(&reports.join("old.dmp"), root),
            Some(reports.clone())
        );
        assert!(valid_path(&reports));
        assert!(!valid_path(&root.join("Documents/reports")));
        assert!(!valid_path(&root.join("App/Crashpad")));
        let routes = Routes::new(vec![reports], &[root.to_path_buf()]);
        assert!(!routes.allows(&root.join("App/Crashpad/pending/resumable.dmp")));
        assert!(!routes.allows(&root.join("App/Local Storage/keep")));
    }
    #[test]
    fn regression_seed_and_reloaded_locations_preserve_full_recommendations() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("C");
        let local = root.join("Local");
        let roaming = root.join("Roaming");
        let quark = local.join("Quark/User Data");
        let cherry = roaming.join("CherryStudio");
        for app in [&quark, &cherry] {
            old_file(&app.join("Local State"));
            old_file(&app.join("Crashpad/settings.dat"));
        }
        for name in ["data_0", "data_1", "data_2", "data_3", "index"] {
            old_file(&quark.join("GraphiteDawnCache").join(name));
        }
        for (app, uuid) in [
            (&quark, "d55338c3-53e2-43d9-b525-2fdd1beed502"),
            (&cherry, "1448c0e1-689e-43c0-a47c-32eb1d6e5f7e"),
            (&cherry, "2ce3ff28-cf43-4a7f-9ca0-f0804cdda444"),
        ] {
            old_file(&app.join(format!("Crashpad/reports/{uuid}.dmp")));
        }
        old_file(&quark.join("Crashpad/pending/keep.dmp"));
        old_file(&quark.join("Local Storage/keep.db"));
        let context = crate::full_cleanup_rules::Context {
            local: Some(local),
            roaming: Some(roaming),
            ..Default::default()
        };
        let indexed = vec![("C".into(), root.clone())];
        let scan = |routes: Option<&Routes>| {
            scan_roots_routed(
                &indexed,
                "regression",
                &AtomicBool::new(false),
                Limits::default(),
                &context,
                |_| Ok(false),
                |_, _| {},
                None,
                routes,
            )
        };
        let (full, preview) = scan(None);
        let paths = |snapshot: Option<Snapshot>| {
            snapshot
                .unwrap()
                .recommended
                .unwrap_or_default()
                .into_iter()
                .map(|c| c.path)
                .collect::<BTreeSet<_>>()
        };
        let full = paths(full);
        assert_eq!(full.len(), 8);
        let roots = vec![root.clone()];
        let standard = Routes::new(seed(&context, &roots), &roots);
        assert_eq!(
            paths(scan(Some(&standard)).0),
            full,
            "standard positions miss supported files"
        );
        let store = tmp.path().join(FILE_NAME);
        save_at(&store, &preview.learned_locations, &AtomicBool::new(false)).unwrap();
        let loaded = load_at(&store).unwrap();
        assert!(loaded.contains(&quark.join("GraphiteDawnCache")));
        assert!(loaded.contains(&quark.join("Crashpad/reports")));
        assert!(loaded.contains(&cherry.join("Crashpad/reports")));
        let routes = Routes::new(loaded, &roots);
        assert_eq!(paths(scan(Some(&routes)).0), full);
        let fresh = cherry.join("Crashpad/reports/11111111-2222-3333-4444-555555555555.dmp");
        old_file(&fresh);
        let current = paths(scan(Some(&routes)).0);
        assert_eq!(current.len(), 9);
        assert!(
            current.contains(&fresh),
            "reload must enumerate new files, not use an old file list"
        );
    }
    #[test]
    fn regression_crashpad_route_never_grants_cleanup_eligibility() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let roaming = root.join("Roaming");
        let app = roaming.join("CherryStudio");
        let unknown = roaming.join("UnknownApplication");
        for base in [&app, &unknown] {
            old_file(&base.join("Local State"));
            old_file(&base.join("Crashpad/settings.dat"));
            old_file(&base.join("Crashpad/reports/11111111-2222-3333-4444-555555555555.dmp"));
        }
        old_file(&app.join("Crashpad/reports/not-a-uuid.dmp"));
        old_file(&app.join("Crashpad/reports/nested/11111111-2222-3333-4444-555555555555.dmp"));
        let young = app.join("Crashpad/reports/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.dmp");
        fs::write(young, b"new crash fixture").unwrap();
        let context = crate::full_cleanup_rules::Context {
            roaming: Some(roaming),
            ..Default::default()
        };
        let routes = Routes::new(
            vec![
                app.join("Crashpad/reports"),
                unknown.join("Crashpad/reports"),
            ],
            &[root.to_path_buf()],
        );
        for running in [false, true] {
            let (snapshot, _) = scan_roots_routed(
                &[("fixture".into(), root.to_path_buf())],
                "guard",
                &AtomicBool::new(false),
                Limits::default(),
                &context,
                |_| Ok(running),
                |_, _| {},
                None,
                Some(&routes),
            );
            let files = snapshot.unwrap().recommended.unwrap_or_default();
            assert_eq!(files.len(), usize::from(!running));
            if let Some(file) = files.first() {
                assert_eq!(
                    file.path,
                    app.join("Crashpad/reports/11111111-2222-3333-4444-555555555555.dmp")
                );
            }
        }
    }
    #[test]
    fn discovery_keeps_each_volume_and_prefers_stable_software_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let c = tmp.path().join("C");
        let d = tmp.path().join("D");
        let e = tmp.path().join("E");
        let roots = vec![c.clone(), d.clone(), e.clone()];
        let mut hints = Hints::new(roots.clone());
        for i in 0..5000 {
            hints.add(&c, c.join(format!("Python/{i:05}/__pycache__")));
        }
        let c_cache = c.join("StableApp/Cache");
        let d_cache = d.join("PortableApp/Code Cache");
        let e_cache = e.join("AnotherApp/GPUCache");
        hints.add(&c, c_cache.clone());
        hints.add(&d, d_cache.clone());
        hints.add(&e, e_cache.clone());
        let paths = hints.finish();
        assert!(paths.len() <= MAX_LOCATIONS);
        for path in [&c_cache, &d_cache, &e_cache] {
            assert!(paths.contains(path));
        }
        let mut merged = paths;
        for i in 0..5000 {
            merged.push(c.join(format!("MorePython/{i:05}/__pycache__")));
        }
        let merged = Hints::select(merged, &roots);
        assert!(merged.contains(&d_cache));
        assert!(merged.contains(&e_cache));
        assert!(merged.contains(&c_cache));
    }
    #[test]
    fn path_and_route_memory_budgets_reject_deep_or_oversized_hints() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("C");
        let deep = (0..MAX_DEPTH)
            .fold(root.clone(), |p, _| p.join("a"))
            .join("Cache");
        assert!(!valid_path(&deep));
        let paths = (0..5000)
            .map(|i| root.join(format!("{i:05}{}/Cache", "x".repeat(180))))
            .collect::<Vec<_>>();
        let bounded = Hints::select(paths.clone(), std::slice::from_ref(&root));
        assert!(bounded.len() <= MAX_LOCATIONS);
        assert!(bounded.iter().map(|p| hint_cost(p)).sum::<usize>() <= MAX_BYTES as usize);
        let routes = Routes::new(paths, std::slice::from_ref(&root));
        assert!(routes.boundaries.len() <= MAX_LOCATIONS);
        assert!(routes.ancestors.iter().map(|p| hint_cost(p)).sum::<usize>() <= 8 * 1024 * 1024);
        assert!(!routes.allows(&root.join("Documents/keep")));
    }
    #[cfg(windows)]
    #[test]
    fn saving_one_volume_preserves_other_volume_hints_under_pressure() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join(FILE_NAME);
        let d = PathBuf::from(r"D:\PinkbinTestMissingLocation\Cache");
        save_at(&store, std::slice::from_ref(&d), &AtomicBool::new(false)).unwrap();
        let c = (0..5000)
            .map(|i| PathBuf::from(format!(r"C:\PinkbinTestMissingLocation\{i}\__pycache__")))
            .collect::<Vec<_>>();
        save_at(&store, &c, &AtomicBool::new(false)).unwrap();
        let loaded = load_at(&store).unwrap();
        assert!(loaded.contains(&d));
        assert!(loaded.len() <= MAX_LOCATIONS);
        assert!(fs::metadata(&store).unwrap().len() <= MAX_BYTES);
    }
    fn old_file(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"fixture only").unwrap();
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
    #[test]
    fn stored_hints_reload_merge_and_cancel_without_publishing_partial_locations() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join(FILE_NAME);
        let c = tmp.path().join("C/App/Cache");
        let d = tmp.path().join("D/App/Code Cache");
        save_at(&store, std::slice::from_ref(&c), &AtomicBool::new(false)).unwrap();
        assert_eq!(load_at(&store).unwrap(), vec![c.clone()]);
        save_at(&store, std::slice::from_ref(&d), &AtomicBool::new(true)).unwrap();
        assert_eq!(load_at(&store).unwrap(), vec![c.clone()]);
        save_at(&store, std::slice::from_ref(&d), &AtomicBool::new(false)).unwrap();
        assert_eq!(load_at(&store).unwrap(), vec![c, d]);
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);
    }
    #[test]
    fn invalid_records_are_bounded_and_never_become_routes() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join(FILE_NAME);
        for location in [
            tmp.path().to_path_buf(),
            tmp.path().join("Windows"),
            tmp.path().join("Users"),
            tmp.path().join("App/../Cache"),
        ] {
            fs::write(
                &store,
                serde_json::to_vec(&Stored {
                    version: 1,
                    saved_at: timestamp(),
                    locations: vec![location],
                })
                .unwrap(),
            )
            .unwrap();
            assert!(load_at(&store).is_err());
        }
        fs::write(&store, b"broken").unwrap();
        assert!(load_at(&store).is_err());
        fs::write(&store, vec![b' '; MAX_BYTES as usize + 1]).unwrap();
        assert!(load_at(&store).is_err());
        fs::write(
            &store,
            serde_json::to_vec(&Stored {
                version: 1,
                saved_at: timestamp(),
                locations: vec![tmp.path().join("App/Cache"); MAX_LOCATIONS + 1],
            })
            .unwrap(),
        )
        .unwrap();
        assert!(load_at(&store).is_err());
        assert!(boundary(&tmp.path().join("App/.cache/unknown.bin"), tmp.path()).is_none());
    }
    #[test]
    fn learned_location_finds_new_files_after_reload_with_root_anchored_parent_chain() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("D");
        let cache = root.join("App/Temp");
        let first = cache.join("first.tmp");
        old_file(&first);
        let context = crate::full_cleanup_rules::Context {
            temp: vec![cache.clone()],
            ..Default::default()
        };
        let indexed = vec![("D".into(), root.clone())];
        let (_, full) = scan_roots_observed(
            &indexed,
            "full",
            &AtomicBool::new(false),
            Limits::default(),
            &context,
            |_| Ok(false),
            |_, _| {},
            None,
        );
        assert_eq!(full.learned_locations, vec![cache.clone()]);
        let store = tmp.path().join(FILE_NAME);
        save_at(&store, &full.learned_locations, &AtomicBool::new(false)).unwrap();
        fs::remove_file(&first).unwrap();
        let fresh = cache.join("fresh.tmp");
        old_file(&fresh);
        old_file(&root.join("unrelated/keep.log"));
        let routes = Routes::new(load_at(&store).unwrap(), std::slice::from_ref(&root));
        let (plan, preview) = scan_roots_routed(
            &indexed,
            "quick",
            &AtomicBool::new(false),
            Limits::default(),
            &context,
            |_| Ok(false),
            |_, _| {},
            None,
            Some(&routes),
        );
        assert_eq!(preview.scan_mode, "quick");
        assert_eq!(preview.stats.scanned_files, 1);
        let plan = plan.unwrap();
        assert!(plan.candidates.is_empty());
        let files = plan.recommended.unwrap();
        assert_eq!(files.len(), 1);
        let candidate = &files[0];
        assert_eq!(candidate.path, fresh);
        assert_eq!(candidate.parent.chain().last().unwrap().path, root);
        assert!(valid_candidate_with_running(
            candidate,
            &|_| Ok(false),
            None
        ));
        fs::write(&fresh, b"changed replacement fixture contents").unwrap();
        assert!(!valid_candidate_with_running(
            candidate,
            &|_| Ok(false),
            None
        ));
    }
    #[test]
    fn selected_drive_routes_never_admit_other_drives_or_unknown_siblings() {
        let tmp = tempfile::tempdir().unwrap();
        let c = tmp.path().join("C");
        let d = tmp.path().join("D");
        let routes = Routes::new(
            vec![c.join("App/Cache"), d.join("App/Cache")],
            std::slice::from_ref(&d),
        );
        assert!(!routes.allows(&c.join("App/Cache/old")));
        assert!(routes.allows(&d.join("App")));
        assert!(routes.allows(&d.join("App/Cache/new")));
        assert!(!routes.allows(&d.join("App/Documents/keep")));
    }
    #[test]
    fn active_execution_survives_rejected_scan_registration() {
        let state = DiskCleanerState::default();
        state.0.lock().unwrap().execution = Some(CleanupExecution {
            drive_ids: vec![],
            plan_id: "running-job".into(),
            status: "running",
            total_files: 12,
            processed_files: 3,
            batch: 1,
            deleted_files: 2,
            deleted_bytes: 10,
            result: None,
            error: None,
        });
        assert!(register_scan(&state, "new-quick").is_err());
        let inner = state.0.lock().unwrap();
        assert!(inner.active.is_none());
        let execution = inner.execution.as_ref().unwrap();
        assert_eq!(execution.plan_id, "running-job");
        assert_eq!(execution.processed_files, 3);
        let job = execution.clone();
        drop(inner);
        state.0.lock().unwrap().execution.as_mut().unwrap().status = "completed";
        register_scan(&state, "new-quick").unwrap();
        assert!(state.0.lock().unwrap().execution.is_none());
        // Reproduce publication by a cleanup starter during scan registration handoff.
        state.0.lock().unwrap().execution = Some(job);
        install_scan_session(
            &state,
            DiskScanSession {
                scan_mode: "quick",
                scan_id: "new-quick".into(),
                drive_ids: vec![],
                status: "running",
                progress: None,
                preview: None,
                error: None,
            },
        )
        .unwrap();
        let inner = state.0.lock().unwrap();
        assert_eq!(inner.execution.as_ref().unwrap().plan_id, "running-job");
        assert_eq!(inner.execution.as_ref().unwrap().processed_files, 3);
    }
    #[cfg(windows)]
    #[test]
    fn linked_record_is_rejected_and_not_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        fs::write(&target, b"owned fixture").unwrap();
        let store = tmp.path().join(FILE_NAME);
        // Junction/symlink creation can require a privilege unavailable to test runners.
        if std::os::windows::fs::symlink_file(&target, &store).is_err() {
            return;
        }
        assert!(load_at(&store).is_err());
        assert!(save_at(&store, &[], &AtomicBool::new(false)).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"owned fixture");
    }
    #[test]
    fn running_app_location_is_learned_but_never_authorizes_cleanup() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let roaming = root.join("Roaming");
        let file = roaming.join("Code/GPUCache/hash");
        old_file(&file);
        let context = crate::full_cleanup_rules::Context {
            roaming: Some(roaming),
            ..Default::default()
        };
        let roots = vec![("fixture".into(), root.to_path_buf())];
        let (plan, preview) = scan_roots_observed(
            &roots,
            "learn-running",
            &AtomicBool::new(false),
            Limits::default(),
            &context,
            |_| Ok(true),
            |_, _| {},
            None,
        );
        assert!(preview
            .learned_locations
            .contains(&file.parent().unwrap().to_path_buf()));
        assert!(plan.unwrap().recommended.unwrap_or_default().is_empty());
        let routes = Routes::new(preview.learned_locations, &[root.to_path_buf()]);
        let (plan, _) = scan_roots_routed(
            &roots,
            "fresh",
            &AtomicBool::new(false),
            Limits::default(),
            &context,
            |_| Ok(false),
            |_, _| {},
            None,
            Some(&routes),
        );
        let plan = plan.unwrap();
        let candidates = plan.recommended.unwrap();
        assert_eq!(candidates.len(), 1);
        assert!(!valid_candidate_with_running(
            &candidates[0],
            &|_| Ok(true),
            None
        ));
        fs::rename(file.parent().unwrap(), root.join("moved-GPUCache")).unwrap();
        assert!(!valid_candidate_with_running(
            &candidates[0],
            &|_| Ok(false),
            None
        ));
    }
}
