//! HF-owned formats only. These checks deliberately perform fresh filesystem reads.
use super::safe_marker;
use base64::{engine::general_purpose::URL_SAFE, Engine};
use sha1::{Digest, Sha1};
use std::{fs, io::Read, path::Path, time::SystemTime};

fn hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn open_single_file(path: &Path) -> Option<fs::File> {
    if !safe_marker(path) {
        return None;
    }
    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .custom_flags(0x00200000)
            .open(path)
            .ok()?
    };
    #[cfg(not(windows))]
    let file = fs::File::open(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || !crate::rule_probe::ordinary(&meta) {
        return None;
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) } == 0
            || info.nNumberOfLinks != 1
        {
            return None;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.nlink() != 1 {
            return None;
        }
    }
    #[cfg(not(any(windows, unix)))]
    {
        return None;
    }
    #[cfg(any(windows, unix))]
    Some(file)
}
fn single_file(path: &Path) -> bool {
    open_single_file(path).is_some()
}

pub(super) fn xet_shard(path: &Path, root: &Path) -> bool {
    let Some(relative) = crate::rule_catalog::beneath(path, root) else {
        return false;
    };
    let parts: Vec<_> = relative.iter().collect();
    if parts.len() != 3 || parts[1] != "shard-cache" {
        return false;
    }
    let environment = parts[0].to_string_lossy();
    // hf_xet names its environment from an endpoint plus a cache-key suffix.
    if !environment.starts_with("https___")
        || environment.len() > 160
        || !environment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
    {
        return false;
    }
    parts[2]
        .to_str()
        .and_then(|s| s.strip_suffix(".mdb"))
        .is_some_and(|s| hex(s, 64))
        && single_file(path)
}

