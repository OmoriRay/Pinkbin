//! Metadata-only allowlists for the full-volume walk; never match by extension alone.
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use crate::rule_catalog::{self, Catalog, Metadata};
use crate::rule_probe::Probe;
#[path = "hf_rules.rs"]
mod hf_rules;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    pub kind: Kind,
    pub app: Arc<str>,
    pub processes: Arc<[String]>,
    pub rule_id: Arc<str>,
    pub title: Arc<str>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Temp,
    Dumps,
    Pip,
    Npm,
    Python,
    AppCache,
    AppLogs,
    PortableCache,
    Reports,
    Nuget,
    Rust,
    LauncherCache,
    IdeCache,
    ExtraBrowserCache,
    PortableBrowserCache,
    RenderCache,
    Cargo,
    InstallerResidue,
    SystemLogs,
    ToolLogs,
    AppDumps,
    HuggingFaceCache,
    HuggingFacePartial,
}
impl Kind {
    pub fn category(self) -> crate::cache_cleaner::RecommendationCategory {
        use crate::cache_cleaner::RecommendationCategory as C;
        match self {
            Self::Temp | Self::InstallerResidue | Self::HuggingFacePartial => C::Temporary,
            Self::Dumps
            | Self::AppLogs
            | Self::Reports
            | Self::SystemLogs
            | Self::ToolLogs
            | Self::AppDumps => C::Diagnostics,
            Self::Pip
            | Self::Npm
            | Self::Python
            | Self::Nuget
            | Self::Rust
            | Self::IdeCache
            | Self::Cargo => C::BuildCache,
            _ => C::ApplicationCache,
        }
    }
    pub fn id(self) -> &'static str {
        match self {
            Self::Temp => "quick-temp",
            Self::Dumps => "quick-dumps",
            Self::Pip => "quick-pip",
            Self::Npm => "quick-npm",
            Self::Python => "quick-python",
            Self::AppCache => "quick-app-cache",
            Self::AppLogs => "quick-app-logs",
            Self::PortableCache => "quick-portable-cache",
            Self::Reports => "quick-reports",
            Self::Nuget => "quick-nuget",
            Self::Rust => "quick-rust",
            Self::LauncherCache => "quick-launcher-cache",
            Self::IdeCache => "quick-ide-cache",
            Self::ExtraBrowserCache => "quick-extra-browser",
            Self::PortableBrowserCache => "quick-portable-browser",
            Self::RenderCache => "quick-render-cache",
            Self::Cargo => "quick-cargo-cache",
            Self::InstallerResidue => "quick-installer-residue",
            Self::SystemLogs => "quick-system-logs",
            Self::ToolLogs => "quick-tool-logs",
            Self::AppDumps => "quick-app-dumps",
            Self::HuggingFaceCache => "quick-hf-xet",
            Self::HuggingFacePartial => "quick-hf-partial",
        }
    }
    pub fn title(self) -> &'static str {
        match self {
            Self::Temp => "过期临时文件",
            Self::Dumps => "旧崩溃转储",
            Self::Pip => "旧 pip 下载缓存",
            Self::Npm => "旧 npm 下载缓存",
            Self::Python => "Python 编译缓存",
            Self::AppCache => "更多应用界面缓存",
            Self::AppLogs => "旧应用日志",
            Self::PortableCache => "便携编辑器缓存",
            Self::Reports => "旧 Windows 错误报告",
            Self::Nuget => "旧 NuGet 下载缓存",
            Self::Rust => "旧 Rust 增量编译缓存",
            Self::LauncherCache => "Epic 启动器网页缓存",
            Self::IdeCache => "JetBrains 旧索引缓存",
            Self::ExtraBrowserCache => "更多浏览器缓存",
            Self::PortableBrowserCache => "独立浏览器缓存",
            Self::RenderCache => "Cinebench 旧渲染缓存",
            Self::Cargo => "旧 Cargo 下载缓存",
            Self::InstallerResidue => "过期安装与解压暂存文件",
            Self::SystemLogs => "旧 Windows 维护归档日志",
            Self::ToolLogs => "旧 npm 运行日志",
            Self::AppDumps => "旧应用崩溃转储",
            Self::HuggingFaceCache => "Hugging Face 过期上传索引缓存",
            Self::HuggingFacePartial => "Hugging Face 旧下载残片",
        }
    }
}

