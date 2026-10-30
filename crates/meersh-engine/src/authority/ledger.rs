//! The ledger of everything meersh created, and the leases on files that are being worked on.
//!
//! It is stored in one JSON file that all meersh processes of the user share, guarded by
//! `flock`. That way several agents working in one project see what the others created and
//! which files they have leased. Entries whose object no longer exists are dropped on write.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use meersh_value::{Record, Value};
use nix::fcntl::{Flock, FlockArg};
use serde::{Deserialize, Serialize};

use super::Principal;
use super::identity::{mtime_ns, proc_start};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub kind: String,
    pub path: Option<String>,
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime: i64,
    pub pid: Option<u32>,
    pub start: Option<u64>,
    pub program: Option<String>,
    pub principal: String,
    pub session: String,
    pub task: Option<String>,
    pub created: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Lease {
    path: String,
    principal: String,
    until: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    entries: Vec<LedgerEntry>,
    leases: Vec<Lease>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ownership {
    /// Created by this principal or session and unchanged since.
    Owned,
    /// Created by this principal or session, but someone changed it since.
    Changed(String),
    /// Created by another principal.
    Other(String),
    NotOwned,
}

pub struct Ledger {
    /// `None` keeps everything in memory, which tests use.
    file: Option<PathBuf>,
    memory: Mutex<Store>,
}

impl Ledger {
    pub fn in_memory() -> Self {
        Ledger { file: None, memory: Mutex::default() }
    }

    /// `$MEERSH_LEDGER`, or `ledger.json` in the meersh data directory.
    pub fn shared() -> Self {
        let file = std::env::var_os("MEERSH_LEDGER")
            .map(PathBuf::from)
            .or_else(|| Some(dirs::data_dir()?.join("meersh").join("ledger.json")));
        Ledger { file, memory: Mutex::default() }
    }

    fn with_store<T>(&self, write: bool, f: impl FnOnce(&mut Store) -> T) -> T {
        let Some(path) = &self.file else {
            return f(&mut self.memory.lock().expect("ledger lock"));
        };
        // Reading creates nothing. A missing ledger is an empty one.
        if !write && !path.exists() {
            return f(&mut Store::default());
        }
        let locked = path
            .parent()
            .map(std::fs::create_dir_all)
            .transpose()
            .ok()
            .and_then(|_| OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path).ok())
            .and_then(|file| Flock::lock(file, FlockArg::LockExclusive).ok());
        let Some(mut file) = locked else {
            // A ledger that cannot be opened degrades to memory instead of breaking the shell.
            return f(&mut self.memory.lock().expect("ledger lock"));
        };
        let mut text = String::new();
        let _ = file.read_to_string(&mut text);
        let mut store: Store = serde_json::from_str(&text).unwrap_or_default();
        let result = f(&mut store);
        if write {
            store.entries.retain(still_exists);
            let now = chrono::Local::now().to_rfc3339();
            store.leases.retain(|l| l.until > now);
            if let Ok(text) = serde_json::to_string(&store) {
                let file: &mut File = &mut file;
                let _ = file.set_len(0);
                let _ = file.rewind();
                let _ = file.write_all(text.as_bytes());
            }
        }
        result
    }

    pub fn record_path(&self, path: &Path, principal: &Principal, session: &str, task: Option<&str>) {
        let Ok(meta) = std::fs::symlink_metadata(path) else { return };
        let entry = LedgerEntry {
            kind: if meta.is_dir() { "dir".into() } else { "file".into() },
            path: Some(path.display().to_string()),
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.size(),
            mtime: mtime_ns(&meta),
            pid: None,
            start: None,
            program: None,
            principal: principal.name(),
            session: session.into(),
            task: task.map(str::to_string),
            created: chrono::Local::now().to_rfc3339(),
        };
        self.with_store(true, |store| {
            store.entries.retain(|e| !(e.dev == entry.dev && e.ino == entry.ino && e.pid.is_none()));
            store.entries.push(entry);
        });
    }

    pub fn record_process(&self, pid: u32, program: &str, principal: &Principal, session: &str, task: Option<&str>) {
        let entry = LedgerEntry {
            kind: "process".into(),
            path: None,
            dev: 0,
            ino: 0,
            size: 0,
            mtime: 0,
            pid: Some(pid),
            start: proc_start(pid),
            program: Some(program.into()),
            principal: principal.name(),
            session: session.into(),
            task: task.map(str::to_string),
            created: chrono::Local::now().to_rfc3339(),
        };
        self.with_store(true, |store| store.entries.push(entry));
    }

    /// The session changed an object it owns itself. It stays owned with the new content.
    pub fn refresh(&self, path: &Path, principal: &Principal, session: &str) {
        let Ok(meta) = std::fs::symlink_metadata(path) else { return };
        self.with_store(true, |store| {
            for e in store.entries.iter_mut().filter(|e| e.dev == meta.dev() && e.ino == meta.ino()) {
                if belongs(e, principal, session) {
                    e.size = meta.size();
                    e.mtime = mtime_ns(&meta);
                }
            }
        });
    }

    pub fn moved(&self, from: &Path, to: &Path) {
        let Ok(meta) = std::fs::symlink_metadata(to) else { return };
        let from = from.display().to_string();
        self.with_store(true, |store| {
            for e in store.entries.iter_mut() {
                let Some(path) = e.path.clone() else { continue };
                if path == from || Path::new(&path).starts_with(&from) {
                    let rest = Path::new(&path).strip_prefix(&from).unwrap_or(Path::new("")).to_path_buf();
                    let new_path = if rest.as_os_str().is_empty() { to.to_path_buf() } else { to.join(rest) };
                    if path == from {
                        e.dev = meta.dev();
                        e.ino = meta.ino();
                    }
                    e.path = Some(new_path.display().to_string());
                }
            }
        });
    }

    pub fn forget(&self, path: &Path) {
        let path = path.display().to_string();
        self.with_store(true, |store| {
            store.entries.retain(|e| e.path.as_deref().is_none_or(|p| p != path && !Path::new(p).starts_with(&path)));
        });
    }

    pub fn ownership(&self, path: &Path, principal: &Principal, session: &str) -> Ownership {
        self.with_store(false, |store| ownership_in(store, path, principal, session, 0))
    }

    pub fn process_ownership(&self, pid: u32, principal: &Principal, session: &str) -> Ownership {
        let start = proc_start(pid);
        self.with_store(false, |store| {
            match store.entries.iter().rev().find(|e| e.pid == Some(pid) && e.start == start && start.is_some()) {
                Some(e) if belongs(e, principal, session) => Ownership::Owned,
                Some(e) => Ownership::Other(e.principal.clone()),
                None => Ownership::NotOwned,
            }
        })
    }

    pub fn lease(&self, path: &Path, principal: &Principal, minutes: i64) -> String {
        let until = (chrono::Local::now() + chrono::Duration::minutes(minutes)).to_rfc3339();
        let lease = Lease { path: path.display().to_string(), principal: principal.name(), until: until.clone() };
        self.with_store(true, |store| {
            store.leases.retain(|l| !(l.path == lease.path && l.principal == lease.principal));
            store.leases.push(lease);
        });
        until
    }

    pub fn release(&self, path: &Path, principal: &Principal) -> bool {
        let path = path.display().to_string();
        self.with_store(true, |store| {
            let before = store.leases.len();
            store.leases.retain(|l| !(l.path == path && l.principal == principal.name()));
            before != store.leases.len()
        })
    }

    /// The holder of a live lease on the path or one of its parent directories.
    pub fn lease_holder(&self, path: &Path) -> Option<(String, String)> {
        let now = chrono::Local::now().to_rfc3339();
        self.with_store(false, |store| {
            store
                .leases
                .iter()
                .find(|l| l.until > now && path.starts_with(&l.path))
                .map(|l| (l.principal.clone(), l.until.chars().take(16).collect()))
        })
    }

    pub fn rows(&self) -> Vec<Value> {
        self.with_store(false, |store| {
            store
                .entries
                .iter()
                .filter(|e| still_exists(e))
                .map(|e| {
                    let cwd = std::env::current_dir().unwrap_or_default();
                    let object = match (&e.path, e.pid) {
                        (Some(path), _) => Path::new(path)
                            .strip_prefix(&cwd)
                            .map_or_else(|_| path.clone(), |p| p.display().to_string()),
                        (None, Some(pid)) => format!("pid {pid} {}", e.program.clone().unwrap_or_default()),
                        _ => String::new(),
                    };
                    let state = match (&e.path, e.pid) {
                        (Some(path), _) => match std::fs::symlink_metadata(path) {
                            Ok(m) if e.kind == "dir" || (m.size() == e.size && mtime_ns(&m) == e.mtime) => "owned",
                            _ => "changed",
                        },
                        _ => "running",
                    };
                    Value::Record(
                        Record::typed("ledger.entry")
                            .with("object", object)
                            .with("kind", e.kind.as_str())
                            .with("principal", e.principal.as_str())
                            .with("task", e.task.clone())
                            .with("state", state)
                            .with(
                                "created",
                                chrono::DateTime::parse_from_rfc3339(&e.created).map_or(Value::Nothing, Value::Date),
                            ),
                    )
                })
                .collect()
        })
    }

    pub fn lease_rows(&self) -> Vec<Value> {
        let now = chrono::Local::now().to_rfc3339();
        self.with_store(false, |store| {
            store
                .leases
                .iter()
                .filter(|l| l.until > now)
                .map(|l| {
                    Value::Record(
                        Record::typed("lease").with("path", l.path.as_str()).with("holder", l.principal.as_str()).with(
                            "until",
                            chrono::DateTime::parse_from_rfc3339(&l.until).map_or(Value::Nothing, Value::Date),
                        ),
                    )
                })
                .collect()
        })
    }
}