pub(super) fn completed_download_residue(
    path: &Path,
    modified: SystemTime,
    now: SystemTime,
) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let Some(stem) = name.strip_suffix(".incomplete") else {
        return false;
    };
    let fields: Vec<_> = stem.split('.').collect();
    // The old non-random name can resume downloads: never select it.
    if fields.len() != 3
        || !hex(fields[2], 8)
        || !(hex(fields[1], 40) || hex(fields[1], 64))
        || fields[0].len() != 28
        || !fields[0].ends_with('=')
        || !single_file(path)
    {
        return false;
    }
    let Some(parent) = path.parent() else {
        return false;
    };
    let Some(download) = parent.ancestors().take(12).find(|p| {
        p.file_name().is_some_and(|n| n == "download")
            && p.parent().is_some_and(|h| {
                h.file_name().is_some_and(|n| n == "huggingface")
                    && h.parent()
                        .is_some_and(|c| c.file_name().is_some_and(|n| n == ".cache"))
            })
    }) else {
        return false;
    };
    let hf = download.parent().unwrap();
    if !single_file(&hf.join(".gitignore")) || !single_file(&hf.join("CACHEDIR.TAG")) {
        return false;
    }
    let local = hf.parent().and_then(Path::parent).unwrap();
    let Ok(entries) = fs::read_dir(parent) else {
        return false;
    };
    for entry in entries.take(512).flatten() {
        let metadata_path = entry.path();
        let Some(metadata_name) = metadata_path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(target_name) = metadata_name
            .strip_suffix(".metadata")
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        if URL_SAFE.encode(Sha1::digest(metadata_name.as_bytes())) != fields[0]
            || !single_file(&metadata_path)
        {
            continue;
        }
        // Read only the software's three-line cache metadata; never the completed asset.
        let Some(file) = open_single_file(&metadata_path) else {
            continue;
        };
        let Ok(meta) = file.metadata() else {
            continue;
        };
        if meta.len() > 512 {
            continue;
        }
        let mut value = String::new();
        if file.take(513).read_to_string(&mut value).is_err() || value.len() > 512 {
            continue;
        }
        let lines: Vec<_> = value.lines().collect();
        if lines.len() != 3 || !hex(lines[0], 40) || lines[1] != fields[1] {
            continue;
        }
        let Ok(timestamp) = lines[2].parse::<f64>() else {
            continue;
        };
        if !timestamp.is_finite() || timestamp < 0.0 {
            continue;
        }
        let seconds = |time: SystemTime| {
            time.duration_since(SystemTime::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs_f64())
        };
        if !seconds(now).is_some_and(|t| timestamp <= t)
            || !seconds(modified).is_some_and(|t| t <= timestamp)
        {
            continue;
        }
        let Ok(subdir) = parent.strip_prefix(download) else {
            continue;
        };
        let target = local.join(subdir).join(target_name);
        if single_file(&target)
            && fs::metadata(&target)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(seconds)
                .is_some_and(|t| t <= timestamp)
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::full_cleanup_rules::{Context, Kind};
    use std::time::Duration;
    fn write(path: &Path, value: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, value).unwrap();
    }
    fn partial_fixture(root: &Path) -> (std::path::PathBuf, std::path::PathBuf, SystemTime) {
        let metadata_name = "weights.safetensors.metadata";
        let download = root.join(".cache/huggingface/download/subdir");
        let etag = "a".repeat(64);
        let partial = download.join(format!(
            "{}.{}.1234abcd.incomplete",
            URL_SAFE.encode(Sha1::digest(metadata_name.as_bytes())),
            etag
        ));
        let metadata = download.join(metadata_name);
        write(&partial, b"partial");
        write(&root.join("subdir/weights.safetensors"), b"asset");
        write(&root.join(".cache/huggingface/.gitignore"), b"*");
        write(&root.join(".cache/huggingface/CACHEDIR.TAG"), b"fixture");
        let timestamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64()
            + 1.0;
        write(
            &metadata,
            format!("{}\n{etag}\n{timestamp}\n", "b".repeat(40)).as_bytes(),
        );
        (
            partial,
            metadata,
            SystemTime::now() + Duration::from_secs(5),
        )
    }
    #[test]
    fn shard_is_flat_expired_and_keeps_upload_staging_and_assets() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("xet");
        let relative = format!(
            "https___cas_serv-fixture/shard-cache/{}.mdb",
            "a".repeat(64)
        );
        let file = root.join(&relative);
        write(&file, b"shard");
        let context = Context {
            huggingface_xet: vec![root.clone()],
            ..Default::default()
        };
        let now = SystemTime::now();
        let old = now - Duration::from_secs(45 * 86400);
        assert_eq!(
            context.identify(&file, old, now).unwrap().kind,
            Kind::HuggingFaceCache
        );
        let legacy = Context {
            catalog: crate::rule_catalog::Catalog::parse(include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../../rules/cleanup-catalog-v4.json"
            )))
            .unwrap(),
            ..context.clone()
        };
        assert!(legacy.identify(&file, old, now).is_none());
        assert!(context
            .identify(&file, old + Duration::from_secs(1), now)
            .is_none());
        for relative in [
            format!(
                "https___cas_serv-fixture/staging/shard-session/{}.mdb",
                "a".repeat(64)
            ),
            format!(
                "https___cas_serv-fixture/shard-cache/nested/{}.mdb",
                "a".repeat(64)
            ),
            format!(
                "https___cas_serv-fixture/chunk_cache/{}.mdb",
                "a".repeat(64)
            ),
            "https___cas_serv-fixture/shard-cache/model.safetensors".to_string(),
        ] {
            let other = root.join(relative);
            write(&other, b"preserve");
            assert!(context.identify(&other, old, now).is_none());
        }
        fs::remove_file(&file).unwrap();
        assert!(context.identify(&file, old, now).is_none());
    }
    #[test]
    fn partial_requires_completed_asset_exact_metadata_and_retention() {
        let temp = tempfile::tempdir().unwrap();
        let (file, metadata, now) = partial_fixture(temp.path());
        let old = now - Duration::from_secs(30 * 86400);
        let context = Context::default();
        assert_eq!(
            context.identify(&file, old, now).unwrap().kind,
            Kind::HuggingFacePartial
        );
        assert!(context
            .identify(&file, old + Duration::from_secs(1), now)
            .is_none());
        let value = fs::read(&metadata).unwrap();
        write(
            &metadata,
            String::from_utf8(value.clone())
                .unwrap()
                .replace(&"a".repeat(64), &"c".repeat(64))
                .as_bytes(),
        );
        assert!(context.identify(&file, old, now).is_none());
        write(&metadata, &value);
        fs::remove_file(temp.path().join("subdir/weights.safetensors")).unwrap();
        assert!(context.identify(&file, old, now).is_none());
    }
    #[test]
    fn partial_rechecks_markers_and_rejects_unknown_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let (file, metadata, now) = partial_fixture(temp.path());
        let old = now - Duration::from_secs(31 * 86400);
        assert!(completed_download_residue(&file, old, now));
        let context = Context::default();
        let scan = crate::rule_probe::ScanMatcher::new(&context);
        assert!(scan.identify(&file, old, now).is_some());
        fs::remove_file(temp.path().join(".cache/huggingface/CACHEDIR.TAG")).unwrap();
        assert!(!completed_download_residue(&file, old, now));
        assert!(scan.identify(&file, old, now).is_none());
        write(
            &temp.path().join(".cache/huggingface/CACHEDIR.TAG"),
            b"fixture",
        );
        for content in ["future-format\n", "\n", "a\nb\nNaN\n", &"x".repeat(513)] {
            write(&metadata, content.as_bytes());
            assert!(!completed_download_residue(&file, old, now));
        }
    }
    #[test]
    fn resumable_files_model_metadata_locks_and_uploads_are_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let (file, _, now) = partial_fixture(temp.path());
        let old = now - Duration::from_secs(31 * 86400);
        let stem = file
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .replace(".1234abcd", "");
        let legacy = file.with_file_name(stem);
        write(&legacy, b"resume");
        assert!(!completed_download_residue(&legacy, old, now));
        for path in [
            temp.path().join("subdir/weights.safetensors"),
            file.with_extension("lock"),
            file.with_extension("metadata"),
            temp.path()
                .join(".cache/huggingface/upload/item.incomplete"),
        ] {
            assert!(!completed_download_residue(&path, old, now));
        }
    }
    #[test]
    fn symbolic_marker_or_asset_is_never_authorization() {
        let temp = tempfile::tempdir().unwrap();
        let (file, metadata, now) = partial_fixture(temp.path());
        let old = now - Duration::from_secs(31 * 86400);
        let backup = temp.path().join("metadata-copy");
        fs::rename(&metadata, &backup).unwrap();
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_file(&backup, &metadata);
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&backup, &metadata);
        if linked.is_ok() {
            assert!(!completed_download_residue(&file, old, now));
        }
    }
    #[test]
    fn hardlinked_partial_and_metadata_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let (file, metadata, now) = partial_fixture(temp.path());
        let old = now - Duration::from_secs(31 * 86400);
        assert!(completed_download_residue(&file, old, now));
        let alias = temp.path().join("alias");
        fs::hard_link(&file, &alias).unwrap();
        assert!(!completed_download_residue(&file, old, now));
        fs::remove_file(&alias).unwrap();
        fs::hard_link(&metadata, &alias).unwrap();
        assert!(!completed_download_residue(&file, old, now));
    }
    #[test]
    fn metadata_future_negative_nan_and_newer_asset_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let (file, metadata, now) = partial_fixture(temp.path());
        let old = now - Duration::from_secs(31 * 86400);
        for timestamp in [
            "NaN".to_string(),
            "inf".to_string(),
            "-1".to_string(),
            "999999999999".to_string(),
            old.duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                .to_string(),
        ] {
            write(
                &metadata,
                format!("{}\n{}\n{timestamp}\n", "b".repeat(40), "a".repeat(64)).as_bytes(),
            );
            assert!(!completed_download_residue(&file, old, now));
        }
    }
}
