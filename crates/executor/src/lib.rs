//! Performs cleanup actions safely. Recycle (default), quarantine, or permanent delete.
//! Every action is appended to `undo.jsonl`.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Recycle,
    Quarantine,
    Delete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub action: Action,
    pub paths: Vec<PathBuf>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoEntry {
    pub timestamp: String,
    pub action: Action,
    pub source: PathBuf,
    pub destination: Option<PathBuf>,
    pub reason: String,
}

pub fn execute(
    plan: &Plan,
    dry_run: bool,
    undo_log: &Path,
    quarantine_root: &Path,
) -> anyhow::Result<Vec<UndoEntry>> {
    let mut out: Vec<UndoEntry> = Vec::new();
    let now = || chrono::Utc::now().to_rfc3339();

    if dry_run {
        for p in &plan.paths {
            out.push(UndoEntry {
                timestamp: now(),
                action: plan.action,
                source: p.clone(),
                destination: None,
                reason: format!("dry-run: {}", plan.reason),
            });
        }
        write_log(undo_log, &out)?;
        return Ok(out);
    }

    match plan.action {
        Action::Recycle => {
            trash::delete_all(&plan.paths)?;
            for p in &plan.paths {
                out.push(UndoEntry {
                    timestamp: now(),
                    action: Action::Recycle,
                    source: p.clone(),
                    destination: None,
                    reason: plan.reason.clone(),
                });
            }
        }
        Action::Quarantine => {
            std::fs::create_dir_all(quarantine_root)?;
            for src in &plan.paths {
                let stamp = chrono::Utc::now().timestamp_millis();
                let leaf = src
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "item".into());
                let dst = quarantine_root.join(format!("{stamp}-{leaf}"));
                if let Err(e) = std::fs::rename(src, &dst) {
                    tracing::warn!("rename failed ({}); falling back to copy+remove", e);
                    copy_then_remove(src, &dst)?;
                }
                out.push(UndoEntry {
                    timestamp: now(),
                    action: Action::Quarantine,
                    source: src.clone(),
                    destination: Some(dst),
                    reason: plan.reason.clone(),
                });
            }
        }
        Action::Delete => {
            for p in &plan.paths {
                let metadata = std::fs::symlink_metadata(p)?;
                if metadata.file_type().is_symlink() {
                    anyhow::bail!("不直接删除链接目标：{}", p.display());
                }
                if metadata.is_dir() {
                    std::fs::remove_dir_all(p)?;
                } else if metadata.is_file() {
                    std::fs::remove_file(p)?;
                } else {
                    anyhow::bail!("不是可删除的普通文件或目录：{}", p.display());
                }
                out.push(UndoEntry {
                    timestamp: now(),
                    action: Action::Delete,
                    source: p.clone(),
                    destination: None,
                    reason: plan.reason.clone(),
                });
            }
        }
    }

    write_log(undo_log, &out)?;
    Ok(out)
}

fn write_log(undo_log: &Path, entries: &[UndoEntry]) -> anyhow::Result<()> {
    if let Some(parent) = undo_log.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(undo_log)?;
    for e in entries {
        writeln!(f, "{}", serde_json::to_string(e)?)?;
    }
    Ok(())
}

fn copy_then_remove(src: &Path, dst: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        copy_dir_recursive(src, dst)?;
        std::fs::remove_dir_all(src)?;
    } else {
        std::fs::copy(src, dst)?;
        std::fs::remove_file(src)?;
    }
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let p = entry.path();
        let d = dst.join(entry.file_name());
        if p.is_dir() {
            copy_dir_recursive(&p, &d)?;
        } else {
            std::fs::copy(&p, &d)?;
        }
    }
    Ok(())
}