/// Within one session every principal owns what the session created. Across sessions only
/// persistent principals (the user and named agents) keep their ownership.
fn belongs(entry: &LedgerEntry, principal: &Principal, session: &str) -> bool {
    entry.session == session
        || (matches!(principal, Principal::User | Principal::Agent(_)) && entry.principal == principal.name())
}

fn still_exists(entry: &LedgerEntry) -> bool {
    match (&entry.path, entry.pid) {
        (Some(path), _) => std::fs::symlink_metadata(path).is_ok_and(|m| m.dev() == entry.dev && m.ino() == entry.ino),
        (None, Some(pid)) => proc_start(pid) == entry.start && entry.start.is_some(),
        _ => false,
    }
}

const MAX_TREE: usize = 10_000;

/// A directory is owned only when everything below it is owned too, so removing it cannot
/// take files with it that someone else put there.
fn ownership_in(store: &Store, path: &Path, principal: &Principal, session: &str, depth: usize) -> Ownership {
    let Ok(meta) = std::fs::symlink_metadata(path) else { return Ownership::NotOwned };
    let Some(entry) =
        store.entries.iter().rev().find(|e| e.pid.is_none() && e.dev == meta.dev() && e.ino == meta.ino())
    else {
        return Ownership::NotOwned;
    };
    if !belongs(entry, principal, session) {
        return Ownership::Other(entry.principal.clone());
    }
    if !meta.is_dir() {
        return if meta.size() == entry.size && mtime_ns(&meta) == entry.mtime {
            Ownership::Owned
        } else {
            Ownership::Changed("changed since it was created".into())
        };
    }
    let Ok(children) = std::fs::read_dir(path) else { return Ownership::Changed("cannot be read".into()) };
    for (count, child) in children.enumerate() {
        if count + depth > MAX_TREE {
            return Ownership::Changed("too large to check".into());
        }
        let Ok(child) = child else { continue };
        match ownership_in(store, &child.path(), principal, session, depth + count + 1) {
            Ownership::Owned => {}
            _ => {
                return Ownership::Changed(format!(
                    "contains {}, which this session did not create",
                    child.file_name().to_string_lossy()
                ));
            }
        }
    }
    Ownership::Owned
}
