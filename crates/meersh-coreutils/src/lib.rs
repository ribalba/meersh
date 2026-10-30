//! File and process commands that produce and consume typed records instead of text. `ls`
//! emits `fs.entry`, `ps` emits `proc`, and `rm`, `cp`, `mv` and `kill` accept those records
//! from a pipe, which is what makes `ls *.log | where size > 10mb | rm` work.
//!
//! Every command that changes something asks `ctx.authorize` right before it acts, with the
//! concrete target, and reports the finished action with `ctx.performed`. Records from `ls`
//! and `ps` carry the identity of their object, and commands check it before they act, so a
//! stale row can never hit a file that changed or a reused pid. Methods such as
//! `$f.delete()` are commands named `<type>.<method>` that take the object as input.
//!
//! The commands keep this object interface and delegate the mechanism to the uutils crates,
//! the way Nushell does. `cp` and `mv` already copy and move through `uu_cp` and `uu_mv`
//! (see `mechanism.rs`). The others are small std based sketch implementations that cover
//! the common cases and skip most GNU edge cases. The same commands also run outside the
//! shell through the `meer-box` multi-call binary.

mod files;
mod jobs;
mod mechanism;
mod methods;
mod nav;
mod net;
mod open;
mod proc;
mod which;

#[cfg(test)]
mod tests;

use std::io;
use std::path::Path;

use meersh_engine::value::{Record, Type, Value};
use meersh_engine::{Ctx, Engine, Permit, PipelineData, Result, TypeInfo};
use nix::errno::Errno;
use nix::unistd::{Uid, User};

pub fn register(engine: &mut Engine) {
    register_types(engine);
    nav::register(engine);
    files::register(engine);
    open::register(engine);
    proc::register(engine);
    methods::register(engine);
    jobs::register(engine);
    which::register(engine);
}

fn register_types(engine: &mut Engine) {
    let mut add = |name: &str, description: &str, fields: Vec<(&str, Type, &str)>| {
        engine.register_type(TypeInfo {
            name: name.into(),
            description: description.into(),
            fields: fields.into_iter().map(|(n, t, d)| (n.to_string(), t, d.to_string())).collect(),
        });
    };
    add(
        "fs.entry",
        "A file, directory or symlink as listed by ls. Its identity is device, inode, size and modification \
         time, so acting on a row that changed since it was listed is refused.",
        vec![
            ("name", Type::String, "the path as listed, relative when ls was given a relative path"),
            ("type", Type::String, "file, dir or symlink"),
            ("size", Type::Filesize, "size in bytes, of the link itself for a symlink"),
            ("modified", Type::Date, "time of the last modification"),
            ("mode", Type::String, "permissions such as rwxr-xr-x, only with ls -l"),
            ("user", Type::String, "owner, only with ls -l"),
            ("group", Type::String, "group, only with ls -l"),
            ("target", Type::String, "where a symlink points, only with ls -l"),
        ],
    );
    add(
        "fs.change",
        "One change to the file system that a command made, would make in a plan, or refused.",
        vec![
            ("path", Type::String, "the path that changed"),
            ("from", Type::String, "the source of a copy or move"),
            ("action", Type::String, "what happened, such as created, trashed, would trash or refused"),
            ("size", Type::Filesize, "size of a removed file"),
            ("because", Type::String, "why the action was refused"),
        ],
    );
    add(
        "proc",
        "A running process as listed by ps. Its identity is pid and start time, so a reused pid is never \
         mistaken for the process that was listed.",
        vec![
            ("pid", Type::Int, "process id"),
            ("ppid", Type::Int, "process id of the parent"),
            ("name", Type::String, "program name"),
            ("user", Type::String, "owner"),
            ("cpu", Type::Float, "percent of one core over a short sample"),
            ("mem", Type::Filesize, "resident memory"),
            ("status", Type::String, "run state such as running, sleeping or zombie"),
            ("command", Type::String, "the full command line"),
        ],
    );
    add(
        "proc.signal",
        "A signal that kill sent, would send in a plan, or refused to send.",
        vec![
            ("pid", Type::Int, "process id"),
            ("name", Type::String, "program name, when a proc record came in"),
            ("signal", Type::String, "signal name such as SIGTERM"),
            ("action", Type::String, "signaled, failed, would signal or refused"),
            ("ok", Type::Bool, "whether the signal was delivered"),
            ("error", Type::String, "why delivery failed"),
            ("because", Type::String, "why the signal was refused"),
        ],
    );
    add(
        "job",
        "A program started in the background with spawn or &. The session owns it and may stop it.",
        vec![
            ("id", Type::Int, "job number for fg"),
            ("pid", Type::Int, "process id"),
            ("command", Type::String, "the command line"),
            ("task", Type::String, "the task the job belongs to"),
            ("unit", Type::String, "the systemd scope of a job inside a task"),
            ("state", Type::String, "running, exited or killed"),
            ("exit", Type::Int, "exit code, 128 plus the signal number for a killed job"),
        ],
    );
    add(
        "net.socket",
        "A TCP or UDP socket of a process that listens or is connected, as returned by proc.ports.",
        vec![
            ("proto", Type::String, "tcp, tcp6, udp or udp6"),
            ("local", Type::String, "local address"),
            ("port", Type::Int, "local port"),
            ("remote", Type::String, "address and port of the peer, empty while listening"),
            ("state", Type::String, "listen, established or another TCP state"),
        ],
    );
}