#[derive(Clone)]
pub struct Context {
    pub catalog: Arc<Catalog>,
    pub windows: Option<PathBuf>,
    pub local: Option<PathBuf>,
    pub roaming: Option<PathBuf>,
    pub temp: Vec<PathBuf>,
    pub pip: Vec<PathBuf>,
    pub npm: Vec<PathBuf>,
    pub nuget: Vec<PathBuf>,
    pub cargo: Vec<PathBuf>,
    pub program_data: Option<PathBuf>,
    pub huggingface_xet: Vec<PathBuf>,
}
impl Default for Context {
    fn default() -> Self {
        Self {
            catalog: rule_catalog::builtin(),
            windows: None,
            local: None,
            roaming: None,
            temp: vec![],
            pip: vec![],
            npm: vec![],
            nuget: vec![],
            cargo: vec![],
            program_data: None,
            huggingface_xet: vec![],
        }
    }
}
impl Context {
    pub fn current() -> Self {
        fn absolute(name: &str) -> Option<PathBuf> {
            std::env::var_os(name)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
        }
        let local = absolute("LOCALAPPDATA");
        let mut temp = Vec::new();
        if let Some(local) = &local {
            temp.push(local.join("Temp"));
        }
        for name in ["TEMP", "TMP"] {
            // Reject drive/profile roots accidentally configured as TEMP.
            if let Some(path) = absolute(name).filter(|p| {
                matches!(
                    p.file_name()
                        .and_then(|s| s.to_str())
                        .map(str::to_ascii_lowercase)
                        .as_deref(),
                    Some("temp" | "tmp")
                )
            }) {
                if !temp.contains(&path) {
                    temp.push(path);
                }
            }
        }
        let mut pip = local
            .iter()
            .map(|p| p.join("pip/Cache"))
            .collect::<Vec<_>>();
        if let Some(p) = absolute("PIP_CACHE_DIR") {
            pip.push(p);
        }
        let mut npm = local
            .iter()
            .map(|p| p.join("npm-cache"))
            .collect::<Vec<_>>();
        if let Some(p) = absolute("NPM_CONFIG_CACHE") {
            npm.push(p);
        }
        let windows = absolute("SystemRoot");
        if let Some(windows) = &windows {
            temp.push(windows.join("Temp"));
        }
        let mut nuget = local
            .iter()
            .map(|p| p.join("NuGet/v3-cache"))
            .collect::<Vec<_>>();
        if let Some(path) = absolute("NUGET_HTTP_CACHE_PATH") {
            nuget.push(path);
        }
        let cargo = absolute("CARGO_HOME")
            .or_else(|| absolute("USERPROFILE").map(|p| p.join(".cargo")))
            .into_iter()
            .collect();
        Self {
            cargo,
            huggingface_xet: absolute("HF_XET_CACHE")
                .or_else(|| absolute("HF_HOME").map(|p| p.join("xet")))
                .or_else(|| absolute("XDG_CACHE_HOME").map(|p| p.join("huggingface/xet")))
                .or_else(|| absolute("USERPROFILE").map(|p| p.join(".cache/huggingface/xet")))
                .into_iter()
                .collect(),
            catalog: rule_catalog::current(),
            nuget,
            program_data: absolute("PROGRAMDATA"),
            windows,
            local,
            roaming: absolute("APPDATA"),
            temp,
            pip,
            npm,
        }
    }
    pub fn identify(&self, path: &Path, modified: SystemTime, now: SystemTime) -> Option<Match> {
        self.identify_with(
            path,
            modified,
            now,
            &Probe {
                profiles: None,
                marker: &safe_marker,
            },
        )
    }
    pub(crate) fn identify_with(
        &self,
        path: &Path,
        modified: SystemTime,
        now: SystemTime,
        probe: &Probe<'_>,
    ) -> Option<Match> {
        if path.components().any(|p| {
            let name = p.as_os_str().to_string_lossy().to_ascii_lowercase();
            matches!(
                name.as_str(),
                "backup" | "backups" | "archive" | "archives" | "归档"
            ) || name.contains(".corrupt-backup")
                || name.contains(".repair-old")
        }) {
            return None;
        }
        let age = now.duration_since(modified).ok()?.as_secs() / 86400;
        if self.catalog.schema_version >= 5
            && age >= self.catalog.policy("hf-xet").min_age_days
            && self
                .huggingface_xet
                .iter()
                .any(|root| hf_rules::xet_shard(path, root))
        {
            return Some(self.policy_match(Kind::HuggingFaceCache, "hf-xet"));
        }
        if self.catalog.schema_version >= 5
            && age >= self.catalog.policy("hf-partial").min_age_days
            && hf_rules::completed_download_residue(path, modified, now)
        {
            return Some(self.policy_match(Kind::HuggingFacePartial, "hf-partial"));
        }
        let ext = path
            .extension()
            .unwrap_or_default()
            .to_string_lossy()
            .to_ascii_lowercase();
        if self.catalog.schema_version >= 6
            && path
                .file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case("installer.exe"))
        {
            if let Some(local) = &self.local {
                for rule in &self.catalog.updater_caches {
                    if !rule.enabled
                        || !rule.reviewed
                        || age
                            < rule
                                .min_age_days
                                .max(self.catalog.policy("installer-residue").min_age_days)
                        || !rule_catalog::beneath(path, &local.join(&rule.directory))
                            .is_some_and(|relative| relative.components().count() == 1)
                    {
                        continue;
                    }
                    let profile = self
                        .catalog
                        .profiles
                        .iter()
                        .find(|p| p.id == rule.profile_id)?;
                    if !profile.enabled || !profile.reviewed {
                        continue;
                    }
                    let has_app = profile.locations.iter().any(|location| {
                        let base = if location.root == "local" {
                            &self.local
                        } else {
                            &self.roaming
                        };
                        base.as_ref().is_some_and(|base| {
                            (probe.marker)(&base.join(&location.path).join("Local State"))
                        })
                    });
                    if has_app {
                        let metadata = rule.meta.as_ref()?;
                        let mut found = matched(Kind::InstallerResidue, metadata);
                        found.title = metadata.cache_title.clone();
                        return Some(found);
                    }
                }
            }
        }
        if let Some(root) = self
            .temp
            .iter()
            .find(|root| crate::maintenance_rules::installer_workspace(path, root))
        {
            return crate::maintenance_rules::installer_residue(
                path,
                root,
                modified,
                now,
                self.catalog.policy("installer-residue").min_age_days,
            )
            .then(|| self.policy_match(Kind::InstallerResidue, "installer-residue"));
        }
        if self.windows.as_ref().is_some_and(|root| {
            crate::maintenance_rules::system_log(
                path,
                root,
                modified,
                now,
                self.catalog.policy("system-logs").min_age_days,
            )
        }) {
            return Some(self.policy_match(Kind::SystemLogs, "system-logs"));
        }
        if self.npm.iter().any(|root| {
            crate::maintenance_rules::npm_log(
                path,
                root,
                modified,
                now,
                self.catalog
                    .policy("npm")
                    .min_age_days
                    .max(self.catalog.policy("logs").min_age_days),
            )
        }) {
            return Some(self.policy_match(Kind::ToolLogs, "npm"));
        }
        if age >= self.catalog.policy("temp").min_age_days
            && matches!(ext.as_str(), "tmp" | "temp" | "log" | "etl")
            && self.temp.iter().any(|root| path.starts_with(root))
            && !path.components().any(|p| {
                matches!(
                    p.as_os_str()
                        .to_string_lossy()
                        .to_ascii_lowercase()
                        .as_str(),
                    "autorecover" | "recovery" | "unsavedfiles"
                )
            })
        {
            return Some(self.policy_match(Kind::Temp, "temp"));
        }
        if age >= self.catalog.policy("dumps").min_age_days
            && matches!(ext.as_str(), "dmp" | "mdmp")
            && (self
                .local
                .as_ref()
                .is_some_and(|root| path.parent() == Some(root.join("CrashDumps").as_path()))
                || self.windows.as_ref().is_some_and(|root| {
                    path.parent() == Some(root.join("Minidump").as_path())
                        || path.starts_with(root.join("LiveKernelReports"))
                        || (path.parent() == Some(root.as_path())
                            && path
                                .file_name()
                                .is_some_and(|name| name.eq_ignore_ascii_case("MEMORY.DMP")))
                }))
        {
            return Some(self.policy_match(Kind::Dumps, "dumps"));
        }
        if age >= self.catalog.policy("pip").min_age_days
            && self.pip.iter().any(|root| {
                ["http", "http-v2", "wheels"]
                    .iter()
                    .any(|sub| path.starts_with(root.join(sub)))
            })
        {
            return Some(self.policy_match(Kind::Pip, "pip"));
        }
        if age >= self.catalog.policy("npm").min_age_days
            && self.npm.iter().any(|root| {
                ["_cacache/content-v2", "_cacache/index-v5"]
                    .iter()
                    .any(|sub| path.starts_with(root.join(sub)))
            })
        {
            return Some(self.policy_match(Kind::Npm, "npm"));
        }
        if age >= self.catalog.policy("python").min_age_days
            && ext == "pyc"
            && python_source(path, probe.marker).is_some()
        {
            return Some(self.policy_match(Kind::Python, "python"));
        }
        if let Some(rule) = self.additional_match(path, age, probe) {
            return Some(rule);
        }
        // The cache files themselves commonly have no extension.
        self.app_match(path, age, modified, now, probe)
    }
    /// Narrow traversal routes through otherwise protected system containers.
    pub fn restricted_children(&self, path: &Path) -> Option<Vec<PathBuf>> {
        if self.windows.as_deref() == Some(path) {
            return Some(
                [
                    "Temp",
                    "Minidump",
                    "LiveKernelReports",
                    "MEMORY.DMP",
                    "Logs",
                ]
                .iter()
                .map(|name| path.join(name))
                .collect(),
            );
        }
        if self
            .windows
            .as_ref()
            .is_some_and(|root| path == root.join("Logs"))
        {
            return Some(["CBS", "DISM"].iter().map(|name| path.join(name)).collect());
        }
        if let Some(base) = &self.program_data {
            for (part, child) in [
                ("", "Microsoft"),
                ("Microsoft", "Windows"),
                ("Microsoft/Windows", "WER"),
                ("Microsoft/Windows/WER", "ReportArchive"),
            ] {
                if path == base.join(part) {
                    return Some(vec![path.join(child)]);
                }
            }
        }
        if let Some(base) = &self.local {
            for (part, child) in [
                ("Microsoft/Windows", "WER"),
                ("Microsoft/Windows/WER", "ReportArchive"),
            ] {
                if path == base.join(part) {
                    return Some(vec![path.join(child)]);
                }
            }
        }
        None
    }
    // New system routes expose only the exact archived-log formats to candidate creation.
    pub fn readonly_maintenance_entry(&self, path: &Path) -> bool {
        let Some(root) = &self.windows else {
            return false;
        };
        let logs = root.join("Logs");
        let Some(relative) = rule_catalog::beneath(path, &logs) else {
            return false;
        };
        if relative.as_os_str().is_empty() {
            return false;
        }
        if relative.components().count() == 1 {
            return !["CBS", "DISM"]
                .iter()
                .any(|name| relative.as_os_str().eq_ignore_ascii_case(name));
        }
        !crate::maintenance_rules::system_log_shape(relative)
    }
    fn policy_match(&self, kind: Kind, name: &str) -> Match {
        matched(
            kind,
            self.catalog
                .policy(name)
                .meta
                .as_ref()
                .expect("validated policy"),
        )
    }
    fn additional_match(&self, path: &Path, age: u64, probe: &Probe<'_>) -> Option<Match> {
        if age >= self.catalog.policy("cargo").min_age_days
            && self
                .cargo
                .iter()
                .any(|home| cargo_archive(path, home, probe.marker))
        {
            return Some(self.policy_match(Kind::Cargo, "cargo"));
        }
        if age >= self.catalog.policy("nuget").min_age_days
            && self
                .nuget
                .iter()
                .any(|root| path.strip_prefix(root).is_ok_and(nuget_response))
        {
            return Some(self.policy_match(Kind::Nuget, "nuget"));
        }
        if age >= self.catalog.policy("reports").min_age_days
            && self
                .local
                .iter()
                .chain(self.program_data.iter())
                .any(|root| {
                    path.strip_prefix(root.join("Microsoft/Windows/WER/ReportArchive"))
                        .is_ok_and(archived_report)
                })
        {
            return Some(self.policy_match(Kind::Reports, "reports"));
        }
        if age >= self.catalog.policy("rust").min_age_days && rust_incremental(path, probe.marker) {
            return Some(self.policy_match(Kind::Rust, "rust"));
        }
        for profile in path.ancestors().skip(1).take(256) {
            if !profile
                .file_name()
                .is_some_and(|s| s.eq_ignore_ascii_case("User Data"))
            {
                continue;
            }
            let relative = path.strip_prefix(profile).ok()?;
            if rule_catalog::protected(relative) || !chromium_cache(&self.catalog, relative) {
                continue;
            }
            if self.local.as_ref().is_some_and(|local| {
                ["chrome", "edge", "brave"].iter().any(|id| {
                    self.catalog
                        .legacy
                        .iter()
                        .find(|r| r.id == *id)
                        .is_some_and(|r| {
                            rule_catalog::beneath(profile, &local.join(&r.prefix))
                                .is_some_and(|p| p.as_os_str().is_empty())
                        })
                })
            }) {
                continue;
            }
            // A portable fallback must not bypass a configured profile's age/exclusions.
            if self
                .catalog
                .profiles
                .iter()
                .filter(|r| r.family == "chromium")
                .any(|rule| {
                    rule.locations.iter().any(|location| {
                        let base = if location.root == "local" {
                            &self.local
                        } else {
                            &self.roaming
                        };
                        base.as_ref().is_some_and(|base| {
                            rule_catalog::beneath(profile, &base.join(&location.path))
                                .is_some_and(|rest| rest.as_os_str().is_empty())
                        })
                    })
                })
            {
                continue;
            }
            let install = profile.parent()?;
            if !(probe.marker)(&profile.join("Local State")) {
                continue;
            }
            for rule in &self.catalog.portable_browsers {
                if (probe.marker)(&install.join("Application").join(&rule.executable))
                    || (probe.marker)(&install.join(&rule.executable))
                {
                    return Some(matched(Kind::PortableBrowserCache, rule.meta.as_ref()?));
                }
            }
        }
        None
    }
    fn app_match(
        &self,
        path: &Path,
        age: u64,
        modified: SystemTime,
        now: SystemTime,
        probe: &Probe<'_>,
    ) -> Option<Match> {
        let likely_cache = path.ancestors().skip(1).take(32).any(|p| {
            p.file_name().is_some_and(|name| {
                matches!(
                    name.to_string_lossy().to_ascii_lowercase().as_str(),
                    "cache"
                        | "cache_data"
                        | "cache2"
                        | "code cache"
                        | "gpucache"
                        | "dawncache"
                        | "dawngraphitecache"
                        | "dawnwebgpucache"
                        | "cacheddata"
                        | "shadercache"
                        | "grshadercache"
                        | "graphitedawncache"
                        | "gpupersistentcache"
                        | "caches"
                        | "index"
                        | "textures"
                        | "meshes"
                        | "compiledshaders"
                )
            })
        });
        let session_log = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("txt"))
            && path.parent().and_then(Path::file_name).is_some_and(|p| {
                p.eq_ignore_ascii_case("logs") || p.eq_ignore_ascii_case("crashes")
            });
        if !likely_cache
            && !session_log
            && !path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("log") || e.eq_ignore_ascii_case("dmp"))
        {
            return None;
        }
        if let Some(profiles) = probe.profiles {
            for resolved in profiles {
                if let Some(found) = self.match_profile(
                    &self.catalog.profiles[resolved.index],
                    &resolved.root,
                    path,
                    modified,
                    now,
                    probe,
                ) {
                    return Some(found);
                }
            }
        } else {
            for rule in &self.catalog.profiles {
                if age < rule.min_age_days || !rule.enabled || !rule.reviewed {
                    continue;
                }
                for location in &rule.locations {
                    let base = match location.root.as_str() {
                        "local" => &self.local,
                        "roaming" => &self.roaming,
                        _ => continue,
                    };
                    if let Some(base) = base {
                        if let Some(found) = self.match_profile(
                            rule,
                            &base.join(&location.path),
                            path,
                            modified,
                            now,
                            probe,
                        ) {
                            return Some(found);
                        }
                    }
                }
            }
        }
        for install in path.ancestors().skip(1).take(12) {
            for rule in &self.catalog.portable_apps {
                if !rule.enabled || !rule.reviewed {
                    continue;
                }
                for data_dir in &rule.data_dirs {
                    let root = install.join(data_dir);
                    let Some(relative) = rule_catalog::beneath(path, &root) else {
                        continue;
                    };
                    if rule_catalog::protected(relative) {
                        continue;
                    }
                    let cache = if rule.family == "wallpaper" {
                        wallpaper_cache(&self.catalog, relative, &root, probe.marker)
                    } else if rule.family == "electron" {
                        electron_cache(&self.catalog, relative, rule.partitions)
                    } else {
                        chromium_cache(&self.catalog, relative)
                            && (probe.marker)(&root.join("Local State"))
                    };
                    let maintenance = crate::maintenance_rules::app_kind(
                        &rule.maintenance,
                        &root,
                        relative,
                        modified,
                        now,
                        self.catalog.policy("logs").min_age_days,
                        self.catalog.policy("dumps").min_age_days,
                    );
                    if (cache || maintenance.is_some())
                        && (probe.marker)(&install.join(&rule.executable))
                        && (rule.markers_any.is_empty()
                            || rule
                                .markers_any
                                .iter()
                                .any(|p| (probe.marker)(&install.join(p))))
                    {
                        return Some(matched(
                            maintenance.unwrap_or(Kind::PortableCache),
                            rule.meta.as_ref()?,
                        ));
                    }
                }
            }
        }
        for ancestor in path.ancestors().skip(1).take(256) {
            if !ancestor
                .file_name()
                .is_some_and(|s| s.eq_ignore_ascii_case("user-data"))
            {
                continue;
            }
            let data = ancestor.parent()?;
            if !data
                .file_name()
                .is_some_and(|s| s.eq_ignore_ascii_case("data"))
            {
                continue;
            }
            let install = data.parent()?;
            let relative = path.strip_prefix(ancestor).ok()?;
            if rule_catalog::protected(relative)
                || !ui_cache(&self.catalog, relative)
                || !(probe.marker)(&install.join("resources/app/product.json"))
            {
                continue;
            }
            for rule in &self.catalog.portable_editors {
                if (probe.marker)(&install.join(&rule.executable)) {
                    return Some(matched(Kind::PortableCache, rule.meta.as_ref()?));
                }
            }
        }
        None
    }
    fn match_profile(
        &self,
        rule: &rule_catalog::Profile,
        root: &Path,
        path: &Path,
        modified: SystemTime,
        now: SystemTime,
        probe: &Probe<'_>,
    ) -> Option<Match> {
        let age = now.duration_since(modified).ok()?.as_secs() / 86400;
        if age < rule.min_age_days || !rule.enabled || !rule.reviewed {
            return None;
        }
        let relative = rule_catalog::beneath(path, root)?;
        if !rule.allows(relative)
            || (!rule.markers_any.is_empty()
                && !rule
                    .markers_any
                    .iter()
                    .any(|m| (probe.marker)(&root.join(m))))
        {
            return None;
        }
        if rule.family == "diagnostics" {
            return crate::maintenance_rules::diagnostic_kind(
                &rule.maintenance,
                root,
                relative,
                modified,
                now,
                self.catalog
                    .policy("logs")
                    .min_age_days
                    .max(rule.min_age_days),
            )
            .map(|kind| {
                let mut found = matched(
                    kind,
                    rule.meta.as_ref().expect("validated diagnostic profile"),
                );
                if kind == Kind::AppDumps {
                    found.title = std::sync::Arc::from(format!("{} 旧崩溃报告", rule.app));
                }
                found
            });
        }
        if let Some(kind) = crate::maintenance_rules::app_kind(
            &rule.maintenance,
            root,
            relative,
            modified,
            now,
            self.catalog.policy("logs").min_age_days,
            self.catalog.policy("dumps").min_age_days,
        ) {
            return Some(matched(kind, rule.meta.as_ref()?));
        }
        let kind = match rule.family.as_str() {
            "electron" if electron_cache(&self.catalog, relative, rule.partitions) => {
                Kind::AppCache
            }
            "electron"
                if rule.logs
                    && age >= self.catalog.policy("logs").min_age_days
                    && path
                        .extension()
                        .is_some_and(|e| e.eq_ignore_ascii_case("log"))
                    && editor_log(relative) =>
            {
                Kind::AppLogs
            }
            "chromium" if chromium_cache(&self.catalog, relative) => Kind::ExtraBrowserCache,
            "epic" if epic_cache(&self.catalog, relative) => Kind::LauncherCache,
            "wegame" if wegame_cache(&self.catalog, relative, root, probe.marker) => Kind::AppCache,
            "firefox" if firefox_cache(relative) => Kind::ExtraBrowserCache,
            "cinebench" if cinebench_cache(relative) => Kind::RenderCache,
            "jetbrains" if jetbrains_cache(relative, rule.product_prefix.as_deref()?) => {
                Kind::IdeCache
            }
            _ => return None,
        };
        Some(matched(kind, rule.meta.as_ref()?))
    }
}
fn matched(kind: Kind, meta: &Metadata) -> Match {
    let title = match kind {
        Kind::AppCache
        | Kind::PortableCache
        | Kind::LauncherCache
        | Kind::ExtraBrowserCache
        | Kind::PortableBrowserCache => meta.cache_title.clone(),
        Kind::AppLogs => meta.logs_title.clone(),
        Kind::AppDumps => meta.dumps_title.clone(),
        Kind::IdeCache => meta.indexes_title.clone(),
        _ => Arc::from(kind.title()),
    };
    Match {
        title,
        kind,
        app: meta.app.clone(),
        processes: meta.processes.clone(),
        rule_id: meta.id.clone(),
    }
}
fn cargo_archive(path: &Path, home: &Path, marker: &dyn Fn(&Path) -> bool) -> bool {
    let Some(relative) = rule_catalog::beneath(path, &home.join("registry/cache")) else {
        return false;
    };
    let parts: Vec<_> = relative.components().collect();
    if parts.len() != 2
        || !path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("crate"))
    {
        return false;
    }
    let Some(package) = path.file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    if package.len() > 180
        || !package
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return false;
    }
    marker(
        &home
            .join("registry/src")
            .join(parts[0].as_os_str())
            .join(package)
            .join("Cargo.toml"),
    )
}
fn wegame_cache(
    catalog: &Catalog,
    relative: &Path,
    root: &Path,
    marker: &dyn Fn(&Path) -> bool,
) -> bool {
    let mut parts = relative.components();
    let Some(version) = parts.next() else {
        return false;
    };
    let version = version.as_os_str().to_string_lossy();
    if !version
        .strip_prefix("qbcore")
        .is_some_and(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
    {
        return false;
    }
    let Some(cache) = parts.next() else {
        return false;
    };
    if !cache.as_os_str().eq_ignore_ascii_case("cache") {
        return false;
    }
    let Some(browser) = parts.next() else {
        return false;
    };
    let browser = browser.as_os_str().to_string_lossy();
    if !browser
        .strip_prefix("qbcore_cache_browser.exe_")
        .is_some_and(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
    {
        return false;
    }
    marker(
        &root
            .join(version.as_ref())
            .join("cache")
            .join(browser.as_ref())
            .join("LocalPrefs.json"),
    ) && ui_cache(catalog, parts.as_path())
}
fn wallpaper_cache(
    catalog: &Catalog,
    relative: &Path,
    root: &Path,
    marker: &dyn Fn(&Path) -> bool,
) -> bool {
    let Some(rest) = rule_catalog::beneath(relative, Path::new("browsewallpapers/base")) else {
        return false;
    };
    marker(&root.join("browsewallpapers/base/Local State")) && chromium_cache(catalog, rest)
}
fn ui_cache(catalog: &Catalog, relative: &Path) -> bool {
    rule_catalog::in_template(catalog, "electron", relative)
}
fn electron_cache(catalog: &Catalog, relative: &Path, partitions: bool) -> bool {
    if ui_cache(catalog, relative)
        || rule_catalog::in_template(catalog, "chromium_shared", relative)
    {
        return true;
    }
    if !partitions {
        return false;
    }
    let Some(rest) = rule_catalog::beneath(relative, Path::new("Partitions")) else {
        return false;
    };
    let mut parts = rest.components();
    let Some(name) = parts.next() else {
        return false;
    };
    let name = name.as_os_str().to_string_lossy();
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && (ui_cache(catalog, parts.as_path())
            || rule_catalog::in_template(catalog, "chromium_shared", parts.as_path()))
}
fn firefox_cache(relative: &Path) -> bool {
    let mut parts = relative.components();
    if parts.next().is_none() {
        return false;
    }
    let Some(file) = rule_catalog::beneath(parts.as_path(), Path::new("cache2/entries")) else {
        return false;
    };
    file.components().count() == 1
        && file.file_name().is_some_and(|name| {
            let name = name.to_string_lossy();
            name.len() == 40
                && name
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b))
        })
}
fn cinebench_cache(relative: &Path) -> bool {
    let mut parts = relative.components();
    let Some(folder) = parts.next() else {
        return false;
    };
    let folder = folder.as_os_str().to_string_lossy();
    let Some(version) = folder.strip_prefix("CinebenchR") else {
        return false;
    };
    if version.len() > 48
        || !version.starts_with(|c: char| c.is_ascii_digit())
        || !version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return false;
    }
    let Some(cache) = rule_catalog::beneath(parts.as_path(), Path::new("Redshift/Cache")) else {
        return false;
    };
    let mut cache_parts = cache.components();
    let Some(kind) = cache_parts.next() else {
        return false;
    };
    if cache_parts.as_path().as_os_str().is_empty() {
        return false;
    }
    let kind = kind.as_os_str().to_string_lossy();
    let ext = relative
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase());
    match kind.as_ref() {
        "Textures" => matches!(ext.as_deref(), Some("rstexbin" | "tx")),
        "Meshes" => ext.is_none(),
        "CompiledShaders" => matches!(ext.as_deref(), None | Some("bin" | "ptx" | "cubin")),
        _ => false,
    }
}
fn epic_cache(catalog: &Catalog, relative: &Path) -> bool {
    let mut parts = relative.components();
    let Some(cache) = parts.next() else {
        return false;
    };
    let name = cache.as_os_str().to_string_lossy();
    (name == "webcache"
        || name
            .strip_prefix("webcache_")
            .is_some_and(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_digit())))
        && web_cache(catalog, parts.as_path())
}
fn jetbrains_cache(relative: &Path, prefix: &str) -> bool {
    let mut parts = relative.components();
    let Some(folder) = parts.next() else {
        return false;
    };
    let name = folder.as_os_str().to_string_lossy();
    name.get(..prefix.len())
        .is_some_and(|v| v.eq_ignore_ascii_case(prefix))
        && name.get(prefix.len()..).is_some_and(version_folder)
        && ["caches", "index"].iter().any(|dir| {
            rule_catalog::beneath(parts.as_path(), Path::new(dir))
                .is_some_and(|rest| !rest.as_os_str().is_empty())
        })
}
fn editor_log(relative: &Path) -> bool {
    let Ok(rest) = relative.strip_prefix("logs") else {
        return false;
    };
    let Some(session) = rest.components().next() else {
        return false;
    };
    let name = session.as_os_str().to_string_lossy();
    name.len() == 15
        && name.as_bytes()[8] == b'T'
        && name
            .chars()
            .enumerate()
            .all(|(i, c)| i == 8 || c.is_ascii_digit())
}
fn python_source(path: &Path, marker: &dyn Fn(&Path) -> bool) -> Option<PathBuf> {
    let cache = path.parent()?;
    if !cache.file_name()?.eq_ignore_ascii_case("__pycache__") {
        return None;
    }
    let name = path.file_name()?.to_str()?;
    let (module, tag) = name
        .split_once(".cpython-")
        .or_else(|| name.split_once(".pypy"))?;
    if module.is_empty() || !tag.ends_with(".pyc") {
        return None;
    }
    let source = cache.parent()?.join(format!("{module}.py"));
    marker(&source).then_some(source)
}
pub(crate) fn safe_marker(path: &Path) -> bool {
    // A marker must be an actual, accessible regular file, with no link ancestors.
    path.ancestors().all(|p| {
        std::fs::symlink_metadata(p).is_ok_and(|m| {
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if m.file_attributes() & (0x400 | 0x1000 | 0x40000 | 0x400000) != 0 {
                    return false;
                }
            }
            !m.file_type().is_symlink()
        })
    }) && std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
}

