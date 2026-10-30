use std::collections::HashSet;
use std::fs::{self, Permissions};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};
use meersh_engine::authority::{absolute, file_id};
use meersh_engine::util::{expand_path, expand_tilde};
use meersh_engine::{Cap, Command, Ctx, Effect, Engine, Permit, PipelineData, Result, Signature};
use meersh_value::{Record, Type, Value};

use crate::{
    arg_paths, change, group_name, kind, local_date, mode_octal, mode_string, piped_paths, reason, record, table,
    user_name,
};

pub fn register(engine: &mut Engine) {
    engine.register(Stat);
    engine.register(Chmod);
    engine.register(Ln);
}

struct Stat;

impl Command for Stat {
    fn signature(&self) -> Signature {
        Signature::build("stat")
            .desc(
                "Metadata of one file, directory or symlink as an fs.stat record. A symlink is described itself, \
                 not the file it points to.",
            )
            .input(Type::Nothing)
            .output(Type::record("fs.stat"))
            .required("path", Type::String, "the file to describe")
            .effect(Effect::Read)
            .example("stat Cargo.toml | get mode_octal", "the permissions of a file in octal")
            .example("stat ~/.ssh | select user mode", "who owns the SSH directory and who may read it")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let raw = ctx.req_string(0, "path")?;
        let path = expand_path(&raw);
        let meta = fs::symlink_metadata(&path).map_err(|e| ctx.err(format!("cannot stat '{raw}' ({})", reason(&e))))?;
        let changed = u32::try_from(meta.ctime_nsec())
            .ok()
            .and_then(|nanos| DateTime::from_timestamp(meta.ctime(), nanos))
            .map(|d| Value::Date(d.with_timezone(&Local).fixed_offset()));
        let mut stat = Record::typed("fs.stat")
            .with("path", raw)
            .with("type", kind(meta.file_type()))
            .with("size", Value::Filesize(meta.len() as i64))
            .with("mode", mode_string(meta.mode()))
            .with("mode_octal", mode_octal(meta.mode()))
            .with("uid", i64::from(meta.uid()))
            .with("user", user_name(meta.uid()))
            .with("gid", i64::from(meta.gid()))
            .with("group", group_name(meta.gid()))
            .with("inode", meta.ino() as i64)
            .with("device", meta.dev() as i64)
            .with("links", meta.nlink() as i64)
            .with("accessed", meta.accessed().ok().map(local_date))
            .with("modified", meta.modified().ok().map(local_date))
            .with("changed", changed);
        if let Some(id) = file_id(&absolute(&path)) {
            stat = stat.with_id(id);
        }
        Ok(record(stat))
    }
}

const MODE_HELP: &str = "Use octal such as 644 or symbolic such as u+x, go-w or a=r";

/// Applies a chmod mode to the current permission bits. Octal modes replace them. Symbolic
/// modes are comma separated clauses of who (u, g, o, a), an operator (+, -, =) and
/// permissions (r, w, x, X, s, t, or u, g, o to copy a class). Without who, the bits of the
/// umask are left alone, as GNU chmod does.
pub(crate) fn new_mode(spec: &str, current: u32, is_dir: bool, umask: u32) -> std::result::Result<u32, String> {
    let invalid = || format!("invalid mode '{spec}'. {MODE_HELP}");
    if !spec.is_empty() && spec.len() <= 4 && spec.chars().all(|c| c.is_digit(8)) {
        return u32::from_str_radix(spec, 8).map_err(|_| invalid());
    }
    let mut mode = current & 0o7777;
    for clause in spec.split(',') {
        let mut chars = clause.chars().peekable();
        let mut who = 0;
        while let Some(class) = chars.next_if(|c| "ugoa".contains(*c)) {
            who |= match class {
                'u' => 0o4700,
                'g' => 0o2070,
                'o' => 0o1007,
                _ => 0o7777,
            };
        }
        let implicit = who == 0;
        if implicit {
            who = 0o7777;
        }
        let mut saw_op = false;
        while let Some(op) = chars.next_if(|c| "+-=".contains(*c)) {
            saw_op = true;
            let mut perms = 0;
            while let Some(p) = chars.next_if(|c| !"+-=".contains(*c)) {
                perms |= match p {
                    'r' => 0o444,
                    'w' => 0o222,
                    'x' => 0o111,
                    'X' if is_dir || mode & 0o111 != 0 => 0o111,
                    'X' => 0,
                    's' => 0o6000,
                    't' => 0o1000,
                    'u' => ((mode >> 6) & 0o7) * 0o111,
                    'g' => ((mode >> 3) & 0o7) * 0o111,
                    'o' => (mode & 0o7) * 0o111,
                    _ => return Err(invalid()),
                };
            }
            let mut bits = who & perms;
            if implicit {
                bits &= !umask;
            }
            mode = match op {
                '+' => mode | bits,
                '-' => mode & !bits,
                _ => {
                    // Directories keep setuid and setgid unless the clause names them.
                    let keep = if is_dir { 0o6000 } else { 0 };
                    (mode & !(who & !keep)) | bits
                }
            };
        }
        if !saw_op || chars.next().is_some() {
            return Err(invalid());
        }
    }
    Ok(mode)
}

/// The umask without changing it. `umask(2)` can only be read by setting it, which races
/// with other threads, so the value comes from `/proc/self/status`.
fn umask() -> u32 {
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            let line = status.lines().find_map(|l| l.strip_prefix("Umask:"))?;
            u32::from_str_radix(line.trim(), 8).ok()
        })
        .unwrap_or(0o022)
}

