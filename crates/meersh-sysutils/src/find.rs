use std::fs::{self, Metadata};
use std::path::PathBuf;
use std::sync::Arc;

use glob::Pattern;
use meersh_engine::authority::{absolute, file_id};
use meersh_engine::util::expand_path;
use meersh_engine::{Command, Ctx, Effect, Engine, PipelineData, Result, Signature};
use meersh_value::{Record, Type, Value};

use crate::disk::join_name;
use crate::{kind, local_date, reason};

pub fn register(engine: &mut Engine) {
    engine.register(Find);
}

struct Find;

impl Command for Find {
    fn signature(&self) -> Signature {
        Signature::build("find")
            .desc(
                "Walk a directory tree and stream fs.entry records while walking. name is the path as reached from \
                 the start, so the rows can be piped into rm, chmod, cat or wc. Every directory is listed in name \
                 order, symlinks are not followed and hidden entries are skipped unless --hidden is given.",
            )
            .input(Type::Nothing)
            .output(Type::table("fs.entry"))
            .optional("path", Type::String, "where to start, the current directory by default")
            .named("name", Type::String, None, "keep entries whose file name matches this glob, such as *.rs")
            .named("type", Type::String, None, "keep only f (files), d (directories) or l (symlinks)")
            .named("max-depth", Type::Int, None, "descend at most this deep, 1 means the start directory only")
            .switch("hidden", Some('a'), "include entries whose name starts with a dot, and look inside them")
            .effect(Effect::Read)
            .example("find src --name *.rs | where size > 20kb", "large Rust files below src")
            .example("find --type f --name *.tmp | rm", "trash every temporary file below here")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let start = ctx.opt_string(0)?.unwrap_or_else(|| ".".into());
        let pattern = match ctx.named_string("name")? {
            Some(glob) => {
                Some(Pattern::new(&glob).map_err(|e| ctx.err(format!("'{glob}' is not a valid pattern ({e})")))?)
            }
            None => None,
        };
        let kind = match ctx.named_string("type")?.as_deref() {
            None => None,
            Some("f" | "file") => Some("file"),
            Some("d" | "dir") => Some("dir"),
            Some("l" | "symlink") => Some("symlink"),
            Some(other) => return Err(ctx.err(format!("--type takes f, d or l, not '{other}'"))),
        };
        let max_depth = match ctx.named("max-depth")? {
            None => None,
            Some(v) => Some(
                v.as_int()
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or_else(|| ctx.err("--max-depth needs a whole number of zero or more"))?,
            ),
        };
        let path = expand_path(&start);
        // Fail now on a missing start, instead of returning an empty stream.
        fs::symlink_metadata(&path).map_err(|e| ctx.err(format!("cannot access '{start}' ({})", reason(&e))))?;
        let name = path.display().to_string();
        let walker = Walker {
            engine: ctx.engine.clone(),
            levels: vec![vec![Node { path, name, depth: 0 }]],
            pattern,
            kind,
            max_depth,
            hidden: ctx.switch("hidden"),
            stopped: false,
        };
        Ok(PipelineData::stream(walker))
    }
}

struct Node {
    path: PathBuf,
    name: String,
    depth: usize,
}

/// A depth first walk that lists one directory at a time. A consumer such as `first 3`
/// that stops pulling also stops the walk.
struct Walker {
    engine: Arc<Engine>,
    /// One listing per open directory, in reverse name order so that `pop` yields the next.
    levels: Vec<Vec<Node>>,
    pattern: Option<Pattern>,
    kind: Option<&'static str>,
    max_depth: Option<usize>,
    hidden: bool,
    stopped: bool,
}

impl Walker {
    fn children(&self, dir: &Node) -> Vec<Node> {
        // Unreadable directories are skipped like entries that vanish while walking.
        let Ok(entries) = fs::read_dir(&dir.path) else { return Vec::new() };
        let mut nodes: Vec<Node> = entries
            .flatten()
            .filter_map(|entry| {
                let file_name = entry.file_name().to_string_lossy().into_owned();
                (self.hidden || !file_name.starts_with('.')).then(|| Node {
                    path: entry.path(),
                    name: join_name(&dir.name, &file_name),
                    depth: dir.depth + 1,
                })
            })
            .collect();
        nodes.sort_by(|a, b| b.name.cmp(&a.name));
        nodes
    }

    fn keeps(&self, node: &Node, meta: &Metadata) -> bool {
        let kind_ok = self.kind.is_none_or(|k| k == kind(meta.file_type()));
        let name_ok = self
            .pattern
            .as_ref()
            .is_none_or(|p| node.path.file_name().is_some_and(|name| p.matches(&name.to_string_lossy())));
        kind_ok && name_ok
    }
}

impl Iterator for Walker {
    type Item = Result<Value>;

    fn next(&mut self) -> Option<Self::Item> {
        while !self.stopped {
            let level = self.levels.last_mut()?;
            let Some(node) = level.pop() else {
                self.levels.pop();
                continue;
            };
            if let Err(e) = self.engine.check_interrupt() {
                self.stopped = true;
                return Some(Err(e));
            }
            let Ok(meta) = fs::symlink_metadata(&node.path) else { continue };
            if meta.is_dir() && self.max_depth.is_none_or(|max| node.depth < max) {
                let children = self.children(&node);
                self.levels.push(children);
            }
            // The start directory itself is not a result, so `find | rm` never meets `.`.
            if node.depth == 0 && meta.is_dir() {
                continue;
            }
            if self.keeps(&node, &meta) {
                return Some(Ok(entry(node, &meta)));
            }
        }
        None
    }
}

fn entry(node: Node, meta: &Metadata) -> Value {
    let id = file_id(&absolute(&node.path));
    let mut record = Record::typed("fs.entry")
        .with("name", node.name)
        .with("type", kind(meta.file_type()))
        .with("size", Value::Filesize(meta.len() as i64))
        .with("modified", meta.modified().ok().map(local_date));
    if let Some(id) = id {
        record = record.with_id(id);
    }
    Value::Record(record)
}