fn version_folder(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_digit())
        && name.len() <= 32
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}
fn archived_report(relative: &Path) -> bool {
    let mut parts = relative.components();
    let Some(folder) = parts.next() else {
        return false;
    };
    let name = folder.as_os_str().to_string_lossy().to_ascii_lowercase();
    [
        "appcrash_",
        "apphang_",
        "kernel_",
        "critical_",
        "noncritical_",
        "bluescreen_",
        "crash_",
        "resourceexhaustion_",
        "criticalservice_",
        "devicedriver_",
        "storeagent_",
        "moca_",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
        && parts.next().is_some()
        && relative.extension().is_some_and(|e| {
            matches!(
                e.to_string_lossy().to_ascii_lowercase().as_str(),
                "wer" | "dmp" | "mdmp" | "hdmp" | "xml" | "txt" | "csv" | "etl" | "cab" | "tmp"
            )
        })
}
fn nuget_response(relative: &Path) -> bool {
    relative.components().count() == 2
        && relative
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("dat"))
        && relative.file_name().is_some_and(|name| {
            [
                "list_",
                "nupkg_",
                "service_index",
                "repository_signatures",
                "vulnerability_",
            ]
            .iter()
            .any(|prefix| name.to_string_lossy().starts_with(prefix))
        })
}
fn rust_incremental(path: &Path, marker: &dyn Fn(&Path) -> bool) -> bool {
    if !path.extension().is_some_and(|e| {
        matches!(
            e.to_string_lossy().as_ref(),
            "bin" | "o" | "bc" | "rmeta" | "d"
        )
    }) {
        return false;
    }
    for incremental in path.ancestors().skip(1).take(256) {
        if !incremental
            .file_name()
            .is_some_and(|name| name == "incremental")
        {
            continue;
        }
        let Some(profile) = incremental.parent() else {
            continue;
        };
        if !profile
            .file_name()
            .is_some_and(|name| name == "debug" || name == "release")
        {
            continue;
        }
        let Some(parent) = profile.parent() else {
            continue;
        };
        let target = if parent.file_name().is_some_and(|name| name == "target") {
            parent
        } else if let Some(target) = parent
            .parent()
            .filter(|p| p.file_name().is_some_and(|name| name == "target"))
        {
            target
        } else {
            continue;
        };
        if target
            .parent()
            .is_some_and(|project| marker(&project.join("Cargo.toml")))
        {
            return true;
        }
    }
    false
}
fn web_cache(catalog: &Catalog, relative: &Path) -> bool {
    ui_cache(catalog, relative)
        || rule_catalog::beneath(relative, Path::new("Default"))
            .is_some_and(|p| ui_cache(catalog, p))
}
fn chromium_cache(catalog: &Catalog, relative: &Path) -> bool {
    if rule_catalog::in_template(catalog, "chromium_shared", relative) {
        return true;
    }
    let mut parts = relative.components();
    let Some(profile) = parts.next() else {
        return false;
    };
    let profile = profile.as_os_str().to_string_lossy();
    (profile.eq_ignore_ascii_case("Default")
        || profile
            .strip_prefix("Profile ")
            .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())))
        && rule_catalog::in_template(catalog, "chromium_profile", parts.as_path())
}

