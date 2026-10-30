//! The mechanism behind `cp` and `mv`. The commands in `files.rs` decide what is copied or
//! moved where and get the right to do it. This module only does the work, through the
//! uutils crates `uu_cp` and `uu_mv` used as libraries, the way Nushell uses them. They carry
//! the GNU edge cases such as reflinks, sparse files, special files and moves across file
//! systems.
//!
//! Both crates resolve relative paths against the working directory of the process, so every
//! path is made absolute first. Failures come back as a reason in the style of
//! [`crate::reason`], such as `permission denied`, without the prefixes of uutils.
//!
//! Two limits of uutils 0.12 as a library shape this module. `uu_cp::copy` prints the reason
//! for every file it could not copy on stderr itself and returns at most that something
//! failed. Inside a tree it even returns Ok after skipping what it could not read, so the
//! copy is checked against the source afterwards. And uucore holds the message catalog of a
//! single utility per process, so the messages of `uu_mv` stay bare ids such as
//! `mv-error-directory-not-empty` once `uu_cp` has loaded its catalog. That is why the common
//! refusals are checked here, in meersh's own words.

use std::fs;
use std::path::{Path, PathBuf};

use uu_cp::{Attributes, ClobberMode, CpError, OverwriteMode, Preserve};

/// Copies `source` to exactly `target`, never into it. With `recursive` a directory is
/// copied with its contents and every symlink, also `source` itself, is copied as a link.
/// Without it a symlink given as `source` is followed, like GNU cp does.
pub(crate) fn copy(source: &Path, target: &Path, recursive: bool) -> Result<(), String> {
    let (source, target) = (absolute(source)?, absolute(target)?);
    let meta = if recursive { fs::symlink_metadata(&source) } else { fs::metadata(&source) };
    let source_is_dir = meta.is_ok_and(|m| m.is_dir());
    refuse_kind_change(source_is_dir, &target)?;
    let options = uu_cp::Options {
        recursive,
        dereference: !recursive,
        // meersh already put the source name on the target.
        no_target_dir: true,
        // meersh asked for the right to overwrite, so uutils must not ask again.
        overwrite: OverwriteMode::Clobber(ClobberMode::Standard),
        // Keep the permission bits but not owner, times or extended attributes, as the std
        // implementation did. A file system without permissions does not fail the copy.
        attributes: Attributes { mode: Preserve::Yes { required: false }, ..Attributes::NONE },
        // Everything else is the GNU default: no backups, reflinks and sparse files where the
        // file system supports them, no progress bar, no prompts.
        ..uu_cp::Options::default()
    };
    // The catalog is per thread and empty until it is set up, which makes uu_cp print bare
    // message ids.
    let _ = uucore::locale::setup_localization("cp");
    let failed = match uu_cp::copy(std::slice::from_ref(&source), &target, &options) {
        Ok(()) if !source_is_dir => return Ok(()),
        Ok(()) => false,
        Err(CpError::NotAllFilesCopied) => true,
        Err(CpError::IoErr(e) | CpError::IoErrContext(e, _)) => return Err(crate::reason(&e)),
        Err(other) => return Err(uu_reason(&other.to_string())),
    };
    // Within a tree uu_cp skips what it cannot read and still returns Ok. In both cases it has
    // printed why on stderr, so naming what is missing is all meersh can add.
    let what = match left_out(&source, &target).as_slice() {
        [] if !failed => return Ok(()),
        [] => "some files were not copied".to_string(),
        [first, ..] if *first == source => "nothing was copied".to_string(),
        [one] => format!("'{}' was left out", one.display()),
        [first, rest @ ..] => format!("'{}' and {} more paths were left out", first.display(), rest.len()),
    };
    Err(format!("{what}, uutils printed why on stderr"))
}

/// The paths of `source` that have no counterpart below `target`, and the directories that
/// could not be read, outermost first. A file that already existed in the target counts as
/// copied, so a failed overwrite inside a merge goes unnoticed.
fn left_out(source: &Path, target: &Path) -> Vec<PathBuf> {
    let mut left_out = Vec::new();
    let mut pending = vec![source.to_path_buf()];
    while let Some(path) = pending.pop() {
        let copy = match path.strip_prefix(source) {
            Ok(rel) if !rel.as_os_str().is_empty() => target.join(rel),
            _ => target.to_path_buf(),
        };
        let Ok(meta) = fs::symlink_metadata(&path) else { continue };
        if fs::symlink_metadata(&copy).is_err() {
            left_out.push(path);
        } else if meta.is_dir() {
            match fs::read_dir(&path) {
                Ok(children) => pending.extend(children.flatten().map(|child| child.path())),
                Err(_) => left_out.push(path),
            }
        }
    }
    left_out
}

