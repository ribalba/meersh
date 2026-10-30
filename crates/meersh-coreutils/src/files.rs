use std::collections::HashSet;
use std::fs::{self, File, FileTimes, Metadata};
use std::io::{self, ErrorKind};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use meersh_engine::authority::absolute;
use meersh_engine::util::{expand_glob, expand_path};
use meersh_engine::value::{Record, Type, Value};
use meersh_engine::{Cap, Command, Ctx, Effect, Engine, Permit, PipelineData, Result, Signature};

use crate::{change, mechanism, piped_paths, reason, report, table};

pub fn register(engine: &mut Engine) {
    engine.register(Mkdir);
    engine.register(Touch);
    engine.register(Rm);
    engine.register(Cp);
    engine.register(Mv);
}

fn path_input() -> Type {
    Type::OneOf(vec![Type::Nothing, Type::table("fs.entry"), Type::list(Type::String)])
}

fn required_paths(ctx: &Ctx) -> Result<Vec<String>> {
    let paths = ctx.rest_strings(0)?;
    if paths.is_empty() {
        return Err(ctx.err("missing argument <paths>"));
    }
    Ok(paths)
}

struct Mkdir;

impl Command for Mkdir {
    fn signature(&self) -> Signature {
        Signature::build("mkdir")
            .desc(
                "Create directories, including missing parents. Every directory that is created gets its own row. \
                 An existing directory is reported, not an error.",
            )
            .input(Type::Nothing)
            .output(Type::table("fs.change"))
            .rest("paths", Type::String, "directories to create")
            .effect(Effect::Write)
            .example("mkdir build/out logs", "create two directories with their parents")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        // Each missing component is its own creation, so the session owns all of them.
        let mut planned = HashSet::new();
        let mut steps: Vec<(PathBuf, Option<Permit>)> = Vec::new();
        for raw in required_paths(ctx)? {
            let path = expand_path(&raw);
            let missing = missing_dirs(&path);
            if missing.is_empty() {
                if !path.is_dir() {
                    return Err(ctx.err(format!("cannot create '{raw}' (file exists)")));
                }
                steps.push((path, None));
            }
            for dir in missing {
                if planned.insert(dir.clone()) {
                    let permit = ctx.authorize(Cap::FsCreate { path: dir.clone() })?;
                    steps.push((dir, Some(permit)));
                }
            }
        }
        let mut rows = Vec::new();
        for (dir, permit) in steps {
            let row = change(&dir).with("action", if permit.is_some() { "created" } else { "exists" });
            match permit {
                None => rows.push(row),
                Some(Permit::Execute) => {
                    fs::create_dir(&dir)
                        .map_err(|e| ctx.err(format!("cannot create '{}' ({})", dir.display(), reason(&e))))?;
                    ctx.performed(Cap::FsCreate { path: dir })?;
                    rows.push(row);
                }
                Some(other) => rows.push(report(row, &other, "create")),
            }
        }
        Ok(table(rows))
    }
}

/// The directories of `path` that do not exist yet, outermost first.
fn missing_dirs(path: &Path) -> Vec<PathBuf> {
    // `a/..` only exists once `a` does, so such paths are resolved first.
    let path: PathBuf =
        if path.components().any(|c| c == Component::ParentDir) { absolute(path) } else { path.components().collect() };
    let mut missing: Vec<PathBuf> = path
        .ancestors()
        .filter(|p| p.file_name().is_some())
        .take_while(|p| fs::symlink_metadata(p).is_err())
        .map(Path::to_path_buf)
        .collect();
    missing.reverse();
    missing
}

struct Touch;

impl Command for Touch {
    fn signature(&self) -> Signature {
        Signature::build("touch")
            .desc("Create empty files, or set the access and modification time of existing ones to now.")
            .input(Type::Nothing)
            .output(Type::table("fs.change"))
            .rest("paths", Type::String, "files to create or touch")
            .effect(Effect::Write)
            .example("touch notes.md", "create the file if it does not exist")
            .example("touch src/main.rs | get action", "see whether the file was created or touched")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let mut seen = HashSet::new();
        let mut targets = Vec::new();
        for raw in required_paths(ctx)? {
            let path = expand_path(&raw);
            if !seen.insert(path.clone()) {
                continue;
            }
            // Touching an existing file changes its modification time, which is a write.
            let exists = fs::symlink_metadata(&path).is_ok();
            let cap = if exists { Cap::FsWrite { path: path.clone() } } else { Cap::FsCreate { path: path.clone() } };
            let permit = ctx.authorize(cap.clone())?;
            targets.push((raw, path, exists, cap, permit));
        }
        let mut rows = Vec::new();
        for (raw, path, exists, cap, permit) in targets {
            let (verb, action) = if exists { ("touch", "touched") } else { ("create", "created") };
            let row = change(&path).with("action", action);
            if permit != Permit::Execute {
                rows.push(report(row, &permit, verb));
                continue;
            }
            let failed = |e: io::Error| ctx.err(format!("cannot touch '{raw}' ({})", reason(&e)));
            if exists {
                // A read only handle is enough for futimens and also works on directories.
                let file = File::open(&path).or_else(|_| File::options().write(true).open(&path)).map_err(failed)?;
                let now = SystemTime::now();
                file.set_times(FileTimes::new().set_accessed(now).set_modified(now)).map_err(failed)?;
            } else {
                File::create_new(&path).map_err(failed)?;
            }
            ctx.performed(cap)?;
            rows.push(row);
        }
        Ok(table(rows))
    }
}

