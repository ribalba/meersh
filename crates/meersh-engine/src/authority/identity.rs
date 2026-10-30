use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

/// Makes a path absolute against the working directory and removes `.` and `..` lexically.
/// Symlinks are not resolved, because removing a symlink must not mean removing its target.
pub fn absolute(path: &Path) -> PathBuf {
    let joined =
        if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir().unwrap_or_default().join(path) };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// Identity of a file system object. A file is the same object only while device, inode,
/// size and modification time match. A directory is identified by device and inode.
pub fn file_id(path: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if meta.is_dir() {
        return Some(format!("file:{}:{}", meta.dev(), meta.ino()));
    }
    Some(format!("file:{}:{}:{}:{}", meta.dev(), meta.ino(), meta.size(), mtime_ns(&meta)))
}

pub(crate) fn mtime_ns(meta: &std::fs::Metadata) -> i64 {
    meta.mtime() * 1_000_000_000 + meta.mtime_nsec()
}

/// Identity of a process. The start time makes a reused pid a different process.
pub fn proc_id(pid: u32) -> Option<String> {
    Some(format!("proc:{pid}:{}", proc_start(pid)?))
}

/// Field 22 of `/proc/<pid>/stat`, the start time in clock ticks since boot. The command
/// name in field 2 may contain spaces and parentheses, so counting starts after the last `)`.
pub(crate) fn proc_start(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// The top of the git repository around the working directory, or the working directory.
pub fn project_root() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_default();
    cwd.ancestors().find(|dir| dir.join(".git").exists()).map_or_else(|| cwd.clone(), Path::to_path_buf)
}

/// Paths that no model may touch and that the user is asked about.
pub(crate) fn protected(path: &Path) -> Option<String> {
    let home = dirs::home_dir().unwrap_or_default();
    if path == Path::new("/") {
        return Some("the root directory".into());
    }
    if home.starts_with(path) {
        return Some("the home directory or one of its parents".into());
    }
    if path.components().any(|c| c.as_os_str() == ".git") {
        return Some("inside a git repository's .git".into());
    }
    for (dir, what) in [(".ssh", "SSH keys"), (".gnupg", "GnuPG keys"), (".config/meersh", "meersh configuration")] {
        if path.starts_with(home.join(dir)) {
            return Some(what.into());
        }
    }
    for system in ["/etc", "/usr", "/boot", "/bin", "/sbin", "/lib", "/lib64", "/var/lib"] {
        if path.starts_with(system) {
            return Some(format!("a system directory ({system})"));
        }
    }
    None
}

/// A file that git tracks and that has no uncommitted changes can be brought back with
/// `git checkout`, so even a permanent delete of it is reversible.
pub(crate) fn git_restorable(path: &Path) -> bool {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else { return false };
    let git = |args: &[&std::ffi::OsStr]| {
        Command::new("git").arg("-C").arg(dir).args(args).stdout(Stdio::piped()).stderr(Stdio::null()).output().ok()
    };
    let tracked = git(&["ls-files".as_ref(), "--error-unmatch".as_ref(), "--".as_ref(), name])
        .is_some_and(|o| o.status.success());
    let clean = git(&["status".as_ref(), "--porcelain".as_ref(), "--".as_ref(), name])
        .is_some_and(|o| o.status.success() && o.stdout.is_empty());
    tracked && clean
}