/// The OS reason of an I/O error without the `(os error N)` suffix, for messages such as
/// `cannot remove 'x' (no such file or directory)`.
fn reason(e: &io::Error) -> String {
    let text = e.raw_os_error().map_or_else(|| e.to_string(), |code| Errno::from_raw(code).desc().to_string());
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| first.to_lowercase().chain(chars).collect())
}

fn user_name(uid: u32) -> String {
    User::from_uid(Uid::from_raw(uid)).ok().flatten().map_or_else(|| uid.to_string(), |u| u.name)
}

/// Paths of piped values. Records give their `name` (fs.entry) or `path` (fs.change)
/// field. Strings and lists of either work as well, so the lines of `^find` do too. A record
/// that carries an identity must still describe the same object.
fn piped_paths(ctx: &Ctx, input: PipelineData) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    for value in input.into_stream() {
        collect_path(ctx, value?, &mut paths)?;
    }
    Ok(paths)
}

fn collect_path(ctx: &Ctx, value: Value, out: &mut Vec<String>) -> Result<()> {
    ctx.verify(&value)?;
    match value {
        Value::String(s) => out.push(s),
        Value::List(items) => {
            for item in items {
                collect_path(ctx, item, out)?;
            }
        }
        Value::Record(r) => match r.get("name").or_else(|| r.get("path")) {
            Some(Value::String(s)) => out.push(s.clone()),
            _ => return Err(ctx.err("expected records with a name or path field, such as the output of ls")),
        },
        other => return Err(ctx.err(format!("cannot use a {} as a path", other.type_name()))),
    }
    Ok(())
}

/// Rewrites the row of a target that was not acted on. In plan mode the action becomes
/// "would <verb>". A skipped target becomes "refused" with the reason in `because`.
fn report(row: Record, permit: &Permit, verb: &str) -> Record {
    match permit {
        Permit::Execute => row,
        Permit::Plan => row.with("action", format!("would {verb}")),
        Permit::Skip(reason) => row.with("action", "refused").with("because", reason.as_str()),
    }
}

fn change(path: &Path) -> Record {
    Record::typed("fs.change").with("path", path.display().to_string())
}

fn table(rows: Vec<Record>) -> PipelineData {
    PipelineData::Value(Value::List(rows.into_iter().map(Value::Record).collect()))
}