struct Rm;

impl Command for Rm {
    fn signature(&self) -> Signature {
        Signature::build("rm")
            .desc(
                "Move files and directories to the trash, or delete them for good with --permanent. Takes paths, \
                 globs or piped fs.entry records. Every path is checked and authorized before anything is \
                 removed, and /, the home directory and its parents, . and .. are always refused.",
            )
            .input(path_input())
            .output(Type::table("fs.change"))
            .rest("paths", Type::String, "files, directories or glob patterns")
            .switch("recursive", Some('r'), "allow removing directories with their contents")
            .switch("permanent", Some('p'), "delete for good instead of moving to the trash")
            .switch("dry-run", Some('n'), "only report what would be removed")
            .switch("force", Some('f'), "ignore paths that do not exist")
            .effect(Effect::Destructive)
            .example("ls *.log | where modified < (date now) - 30day | rm", "trash logs older than 30 days")
            .example("rm -rn build", "preview removing the build directory")
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let (recursive, permanent) = (ctx.switch("recursive"), ctx.switch("permanent"));
        let (dry_run, force) = (ctx.switch("dry-run"), ctx.switch("force"));
        let mut raws = piped_paths(ctx, input)?;
        raws.extend(ctx.rest_strings(0)?.iter().flat_map(|arg| expand_glob(arg)));
        if raws.is_empty() {
            return Err(ctx.err("nothing to remove. Give paths or pipe in rows from ls"));
        }

        // Check and authorize everything first, so a refused path in the middle does not
        // leave the job half done.
        let mut seen = HashSet::new();
        let mut targets: Vec<(PathBuf, Metadata)> = Vec::new();
        for raw in raws {
            if !seen.insert(raw.clone()) {
                continue;
            }
            let path = expand_path(&raw);
            if protected(&raw, &path) {
                return Err(ctx.err(format!("refusing to remove '{raw}'")));
            }
            let meta = match fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(e) if force && e.kind() == ErrorKind::NotFound => continue,
                Err(e) => return Err(ctx.err(format!("cannot remove '{raw}' ({})", reason(&e)))),
            };
            if meta.is_dir() && !recursive {
                return Err(ctx.err(format!("'{raw}' is a directory. Use -r to remove it with its contents")));
            }
            targets.push((path, meta));
        }
        let mut permits = Vec::new();
        for (path, _) in &targets {
            // A dry run changes nothing, so it needs no right to act.
            permits.push(if dry_run {
                Permit::Plan
            } else {
                ctx.authorize(Cap::FsDelete { path: path.clone(), permanent })?
            });
        }

        let mut rows = Vec::new();
        let mut done = 0;
        for ((path, meta), permit) in targets.iter().zip(&permits) {
            rows.push(remove(ctx, path, meta, permanent, permit, done)?);
            done += usize::from(*permit == Permit::Execute);
        }
        Ok(table(rows))
    }
}

/// Removes one target that passed every check and was authorized, and reports it.
fn remove(ctx: &Ctx, path: &Path, meta: &Metadata, permanent: bool, permit: &Permit, done: usize) -> Result<Record> {
    let size = if meta.is_dir() { Value::Nothing } else { Value::Filesize(meta.len() as i64) };
    let (verb, action) = if permanent { ("delete", "deleted") } else { ("trash", "trashed") };
    let row = change(path).with("action", action).with("size", size);
    if *permit != Permit::Execute {
        return Ok(report(row, permit, verb));
    }
    if permanent {
        let removed = if meta.is_dir() { fs::remove_dir_all(path) } else { fs::remove_file(path) };
        removed.map_err(|e| ctx.err(partial("delete", path, &reason(&e), done)))?;
    } else {
        trash::delete(path).map_err(|e| ctx.err(partial("trash", path, &e.to_string(), done)))?;
    }
    ctx.performed(Cap::FsDelete { path: path.to_path_buf(), permanent })?;
    Ok(row)
}

