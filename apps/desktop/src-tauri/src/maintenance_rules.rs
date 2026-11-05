//! Strict metadata-only formats for expired diagnostics and installer work files.
use crate::{
    full_cleanup_rules::{safe_marker, Kind},
    rule_catalog,
};
use std::{fs, path::Path, time::SystemTime};

fn old(time: SystemTime, now: SystemTime, days: u64) -> bool {
    now.duration_since(time)
        .is_ok_and(|age| age.as_secs() / 86400 >= days)
}
fn dated(value: &str, format: &str, now: SystemTime, days: u64) -> bool {
    let Ok(date) = chrono::NaiveDateTime::parse_from_str(value, format) else {
        return false;
    };
    old(SystemTime::from(date.and_utc()), now, days)
}
fn day_old(value: &str, format: &str, now: SystemTime, days: u64) -> bool {
    let Ok(date) = chrono::NaiveDate::parse_from_str(value, format) else {
        return false;
    };
    old(
        SystemTime::from(date.and_hms_opt(23, 59, 59).unwrap().and_utc()),
        now,
        days,
    )
}
fn uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}
fn newer_log(folder: &Path, prefix: &str, date: &str) -> bool {
    let Ok(entries) = fs::read_dir(folder) else {
        return false;
    };
    // Bounded even when an untrusted application directory contains many entries.
    entries.take(512).flatten().any(|entry| {
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        let Some(candidate) = name
            .strip_prefix(prefix)
            .and_then(|s| s.strip_suffix(".log"))
        else {
            return false;
        };
        candidate.len() == date.len()
            && candidate > date
            && chrono::NaiveDate::parse_from_str(
                candidate,
                if date.len() == 8 {
                    "%Y%m%d"
                } else {
                    "%Y-%m-%d"
                },
            )
            .is_ok()
            && safe_marker(&entry.path())
    })
}

