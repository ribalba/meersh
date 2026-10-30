use std::collections::HashMap;
use std::fs::{self, Metadata};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::SystemTime;

use chrono::{DateTime, Local};
use meersh_engine::authority::{absolute, file_id};
use meersh_engine::util::{expand_glob, expand_path, is_glob};
use meersh_engine::value::{Record, Type, Value};
use meersh_engine::{Command, Ctx, Effect, Engine, PipelineData, Result, Signature};
use nix::unistd::{Gid, Group};

use crate::{reason, table, user_name};

pub fn register(engine: &mut Engine) {
    engine.register(Ls);
    engine.register(Cd::default());
    engine.register(Pwd);
}

struct Ls;

impl Command for Ls {
    fn signature(&self) -> Signature {
        Signature::build("ls")
            .desc(
                "List files and directories as fs.entry records. Names are relative to the given path, so the rows \
                 can be piped into rm, cp, mv or open.",
            )
            .input(Type::Nothing)
            .output(Type::table("fs.entry"))
            .rest("paths", Type::String, "directories, files or glob patterns, the current directory by default")
            .switch("all", Some('a'), "include hidden entries whose name starts with a dot")
            .switch("long", Some('l'), "add the mode, user, group and symlink target columns")
            .effect(Effect::Read)
            .example("ls src | where size > 10kb | sort-by modified", "large files in src, oldest first")
            .example("ls -la ~/Downloads/*.pdf", "PDFs in Downloads with owner and permissions")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let all = ctx.switch("all");
        let long = ctx.switch("long");
        let mut args = ctx.rest_strings(0)?;
        if args.is_empty() {
            args.push(".".into());
        }
        let mut owners = Owners::default();
        let mut rows = Vec::new();
        for arg in &args {
            if is_glob(arg) {
                // The glob crate matches dotfiles with `*`. Like bash, only a pattern whose
                // last part starts with a dot shows them without -a.
                let hidden_ok = all || arg.rsplit('/').next().is_some_and(|last| last.starts_with('.'));
                for matched in expand_glob(arg) {
                    let path = PathBuf::from(&matched);
                    // A pattern without matches comes back unchanged and does not exist.
                    let meta = fs::symlink_metadata(&path).map_err(|_| ctx.err(format!("no matches for '{arg}'")))?;
                    if hidden_ok || !is_hidden(&path) {
                        rows.push(entry(matched, &path, &meta, long, &mut owners));
                    }
                }
                continue;
            }
            let path = expand_path(arg);
            let meta =
                fs::symlink_metadata(&path).map_err(|e| ctx.err(format!("cannot access '{arg}' ({})", reason(&e))))?;
            if !path.is_dir() {
                rows.push(entry(arg.clone(), &path, &meta, long, &mut owners));
                continue;
            }
            list_dir(ctx, arg, &path, all, long, &mut owners, &mut rows)?;
        }
        Ok(sorted(rows))
    }
}

/// Appends the entries of a directory. Names are `arg` joined with the file name, so they
/// stay relative when `arg` is.
pub(crate) fn list_dir(
    ctx: &Ctx,
    arg: &str,
    path: &Path,
    all: bool,
    long: bool,
    owners: &mut Owners,
    rows: &mut Vec<Record>,
) -> Result<()> {
    let children = fs::read_dir(path).map_err(|e| ctx.err(format!("cannot read '{arg}' ({})", reason(&e))))?;
    for child in children {
        let child = child?;
        let file_name = child.file_name().to_string_lossy().into_owned();
        if !all && file_name.starts_with('.') {
            continue;
        }
        // The entry can vanish between read_dir and stat.
        let Ok(meta) = child.metadata() else { continue };
        let name = if arg == "." { file_name } else { path.join(&file_name).display().to_string() };
        rows.push(entry(name, &child.path(), &meta, long, owners));
    }
    Ok(())
}

pub(crate) fn sorted(mut rows: Vec<Record>) -> PipelineData {
    rows.sort_by(|a, b| name_of(a).cmp(name_of(b)));
    table(rows)
}

fn is_hidden(path: &Path) -> bool {
    path.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.'))
}

fn name_of(r: &Record) -> &str {
    r.get("name").and_then(Value::as_str).unwrap_or_default()
}

/// The fs.entry for one path, without the long columns.
pub(crate) fn entry_at(ctx: &Ctx, name: String, path: &Path) -> Result<Record> {
    let meta = fs::symlink_metadata(path).map_err(|e| ctx.err(format!("cannot access '{name}' ({})", reason(&e))))?;
    Ok(entry(name, path, &meta, false, &mut Owners::default()))
}