/// Trashes or deletes one object, directories with their contents, for `fs.entry.delete`.
pub(crate) fn remove_one(ctx: &Ctx, raw: &str, permanent: bool) -> Result<Record> {
    let path = expand_path(raw);
    if protected(raw, &path) {
        return Err(ctx.err(format!("refusing to remove '{raw}'")));
    }
    let meta = fs::symlink_metadata(&path).map_err(|e| ctx.err(format!("cannot remove '{raw}' ({})", reason(&e))))?;
    let permit = ctx.authorize(Cap::FsDelete { path: path.clone(), permanent })?;
    remove(ctx, &path, &meta, permanent, &permit, 0)
}

fn partial(verb: &str, path: &Path, why: &str, done: usize) -> String {
    let mut msg = format!("cannot {verb} '{}' ({why})", path.display());
    if done > 0 {
        msg.push_str(&format!(". {done} earlier paths were already removed"));
    }
    msg
}

/// Paths that `rm` never touches, whatever the flags say.
fn protected(raw: &str, path: &Path) -> bool {
    let trimmed = raw.trim_end_matches('/');
    let last = trimmed.rsplit('/').next().unwrap_or_default();
    if trimmed.is_empty() || last == "." || last == ".." {
        return true;
    }
    // Removing a symlink leaves its target alone, even when the target is `/`.
    if !raw.ends_with('/') && fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return false;
    }
    let Ok(canonical) = fs::canonicalize(path) else { return false };
    let home = fs::canonicalize(expand_path("~")).ok();
    canonical == Path::new("/") || home.is_some_and(|home| home.starts_with(&canonical))
}

/// Splits arguments the way POSIX cp and mv do, with the destination last. Piped paths
/// are sources too, so `ls *.png | mv images/` works.
fn sources_and_destination(ctx: &Ctx, input: PipelineData) -> Result<(Vec<String>, PathBuf)> {
    let mut sources = piped_paths(ctx, input)?;
    let mut args = ctx.rest_strings(0)?;
    let destination = args.pop().ok_or_else(|| ctx.err("missing the destination"))?;
    sources.extend(args.iter().flat_map(|arg| expand_glob(arg)));
    if sources.is_empty() {
        return Err(ctx.err("missing the source. Give sources before the destination or pipe them in"));
    }
    Ok((sources, expand_path(&destination)))
}