pub(crate) fn app_kind(
    formats: &[String],
    root: &Path,
    relative: &Path,
    modified: SystemTime,
    now: SystemTime,
    log_days: u64,
    dump_days: u64,
) -> Option<Kind> {
    // This standard session marker prevents a similarly named arbitrary documents tree matching.
    if formats.is_empty()
        || !relative
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("log") || e.eq_ignore_ascii_case("dmp"))
        || !safe_marker(&root.join("Local State"))
    {
        return None;
    }
    for format in formats {
        match format.as_str() {
            "crashpad" if old(modified, now, dump_days) => {
                if let Some(rest) = rule_catalog::beneath(relative, Path::new("Crashpad/reports")) {
                    if rest.components().count() == 1 {
                        let name = rest.to_string_lossy().to_ascii_lowercase();
                        if name.strip_suffix(".dmp").is_some_and(uuid)
                            && safe_marker(&root.join("Crashpad/settings.dat"))
                        {
                            return Some(Kind::AppDumps);
                        }
                    }
                }
            }
            "electron-rotated" if old(modified, now, log_days) => {
                let name = relative
                    .to_string_lossy()
                    .replace('\\', "/")
                    .to_ascii_lowercase();
                for base in ["main", "renderer"] {
                    if name == format!("logs/{base}.old.log")
                        && safe_marker(&root.join(format!("logs/{base}.log")))
                    {
                        return Some(Kind::AppLogs);
                    }
                }
            }
            "cherry-daily" if old(modified, now, log_days) => {
                if let Some(rest) = rule_catalog::beneath(relative, Path::new("logs")) {
                    if rest.components().count() == 1 {
                        let name = rest.to_string_lossy().to_ascii_lowercase();
                        for prefix in ["app-error.", "app."] {
                            if let Some(date) = name
                                .strip_prefix(prefix)
                                .and_then(|s| s.strip_suffix(".log"))
                            {
                                if day_old(date, "%Y-%m-%d", now, log_days)
                                    && newer_log(&root.join("logs"), prefix, date)
                                {
                                    return Some(Kind::AppLogs);
                                }
                            }
                        }
                    }
                }
            }
            "qq-launcher" if old(modified, now, log_days) => {
                if let Some(rest) = rule_catalog::beneath(relative, Path::new("log")) {
                    if rest.components().count() == 1 {
                        let name = rest.to_string_lossy().to_ascii_lowercase();
                        if let Some(date) = name
                            .strip_prefix("app_launcher-")
                            .and_then(|s| s.strip_suffix(".log"))
                        {
                            if day_old(date, "%Y%m%d", now, log_days)
                                && newer_log(&root.join("log"), "app_launcher-", date)
                            {
                                return Some(Kind::AppLogs);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    None
}

pub(crate) fn diagnostic_kind(
    formats: &[String],
    root: &Path,
    relative: &Path,
    modified: SystemTime,
    now: SystemTime,
    days: u64,
) -> Option<Kind> {
    if !old(modified, now, days.max(30)) {
        return None;
    }
    let text = relative
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    for format in formats {
        let matches = match format.as_str() {
            "obs-session" => {
                let crash = text.starts_with("crashes/crash ");
                let stamp = text
                    .strip_prefix(if crash { "crashes/crash " } else { "logs/" })
                    .and_then(|p| p.strip_suffix(".txt"));
                stamp.is_some_and(|stamp| {
                    stamp.len() == 19
                        && dated(stamp, "%Y-%m-%d %H-%M-%S", now, days.max(30))
                        && safe_marker(&root.join("global.ini"))
                        && fs::read_dir(root.join(if crash { "crashes" } else { "logs" }))
                            .into_iter()
                            .flatten()
                            .take(512)
                            .flatten()
                            .any(|entry| {
                                let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
                                let name = if crash {
                                    name.strip_prefix("crash ").unwrap_or("")
                                } else {
                                    &name
                                };
                                name.strip_suffix(".txt").is_some_and(|newer| {
                                    newer.len() == 19
                                        && newer > stamp
                                        && dated(newer, "%Y-%m-%d %H-%M-%S", now, 0)
                                        && safe_marker(&entry.path())
                                })
                            })
                })
            }
            "stability-archive" => text
                .strip_prefix("logs/app.")
                .and_then(|p| p.strip_suffix(".log"))
                .is_some_and(|stamp| {
                    stamp.len() == 19
                        && dated(stamp, "%Y-%m-%d %H_%M_%S", now, days.max(30))
                        && safe_marker(&root.join("global.json"))
                        && safe_marker(&root.join("Logs/app.log"))
                }),
            "unreal-backup" => text
                .strip_prefix("saved/logs/crashreportclient-backup-")
                .and_then(|p| p.strip_suffix(".log"))
                .is_some_and(|stamp| {
                    stamp.len() == 19
                        && dated(stamp, "%Y.%m.%d-%H.%M.%S", now, days.max(30))
                        && safe_marker(&root.join("Saved/Logs/CrashReportClient.log"))
                        && ["Windows", "WindowsEditor"].iter().any(|p| {
                            safe_marker(&root.join("Saved/Config").join(p).join("Engine.ini"))
                        })
                }),
            _ => false,
        };
        if matches {
            return Some(if format == "obs-session" && text.starts_with("crashes/") {
                Kind::AppDumps
            } else {
                Kind::AppLogs
            });
        }
    }
    None
}

pub(crate) fn system_log_shape(relative: &Path) -> bool {
    let text = relative
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    if text == "dism/dism.log.bak" {
        return true;
    }
    let Some(stem) = text
        .strip_prefix("cbs/cbspersist_")
        .and_then(|s| s.strip_suffix(".log").or_else(|| s.strip_suffix(".cab")))
    else {
        return false;
    };
    stem.len() == 14
        && stem.bytes().all(|b| b.is_ascii_digit())
        && chrono::NaiveDateTime::parse_from_str(stem, "%Y%m%d%H%M%S").is_ok()
}
pub(crate) fn system_log(
    path: &Path,
    windows: &Path,
    modified: SystemTime,
    now: SystemTime,
    days: u64,
) -> bool {
    if !old(modified, now, days) {
        return false;
    }
    let logs = windows.join("Logs");
    let Some(relative) = rule_catalog::beneath(path, &logs) else {
        return false;
    };
    if !system_log_shape(relative) {
        return false;
    }
    let text = relative
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    if text == "dism/dism.log.bak" {
        return safe_marker(&logs.join("DISM/dism.log"));
    }
    let stamp = &text[15..29]; // cbs/cbspersist_ + the 14-digit timestamp
    dated(stamp, "%Y%m%d%H%M%S", now, days) && safe_marker(&logs.join("CBS/CBS.log"))
}

pub(crate) fn npm_log(
    path: &Path,
    root: &Path,
    modified: SystemTime,
    now: SystemTime,
    days: u64,
) -> bool {
    if !old(modified, now, days) {
        return false;
    }
    let Some(relative) = rule_catalog::beneath(path, &root.join("_logs")) else {
        return false;
    };
    if relative.components().count() != 1 {
        return false;
    }
    let name = relative.to_string_lossy();
    let Some((timestamp, index)) = name.split_once("-debug-") else {
        return false;
    };
    let Some(index) = index.strip_suffix(".log") else {
        return false;
    };
    !index.is_empty()
        && index.len() <= 3
        && index.bytes().all(|b| b.is_ascii_digit())
        && dated(timestamp, "%Y-%m-%dT%H_%M_%S_%3fZ", now, days)
}

fn installer_format(folder: &str) -> Option<bool> {
    let inno = folder
        .strip_prefix("is-")
        .and_then(|s| s.strip_suffix(".tmp"))
        .is_some_and(|s| {
            (4..=16).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        });
    let nsis = folder
        .strip_prefix("ns")
        .and_then(|s| s.strip_suffix(".tmp"))
        .is_some_and(|s| (4..=8).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit()));
    if inno || nsis {
        Some(nsis)
    } else {
        None
    }
}
pub(crate) fn installer_workspace(path: &Path, root: &Path) -> bool {
    let Some(relative) = rule_catalog::beneath(path, root) else {
        return false;
    };
    let mut parts = relative.components();
    let Some(first) = parts.next() else {
        return false;
    };
    !parts.as_path().as_os_str().is_empty()
        && installer_format(&first.as_os_str().to_string_lossy().to_ascii_lowercase()).is_some()
}
pub(crate) fn installer_residue(
    path: &Path,
    root: &Path,
    modified: SystemTime,
    now: SystemTime,
    days: u64,
) -> bool {
    if !old(modified, now, days) {
        return false;
    }
    let Some(relative) = rule_catalog::beneath(path, root) else {
        return false;
    };
    let mut parts = relative.components();
    let Some(component) = parts.next() else {
        return false;
    };
    if parts.as_path().as_os_str().is_empty() || relative.components().count() > 24 {
        return false;
    }
    let folder = component.as_os_str().to_string_lossy().to_ascii_lowercase();
    let Some(nsis) = installer_format(&folder) else {
        return false;
    };
    let scratch = root.join(component.as_os_str());
    if nsis
        && !["System.dll", "nsExec.dll"]
            .iter()
            .any(|m| safe_marker(&scratch.join(m)))
    {
        return false;
    }
    let Some(mut parent) = path.parent() else {
        return false;
    };
    loop {
        if parent == root {
            break;
        }
        let Ok(meta) = fs::symlink_metadata(parent) else {
            return false;
        };
        let name = parent
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_ascii_lowercase();
        if !meta.is_dir()
            || meta.file_type().is_symlink()
            || ["recover", "unsaved", "backup", "session"]
                .iter()
                .any(|s| name.contains(s))
            || !meta.modified().is_ok_and(|v| old(v, now, days))
            || !meta.created().is_ok_and(|v| old(v, now, days))
        {
            return false;
        }
        let Some(next) = parent.parent() else {
            return false;
        };
        parent = next;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    fn put(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"owned fixture").unwrap();
    }
    fn when() -> SystemTime {
        SystemTime::from(
            chrono::NaiveDate::from_ymd_opt(2026, 10, 27)
                .unwrap()
                .and_hms_opt(12, 0, 0)
                .unwrap()
                .and_utc(),
        )
    }
    fn age() -> SystemTime {
        when() - Duration::from_secs(120 * 86400)
    }
    #[test]
    fn diagnostics_match_exact_archives_and_preserve_live_and_database_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        put(&root.join("Local State"));
        put(&root.join("Crashpad/settings.dat"));
        put(&root.join("logs/main.log"));
        put(&root.join("logs/app-error.2026-10-27.log"));
        put(&root.join("log/app_launcher-20261027.log"));
        let formats = vec![
            "crashpad".into(),
            "electron-rotated".into(),
            "cherry-daily".into(),
            "qq-launcher".into(),
        ];
        for p in [
            "Crashpad/reports/1448c0e1-689e-43c0-a47c-32eb1d6e5f7e.dmp",
            "logs/main.old.log",
            "logs/app-error.2026-06-01.log",
            "log/app_launcher-20260601.log",
        ] {
            assert!(
                app_kind(&formats, root, Path::new(p), age(), when(), 30, 30).is_some(),
                "{p}"
            );
            assert!(app_kind(&formats, root, Path::new(p), when(), when(), 30, 30).is_none());
        }
        for p in [
            "logs/main.log",
            "logs/000003.log",
            "IndexedDB/000003.log",
            "logs/app-error.2026-10-27.log",
            "Crashpad/settings.dat",
            "Crashpad/metadata",
            "Crashpad/pending/1448c0e1-689e-43c0-a47c-32eb1d6e5f7e.dmp",
            "Crashpad/reports/not-a-uuid.dmp",
            "Crashpad/reports/nested/1448c0e1-689e-43c0-a47c-32eb1d6e5f7e.dmp",
        ] {
            assert!(
                app_kind(&formats, root, Path::new(p), age(), when(), 30, 30).is_none(),
                "{p}"
            );
        }
        fs::remove_file(root.join("Local State")).unwrap();
        assert!(app_kind(
            &formats,
            root,
            Path::new("logs/main.old.log"),
            age(),
            when(),
            30,
            30
        )
        .is_none());
    }
    #[test]
    fn diagnostic_sessions_keep_latest_live_config_and_unrecognized_files() {
        let temp = tempfile::tempdir().unwrap();
        let obs = temp.path().join("obs");
        put(&obs.join("global.ini"));
        let old_log = "logs/2026-06-01 12-00-00.txt";
        let old_crash = "crashes/Crash 2026-06-01 12-00-00.txt";
        let newer_log = obs.join("logs/2026-10-27 08-00-00.txt");
        let newer_crash = obs.join("crashes/Crash 2026-10-27 08-00-00.txt");
        for p in [
            &obs.join(old_log),
            &obs.join(old_crash),
            &newer_log,
            &newer_crash,
        ] {
            put(p);
        }
        let formats = vec!["obs-session".into()];
        assert_eq!(
            diagnostic_kind(&formats, &obs, Path::new(old_log), age(), when(), 30),
            Some(Kind::AppLogs)
        );
        assert_eq!(
            diagnostic_kind(&formats, &obs, Path::new(old_crash), age(), when(), 30),
            Some(Kind::AppDumps)
        );
        for p in [
            "logs/2026-10-27 08-00-00.txt",
            "logs/2026-02-30 12-00-00.txt",
            "logs/2027-01-01 12-00-00.txt",
            "basic/scenes/2026-06-01 12-00-00.txt",
            "logs/nested/2026-06-01 12-00-00.txt",
            "logs/notes.txt",
        ] {
            assert!(
                diagnostic_kind(&formats, &obs, Path::new(p), age(), when(), 30).is_none(),
                "{p}"
            );
        }
        assert!(diagnostic_kind(&formats, &obs, Path::new(old_log), when(), when(), 30).is_none());
        fs::remove_file(newer_log).unwrap();
        fs::remove_file(newer_crash).unwrap();
        assert!(diagnostic_kind(&formats, &obs, Path::new(old_log), age(), when(), 30).is_none());
        assert!(diagnostic_kind(&formats, &obs, Path::new(old_crash), age(), when(), 30).is_none());
        let stability = temp.path().join("stability");
        put(&stability.join("global.json"));
        put(&stability.join("Logs/app.log"));
        let formats = vec!["stability-archive".into()];
        let archived = Path::new("Logs/app.2026-06-01 12_00_00.log");
        assert_eq!(
            diagnostic_kind(&formats, &stability, archived, age(), when(), 30),
            Some(Kind::AppLogs)
        );
        for p in [
            "Logs/app.log",
            "user-secrets.data",
            "Data/Models/model.safetensors",
            "Logs/app.2027-01-01 00_00_00.log",
            "Logs/app.2026-02-30 00_00_00.log",
        ] {
            assert!(
                diagnostic_kind(&formats, &stability, Path::new(p), age(), when(), 30).is_none(),
                "{p}"
            );
        }
        fs::remove_file(stability.join("global.json")).unwrap();
        assert!(diagnostic_kind(&formats, &stability, archived, age(), when(), 30).is_none());
        let unreal = temp.path().join("unreal");
        put(&unreal.join("Saved/Logs/CrashReportClient.log"));
        put(&unreal.join("Saved/Config/WindowsEditor/Engine.ini"));
        let formats = vec!["unreal-backup".into()];
        let archived = Path::new("Saved/Logs/CrashReportClient-backup-2026.06.01-12.00.00.log");
        assert_eq!(
            diagnostic_kind(&formats, &unreal, archived, age(), when(), 30),
            Some(Kind::AppLogs)
        );
        for p in [
            "Saved/Logs/CrashReportClient.log",
            "Saved/Logs/keep.log",
            "Saved/Config/WindowsEditor/Engine.ini",
            "Saved/Crashes/report.dmp",
            "Saved/Logs/CrashReportClient-backup-2027.01.01-12.00.00.log",
        ] {
            assert!(
                diagnostic_kind(&formats, &unreal, Path::new(p), age(), when(), 30).is_none(),
                "{p}"
            );
        }
        fs::remove_file(unreal.join("Saved/Config/WindowsEditor/Engine.ini")).unwrap();
        assert!(diagnostic_kind(&formats, &unreal, archived, age(), when(), 30).is_none());
    }
    #[test]
    fn system_and_npm_logs_keep_current_files_and_reject_malformed_dates() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        put(&root.join("Logs/CBS/CBS.log"));
        put(&root.join("Logs/DISM/dism.log"));
        for p in [
            "Logs/CBS/CbsPersist_20260601120000.cab",
            "Logs/CBS/CbsPersist_20260601120000.log",
            "Logs/DISM/dism.log.bak",
        ] {
            assert!(system_log(&root.join(p), root, age(), when(), 30), "{p}");
        }
        for p in [
            "Logs/CBS/CBS.log",
            "Logs/CBS/CbsPersist_20261301120000.cab",
            "Logs/CBS/CbsPersist_20261027120000.log",
            "Logs/CBS/other.cab",
            "Logs/DISM/dism.log",
            "Logs/CBS/nested/CbsPersist_20260601120000.log",
        ] {
            assert!(!system_log(&root.join(p), root, age(), when(), 30), "{p}");
        }
        assert!(npm_log(
            &root.join("_logs/2026-06-01T12_00_00_000Z-debug-0.log"),
            root,
            age(),
            when(),
            30
        ));
        for p in [
            "_logs/000003.log",
            "_logs/2026-99-01T12_00_00_000Z-debug-0.log",
            "_logs/2026-10-27T12_00_00_000Z-debug-0.log",
        ] {
            assert!(!npm_log(&root.join(p), root, age(), when(), 30), "{p}");
        }
    }
    #[cfg(windows)]
    #[test]
    fn aged_installer_tree_is_eligible_but_new_subdirectories_and_recovery_are_kept() {
        use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
        use windows_sys::Win32::{Foundation::FILETIME, Storage::FileSystem::SetFileTime};
        fn make_old(dir: &Path) {
            let ticks = (SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                - 120 * 86400
                + 11644473600)
                * 10_000_000;
            let stamp = FILETIME {
                dwLowDateTime: ticks as u32,
                dwHighDateTime: (ticks >> 32) as u32,
            };
            let handle = fs::OpenOptions::new()
                .access_mode(0x100)
                .share_mode(7)
                .custom_flags(0x02000000)
                .open(dir)
                .unwrap();
            assert_ne!(
                unsafe {
                    SetFileTime(
                        handle.as_raw_handle() as _,
                        &stamp,
                        std::ptr::null(),
                        &stamp,
                    )
                },
                0
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let scratch = root.join("is-A1B2C3.tmp");
        let path = scratch.join("payload/data.bin");
        put(&path);
        make_old(&scratch.join("payload"));
        make_old(&scratch);
        let now = SystemTime::now();
        let old = now - std::time::Duration::from_secs(120 * 86400);
        assert!(installer_residue(&path, root, old, now, 30));
        let context = crate::full_cleanup_rules::Context {
            temp: vec![root.into()],
            ..Default::default()
        };
        assert_eq!(
            context.identify(&path, old, now).map(|r| r.kind),
            Some(Kind::InstallerResidue)
        );
        let fresh = scratch.join("fresh/data.bin");
        put(&fresh);
        make_old(&scratch);
        assert!(!installer_residue(&fresh, root, old, now, 30));
        let recovery = scratch.join("UnsavedFiles/data.bin");
        put(&recovery);
        make_old(&scratch.join("UnsavedFiles"));
        make_old(&scratch);
        assert!(!installer_residue(&recovery, root, old, now, 30));
        let nsis = root.join("nsAB12.tmp");
        let payload = nsis.join("data.dll");
        put(&payload);
        make_old(&nsis);
        assert!(!installer_residue(&payload, root, old, now, 30));
        put(&nsis.join("nsExec.dll"));
        make_old(&nsis);
        assert!(installer_residue(&payload, root, old, now, 30));
    }
    #[test]
    fn newly_extracted_old_payload_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let p = root.join("is-A1B2C3.tmp/setup.tmp");
        put(&p);
        assert!(!installer_residue(&p, root, age(), SystemTime::now(), 30));
        let context = crate::full_cleanup_rules::Context {
            temp: vec![root.into()],
            ..Default::default()
        };
        assert!(
            context.identify(&p, age(), SystemTime::now()).is_none(),
            "fresh workspace must not fall through to the old .tmp rule"
        );
        assert!(!installer_residue(
            &root.join("downloads/is-A1B2C3.tmp/setup.tmp"),
            root,
            age(),
            when(),
            30
        ));
        assert!(!installer_residue(
            &root.join("arbitrary.tmp/data.dll"),
            root,
            age(),
            when(),
            30
        ));
    }
}
