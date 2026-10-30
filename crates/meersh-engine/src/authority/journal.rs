use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};
use meersh_value::{Record, Value};

use super::{Ledger, Ownership, Principal};
use crate::{Result, ShellError};

/// How to take an action back.
#[derive(Debug, Clone)]
pub enum Inverse {
    /// Signals, permanent deletes and external programs.
    None,
    /// Restore the newest trash item for `path` deleted after `after` (seconds since epoch).
    RestoreTrash {
        path: PathBuf,
        after: i64,
    },
    MoveBack {
        original: PathBuf,
        current: PathBuf,
    },
    /// Remove something the session created, as long as it is unchanged.
    RemoveCreated {
        path: PathBuf,
    },
    RestoreSnapshot {
        path: PathBuf,
        snapshot: PathBuf,
    },
    Chmod {
        path: PathBuf,
        mode: u32,
    },
}

#[derive(Debug, Clone)]
pub struct JournalEntry {
    pub id: u64,
    pub at: DateTime<Local>,
    pub principal: String,
    pub task: Option<String>,
    pub action: String,
    pub target: String,
    pub detail: String,
    pub inverse: Inverse,
    pub undone: bool,
}

impl JournalEntry {
    pub fn to_value(&self) -> Value {
        Value::Record(
            Record::typed("journal.entry")
                .with("id", self.id as i64)
                .with("at", Value::Date(self.at.fixed_offset()))
                .with("principal", self.principal.as_str())
                .with("task", self.task.clone())
                .with("action", self.action.as_str())
                .with("target", self.target.as_str())
                .with("detail", (!self.detail.is_empty()).then(|| self.detail.clone()))
                .with("undoable", !matches!(self.inverse, Inverse::None) && !self.undone)
                .with("undone", self.undone),
        )
    }
}

/// Copies a file into the snapshot directory before it is overwritten.
pub(crate) fn snapshot(path: &Path) -> Result<PathBuf> {
    let dir = dirs::cache_dir()
        .ok_or_else(|| ShellError::Other("no cache directory for snapshots".into()))?
        .join("meersh")
        .join("snapshots");
    std::fs::create_dir_all(&dir)?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let target = dir.join(format!("{}-{name}", Local::now().format("%Y%m%d-%H%M%S%.f")));
    std::fs::copy(path, &target)?;
    Ok(target)
}

pub(crate) fn reverse(inverse: &Inverse, ledger: &Ledger, session: &str) -> Result<String> {
    match inverse {
        Inverse::None => Err(ShellError::Other("this action cannot be undone".into())),
        Inverse::RestoreTrash { path, after } => {
            let items =
                trash::os_limited::list().map_err(|e| ShellError::Other(format!("cannot read the trash ({e})")))?;
            let item = items
                .into_iter()
                .filter(|item| item.original_path() == *path && item.time_deleted >= *after)
                .max_by_key(|item| item.time_deleted)
                .ok_or_else(|| ShellError::Other(format!("{} is no longer in the trash", path.display())))?;
            if path.exists() {
                return Err(ShellError::Other(format!("{} exists again, not restoring over it", path.display())));
            }
            trash::os_limited::restore_all([item]).map_err(|e| ShellError::Other(format!("cannot restore ({e})")))?;
            Ok("restored from the trash".into())
        }
        Inverse::MoveBack { original, current } => {
            if original.exists() {
                return Err(ShellError::Other(format!("{} exists again, not moving over it", original.display())));
            }
            std::fs::rename(current, original)?;
            ledger.moved(current, original);
            Ok(format!("moved back to {}", original.display()))
        }
        Inverse::RemoveCreated { path } => {
            // Undo runs for the user, who owns everything the session created.
            match ledger.ownership(path, &Principal::User, session) {
                Ownership::Owned => {
                    trash::delete(path)
                        .map_err(|e| ShellError::Other(format!("cannot trash {} ({e})", path.display())))?;
                    ledger.forget(path);
                    Ok("moved to the trash".into())
                }
                Ownership::NotOwned if !path.exists() => Ok("already gone".into()),
                _ => {
                    Err(ShellError::Other(format!("{} changed since it was created, not removing it", path.display())))
                }
            }
        }
        Inverse::RestoreSnapshot { path, snapshot } => {
            std::fs::copy(snapshot, path)?;
            Ok("old content restored".into())
        }
        Inverse::Chmod { path, mode } => {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(*mode))?;
            Ok(format!("mode {mode:o} restored"))
        }
    }
}
