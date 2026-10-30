use std::collections::HashSet;
use std::fs::{self, Metadata};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use meersh_engine::util::expand_path;
use meersh_engine::{Command, Ctx, Effect, Engine, PipelineData, Result, Signature};
use meersh_value::{Record, Type, Value};
use nix::sys::statvfs::statvfs;

use crate::{arg_paths, reason, table};

pub fn register(engine: &mut Engine) {
    engine.register(Du);
    engine.register(Df);
}

/// A child name as shown in a table. Below `.` the bare relative path reads best.
pub(crate) fn join_name(parent: &str, child: &str) -> String {
    if parent == "." || parent == "./" {
        child.to_string()
    } else {
        Path::new(parent).join(child).display().to_string()
    }
}

struct Du;

impl Command for Du {
    fn signature(&self) -> Signature {
        Signature::build("du")
            .desc(
                "Disk usage as fs.usage records, one per path, with the apparent size and the number of all files \
                 below it. With --max-depth, directories down to that depth get rows of their own. Symlinks are \
                 not followed and hard links count once.",
            )
            .input(Type::Nothing)
            .output(Type::table("fs.usage"))
            .rest("paths", Type::String, "files or directories, the current directory by default")
            .named("max-depth", Type::Int, Some('d'), "also list directories down to this depth below each path")
            .effect(Effect::Read)
            .example("du ~/Downloads ~/Videos", "how much two folders hold")
            .example("du -d 1 | sort-by size -r | first 5", "the five largest directories here")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let max_depth = match ctx.named("max-depth")? {
            None => 0,
            Some(v) => v
                .as_int()
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| ctx.err("--max-depth needs a whole number of zero or more"))?,
        };
        let mut args = arg_paths(ctx, 0)?;
        if args.is_empty() {
            args.push(".".into());
        }
        let mut walk = Walk { ctx, max_depth, seen: HashSet::new(), rows: Vec::new() };
        for arg in args {
            let path = expand_path(&arg);
            let meta =
                fs::symlink_metadata(&path).map_err(|e| ctx.err(format!("cannot access '{arg}' ({})", reason(&e))))?;
            walk.visit(&path, arg, &meta, 0)?;
        }
        Ok(table(walk.rows))
    }
}

#[derive(Default, Clone, Copy)]
struct Usage {
    size: u64,
    files: u64,
}

struct Walk<'a, 'b> {
    ctx: &'a Ctx<'b>,
    max_depth: usize,
    /// Device and inode of files with more than one link, so each is counted once.
    seen: HashSet<(u64, u64)>,
    rows: Vec<Record>,
}

impl Walk<'_, '_> {
    fn visit(&mut self, path: &Path, name: String, meta: &Metadata, depth: usize) -> Result<Usage> {
        if !meta.is_dir() {
            let counted = meta.nlink() <= 1 || self.seen.insert((meta.dev(), meta.ino()));
            let usage = if counted { Usage { size: meta.len(), files: 1 } } else { Usage::default() };
            if depth == 0 {
                self.rows.push(row(name, usage));
            }
            return Ok(usage);
        }
        self.ctx.engine.check_interrupt()?;
        // The row goes in before the rows of the subdirectories and is filled in afterwards.
        let slot = (depth <= self.max_depth).then(|| {
            self.rows.push(Record::new());
            self.rows.len() - 1
        });
        let mut children: Vec<(String, PathBuf, Metadata)> = match fs::read_dir(path) {
            // Unreadable directories and entries that vanish while walking are skipped.
            Ok(entries) => entries
                .flatten()
                .filter_map(|e| Some((e.file_name().to_string_lossy().into_owned(), e.path(), e.metadata().ok()?)))
                .collect(),
            Err(_) => Vec::new(),
        };
        children.sort_by(|a, b| a.0.cmp(&b.0));
        let mut total = Usage::default();
        for (file_name, child, child_meta) in children {
            let usage = self.visit(&child, join_name(&name, &file_name), &child_meta, depth + 1)?;
            total.size += usage.size;
            total.files += usage.files;
        }
        if let Some(i) = slot {
            self.rows[i] = row(name, total);
        }
        Ok(total)
    }
}

fn row(path: String, usage: Usage) -> Record {
    Record::typed("fs.usage")
        .with("path", path)
        .with("size", Value::Filesize(usage.size as i64))
        .with("files", usage.files as i64)
}

/// `/proc/self/mounts` writes space, tab, newline and backslash as octal escapes like `\040`.
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let octal =
            bytes.get(i + 1..i + 4).filter(|d| bytes[i] == b'\\' && d.iter().all(|c| (b'0'..=b'7').contains(c)));
        match octal.and_then(|d| u8::from_str_radix(std::str::from_utf8(d).ok()?, 8).ok()) {
            Some(byte) => {
                out.push(byte);
                i += 4;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The statvfs counters are `c_ulong` or `fsblkcnt_t`, whose width depends on the platform.
fn widen(n: impl Into<u64>) -> u64 {
    n.into()
}

struct Df;

impl Command for Df {
    fn signature(&self) -> Signature {
        Signature::build("df")
            .desc(
                "Mounted filesystems as fs.mount records with their size, used and available space. use is the \
                 used share in percent of what unprivileged users can have, as in GNU df. Pseudo filesystems of \
                 size 0, automount points and mounts that cannot be queried are left out unless --all is given.",
            )
            .input(Type::Nothing)
            .output(Type::table("fs.mount"))
            .switch("all", Some('a'), "include pseudo filesystems, automount points and mounts that refuse statvfs")
            .effect(Effect::Read)
            .example("df | where use > 90", "filesystems that are almost full")
            .example("df | where fstype == btrfs | get available", "free space on btrfs volumes")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let all = ctx.switch("all");
        let mounts = fs::read_to_string("/proc/self/mounts")
            .map_err(|e| ctx.err(format!("cannot read /proc/self/mounts ({})", reason(&e))))?;
        let mut rows = Vec::new();
        for line in mounts.lines() {
            let mut fields = line.split_whitespace();
            let (Some(source), Some(mount), Some(fstype)) = (fields.next(), fields.next(), fields.next()) else {
                continue;
            };
            let (mount, fstype) = (unescape(mount), unescape(fstype));
            // An automount point reports the size of whatever is mounted on it, which then
            // shows up a second time as its own mount.
            if fstype == "autofs" && !all {
                continue;
            }
            let row = Record::typed("fs.mount")
                .with("mount", mount.as_str())
                .with("source", unescape(source))
                .with("fstype", fstype);
            // Some mounts refuse statvfs, for example the document portal of another user.
            let Ok(stats) = statvfs(mount.as_str()) else {
                if all {
                    rows.push(row);
                }
                continue;
            };
            let fragment = widen(stats.fragment_size());
            let size = widen(stats.blocks()) * fragment;
            if size == 0 && !all {
                continue;
            }
            let free = widen(stats.blocks_free()) * fragment;
            let available = widen(stats.blocks_available()) * fragment;
            let used = size.saturating_sub(free);
            let usable = used + available;
            let percent = if usable == 0 { 0.0 } else { (used as f64 * 1000.0 / usable as f64).round() / 10.0 };
            rows.push(
                row.with("size", Value::Filesize(size as i64))
                    .with("used", Value::Filesize(used as i64))
                    .with("available", Value::Filesize(available as i64))
                    .with("use", percent),
            );
        }
        Ok(table(rows))
    }
}