#[cfg(test)]
mod catalog_tests {
    use super::*;
    #[test]
    fn portable_fallback_respects_configured_browser_exclusions() {
        let temp = tempfile::tempdir().unwrap();
        let install = temp.path().join("Local/Vivaldi");
        std::fs::create_dir_all(install.join("Application")).unwrap();
        std::fs::create_dir_all(install.join("User Data/Default/Cache/Cache_Data")).unwrap();
        std::fs::write(install.join("Application/vivaldi.exe"), b"fixture").unwrap();
        std::fs::write(install.join("User Data/Local State"), b"fixture").unwrap();
        let file = install.join("User Data/Default/Cache/Cache_Data/item");
        let context = Context {
            local: Some(temp.path().join("Local")),
            ..Context::default()
        };
        let now = SystemTime::now();
        assert_eq!(
            context.identify(&file, now, now).unwrap().kind,
            Kind::ExtraBrowserCache
        );
        let mut value: serde_json::Value = serde_json::from_str(rule_catalog::BUILTIN).unwrap();
        let index = value["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .position(|p| p["id"] == "vivaldi")
            .unwrap();
        value["profiles"][index]["exclude"] = serde_json::json!(["Default/Cache/**"]);
        let next = Context {
            catalog: Catalog::parse(&serde_json::to_string(&value).unwrap()).unwrap(),
            ..context
        };
        assert!(next.identify(&file, now, now).is_none());
    }
    #[test]
    fn new_rules_load_without_engine_changes_and_keep_captured_catalog() {
        let mut value: serde_json::Value = serde_json::from_str(rule_catalog::BUILTIN).unwrap();
        let mut rule = value["profiles"][0].clone();
        rule["id"] = "fixture-app".into();
        rule["locations"][0]["path"] = "FixtureApp".into();
        rule["exclude"] = serde_json::json!(["**/keep/**"]);
        value["profiles"].as_array_mut().unwrap().push(rule);
        let catalog = Catalog::parse(&serde_json::to_string(&value).unwrap()).unwrap();
        let context = Context {
            catalog,
            roaming: Some(PathBuf::from("R:/Roaming")),
            ..Context::default()
        };
        let now = SystemTime::now();
        let file = Path::new("r:/roaming/fixtureapp/Cache/Cache_Data/item");
        assert_eq!(
            context.identify(file, now, now).unwrap().rule_id.as_ref(),
            "fixture-app"
        );
        assert!(Context::default().identify(file, now, now).is_none());
        for suffix in [
            "Cache/Cache_Data/keep/file",
            "Cache/Cache_Data/User/note",
            "IndexedDB/state",
            "Local Storage/state",
            "Cache/Cache_Data/item.db",
            "Media Cache/item",
        ] {
            assert!(
                context
                    .identify(&Path::new("R:/Roaming/FixtureApp").join(suffix), now, now)
                    .is_none(),
                "{suffix}"
            );
        }
        value["profiles"]
            .as_array_mut()
            .unwrap()
            .last_mut()
            .unwrap()["enabled"] = false.into();
        let next = Context {
            catalog: Catalog::parse(&serde_json::to_string(&value).unwrap()).unwrap(),
            ..context.clone()
        };
        assert!(next.identify(file, now, now).is_none());
        assert!(context.identify(file, now, now).is_some());
    }
    fn fixture(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"owned fixture").unwrap();
    }
    #[test]
    fn installed_updater_copies_are_old_scoped_and_never_pending() {
        let dir = tempfile::tempdir().unwrap();
        let context = Context {
            local: Some(dir.path().join("Local")),
            roaming: Some(dir.path().join("Roaming")),
            ..Default::default()
        };
        let now = SystemTime::now();
        let old = now - std::time::Duration::from_secs(31 * 86400);
        for rule in &context.catalog.updater_caches {
            let profile = context
                .catalog
                .profiles
                .iter()
                .find(|p| p.id == rule.profile_id)
                .unwrap();
            let location = &profile.locations[0];
            let marker = context
                .roaming
                .as_ref()
                .unwrap()
                .join(&location.path)
                .join("Local State");
            fixture(&marker);
            let root = context.local.as_ref().unwrap().join(&rule.directory);
            let installer = root.join("installer.exe");
            fixture(&installer);
            let found = context.identify(&installer, old, now).unwrap();
            assert_eq!(found.kind, Kind::InstallerResidue);
            assert_eq!(found.rule_id.as_ref(), profile.id);
            assert!(found.title.ends_with("旧更新安装包"));
            for suffix in [
                "pending/installer.exe",
                "pending/update.exe",
                "pending/update-info.json",
                "package.7z",
                "update.exe",
                "setup.exe",
                "installer.exe.bak",
                "nested/installer.exe",
            ] {
                assert!(
                    context.identify(&root.join(suffix), old, now).is_none(),
                    "{suffix}"
                );
            }
            assert!(context.identify(&installer, now, now).is_none());
            assert!(context
                .identify(&installer, now + std::time::Duration::from_secs(1), now)
                .is_none());
            std::fs::remove_file(marker).unwrap();
            assert!(context.identify(&installer, old, now).is_none());
        }
        assert!(context
            .identify(
                &context
                    .local
                    .as_ref()
                    .unwrap()
                    .join("unknown-updater/installer.exe"),
                old,
                now
            )
            .is_none());
        let old_catalog = Catalog::parse(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../rules/cleanup-catalog-v5.json"
        )))
        .unwrap();
        let previous = Context {
            catalog: old_catalog,
            ..context.clone()
        };
        assert!(previous
            .identify(
                &dir.path().join("Local/cherrystudio-updater/installer.exe"),
                old,
                now
            )
            .is_none());
    }
    #[test]
    fn steam_web_cache_keeps_accounts_games_and_recent_files() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("Local");
        let root = local.join("Steam/htmlcache");
        let marker = root.join("Local State");
        fixture(&marker);
        let context = Context {
            local: Some(local),
            ..Default::default()
        };
        let now = SystemTime::now();
        let old = now - std::time::Duration::from_secs(8 * 86400);
        for suffix in [
            "Default/Cache/Cache_Data/item",
            "Default/Code Cache/js/item",
            "GraphiteDawnCache/item",
        ] {
            assert_eq!(
                context
                    .identify(&root.join(suffix), old, now)
                    .unwrap()
                    .rule_id
                    .as_ref(),
                "steam-web"
            );
            assert!(context.identify(&root.join(suffix), now, now).is_none());
        }
        for suffix in [
            "Default/Cookies",
            "Default/Login Data",
            "Default/Local Storage/file",
            "Default/IndexedDB/file",
            "Default/Service Worker/CacheStorage/file",
            "steamapps/common/game/save",
            "userdata/account/file",
            "Default/Cache/Cache_Data/file.db",
        ] {
            assert!(
                context.identify(&root.join(suffix), old, now).is_none(),
                "{suffix}"
            );
        }
        std::fs::remove_file(marker).unwrap();
        assert!(context
            .identify(&root.join("Default/Cache/Cache_Data/item"), old, now)
            .is_none());
    }
    #[test]
    fn new_portable_and_game_rules_keep_user_data_and_backup_copies() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let context = Context {
            roaming: Some(root.join("Roaming")),
            ..Context::default()
        };
        let now = SystemTime::now();
        let music = root.join("D/MusicFree");
        fixture(&music.join("MusicFree.exe"));
        fixture(&music.join("resources/app/package.json"));
        assert!(context
            .identify(
                &music.join("portable/userData/Cache/Cache_Data/item"),
                now,
                now
            )
            .is_some());
        for p in [
            "portable/userData/musicfree-plugins/plugin.js",
            "portable/userData/Downloads/song.mp3",
            "portable/userData/IndexedDB/item",
            "portable/userData/Local Storage/item",
        ] {
            assert!(context.identify(&music.join(p), now, now).is_none(), "{p}");
        }
        let wegame =
            root.join("Roaming/Tencent/WeGame/qbcore109/cache/qbcore_cache_browser.exe_130005");
        fixture(&wegame.join("LocalPrefs.json"));
        assert!(context
            .identify(&wegame.join("Cache/Cache_Data/item"), now, now)
            .is_some());
        for p in ["Local Storage/account", "IndexedDB/db", "000003.log"] {
            assert!(context.identify(&wegame.join(p), now, now).is_none());
        }
        let qq = root.join("Roaming/QQ");
        fixture(&qq.join("Local State"));
        assert!(context
            .identify(&qq.join("Partitions/qq-browser/Code Cache/item"), now, now)
            .is_some());
        assert!(context
            .identify(&qq.join("Msg/chat.db"), now, now)
            .is_none());
        assert!(context
            .identify(&qq.join("users/account/Cache/Cache_Data/item"), now, now)
            .is_none());
        let wallpaper = root.join("D/wallpaper_engine");
        fixture(&wallpaper.join("wallpaper64.exe"));
        fixture(&wallpaper.join("version.json"));
        fixture(&wallpaper.join("ui/uicache/browsewallpapers/base/Local State"));
        assert!(context
            .identify(
                &wallpaper.join("ui/uicache/browsewallpapers/base/Default/Cache/Cache_Data/item"),
                now,
                now
            )
            .is_some());
        assert!(context
            .identify(&wallpaper.join("projects/my-wallpaper.mp4"), now, now)
            .is_none());
        let backup = root.join("D/wallpaper_engine.corrupt-backup-20260710");
        fixture(&backup.join("wallpaper64.exe"));
        fixture(&backup.join("version.json"));
        fixture(&backup.join("ui/uicache/browsewallpapers/base/Local State"));
        assert!(context
            .identify(
                &backup.join("ui/uicache/browsewallpapers/base/Default/Cache/Cache_Data/item"),
                now,
                now
            )
            .is_none());
    }
    #[test]
    fn cargo_archives_need_age_exact_location_and_existing_source() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("cargo");
        let context = Context {
            cargo: vec![home.clone()],
            ..Context::default()
        };
        let file = home.join("registry/cache/index-example/serde-1.0.0.crate");
        let now = SystemTime::now();
        let old = now - std::time::Duration::from_secs(31 * 86400);
        assert!(context.identify(&file, old, now).is_none());
        let source = home.join("registry/src/index-example/serde-1.0.0/Cargo.toml");
        fixture(&source);
        assert_eq!(context.identify(&file, old, now).unwrap().kind, Kind::Cargo);
        assert!(context.identify(&file, now, now).is_none());
        assert!(context
            .identify(
                &home.join("registry/cache/index-example/subdir/serde-1.0.0.crate"),
                old,
                now
            )
            .is_none());
        std::fs::remove_file(source).unwrap();
        assert!(context.identify(&file, old, now).is_none());
    }
    #[test]
    fn new_note_apps_only_match_renderer_cache() {
        let context = Context {
            roaming: Some(PathBuf::from("R:/Roaming")),
            ..Context::default()
        };
        let now = SystemTime::now();
        for app in ["Obsidian", "Notion"] {
            assert!(context
                .identify(
                    &Path::new("R:/Roaming")
                        .join(app)
                        .join("Cache/Cache_Data/item"),
                    now,
                    now
                )
                .is_some());
            for suffix in [
                "IndexedDB/item",
                "Local Storage/item",
                "vault/notes.md",
                "Backups/note.md",
            ] {
                assert!(context
                    .identify(&Path::new("R:/Roaming").join(app).join(suffix), now, now)
                    .is_none());
            }
        }
    }
}
