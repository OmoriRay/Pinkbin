//! Bounded, metadata-only space tree produced by the cleanup traversal itself.
use pinkbin_scaffold::{detect_compiled, CompiledScaffold};
use pinkbin_scanner::{ExtShare, Node};
use std::{
    cmp::Reverse,
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
};
const DIR_BYTES: usize = 32 * 1024 * 1024;
const FILE_BYTES: usize = 12 * 1024 * 1024;
const MAX_DIRS: usize = 40_000;
const MAX_FILES: usize = 40_000;
const MAX_EXTENSIONS: usize = 1024;
type Key = (bool, u64, Reverse<usize>, u64);
struct Retained<T> {
    items: BTreeMap<Key, (T, usize)>,
    bytes: usize,
    budget: usize,
    count: usize,
    sequence: u64,
    limited: bool,
}
impl<T> Retained<T> {
    fn new(budget: usize, count: usize) -> Self {
        Self {
            items: BTreeMap::new(),
            bytes: 0,
            budget,
            count,
            sequence: 0,
            limited: false,
        }
    }
    fn push(&mut self, value: T, size: u64, depth: usize, tagged: bool, cost: usize) {
        self.sequence += 1;
        self.bytes = self.bytes.saturating_add(cost);
        self.items
            .insert((tagged, size, Reverse(depth), self.sequence), (value, cost));
        while self.bytes > self.budget || self.items.len() > self.count {
            self.limited = true;
            if let Some((_, (_, cost))) = self.items.pop_first() {
                self.bytes = self.bytes.saturating_sub(cost)
            } else {
                break;
            }
        }
    }
}
#[derive(Default)]
pub(crate) struct Summary {
    pub bytes: u64,
    pub files: u64,
    ext: HashMap<String, (u64, u64)>,
    incomplete: bool,
    pub tagged: bool,
}
impl Summary {
    pub fn file(&mut self, path: &Path, bytes: u64) {
        self.bytes = self.bytes.saturating_add(bytes);
        self.files += 1;
        let ext = path
            .extension()
            .and_then(|v| v.to_str())
            .filter(|v| v.len() <= 64)
            .unwrap_or("")
            .to_ascii_lowercase();
        self.extension(ext, bytes, 1);
    }
    fn extension(&mut self, ext: String, bytes: u64, files: u64) {
        if self.incomplete {
            return;
        }
        if !self.ext.contains_key(&ext) && self.ext.len() >= MAX_EXTENSIONS {
            self.ext.clear();
            self.incomplete = true;
            return;
        }
        let entry = self.ext.entry(ext).or_default();
        entry.0 = entry.0.saturating_add(bytes);
        entry.1 = entry.1.saturating_add(files);
    }
    pub fn child(&mut self, other: &Self) {
        self.bytes = self.bytes.saturating_add(other.bytes);
        self.files = self.files.saturating_add(other.files);
        self.tagged |= other.tagged;
        if other.incomplete {
            self.ext.clear();
            self.incomplete = true
        }
        for (ext, (bytes, files)) in &other.ext {
            self.extension(ext.clone(), *bytes, *files)
        }
    }
    fn top(&self) -> Vec<ExtShare> {
        if self.incomplete {
            return vec![];
        }
        let mut out = self
            .ext
            .iter()
            .map(|(ext, (bytes, count))| ExtShare {
                ext: ext.clone(),
                bytes: *bytes,
                count: *count,
            })
            .collect::<Vec<_>>();
        out.sort_by_key(|s| Reverse(s.bytes));
        out.truncate(8);
        out
    }
}
struct Dir {
    path: PathBuf,
    size: u64,
    files: u64,
    ext: Vec<ExtShare>,
    tag: Option<String>,
    tagged: bool,
}
struct Leaf {
    path: PathBuf,
    size: u64,
}
pub(crate) struct TreeSink {
    dirs: Retained<Dir>,
    files: Retained<Leaf>,
    compiled: Arc<Vec<CompiledScaffold>>,
    pub trees: HashMap<PathBuf, Arc<Node>>,
    pub limited: bool,
    pub blocked: HashMap<PathBuf, Vec<String>>,
    current_root: PathBuf,
}
pub(crate) fn excluded(path: &Path) -> bool {
    path.file_name().is_some_and(|s| {
        matches!(
            s.to_string_lossy().to_ascii_lowercase().as_str(),
            "$recycle.bin" | "system volume information" | ".trash" | ".trashes"
        )
    })
}
impl TreeSink {
    pub fn new(compiled: Arc<Vec<CompiledScaffold>>) -> Self {
        Self {
            dirs: Retained::new(DIR_BYTES, MAX_DIRS),
            files: Retained::new(FILE_BYTES, MAX_FILES),
            compiled,
            trees: HashMap::new(),
            limited: false,
            blocked: HashMap::new(),
            current_root: PathBuf::new(),
        }
    }
    pub fn begin_root(&mut self, root: &Path) {
        self.current_root = root.into();
    }
    pub fn blocked(&mut self, name: &str) {
        self.blocked
            .entry(self.current_root.clone())
            .or_default()
            .push(name.into());
    }
    pub fn file(&mut self, path: &Path, bytes: u64) {
        if path.as_os_str().len() > 4096 {
            self.limited = true;
            return;
        }
        let cost = path.as_os_str().len().saturating_mul(4) + 256;
        self.files.push(
            Leaf {
                path: path.into(),
                size: bytes,
            },
            bytes,
            path.components().count(),
            false,
            cost,
        );
    }
    pub fn directory(&mut self, path: &Path, s: &mut Summary) {
        if path.as_os_str().len() > 4096 {
            self.limited = true;
            return;
        }
        let ext = s.top();
        let tag = detect_compiled(&self.compiled, path);
        s.tagged |= tag.is_some();
        let cost = path.as_os_str().len().saturating_mul(4)
            + 768
            + ext.iter().map(|e| e.ext.len() * 2 + 64).sum::<usize>();
        self.dirs.push(
            Dir {
                path: path.into(),
                size: s.bytes,
                files: s.files,
                ext,
                tag: tag.clone(),
                tagged: s.tagged,
            },
            s.bytes,
            path.components().count(),
            s.tagged,
            cost,
        );
        self.limited |= s.incomplete;
    }
    pub fn finish_root(&mut self, root: &Path, summary: &Summary) {
        self.finish_root_with_budget(root, summary, 60_000);
    }
    fn finish_root_with_budget(&mut self, root: &Path, summary: &Summary, node_limit: usize) {
        let dirs = std::mem::replace(&mut self.dirs, Retained::new(DIR_BYTES, MAX_DIRS));
        let files = std::mem::replace(&mut self.files, Retained::new(FILE_BYTES, MAX_FILES));
        self.limited |= dirs.limited || files.limited;
        let mut records = HashMap::new();
        let mut children: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
        for (dir, _) in dirs.items.into_values() {
            if dir.path != root {
                if let Some(parent) = dir.path.parent() {
                    children
                        .entry(parent.into())
                        .or_default()
                        .push(dir.path.clone());
                }
            }
            records.insert(dir.path.clone(), dir);
        }
        records.insert(
            root.into(),
            Dir {
                path: root.into(),
                size: summary.bytes,
                files: summary.files,
                ext: summary.top(),
                tag: detect_compiled(&self.compiled, root),
                tagged: summary.tagged,
            },
        );
        let mut leaves: HashMap<PathBuf, Vec<Leaf>> = HashMap::new();
        for (leaf, _) in files.items.into_values() {
            if let Some(parent) = leaf.path.parent() {
                leaves.entry(parent.into()).or_default().push(leaf)
            }
        }
        enum Child {
            Directory(Dir),
            File(Leaf),
        }
        impl Child {
            fn size(&self) -> u64 {
                match self {
                    Self::Directory(d) => d.size,
                    Self::File(f) => f.size,
                }
            }
            fn tagged(&self) -> bool {
                matches!(self, Self::Directory(d) if d.tagged)
            }
            fn path(&self) -> &Path {
                match self {
                    Self::Directory(d) => &d.path,
                    Self::File(f) => &f.path,
                }
            }
        }
        // A node's own slot is reserved by its parent. Select and reserve all
        // immediate children before any recursive details can consume the budget.
        fn build(
            dir: Dir,
            records: &mut HashMap<PathBuf, Dir>,
            children: &mut HashMap<PathBuf, Vec<PathBuf>>,
            leaves: &mut HashMap<PathBuf, Vec<Leaf>>,
            budget: &mut usize,
            depth: usize,
            limited: &mut bool,
        ) -> Node {
            let mut candidates = children
                .remove(&dir.path)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|p| records.remove(&p).map(Child::Directory))
                .chain(
                    leaves
                        .remove(&dir.path)
                        .unwrap_or_default()
                        .into_iter()
                        .map(Child::File),
                )
                .collect::<Vec<_>>();
            candidates.sort_by(|a, b| {
                b.tagged()
                    .cmp(&a.tagged())
                    .then_with(|| b.size().cmp(&a.size()))
                    .then_with(|| a.path().cmp(b.path()))
            });
            let width = if depth < 2 {
                100
            } else if depth < 4 {
                50
            } else {
                20
            };
            let keep = width.min(*budget);
            if candidates.len() > keep {
                *limited = true;
                candidates.truncate(keep);
            }
            *budget -= candidates.len();
            let mut nodes = candidates
                .into_iter()
                .map(|child| match child {
                    Child::Directory(child) => {
                        build(child, records, children, leaves, budget, depth + 1, limited)
                    }
                    Child::File(leaf) => Node {
                        name: leaf
                            .path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into(),
                        path: leaf.path.to_string_lossy().into(),
                        is_dir: false,
                        size: leaf.size,
                        file_count: 1,
                        children: vec![],
                        scaffold_id: None,
                        top_extensions: vec![],
                    },
                })
                .collect::<Vec<_>>();
            nodes.sort_by_key(|n| Reverse(n.size));
            Node {
                name: dir
                    .path
                    .file_name()
                    .unwrap_or(dir.path.as_os_str())
                    .to_string_lossy()
                    .into(),
                path: dir.path.to_string_lossy().into(),
                is_dir: true,
                size: dir.size,
                file_count: dir.files,
                children: nodes,
                scaffold_id: dir.tag,
                top_extensions: dir.ext,
            }
        }
        let mut budget = node_limit.saturating_sub(1);
        if node_limit == 0 {
            self.limited = true;
            return;
        }
        if let Some(dir) = records.remove(root) {
            let mut tree = build(
                dir,
                &mut records,
                &mut children,
                &mut leaves,
                &mut budget,
                0,
                &mut self.limited,
            );
            crate::tag_and_truncate(&mut tree, &self.compiled, 0);
            self.trees.insert(root.into(), Arc::new(tree));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_root_file_survives_many_tiny_directories() {
        let root = PathBuf::from("inventory");
        let mut sink = TreeSink::new(Arc::new(vec![]));
        let mut total = Summary::default();
        let large = root.join("large.bin");
        let large_bytes = 100 * 1024 * 1024 * 1024u64;
        sink.file(&large, large_bytes);
        total.file(&large, large_bytes);
        for i in 0..40_000 {
            let dir = root.join(format!("directory-{i:05}"));
            let file = dir.join("small.bin");
            let mut summary = Summary::default();
            summary.file(&file, 1);
            sink.file(&file, 1);
            sink.directory(&dir, &mut summary);
            total.child(&summary);
        }
        sink.finish_root(&root, &total);
        let tree = &sink.trees[&root];
        assert_eq!(tree.size, large_bytes + 40_000);
        assert_eq!(tree.file_count, 40_001);
        assert!(tree.children.iter().any(|n| Path::new(&n.path) == large));
        assert_eq!(tree.children[0].size, large_bytes);
    }

    #[test]
    fn large_sibling_file_is_reserved_before_deep_directory_details() {
        let root = PathBuf::from("inventory");
        let mut sink = TreeSink::new(Arc::new(vec![]));
        let mut total = Summary::default();
        let large = root.join("large.bin");
        total.file(&large, 50);
        sink.file(&large, 50);
        let mut paths = vec![root.join("deeper")];
        for _ in 0..12 {
            paths.push(paths.last().unwrap().join("nested"));
        }
        let file = paths.last().unwrap().join("inside.bin");
        let mut summary = Summary::default();
        summary.file(&file, 100);
        sink.file(&file, 100);
        for path in paths.iter().rev() {
            sink.directory(path, &mut summary);
        }
        total.child(&summary);
        sink.finish_root_with_budget(&root, &total, 5);
        let tree = &sink.trees[&root];
        fn count(node: &Node) -> usize {
            1 + node.children.iter().map(count).sum::<usize>()
        }
        assert_eq!((tree.size, tree.file_count), (150, 2));
        assert!(count(tree) <= 5);
        assert!(tree.children.iter().any(|n| Path::new(&n.path) == large));
        assert!(sink.limited);
    }

    #[test]
    fn tagged_descendant_survives_early_width_selection() {
        let scaffold = pinkbin_scaffold::parse_toml(
            r#"
id = "fixture"
name = "Fixture"
risk = "low"
disclaimer = "test"
detect = ["**/target"]
"#,
        )
        .unwrap();
        let root = PathBuf::from("inventory");
        let mut sink = TreeSink::new(Arc::new(pinkbin_scaffold::compile_all(&[scaffold])));
        let mut total = Summary::default();
        for i in 0..101 {
            let path = root.join(format!("ordinary-{i}"));
            let mut summary = Summary::default();
            summary.file(&path.join("large.bin"), 1000);
            sink.directory(&path, &mut summary);
            total.child(&summary);
        }
        let parent = root.join("known");
        let child = parent.join("target");
        let mut child_summary = Summary::default();
        child_summary.file(&child.join("small.bin"), 1);
        sink.directory(&child, &mut child_summary);
        let mut parent_summary = Summary::default();
        parent_summary.child(&child_summary);
        sink.directory(&parent, &mut parent_summary);
        total.child(&parent_summary);
        sink.finish_root(&root, &total);
        let tree = &sink.trees[&root];
        assert!(tree.children.len() <= 100);
        let known = tree.children.iter().find(|n| n.name == "known").unwrap();
        assert_eq!(known.children[0].scaffold_id.as_deref(), Some("fixture"));
        assert_eq!((tree.size, tree.file_count), (101001, 102));
    }
    #[test]
    fn extensions_and_nested_totals_are_exact_without_content_reads() {
        let mut parent = Summary::default();
        let mut child = Summary::default();
        parent.file(Path::new("a.TXT"), 7);
        child.file(Path::new("b.txt"), 11);
        child.file(Path::new("c.png"), 13);
        parent.child(&child);
        assert_eq!((parent.bytes, parent.files), (31, 3));
        assert_eq!(parent.top()[0].ext, "txt");
        assert_eq!(parent.top()[0].bytes, 18);
        assert_eq!(parent.top()[0].count, 2);
    }
    #[test]
    fn extension_limit_does_not_publish_misleading_partial_percentages() {
        let mut s = Summary::default();
        for i in 0..MAX_EXTENSIONS + 1 {
            s.file(Path::new(&format!("x.{i}")), 1);
        }
        assert!(s.top().is_empty());
        assert_eq!(s.bytes, MAX_EXTENSIONS as u64 + 1);
        let mut parent = Summary::default();
        parent.child(&s);
        assert!(parent.top().is_empty());
    }
    #[test]
    fn bounded_detail_retains_parent_totals_and_largest_children() {
        let root = PathBuf::from("inventory");
        let mut sink = TreeSink::new(Arc::new(vec![]));
        sink.files = Retained::new(4096, 2);
        sink.dirs = Retained::new(4096, 2);
        let mut parent = Summary::default();
        for (name, size) in [("small", 1), ("middle", 3), ("large", 8)] {
            let dir = root.join(name);
            let file = dir.join("x.bin");
            let mut summary = Summary::default();
            summary.file(&file, size);
            sink.file(&file, size);
            sink.directory(&dir, &mut summary);
            parent.child(&summary);
        }
        sink.finish_root(&root, &parent);
        let tree = &sink.trees[&root];
        assert_eq!((tree.size, tree.file_count), (12, 3));
        assert_eq!(tree.children.len(), 2);
        assert_eq!(tree.children[0].name, "large");
        assert_eq!(tree.children[0].children[0].size, 8);
        assert!(sink.limited);
    }
    #[test]
    fn record_budget_and_long_paths_stay_bounded() {
        let mut values = Retained::new(100, 2);
        for i in 0..100 {
            values.push(i, i, 1, false, 40);
        }
        assert_eq!(values.items.len(), 2);
        assert!(values.bytes <= 100);
        assert!(values.limited);
        let mut sink = TreeSink::new(Arc::new(vec![]));
        sink.file(Path::new(&"x".repeat(4097)), 1);
        assert!(sink.files.items.is_empty());
        assert!(sink.limited);
    }
    #[test]
    fn recycle_tree_is_excluded_without_excluding_ordinary_names() {
        assert!(excluded(Path::new("root").join("$RECYCLE.BIN").as_path()));
        assert!(excluded(Path::new("System Volume Information")));
        assert!(!excluded(Path::new("My Trash Photos")));
    }
}
