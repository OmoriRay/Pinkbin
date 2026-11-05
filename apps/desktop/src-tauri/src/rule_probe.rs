//! Discovery-only reuse. Execution constructs a fresh probe and never borrows this cache.
use crate::full_cleanup_rules::{Context, Match};
use serde::Serialize;
use std::{
    cell::RefCell,
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};
const MARKER_LIMIT: usize = 8192;
const DIRECTORY_LIMIT: usize = 4096;
const PATH_LIMIT: usize = 4096;
const MEMORY_LIMIT: usize = 8 * 1024 * 1024;
pub(crate) struct ResolvedProfile {
    pub index: usize,
    pub root: PathBuf,
}
pub(crate) struct Probe<'a> {
    pub profiles: Option<&'a [ResolvedProfile]>,
    pub marker: &'a dyn Fn(&Path) -> bool,
}
#[derive(Default, Clone, Debug, Serialize)]
pub(crate) struct ReuseStats {
    pub marker_cache_hits: u64,
    pub directory_cache_hits: u64,
    pub filesystem_queries: u64,
    pub prepared_profile_roots: usize,
    pub marker_entries: usize,
    pub directory_entries: usize,
    pub memory_bytes: usize,
}
#[derive(Default)]
struct MarkerCache {
    markers: HashMap<PathBuf, bool>,
    directories: HashMap<PathBuf, bool>,
    stats: ReuseStats,
}
pub(crate) fn ordinary(meta: &fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & (0x400 | 0x1000 | 0x40000 | 0x400000) != 0 {
            return false;
        }
    }
    true
}
impl MarkerCache {
    fn reserve(&mut self, path: &Path) -> bool {
        let length = path.as_os_str().len();
        let cost = length.saturating_mul(2).saturating_add(128);
        if length > PATH_LIMIT || self.stats.memory_bytes.saturating_add(cost) > MEMORY_LIMIT {
            return false;
        }
        self.stats.memory_bytes += cost;
        true
    }
    fn check(&mut self, path: &Path) -> bool {
        if let Some(value) = self.markers.get(path) {
            self.stats.marker_cache_hits += 1;
            return *value;
        }
        self.stats.filesystem_queries += 1;
        let mut valid = fs::symlink_metadata(path).is_ok_and(|m| m.is_file() && ordinary(&m));
        if valid {
            for parent in path.parent().into_iter().flat_map(Path::ancestors) {
                let safe = if let Some(value) = self.directories.get(parent) {
                    self.stats.directory_cache_hits += 1;
                    *value
                } else {
                    self.stats.filesystem_queries += 1;
                    let value =
                        fs::symlink_metadata(parent).is_ok_and(|m| m.is_dir() && ordinary(&m));
                    if self.directories.len() < DIRECTORY_LIMIT && self.reserve(parent) {
                        self.directories.insert(parent.into(), value);
                    }
                    value
                };
                if !safe {
                    valid = false;
                    break;
                }
            }
        }
        if self.markers.len() < MARKER_LIMIT && self.reserve(path) {
            self.markers.insert(path.into(), valid);
        }
        valid
    }
}
pub(crate) struct ScanMatcher<'a> {
    context: &'a Context,
    profiles: Vec<ResolvedProfile>,
    markers: RefCell<MarkerCache>,
}
impl<'a> ScanMatcher<'a> {
    pub fn new(context: &'a Context) -> Self {
        let mut profiles = Vec::new();
        for (index, rule) in context.catalog.profiles.iter().enumerate() {
            for location in &rule.locations {
                let base = match location.root.as_str() {
                    "local" => &context.local,
                    "roaming" => &context.roaming,
                    _ => continue,
                };
                if let Some(base) = base {
                    profiles.push(ResolvedProfile {
                        index,
                        root: base.join(&location.path),
                    });
                }
            }
        }
        Self {
            context,
            profiles,
            markers: RefCell::new(MarkerCache::default()),
        }
    }
    pub fn identify(&self, path: &Path, modified: SystemTime, now: SystemTime) -> Option<Match> {
        self.context.identify_with(
            path,
            modified,
            now,
            &Probe {
                profiles: Some(&self.profiles),
                marker: &|p| self.markers.borrow_mut().check(p),
            },
        )
    }
    pub fn stats(&self) -> ReuseStats {
        let cache = self.markers.borrow();
        ReuseStats {
            prepared_profile_roots: self.profiles.len(),
            marker_entries: cache.markers.len(),
            directory_entries: cache.directories.len(),
            ..cache.stats.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn put(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"owned marker fixture").unwrap();
    }
    #[test]
    fn scan_reuses_markers_but_fresh_validation_and_new_scans_observe_changes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let context = Context {
            local: Some(root.join("Local")),
            roaming: Some(root.join("Roaming")),
            ..Default::default()
        };
        let marker = root.join("Local/Quark/User Data/Local State");
        put(&marker);
        let paths = (0..16)
            .map(|i| {
                root.join(format!(
                    "Local/Quark/User Data/Default/Cache/Cache_Data/item-{i}"
                ))
            })
            .collect::<Vec<_>>();
        let matcher = ScanMatcher::new(&context);
        let now = SystemTime::now();
        for path in &paths {
            assert_eq!(
                matcher.identify(path, now, now),
                context.identify(path, now, now)
            );
        }
        let stats = matcher.stats();
        assert!(stats.marker_cache_hits >= 15);
        assert!(stats.prepared_profile_roots >= 45);
        fs::remove_file(&marker).unwrap();
        assert!(matcher.identify(&paths[0], now, now).is_some());
        assert!(
            context.identify(&paths[0], now, now).is_none(),
            "execution entry point must ignore scan reuse"
        );
        let next = ScanMatcher::new(&context);
        assert!(next.identify(&paths[0], now, now).is_none());
        put(&marker);
        assert!(
            next.identify(&paths[0], now, now).is_none(),
            "negative cache stays inside its original scan"
        );
        assert!(context.identify(&paths[0], now, now).is_some());
        assert!(ScanMatcher::new(&context)
            .identify(&paths[0], now, now)
            .is_some());
    }
    #[test]
    fn shared_parent_checks_are_reused_and_marker_cache_is_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let mut cache = MarkerCache::default();
        for name in ["product.json", "state.json"] {
            put(&root.join(name));
            assert!(cache.check(&root.join(name)));
        }
        assert!(cache.stats.directory_cache_hits > 0);
        for i in 0..MARKER_LIMIT + 32 {
            assert!(!cache.check(&root.join(format!("absent-{i}"))));
        }
        assert_eq!(cache.markers.len(), MARKER_LIMIT);
        assert!(cache.directories.len() <= DIRECTORY_LIMIT);
        assert!(cache.stats.memory_bytes <= MEMORY_LIMIT);
        let retained = cache.markers.len();
        assert!(!cache.check(&root.join("x".repeat(PATH_LIMIT + 1))));
        assert_eq!(cache.markers.len(), retained);
        assert!(cache.check(&root.join("product.json")));
    }
}