pub(crate) struct Chmod;

impl Command for Chmod {
    fn signature(&self) -> Signature {
        Signature::build("chmod")
            .desc(
                "Change permission bits, in octal (644) or symbolic (u+x, go-w, a=r). Takes paths, globs or piped \
                 fs.entry records. Every path is checked before any is changed. Each change is authorized on its \
                 own, so in a plan the rows say would chmod and a refused path says why.",
            )
            .input(Type::OneOf(vec![Type::Nothing, Type::table("fs.entry"), Type::list(Type::String)]))
            .output(Type::table("fs.change"))
            .required("mode", Type::String, "octal like 755, or symbolic like u+x and, quoted, 'u+x,go-w'")
            .rest("paths", Type::String, "files, directories or glob patterns")
            .effect(Effect::Write)
            .example("chmod +x build.sh", "make a script executable")
            .example("find --type f --name *.key | chmod 600", "make every key file readable by its owner only")
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let spec = ctx.req_string(0, "mode")?;
        let umask = umask();
        new_mode(&spec, 0, false, umask).map_err(|e| ctx.err(e))?;
        let mut raws = piped_paths(ctx, input)?;
        raws.extend(arg_paths(ctx, 1)?);
        if raws.is_empty() {
            return Err(ctx.err("nothing to change. Give paths after the mode or pipe in rows from find or ls"));
        }

        // Check everything first, so a missing path does not leave the job half done.
        let mut seen = HashSet::new();
        let mut targets: Vec<(PathBuf, u32)> = Vec::new();
        for raw in raws {
            if !seen.insert(raw.clone()) {
                continue;
            }
            let path = expand_path(&raw);
            let meta = fs::metadata(&path).map_err(|e| ctx.err(format!("cannot access '{raw}' ({})", reason(&e))))?;
            let mode = new_mode(&spec, meta.mode(), meta.is_dir(), umask).map_err(|e| ctx.err(e))?;
            targets.push((path, mode));
        }

        let mut rows = Vec::new();
        let mut done = 0;
        for (path, mode) in targets {
            let cap = || Cap::FsChmod { path: path.clone(), mode };
            let (action, because) = match ctx.authorize(cap())? {
                Permit::Execute => {
                    fs::set_permissions(&path, Permissions::from_mode(mode)).map_err(|e| {
                        let mut msg = format!("cannot chmod '{}' ({})", path.display(), reason(&e));
                        if done > 0 {
                            msg.push_str(&format!(". {done} earlier paths were already changed"));
                        }
                        ctx.err(msg)
                    })?;
                    ctx.performed(cap())?;
                    done += 1;
                    ("chmod", None)
                }
                Permit::Plan => ("would chmod", None),
                Permit::Skip(why) => ("refused", Some(why)),
            };
            let row = change(&path).with("action", action).with("mode", mode_octal(mode));
            rows.push(if let Some(why) = because { row.with("because", why) } else { row });
        }
        Ok(table(rows))
    }
}

struct Ln;

impl Command for Ln {
    fn signature(&self) -> Signature {
        Signature::build("ln")
            .desc(
                "Create a link. Without --symbolic it is a hard link to an existing file. If the link path is an \
                 existing directory, the link goes inside it under the name of the target. An existing file is \
                 never replaced.",
            )
            .input(Type::Nothing)
            .output(Type::table("fs.change"))
            .required("target", Type::String, "what the link points to. A symlink stores it as written")
            .required("link", Type::String, "the path of the new link, or a directory to put it in")
            .switch("symbolic", Some('s'), "create a symbolic link instead of a hard link")
            .effect(Effect::Write)
            .example("ln -s ../shared/config.toml config.toml", "a symlink to a file in a sibling directory")
            .example("ln big.iso backup/", "a hard link to big.iso inside backup")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let symbolic = ctx.switch("symbolic");
        let target_raw = ctx.req_string(0, "target")?;
        let link_raw = ctx.req_string(1, "link")?;
        let target = expand_tilde(&target_raw);
        let mut link = expand_path(&link_raw);
        if link.is_dir() {
            let name =
                Path::new(&target).file_name().ok_or_else(|| ctx.err(format!("'{target_raw}' has no file name")))?;
            link = link.join(name);
        }
        if fs::symlink_metadata(&link).is_ok() {
            return Err(ctx.err(format!("'{}' already exists", link.display())));
        }
        if !symbolic {
            let meta = fs::symlink_metadata(&target)
                .map_err(|e| ctx.err(format!("cannot link to '{target_raw}' ({})", reason(&e))))?;
            if meta.is_dir() {
                return Err(ctx.err(format!("'{target_raw}' is a directory. Use --symbolic to link to it")));
            }
        }

        let verb = if symbolic { "symlink" } else { "link" };
        let cap = || Cap::FsCreate { path: link.clone() };
        let (action, because) = match ctx.authorize(cap())? {
            Permit::Execute => {
                let made = if symbolic { symlink(&target, &link) } else { fs::hard_link(&target, &link) };
                made.map_err(|e| ctx.err(format!("cannot create '{}' ({})", link.display(), reason(&e))))?;
                ctx.performed(cap())?;
                (verb.to_string(), None)
            }
            Permit::Plan => (format!("would {verb}"), None),
            Permit::Skip(why) => ("refused".to_string(), Some(why)),
        };
        let row = change(&link).with("action", action).with("target", target);
        Ok(table(vec![if let Some(why) = because { row.with("because", why) } else { row }]))
    }
}