/// Pairs every source with its target. An existing directory as destination receives the
/// sources under their own names.
fn pairs(ctx: &Ctx, sources: &[String], destination: &Path) -> Result<Vec<(PathBuf, PathBuf)>> {
    let into_dir = destination.is_dir();
    if !into_dir && sources.len() > 1 {
        return Err(
            ctx.err(format!("'{}' is not a directory, so it cannot take several sources", destination.display()))
        );
    }
    let mut pairs = Vec::new();
    for raw in sources {
        let source = expand_path(raw);
        let meta =
            fs::symlink_metadata(&source).map_err(|e| ctx.err(format!("cannot access '{raw}' ({})", reason(&e))))?;
        let target = if into_dir {
            let name = source.file_name().ok_or_else(|| ctx.err(format!("'{raw}' has no file name")))?;
            destination.join(name)
        } else {
            destination.to_path_buf()
        };
        if same_file(&source, &target) {
            return Err(ctx.err(format!("'{}' and '{}' are the same file", source.display(), target.display())));
        }
        if meta.is_dir() && inside(&source, &target) {
            return Err(ctx.err(format!("cannot put '{raw}' inside itself")));
        }
        pairs.push((source, target));
    }
    Ok(pairs)
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

fn inside(dir: &Path, target: &Path) -> bool {
    let parent = target.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    match (fs::canonicalize(dir), fs::canonicalize(parent)) {
        (Ok(dir), Ok(parent)) => parent.starts_with(dir),
        _ => false,
    }
}

/// A copy creates its target or overwrites an existing one.
fn copy_cap(target: &Path) -> Cap {
    let path = target.to_path_buf();
    if fs::symlink_metadata(target).is_ok() { Cap::FsWrite { path } } else { Cap::FsCreate { path } }
}

/// Copies one authorized source. Only the top level target was authorized, but every path
/// the copy created is reported, so the session owns the whole tree. What was there before
/// stays foreign, also when the copy merges into an existing directory.
fn copy(ctx: &Ctx, source: &Path, target: &Path, recursive: bool, permit: &Permit) -> Result<Record> {
    let row = change(target).with("from", source.display().to_string()).with("action", "copied");
    if *permit != Permit::Execute {
        return Ok(report(row, permit, "copy"));
    }
    let existed = fs::symlink_metadata(target).is_ok();
    let before: HashSet<PathBuf> = mechanism::tree(target).into_iter().collect();
    let copied = mechanism::copy(source, target, recursive);
    // Also after a failure halfway, so the session can clean up what it left behind.
    for path in mechanism::tree(target).into_iter().filter(|path| !before.contains(path)) {
        ctx.performed(Cap::FsCreate { path })?;
    }
    copied.map_err(|why| ctx.err(format!("cannot copy '{}' to '{}' ({why})", source.display(), target.display())))?;
    if existed {
        ctx.performed(Cap::FsWrite { path: target.to_path_buf() })?;
    }
    Ok(row)
}

/// Copies one object, directories with their contents, for `fs.entry.copy`.
pub(crate) fn copy_one(ctx: &Ctx, source: &str, destination: &Path) -> Result<Record> {
    let (source, target) = pairs(ctx, &[source.to_string()], destination)?.remove(0);
    let permit = ctx.authorize(copy_cap(&target))?;
    copy(ctx, &source, &target, true, &permit)
}

struct Cp;

impl Command for Cp {
    fn signature(&self) -> Signature {
        Signature::build("cp")
            .desc(
                "Copy files and directories. The last argument is the destination. If it is an existing \
                 directory, the sources are copied into it. Sources can also be piped in as fs.entry records or \
                 strings, with the destination as the only argument.",
            )
            .input(path_input())
            .output(Type::table("fs.change"))
            .rest("paths", Type::String, "sources followed by the destination, or only the destination when piping")
            .switch("recursive", Some('r'), "copy directories with their contents")
            .effect(Effect::Write)
            .example("cp Cargo.toml Cargo.toml.bak", "back up one file")
            .example("ls *.rs | where size > 10kb | cp backup/", "copy the large Rust files into backup/")
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let recursive = ctx.switch("recursive");
        let (sources, destination) = sources_and_destination(ctx, input)?;
        let pairs = pairs(ctx, &sources, &destination)?;
        if !recursive && let Some((dir, _)) = pairs.iter().find(|(source, _)| source.is_dir()) {
            return Err(ctx.err(format!("'{}' is a directory. Use -r to copy it with its contents", dir.display())));
        }
        let permits = pairs.iter().map(|(_, target)| ctx.authorize(copy_cap(target))).collect::<Result<Vec<_>>>()?;
        let rows = pairs
            .iter()
            .zip(&permits)
            .map(|((source, target), permit)| copy(ctx, source, target, recursive, permit))
            .collect::<Result<_>>()?;
        Ok(table(rows))
    }
}

struct Mv;

impl Command for Mv {
    fn signature(&self) -> Signature {
        Signature::build("mv")
            .desc(
                "Move or rename files and directories. The last argument is the destination. If it is an existing \
                 directory, the sources are moved into it. Sources can also be piped in as fs.entry records or \
                 strings, with the destination as the only argument.",
            )
            .input(path_input())
            .output(Type::table("fs.change"))
            .rest("paths", Type::String, "sources followed by the destination, or only the destination when piping")
            .effect(Effect::Write)
            .example("mv draft.md notes/final.md", "rename and move one file")
            .example("ls *.png | mv images/", "move all PNG files into images/")
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let (sources, destination) = sources_and_destination(ctx, input)?;
        let pairs = pairs(ctx, &sources, &destination)?;
        let permits = pairs
            .iter()
            .map(|(from, to)| ctx.authorize(Cap::FsMove { from: from.clone(), to: to.clone() }))
            .collect::<Result<Vec<_>>>()?;
        let rows = pairs
            .iter()
            .zip(&permits)
            .map(|((source, target), permit)| move_to(ctx, source, target, permit))
            .collect::<Result<_>>()?;
        Ok(table(rows))
    }
}

fn move_to(ctx: &Ctx, source: &Path, target: &Path, permit: &Permit) -> Result<Record> {
    let row = change(target).with("from", source.display().to_string()).with("action", "moved");
    if *permit != Permit::Execute {
        return Ok(report(row, permit, "move"));
    }
    mechanism::move_path(source, target)
        .map_err(|why| ctx.err(format!("cannot move '{}' to '{}' ({why})", source.display(), target.display())))?;
    ctx.performed(Cap::FsMove { from: source.to_path_buf(), to: target.to_path_buf() })?;
    Ok(row)
}

/// Moves one object, for `fs.entry.move`.
pub(crate) fn move_one(ctx: &Ctx, source: &str, destination: &Path) -> Result<Record> {
    let (source, target) = pairs(ctx, &[source.to_string()], destination)?.remove(0);
    let permit = ctx.authorize(Cap::FsMove { from: source.clone(), to: target.clone() })?;
    move_to(ctx, &source, &target, &permit)
}
