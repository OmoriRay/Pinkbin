//! Curated metadata rules. Cache boundaries and deletion guards remain engine-owned.
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

pub const BUILTIN: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../rules/cleanup-catalog-v7.json"
));
const MAX_BYTES: usize = 512 * 1024;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Metadata {
    pub id: Arc<str>,
    pub app: Arc<str>,
    pub cache_title: Arc<str>,
    pub logs_title: Arc<str>,
    pub indexes_title: Arc<str>,
    pub dumps_title: Arc<str>,
    pub processes: Arc<[String]>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub min_age_days: u64,
    pub app: String,
    pub processes: Vec<String>,
    #[serde(skip)]
    pub meta: Option<Metadata>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Legacy {
    pub id: String,
    pub app: String,
    pub title: String,
    pub family: String,
    pub prefix: String,
    pub min_age_days: u64,
    pub processes: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Location {
    pub root: String,
    pub path: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub id: String,
    pub app: String,
    pub family: String,
    pub locations: Vec<Location>,
    pub processes: Vec<String>,
    pub min_age_days: u64,
    pub enabled: bool,
    pub reviewed: bool,
    pub exclude: Vec<String>,
    pub markers_any: Vec<String>,
    pub logs: bool,
    #[serde(default)]
    pub maintenance: Vec<String>,
    #[serde(default)]
    pub partitions: bool,
    pub sources: Vec<String>,
    pub product_prefix: Option<String>,
    #[serde(skip)]
    pub exclusions: GlobSet,
    #[serde(skip)]
    pub meta: Option<Metadata>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Portable {
    pub id: String,
    pub app: String,
    pub executable: String,
    pub processes: Vec<String>,
    #[serde(skip)]
    pub meta: Option<Metadata>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortableApp {
    pub id: String,
    pub app: String,
    pub executable: String,
    pub processes: Vec<String>,
    pub family: String,
    pub data_dirs: Vec<String>,
    pub markers_any: Vec<String>,
    pub partitions: bool,
    pub enabled: bool,
    pub reviewed: bool,
    #[serde(skip)]
    pub meta: Option<Metadata>,
    #[serde(default)]
    pub maintenance: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdaterCache {
    pub id: String,
    pub profile_id: String,
    pub directory: String,
    pub min_age_days: u64,
    pub enabled: bool,
    pub reviewed: bool,
    pub sources: Vec<String>,
    #[serde(skip)]
    pub meta: Option<Metadata>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    pub schema_version: u32,
    pub revision: String,
    pub license: String,
    pub templates: BTreeMap<String, Vec<String>>,
    pub policies: BTreeMap<String, Policy>,
    pub legacy: Vec<Legacy>,
    pub profiles: Vec<Profile>,
    pub portable_editors: Vec<Portable>,
    pub portable_browsers: Vec<Portable>,
    #[serde(default)]
    pub portable_apps: Vec<PortableApp>,
    #[serde(default)]
    pub updater_caches: Vec<UpdaterCache>,
}
fn meta(id: &str, app: &str, processes: &[String]) -> Metadata {
    Metadata {
        id: Arc::from(id),
        app: Arc::from(app),
        cache_title: Arc::from(if app.ends_with("缓存") {
            app.to_string()
        } else {
            format!("{app} 缓存")
        }),
        logs_title: Arc::from(format!("{app} 旧日志")),
        indexes_title: Arc::from(format!("{app} 旧索引缓存")),
        dumps_title: Arc::from(format!("{app} 旧崩溃转储")),
        processes: Arc::from(processes),
    }
}
fn text_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 160 && !s.chars().any(char::is_control)
}
fn relative(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 240
        && s.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && !part.ends_with(['.', ' '])
                && !part
                    .chars()
                    .any(|c| c.is_control() || "\\:*?\"<>|%".contains(c))
        })
}
fn maintenance_ok(formats: &[String]) -> bool {
    formats.len() <= 4
        && formats.iter().all(|s| {
            matches!(
                s.as_str(),
                "crashpad"
                    | "electron-rotated"
                    | "cherry-daily"
                    | "qq-launcher"
                    | "obs-session"
                    | "stability-archive"
                    | "unreal-backup"
            )
        })
}
fn processes_ok(list: &[String]) -> bool {
    list.len() <= 24
        && list
            .iter()
            .all(|s| relative(s) && !s.contains('/') && s.to_ascii_lowercase().ends_with(".exe"))
}
fn check(ok: bool, message: &str) -> Result<(), String> {
    if ok {
        Ok(())
    } else {
        Err(format!("清理规则库无效：{message}"))
    }
}
pub(crate) fn allowed_template(name: &str) -> Option<&'static [&'static str]> {
    match name {
        "chromium_profile" => Some(&[
            "Cache/Cache_Data",
            "Code Cache",
            "GPUCache",
            "DawnCache",
            "DawnGraphiteCache",
            "DawnWebGPUCache",
            "Media Cache",
        ]),
        "chromium_shared" => Some(&[
            "ShaderCache",
            "GrShaderCache",
            "GraphiteDawnCache",
            "GPUPersistentCache",
        ]),
        "electron_legacy" => Some(&[
            "Cache/Cache_Data",
            "Code Cache",
            "GPUCache",
            "DawnGraphiteCache",
            "DawnWebGPUCache",
        ]),
        "electron" => Some(&[
            "Cache/Cache_Data",
            "Code Cache",
            "GPUCache",
            "DawnCache",
            "DawnGraphiteCache",
            "DawnWebGPUCache",
            "CachedData",
        ]),
        _ => None,
    }
}
impl Catalog {
    pub fn parse(input: &str) -> Result<Arc<Self>, String> {
        check(input.len() <= MAX_BYTES, "文件超过大小上限")?;
        let mut catalog: Self = serde_json::from_str(input)
            .map_err(|_| "清理规则库无效：JSON 格式或字段有误".to_string())?;
        let baseline: Self = serde_json::from_str(BUILTIN).expect("builtin rule schema");
        check(
            matches!(catalog.schema_version, 4..=7)
                && catalog.license == "MIT"
                && text_ok(&catalog.revision)
                && catalog.revision.len() <= 48,
            "不支持的版本或许可",
        )?;
        check(
            catalog.templates.len() == baseline.templates.len(),
            "缓存模板不完整",
        )?;
        for (name, paths) in &catalog.templates {
            let allowed = allowed_template(name).ok_or("清理规则库无效：未知缓存模板")?;
            check(
                !paths.is_empty()
                    && paths.len() <= allowed.len()
                    && paths.iter().all(|p| allowed.contains(&p.as_str())),
                "缓存模板超出引擎允许的范围",
            )?;
        }
        // v4 had no HF rules. Migrate only this exact legacy schema, never repair v5.
        if catalog.schema_version == 4 {
            check(
                !catalog.policies.contains_key("hf-xet")
                    && !catalog.policies.contains_key("hf-partial"),
                "v4 不接受新版本策略",
            )?;
            for id in ["hf-xet", "hf-partial"] {
                let mut policy: Policy = serde_json::from_value(
                    serde_json::from_str::<serde_json::Value>(BUILTIN).unwrap()["policies"][id]
                        .clone(),
                )
                .expect("builtin HF policy");
                policy.meta = None;
                catalog.policies.insert(id.to_string(), policy);
            }
        }
        check(
            catalog.policies.len() == baseline.policies.len(),
            "安全策略不完整",
        )?;
        for (id, policy) in &mut catalog.policies {
            let base = baseline
                .policies
                .get(id)
                .ok_or("清理规则库无效：未知安全策略")?;
            check(
                policy.min_age_days >= base.min_age_days
                    && policy.min_age_days <= 3650
                    && processes_ok(&policy.processes)
                    && base
                        .processes
                        .iter()
                        .all(|p| policy.processes.iter().any(|v| v.eq_ignore_ascii_case(p))),
                "保留天数或进程保护不足",
            )?;
            policy.meta = Some(meta(id, &policy.app, &policy.processes));
        }
        check(
            catalog.legacy.len() == baseline.legacy.len(),
            "常用缓存规则不完整",
        )?;
        let mut ids = HashSet::new();
        for rule in &catalog.legacy {
            let base = baseline
                .legacy
                .iter()
                .find(|b| b.id == rule.id)
                .ok_or("清理规则库无效：未知常用缓存规则")?;
            check(
                ids.insert(rule.id.clone())
                    && rule.family == base.family
                    && rule.prefix == base.prefix
                    && text_ok(&rule.title)
                    && rule.min_age_days >= base.min_age_days
                    && rule.min_age_days <= 3650
                    && processes_ok(&rule.processes)
                    && base
                        .processes
                        .iter()
                        .all(|p| rule.processes.iter().any(|v| v.eq_ignore_ascii_case(p))),
                "常用缓存路径或保护不符合要求",
            )?;
        }
        check(catalog.profiles.len() <= 256, "应用规则过多")?;
        for rule in &mut catalog.profiles {
            check(
                text_ok(&rule.id)
                    && ids.insert(rule.id.clone())
                    && text_ok(&rule.app)
                    && matches!(
                        rule.family.as_str(),
                        "electron"
                            | "chromium"
                            | "jetbrains"
                            | "epic"
                            | "cinebench"
                            | "firefox"
                            | "wegame"
                            | "diagnostics"
                    )
                    && processes_ok(&rule.processes)
                    && !rule.processes.is_empty()
                    && rule.min_age_days <= 3650,
                "应用类型、标识或进程保护有误",
            )?;
            check(
                !rule.locations.is_empty()
                    && rule.locations.len() <= 8
                    && rule.locations.iter().all(|l| {
                        matches!(l.root.as_str(), "local" | "roaming")
                            && relative(&l.path)
                            && l.path.split('/').count() <= 5
                    }),
                "应用目录必须是 AppData 内的相对路径",
            )?;
            check(
                rule.markers_any.len() <= 8 && rule.markers_any.iter().all(|m| relative(m)),
                "软件标记路径有误",
            )?;
            check(
                rule.exclude.len() <= 32
                    && rule
                        .exclude
                        .iter()
                        .all(|s| s.len() <= 240 && !s.contains('\\')),
                "排除项有误",
            )?;
            check(
                !rule.sources.is_empty()
                    && rule.sources.len() <= 8
                    && rule
                        .sources
                        .iter()
                        .all(|s| s.starts_with("https://") && s.len() <= 400),
                "缺少规则来源",
            )?;
            if rule.family == "cinebench" {
                check(rule.min_age_days >= 30, "渲染缓存至少保留 30 天")?;
            }
            if rule.family == "jetbrains" {
                check(
                    rule.min_age_days >= 7
                        && rule
                            .product_prefix
                            .as_deref()
                            .is_some_and(|s| relative(s) && !s.contains('/')),
                    "IDE 缓存保护不足",
                )?;
            } else {
                check(rule.product_prefix.is_none(), "应用类型与产品前缀不一致")?;
            }
            // Established process aliases cannot be removed by a rule update.
            if let Some(base) = baseline.profiles.iter().find(|b| b.id == rule.id) {
                check(
                    rule.min_age_days >= base.min_age_days
                        && base
                            .processes
                            .iter()
                            .all(|p| rule.processes.iter().any(|v| v.eq_ignore_ascii_case(p))),
                    "已有应用的进程保护不足",
                )?;
            }
            check(maintenance_ok(&rule.maintenance), "未知日志或故障报告格式")?;
            if rule.family == "diagnostics" {
                let base = baseline
                    .profiles
                    .iter()
                    .find(|p| p.id == rule.id && p.family == "diagnostics");
                check(
                    catalog.schema_version >= 7
                        && rule.min_age_days >= 30
                        && base.is_some_and(|b| {
                            rule.markers_any == b.markers_any
                                && rule.maintenance == b.maintenance
                                && rule.locations.len() == b.locations.len()
                                && rule
                                    .locations
                                    .iter()
                                    .zip(&b.locations)
                                    .all(|(a, b)| a.root == b.root && a.path == b.path)
                        }),
                    "归档日志目录、标记或格式有误",
                )?;
            } else {
                check(
                    !rule.maintenance.iter().any(|p| {
                        matches!(
                            p.as_str(),
                            "obs-session" | "stability-archive" | "unreal-backup"
                        )
                    }),
                    "日志格式与应用类型不符",
                )?;
            }
            if rule.id == "steam-web" {
                check(
                    rule.markers_any == ["Local State"]
                        && rule.locations.len() == 1
                        && rule.locations[0].root == "local"
                        && rule.locations[0].path == "Steam/htmlcache",
                    "Steam 网页缓存目录或标记有误",
                )?;
            }
            let mut builder = GlobSetBuilder::new();
            for pattern in &rule.exclude {
                builder.add(
                    GlobBuilder::new(pattern)
                        .case_insensitive(true)
                        .literal_separator(true)
                        .build()
                        .map_err(|_| "清理规则库无效：排除表达式有误".to_string())?,
                );
            }
            rule.exclusions = builder
                .build()
                .map_err(|_| "清理规则库无效：排除表达式过于复杂".to_string())?;
            rule.meta = Some(meta(&rule.id, &rule.app, &rule.processes));
        }
        check(
            if catalog.schema_version >= 6 {
                catalog.updater_caches.len() == baseline.updater_caches.len()
            } else {
                catalog.updater_caches.is_empty()
            },
            "更新安装包规则与版本不一致",
        )?;
        let installer_processes = catalog.policy("installer-residue").processes.clone();
        for rule in &mut catalog.updater_caches {
            let base = baseline
                .updater_caches
                .iter()
                .find(|r| r.id == rule.id)
                .ok_or("清理规则库无效：未知更新安装包规则")?;
            check(
                ids.insert(rule.id.clone())
                    && rule.profile_id == base.profile_id
                    && rule.directory == base.directory
                    && rule.min_age_days >= base.min_age_days.max(30)
                    && rule.min_age_days <= 3650
                    && !rule.sources.is_empty()
                    && rule.sources.len() <= 8
                    && rule
                        .sources
                        .iter()
                        .all(|s| s.starts_with("https://") && s.len() <= 400),
                "更新安装包目录或保护条件有误",
            )?;
            let profile = catalog
                .profiles
                .iter()
                .find(|p| p.id == rule.profile_id)
                .ok_or("清理规则库无效：更新安装包缺少对应软件")?;
            check(
                profile.family == "electron"
                    && baseline.profiles.iter().any(|p| {
                        p.id == profile.id
                            && p.family == profile.family
                            && p.locations.len() == profile.locations.len()
                            && p.locations
                                .iter()
                                .zip(&profile.locations)
                                .all(|(a, b)| a.root == b.root && a.path == b.path)
                    }),
                "更新安装包的软件标记目录有误",
            )?;
            let mut processes = profile.processes.clone();
            for process in installer_processes.iter().map(String::as_str).chain([
                "installer.exe",
                "update.exe",
                "elevate.exe",
            ]) {
                if !processes.iter().any(|p| p.eq_ignore_ascii_case(process)) {
                    processes.push(process.to_string());
                }
            }
            let mut metadata = meta(&profile.id, &profile.app, &processes);
            metadata.cache_title = Arc::from(format!("{} 旧更新安装包", profile.app));
            rule.meta = Some(metadata);
        }
        for list in [
            &mut catalog.portable_editors,
            &mut catalog.portable_browsers,
        ] {
            check(list.len() <= 32, "便携软件规则过多")?;
            for rule in list {
                check(
                    text_ok(&rule.id)
                        && ids.insert(rule.id.clone())
                        && text_ok(&rule.app)
                        && processes_ok(&rule.processes)
                        && !rule.processes.is_empty()
                        && processes_ok(std::slice::from_ref(&rule.executable))
                        && rule
                            .processes
                            .iter()
                            .any(|p| p.eq_ignore_ascii_case(&rule.executable)),
                    "便携软件标记或进程保护有误",
                )?;
                rule.meta = Some(meta(&rule.id, &rule.app, &rule.processes));
            }
        }
        check(catalog.portable_apps.len() <= 64, "便携应用规则过多")?;
        for rule in &mut catalog.portable_apps {
            check(
                text_ok(&rule.id)
                    && ids.insert(rule.id.clone())
                    && text_ok(&rule.app)
                    && processes_ok(&rule.processes)
                    && !rule.processes.is_empty()
                    && processes_ok(std::slice::from_ref(&rule.executable))
                    && rule
                        .processes
                        .iter()
                        .any(|p| p.eq_ignore_ascii_case(&rule.executable))
                    && matches!(rule.family.as_str(), "electron" | "chromium" | "wallpaper"),
                "便携应用类型或进程保护有误",
            )?;
            check(
                !rule.data_dirs.is_empty()
                    && rule.data_dirs.len() <= 8
                    && rule
                        .data_dirs
                        .iter()
                        .all(|p| relative(p) && p.split('/').count() <= 3)
                    && rule.markers_any.len() <= 8
                    && rule.markers_any.iter().all(|p| relative(p)),
                "便携应用数据或标记目录有误",
            )?;
            check(
                maintenance_ok(&rule.maintenance),
                "未知便携日志或故障报告格式",
            )?;
            if rule.family == "wallpaper" {
                check(
                    rule.data_dirs == ["ui/uicache"]
                        && matches!(
                            rule.executable.as_str(),
                            "wallpaper64.exe" | "wallpaper32.exe"
                        )
                        && !rule.markers_any.is_empty(),
                    "壁纸引擎只允许已核验的界面缓存",
                )?;
            }
            check(
                rule.family != "electron" || !rule.markers_any.is_empty(),
                "Electron 便携应用缺少产品标记",
            )?;
            rule.meta = Some(meta(&rule.id, &rule.app, &rule.processes));
        }
        Ok(Arc::new(catalog))
    }
    pub fn template(&self, name: &str) -> &[String] {
        &self.templates[name]
    }
    pub fn policy(&self, name: &str) -> &Policy {
        &self.policies[name]
    }
}
impl Profile {
    pub fn allows(&self, relative: &Path) -> bool {
        self.enabled
            && self.reviewed
            && !protected(relative)
            && !self
                .exclusions
                .is_match(relative.to_string_lossy().replace('\\', "/"))
    }
}
pub fn protected(path: &Path) -> bool {
    path.components().any(|part| {
        matches!(
            part.as_os_str()
                .to_string_lossy()
                .to_ascii_lowercase()
                .as_str(),
            "cookies"
                | "login data"
                | "bookmarks"
                | "indexeddb"
                | "local storage"
                | "session storage"
                | "localhistory"
                | "backups"
                | "user"
                | "autorecover"
                | "recovery"
                | "unsavedfiles"
                | "accounts"
                | "msg"
                | "multimsg"
                | "db_storage"
        )
    }) || path.extension().is_some_and(|s| {
        matches!(
            s.to_string_lossy().to_ascii_lowercase().as_str(),
            "db" | "db-wal" | "db-shm" | "sqlite" | "sqlite3"
        )
    })
}
pub fn beneath<'a>(path: &'a Path, root: &Path) -> Option<&'a Path> {
    let mut parts = path.components();
    for expected in root.components() {
        if !parts
            .next()?
            .as_os_str()
            .eq_ignore_ascii_case(expected.as_os_str())
        {
            return None;
        }
    }
    Some(parts.as_path())
}
pub fn in_template(catalog: &Catalog, name: &str, relative: &Path) -> bool {
    catalog
        .template(name)
        .iter()
        .filter(|p| p.as_str() != "Media Cache")
        .any(|root| {
            beneath(relative, Path::new(root)).is_some_and(|rest| !rest.as_os_str().is_empty())
        })
}
static BUILTIN_CATALOG: OnceLock<Arc<Catalog>> = OnceLock::new();
static ACTIVE: OnceLock<Result<Arc<Catalog>, String>> = OnceLock::new();
fn builtin_ref() -> &'static Arc<Catalog> {
    BUILTIN_CATALOG.get_or_init(|| Catalog::parse(BUILTIN).expect("invalid builtin rules"))
}
pub fn builtin() -> Arc<Catalog> {
    builtin_ref().clone()
}
fn load(path: &Path) -> Result<Arc<Catalog>, String> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(builtin()),
        Err(_) => return Err("无法读取清理规则库，请检查 rules/cleanup-catalog-v7.json".into()),
    };
    let mut input = String::new();
    file.take((MAX_BYTES + 1) as u64)
        .read_to_string(&mut input)
        .map_err(|_| "清理规则库不是有效的 UTF-8 文件".to_string())?;
    Catalog::parse(&input)
}
fn active() -> &'static Result<Arc<Catalog>, String> {
    ACTIVE.get_or_init(|| {
        let exe = std::env::current_exe().map_err(|_| "无法定位清理规则库".to_string())?;
        let path: PathBuf = exe
            .parent()
            .ok_or("无法定位程序目录")?
            .join("rules/cleanup-catalog-v7.json");
        load(&path)
    })
}
pub fn ensure_ready() -> Result<(), String> {
    active().as_ref().map(|_| ()).map_err(Clone::clone)
}
pub fn current() -> Arc<Catalog> {
    active().as_ref().cloned().unwrap_or_else(|_| builtin())
}
pub fn legacy(id: &str) -> Option<&'static Legacy> {
    let catalog = active().as_ref().unwrap_or_else(|_| builtin_ref());
    catalog.legacy.iter().find(|rule| rule.id == id)
}
pub fn template(name: &str) -> &'static [String] {
    let catalog = active().as_ref().unwrap_or_else(|_| builtin_ref());
    catalog.template(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn edited(f: impl FnOnce(&mut serde_json::Value)) -> String {
        let mut value = serde_json::from_str(BUILTIN).unwrap();
        f(&mut value);
        serde_json::to_string(&value).unwrap()
    }
    #[test]
    fn maintenance_templates_reject_unknown_cleanup_semantics() {
        for input in [
            edited(|v| v["profiles"][0]["maintenance"] = serde_json::json!(["all-logs"])),
            edited(|v| {
                v["portable_apps"][0]["maintenance"] = serde_json::json!(["delete-everything"])
            }),
            edited(|v| v["policies"]["installer-residue"]["min_age_days"] = 0.into()),
            edited(|v| v["policies"]["system-logs"]["processes"] = serde_json::json!([])),
        ] {
            assert!(Catalog::parse(&input).is_err());
        }
    }
    #[test]
    fn curated_catalog_is_valid() {
        let c = Catalog::parse(BUILTIN).unwrap();
        assert_eq!(c.legacy.len(), 14);
        assert!(c.profiles.len() >= 20);
    }
    #[test]
    fn diagnostics_are_fixed_and_old_catalogs_do_not_acquire_new_formats() {
        for text in [
            edited(|v| {
                let p = v["profiles"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|p| p["id"] == "obs-logs")
                    .unwrap();
                p["markers_any"] = serde_json::json!([]);
            }),
            edited(|v| {
                let p = v["profiles"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|p| p["id"] == "obs-logs")
                    .unwrap();
                p["min_age_days"] = 29.into();
            }),
            edited(|v| {
                let p = v["profiles"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|p| p["id"] == "obs-logs")
                    .unwrap();
                p["locations"][0]["path"] = "Documents".into();
            }),
            edited(|v| {
                v["profiles"][0]["maintenance"] = serde_json::json!(["obs-session"]);
            }),
            edited(|v| v["schema_version"] = 6.into()),
        ] {
            assert!(Catalog::parse(&text).is_err());
        }
        let previous = Catalog::parse(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../rules/cleanup-catalog-v6.json"
        )))
        .unwrap();
        assert_eq!(previous.updater_caches.len(), 3);
        assert!(previous.profiles.iter().all(|p| p.family != "diagnostics"));
    }
    #[test]
    fn updater_catalog_keeps_fixed_roots_app_markers_and_age() {
        for input in [
            edited(|v| v["updater_caches"][0]["directory"] = "Documents".into()),
            edited(|v| v["updater_caches"][0]["profile_id"] = "app-code".into()),
            edited(|v| v["updater_caches"][0]["min_age_days"] = 29.into()),
            edited(|v| v["schema_version"] = 5.into()),
            edited(|v| {
                v.as_object_mut().unwrap().remove("updater_caches");
            }),
            edited(|v| {
                let profiles = v["profiles"].as_array_mut().unwrap();
                let cherry = profiles
                    .iter_mut()
                    .find(|p| p["id"] == "app-cherry-partitions")
                    .unwrap();
                cherry["locations"][0]["path"] = "Documents".into();
            }),
        ] {
            assert!(Catalog::parse(&input).is_err());
        }
        let old = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../rules/cleanup-catalog-v5.json"
        ));
        let old = Catalog::parse(old).unwrap();
        assert_eq!(old.schema_version, 5);
        assert!(old.updater_caches.is_empty());
        let current = Catalog::parse(BUILTIN).unwrap();
        for updater in &current.updater_caches {
            let meta = updater.meta.as_ref().unwrap();
            assert_eq!(meta.id.as_ref(), updater.profile_id);
            assert!(meta.processes.iter().any(|p| p == "installer.exe"));
            assert!(meta.processes.iter().any(|p| p == "msiexec.exe"));
        }
    }
    #[test]
    fn v5_requires_both_hf_policies_and_retains_process_aliases() {
        for input in [
            edited(|v| {
                v["policies"].as_object_mut().unwrap().remove("hf-xet");
            }),
            edited(|v| {
                v["policies"].as_object_mut().unwrap().remove("hf-partial");
            }),
            edited(|v| v["policies"]["hf-xet"]["min_age_days"] = 44.into()),
            edited(|v| v["policies"]["hf-partial"]["min_age_days"] = 29.into()),
            edited(|v| {
                v["policies"]["hf-partial"]["processes"] = serde_json::json!(["python.exe"])
            }),
            edited(|v| v["policies"]["hf-xet"]["delete_staging"] = true.into()),
        ] {
            assert!(Catalog::parse(&input).is_err());
        }
        let old = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../rules/cleanup-catalog-v4.json"
        ));
        assert_eq!(Catalog::parse(old).unwrap().schema_version, 4);
    }
    #[test]
    fn malformed_or_unknown_schema_fails_closed() {
        for s in [
            "{}".to_string(),
            "bad".to_string(),
            edited(|v| v["schema_version"] = 99.into()),
            edited(|v| v["commands"] = "delete".into()),
        ] {
            assert!(Catalog::parse(&s).is_err());
        }
    }
    #[test]
    fn paths_and_templates_cannot_escape_cache_boundaries() {
        for path in [
            "../Desktop",
            "C:/Users",
            "Code/../User",
            "Code\\User",
            "Code/*",
            "Code/",
            "Code.",
        ] {
            assert!(
                Catalog::parse(&edited(
                    |v| v["profiles"][0]["locations"][0]["path"] = path.into()
                ))
                .is_err(),
                "{path}"
            );
        }
        assert!(Catalog::parse(&edited(
            |v| v["templates"]["electron"] = serde_json::json!(["IndexedDB"])
        ))
        .is_err());
        assert!(
            Catalog::parse(&edited(|v| v["legacy"][0]["prefix"] = "Documents".into())).is_err()
        );
    }
    #[test]
    fn cannot_lower_retention_or_remove_process_guards() {
        for s in [
            edited(|v| v["policies"]["python"]["min_age_days"] = 0.into()),
            edited(|v| v["policies"]["rust"]["processes"] = serde_json::json!([])),
            edited(|v| v["profiles"][0]["processes"] = serde_json::json!([])),
            edited(|v| v["legacy"][1]["processes"] = serde_json::json!([])),
        ] {
            assert!(Catalog::parse(&s).is_err());
        }
    }
    #[test]
    fn review_is_explicit_and_exclusions_are_case_insensitive() {
        assert!(Catalog::parse(&edited(|v| {
            v["profiles"][0].as_object_mut().unwrap().remove("reviewed");
        }))
        .is_err());
        let c = Catalog::parse(&edited(|v| {
            v["profiles"][0]["reviewed"] = false.into();
        }))
        .unwrap();
        assert!(!c.profiles[0].allows(Path::new("Cache/Cache_Data/item")));
        let c = Catalog::parse(&edited(|v| {
            v["profiles"][0]["exclude"] = serde_json::json!(["**/KEEP/**"])
        }))
        .unwrap();
        assert!(!c.profiles[0].allows(Path::new("Cache/Cache_Data/keep/file")));
        for p in [
            "Cache/Cache_Data/Backups/note",
            "IndexedDB/db",
            "Cache/Cache_Data/data.db",
        ] {
            assert!(!c.profiles[0].allows(Path::new(p)));
        }
    }
    #[test]
    fn missing_file_falls_back_but_invalid_file_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("rules.json");
        assert!(load(&p).is_ok());
        fs::write(&p, "bad").unwrap();
        assert!(load(&p).is_err());
        fs::write(&p, BUILTIN).unwrap();
        let snapshot = load(&p).unwrap();
        fs::write(&p, "bad").unwrap();
        assert!(load(&p).is_err());
        assert_eq!(snapshot.revision, builtin().revision);
    }
}