/// `meta` comes from `symlink_metadata`, so a symlink reports as one. The identity lets a
/// command check later that the file is still the one that was listed.
pub(crate) fn entry(name: String, path: &Path, meta: &Metadata, long: bool, owners: &mut Owners) -> Record {
    let file_type = meta.file_type();
    let kind = if file_type.is_symlink() {
        "symlink"
    } else if file_type.is_dir() {
        "dir"
    } else {
        "file"
    };
    let record = Record::typed("fs.entry")
        .with("name", name)
        .with("type", kind)
        .with("size", Value::Filesize(meta.len() as i64))
        .with("modified", meta.modified().ok().map(local_date));
    let record = match file_id(&absolute(path)) {
        Some(id) => record.with_id(id),
        None => record,
    };
    if !long {
        return record;
    }
    let target = file_type.is_symlink().then(|| fs::read_link(path).ok()).flatten();
    record
        .with("mode", mode_string(meta.mode()))
        .with("user", owners.user(meta.uid()))
        .with("group", owners.group(meta.gid()))
        .with("target", target.map(|t| t.display().to_string()))
}

pub(crate) fn local_date(time: SystemTime) -> Value {
    Value::Date(DateTime::<Local>::from(time).fixed_offset())
}

/// The permission part of `ls -l`, for example `rwxr-xr-x`. Setuid, setgid and sticky bits
/// show as `s` and `t`, upper case when the execute bit underneath is missing.
pub(crate) fn mode_string(mode: u32) -> String {
    let mut out = String::with_capacity(9);
    for (shift, special, mark) in [(6, 0o4000, 's'), (3, 0o2000, 's'), (0, 0o1000, 't')] {
        let bits = (mode >> shift) & 0o7;
        out.push(if bits & 0o4 != 0 { 'r' } else { '-' });
        out.push(if bits & 0o2 != 0 { 'w' } else { '-' });
        out.push(match (mode & special != 0, bits & 0o1 != 0) {
            (true, true) => mark,
            (true, false) => mark.to_ascii_uppercase(),
            (false, true) => 'x',
            (false, false) => '-',
        });
    }
    out
}

/// Caches name lookups, since a directory listing asks for the same few ids over and over.
#[derive(Default)]
pub(crate) struct Owners {
    users: HashMap<u32, String>,
    groups: HashMap<u32, String>,
}

impl Owners {
    fn user(&mut self, uid: u32) -> String {
        self.users.entry(uid).or_insert_with(|| user_name(uid)).clone()
    }

    fn group(&mut self, gid: u32) -> String {
        self.groups
            .entry(gid)
            .or_insert_with(|| {
                Group::from_gid(Gid::from_raw(gid)).ok().flatten().map_or_else(|| gid.to_string(), |g| g.name)
            })
            .clone()
    }
}

#[derive(Default)]
struct Cd {
    /// The directory before the last successful `cd`, for `cd -`. It lives here because
    /// edition 2024 makes `std::env::set_var` unsafe, which rules out `OLDPWD`.
    previous: Mutex<Option<PathBuf>>,
}

impl Command for Cd {
    fn signature(&self) -> Signature {
        Signature::build("cd")
            .desc("Change the working directory of the shell.")
            .input(Type::Nothing)
            .output(Type::Nothing)
            .optional("path", Type::String, "target directory, home by default and - for the previous one")
            .effect(Effect::Read)
            .example("cd ~/code/meersh", "go to a project")
            .example("cd -", "go back to the previous directory")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let mut previous = self.previous.lock().unwrap_or_else(PoisonError::into_inner);
        let target = match ctx.opt_string(0)?.as_deref() {
            None => expand_path("~"),
            Some("-") => previous.clone().ok_or_else(|| ctx.err("there is no previous directory yet"))?,
            Some(path) => expand_path(path),
        };
        let current = std::env::current_dir();
        std::env::set_current_dir(&target)
            .map_err(|e| ctx.err(format!("cannot enter '{}' ({})", target.display(), reason(&e))))?;
        if let Ok(current) = current {
            *previous = Some(current);
        }
        Ok(PipelineData::Empty)
    }
}

struct Pwd;

impl Command for Pwd {
    fn signature(&self) -> Signature {
        Signature::build("pwd")
            .desc("Return the current working directory as a string.")
            .input(Type::Nothing)
            .output(Type::String)
            .effect(Effect::Read)
            .example("pwd", "the current directory")
            .example("ls (pwd)", "list the current directory by its full path")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let cwd = std::env::current_dir()
            .map_err(|e| ctx.err(format!("cannot read the working directory ({})", reason(&e))))?;
        Ok(PipelineData::Value(Value::String(cwd.display().to_string())))
    }
}
