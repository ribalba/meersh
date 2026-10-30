//! Overlay sandboxes for external programs, in the spirit of the `try` tool.
//!
//! The program runs in a private user and mount namespace in which the project directory is
//! an overlay. Reads see the real files, writes land in an upper directory outside the
//! project. Afterwards meersh lists the changes and either commits them, where they enter
//! the ledger and the journal like any other action, or throws them away.

use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use meersh_value::{Record, Value};

use super::{Authority, Cap};
use crate::{Result, ShellError, Stack};

#[derive(Debug)]
pub struct Sandbox {
    pub id: String,
    /// The directory the program sees as an overlay, normally the project root.
    pub lower: PathBuf,
    dir: PathBuf,
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    Added,
    Modified,
    Deleted,
}

impl Sandbox {
    pub fn create(lower: &Path) -> Result<Sandbox> {
        let id = format!("{}-{}", chrono::Local::now().format("%H%M%S"), COUNTER.fetch_add(1, Ordering::Relaxed));
        let dir = dirs::cache_dir()
            .ok_or_else(|| ShellError::Other("no cache directory for sandboxes".into()))?
            .join("meersh")
            .join("sandbox")
            .join(format!("{}-{id}", std::process::id()));
        std::fs::create_dir_all(dir.join("upper"))?;
        std::fs::create_dir_all(dir.join("work"))?;
        Ok(Sandbox { id, lower: lower.to_path_buf(), dir })
    }

    pub fn upper(&self) -> PathBuf {
        self.dir.join("upper")
    }

    /// Arguments for `meersh __exec` that run a program inside this sandbox.
    pub fn helper_args(&self) -> Vec<String> {
        vec![
            "--overlay".into(),
            self.lower.display().to_string(),
            self.upper().display().to_string(),
            self.dir.join("work").display().to_string(),
        ]
    }

    fn walk(&self) -> Vec<(PathBuf, Change, bool)> {
        let mut out = Vec::new();
        walk_upper(&self.upper(), Path::new(""), &self.lower, &mut out);
        out
    }

    pub fn changes(&self) -> Vec<Value> {
        self.walk()
            .into_iter()
            .map(|(rel, change, is_dir)| {
                let upper = self.upper().join(&rel);
                let size = std::fs::symlink_metadata(&upper)
                    .ok()
                    .filter(|m| m.is_file())
                    .map_or(Value::Nothing, |m| Value::Filesize(m.len() as i64));
                Value::Record(
                    Record::typed("sandbox.change")
                        .with("path", super::display_path(&self.lower.join(&rel)))
                        .with(
                            "change",
                            match change {
                                Change::Added if is_dir => "added dir",
                                Change::Added => "added",
                                Change::Modified => "modified",
                                Change::Deleted => "deleted",
                            },
                        )
                        .with("size", size),
                )
            })
            .collect()
    }

    /// Applies the changes to the real directory. New files become owned by the principal,
    /// overwritten files are snapshotted first and deletions go to the trash.
    pub fn commit(&self, authority: &Authority, stack: &Stack) -> Result<Vec<Value>> {
        let mut rows = Vec::new();
        for (rel, change, is_dir) in self.walk() {
            let source = self.upper().join(&rel);
            let target = self.lower.join(&rel);
            let action = match change {
                Change::Added if is_dir => {
                    std::fs::create_dir_all(&target)?;
                    authority.performed(stack, Cap::FsCreate { path: target.clone() })?;
                    "created"
                }
                Change::Added => {
                    copy_entry(&source, &target)?;
                    authority.performed(stack, Cap::FsCreate { path: target.clone() })?;
                    "created"
                }
                Change::Modified => {
                    let cap = Cap::FsWrite { path: target.clone() };
                    authority.prepare(&cap)?;
                    copy_entry(&source, &target)?;
                    authority.performed(stack, cap)?;
                    "overwritten"
                }
                Change::Deleted => {
                    trash::delete(&target)
                        .map_err(|e| ShellError::Other(format!("cannot trash {} ({e})", target.display())))?;
                    authority.performed(stack, Cap::FsDelete { path: target.clone(), permanent: false })?;
                    "trashed"
                }
            };
            rows.push(Value::Record(
                Record::typed("fs.change").with("path", super::display_path(&target)).with("action", action),
            ));
        }
        self.discard();
        Ok(rows)
    }

    pub fn discard(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn copy_entry(source: &Path, target: &Path) -> Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let meta = std::fs::symlink_metadata(source)?;
    if meta.file_type().is_symlink() {
        let _ = std::fs::remove_file(target);
        std::os::unix::fs::symlink(std::fs::read_link(source)?, target)?;
    } else {
        std::fs::copy(source, target)?;
    }
    Ok(())
}

/// Overlayfs marks a deleted file in the upper directory as a character device 0:0.
fn walk_upper(upper_root: &Path, rel: &Path, lower: &Path, out: &mut Vec<(PathBuf, Change, bool)>) {
    let Ok(entries) = std::fs::read_dir(upper_root.join(rel)) else { return };
    let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let child = rel.join(entry.file_name());
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else { continue };
        let in_lower = std::fs::symlink_metadata(lower.join(&child)).is_ok();
        if meta.file_type().is_char_device() && meta.rdev() == 0 {
            out.push((child, Change::Deleted, false));
        } else if meta.is_dir() {
            if !in_lower {
                out.push((child.clone(), Change::Added, true));
            }
            walk_upper(upper_root, &child, lower, out);
        } else {
            out.push((child, if in_lower { Change::Modified } else { Change::Added }, false));
        }
    }
}