/// Moves `source` to exactly `target`, never into it. A rename where possible, otherwise a
/// copy that keeps permissions, links and extended attributes, followed by removing the
/// source.
pub(crate) fn move_path(source: &Path, target: &Path) -> Result<(), String> {
    let (source, target) = (absolute(source)?, absolute(target)?);
    let source_is_dir = fs::symlink_metadata(&source).is_ok_and(|m| m.is_dir());
    refuse_kind_change(source_is_dir, &target)?;
    // A directory only replaces an empty one, like rename(2) does.
    if source_is_dir && fs::read_dir(&target).is_ok_and(|mut children| children.next().is_some()) {
        return Err("the target is a directory that is not empty".into());
    }
    let options = uu_mv::Options {
        // meersh asked for the right to replace the target, so uutils must not ask again.
        overwrite: uu_mv::OverwriteMode::Force,
        no_target_dir: true,
        ..uu_mv::Options::default()
    };
    uu_mv::mv(&[source.into_os_string(), target.into_os_string()], &options).map_err(|e| uu_reason(&e.to_string()))
}

/// Every path at and below `root`, parents before their children. Symlinks are listed but
/// not followed.
pub(crate) fn tree(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let Ok(meta) = fs::symlink_metadata(&path) else { continue };
        if meta.is_dir()
            && let Ok(children) = fs::read_dir(&path)
        {
            pending.extend(children.flatten().map(|child| child.path()));
        }
        paths.push(path);
    }
    paths
}

fn absolute(path: &Path) -> Result<PathBuf, String> {
    std::path::absolute(path).map_err(|e| crate::reason(&e))
}

/// GNU cp and mv never replace a directory with something else or the other way round.
fn refuse_kind_change(source_is_dir: bool, target: &Path) -> Result<(), String> {
    if fs::symlink_metadata(target).is_err() {
        return Ok(());
    }
    match (source_is_dir, target.is_dir()) {
        (false, true) => Err("the target is a directory and the source is not".into()),
        (true, false) => Err("the source is a directory and the target is not".into()),
        _ => Ok(()),
    }
}

/// The reason at the end of a uutils message such as `cannot move 'a' to 'b': Permission
/// denied`, in the style of [`crate::reason`]. The context in front of it is dropped, because
/// it may be a bare message id.
fn uu_reason(message: &str) -> String {
    let reason = message.rsplit_once(": ").map_or(message, |(_, reason)| reason).trim();
    let reason = reason.split_once(" (os error ").map_or(reason, |(reason, _)| reason);
    if !reason.contains(' ') && reason.contains("-error-") {
        return format!("uutils reported {reason}");
    }
    let mut chars = reason.chars();
    chars.next().map_or_else(String::new, |first| first.to_lowercase().chain(chars).collect())
}

/// The std implementation that uutils replaced, kept so the tests can compare both.
#[cfg(test)]
pub(crate) mod std_impl {
    use std::fs;
    use std::io::{self, ErrorKind};
    use std::os::unix::fs::symlink;
    use std::path::Path;

    /// Below the top level, and at the top level when `keep_links` is set, symlinks are
    /// recreated instead of followed.
    pub(crate) fn copy(source: &Path, target: &Path, keep_links: bool) -> io::Result<()> {
        if keep_links && fs::symlink_metadata(source)?.file_type().is_symlink() {
            return symlink(fs::read_link(source)?, target);
        }
        let meta = fs::metadata(source)?;
        if !meta.is_dir() {
            return fs::copy(source, target).map(drop);
        }
        fs::create_dir_all(target)?;
        for child in fs::read_dir(source)? {
            let child = child?;
            copy(&child.path(), &target.join(child.file_name()), true)?;
        }
        // Set last, so a read only source directory does not block copying its contents.
        fs::set_permissions(target, meta.permissions())
    }

    pub(crate) fn move_path(source: &Path, target: &Path) -> io::Result<()> {
        match fs::rename(source, target) {
            Err(e) if e.kind() == ErrorKind::CrossesDevices => {
                copy(source, target, true)?;
                if fs::symlink_metadata(source)?.is_dir() {
                    fs::remove_dir_all(source)
                } else {
                    fs::remove_file(source)
                }
            }
            other => other,
        }
    }
}
