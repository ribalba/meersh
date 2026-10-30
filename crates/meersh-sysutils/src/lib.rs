//! System and text tools that speak records instead of text. `find` streams `fs.entry`
//! records that `rm` and `chmod` accept from a pipe, `du`, `df` and `stat` describe disk
//! usage, mounts and file metadata as typed records, and `free`, `uptime` and `uname` turn
//! the kernel's text files into values.
//!
//! Like `meersh-coreutils`, these are small std based sketch implementations. They cover
//! the common cases and leave out most GNU options. Commands that change something ask
//! [`Ctx::authorize`] right before they act, so they plan, apply and refuse like `rm`.

mod disk;
mod files;
mod find;
mod methods;
mod system;
mod text;

#[cfg(test)]
mod tests;

use std::fs::FileType;
use std::io;
use std::os::unix::fs::FileTypeExt;
use std::path::Path;
use std::time::SystemTime;

use chrono::{DateTime, Local};
use meersh_engine::util::expand_glob;
use meersh_engine::{Ctx, Engine, PipelineData, Result, TypeInfo};
use meersh_value::{Record, Type, Value};
use nix::errno::Errno;
use nix::unistd::{Gid, Group, Uid, User};

pub fn register(engine: &mut Engine) {
    text::register(engine);
    disk::register(engine);
    find::register(engine);
    files::register(engine);
    system::register(engine);
    methods::register(engine);
    register_types(engine);
}

fn register_types(engine: &mut Engine) {
    use Type::{Date, Duration, Filesize, Float, Int, String};
    let types = [
        info(
            "fs.entry",
            "A file, directory or symlink, as listed by ls and find. Piped into rm, chmod or cp it names the file.",
            vec![
                ("name", String, "path as reached from the start directory"),
                ("type", String, "file, dir, symlink, fifo, socket, block or char"),
                ("size", Filesize, "apparent size"),
                ("modified", Date, "last modification"),
            ],
        ),
        info(
            "fs.change",
            "What a command that writes did, would do, or refused to do to one path.",
            vec![
                ("path", String, "the path that was changed"),
                ("action", String, "the action, for example chmod, would chmod or refused"),
                ("mode", String, "new permission bits in octal, from chmod"),
                ("target", String, "what a new link points to, from ln"),
                ("because", String, "why the action was refused"),
            ],
        ),
        info(
            "fs.usage",
            "Disk usage of a directory tree, from du.",
            vec![
                ("path", String, "the directory or file"),
                ("size", Filesize, "apparent size of all files below the path, hard links counted once"),
                ("files", Int, "number of files and symlinks below the path"),
            ],
        ),
        info(
            "fs.mount",
            "A mounted filesystem with its capacity, from df.",
            vec![
                ("mount", String, "mount point"),
                ("source", String, "device or source of the mount"),
                ("fstype", String, "filesystem type"),
                ("size", Filesize, "total capacity"),
                ("used", Filesize, "space in use"),
                ("available", Filesize, "space available to unprivileged users"),
                ("use", Float, "used space in percent of what users can have"),
            ],
        ),
        info(
            "fs.stat",
            "Metadata of one file system object, from stat. Symlinks are described, not followed.",
            vec![
                ("path", String, "the path as given"),
                ("type", String, "file, dir, symlink, fifo, socket, block or char"),
                ("size", Filesize, "apparent size"),
                ("mode", String, "permissions as in ls -l, for example rwxr-xr-x"),
                ("mode_octal", String, "permissions in octal, for example 755"),
                ("uid", Int, "owner id"),
                ("user", String, "owner name"),
                ("gid", Int, "group id"),
                ("group", String, "group name"),
                ("inode", Int, "inode number"),
                ("device", Int, "id of the device that holds the file"),
                ("links", Int, "number of hard links"),
                ("accessed", Date, "last access"),
                ("modified", Date, "last change of the content"),
                ("changed", Date, "last change of the metadata"),
            ],
        ),
        info(
            "mem.info",
            "Memory and swap of the machine, from /proc/meminfo.",
            vec![
                ("total", Filesize, "installed memory that the kernel can use"),
                ("used", Filesize, "total minus available"),
                ("free", Filesize, "memory nobody uses"),
                ("available", Filesize, "memory that new programs can get without swapping"),
                ("buffers", Filesize, "block device buffers"),
                ("cached", Filesize, "page cache and reclaimable slab"),
                ("swap_total", Filesize, "size of all swap areas"),
                ("swap_free", Filesize, "unused swap"),
            ],
        ),
        info(
            "sys.uptime",
            "How long the machine has been running and how busy it is, from uptime.",
            vec![
                ("uptime", Duration, "time since boot"),
                ("boot", Date, "when the machine booted"),
                ("load1", Float, "load average over one minute"),
                ("load5", Float, "load average over five minutes"),
                ("load15", Float, "load average over fifteen minutes"),
                ("users", Int, "logged in sessions as procps uptime counts them, missing when unknown"),
            ],
        ),
        info(
            "sys.uname",
            "The running kernel and machine, from uname.",
            vec![
                ("sysname", String, "operating system name, for example Linux"),
                ("release", String, "kernel release"),
                ("version", String, "kernel build version"),
                ("machine", String, "hardware architecture, for example x86_64"),
            ],
        ),
        info(
            "text.count",
            "Lines, words, bytes and characters of text, from wc.",
            vec![
                ("path", String, "the file, only when wc counts files"),
                ("lines", Int, "number of newline characters"),
                ("words", Int, "runs of non whitespace"),
                ("bytes", Int, "length in bytes"),
                ("chars", Int, "length in UTF-8 characters"),
            ],
        ),
    ];
    for info in types {
        // fs.entry and fs.change belong to meersh-coreutils. Its description wins if it has one.
        if engine.type_info(&info.name).is_none() {
            engine.register_type(info);
        }
    }
}

fn info(name: &str, description: &str, fields: Vec<(&str, Type, &str)>) -> TypeInfo {
    TypeInfo {
        name: name.into(),
        description: description.into(),
        fields: fields.into_iter().map(|(field, ty, desc)| (field.into(), ty, desc.into())).collect(),
    }
}

/// The OS reason of an I/O error without the `(os error N)` suffix, for messages such as
/// `cannot read 'x' (no such file or directory)`.
fn reason(e: &io::Error) -> String {
    let text = e.raw_os_error().map_or_else(|| e.to_string(), |code| Errno::from_raw(code).desc().to_string());
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| first.to_lowercase().chain(chars).collect())
}

fn user_name(uid: u32) -> String {
    User::from_uid(Uid::from_raw(uid)).ok().flatten().map_or_else(|| uid.to_string(), |u| u.name)
}

fn group_name(gid: u32) -> String {
    Group::from_gid(Gid::from_raw(gid)).ok().flatten().map_or_else(|| gid.to_string(), |g| g.name)
}

fn local_date(time: SystemTime) -> Value {
    Value::Date(DateTime::<Local>::from(time).fixed_offset())
}

/// The type names that `ls` uses, plus the special files it lumps in with `file`.
fn kind(file_type: FileType) -> &'static str {
    if file_type.is_symlink() {
        "symlink"
    } else if file_type.is_dir() {
        "dir"
    } else if file_type.is_fifo() {
        "fifo"
    } else if file_type.is_socket() {
        "socket"
    } else if file_type.is_block_device() {
        "block"
    } else if file_type.is_char_device() {
        "char"
    } else {
        "file"
    }
}

/// The permission part of `ls -l`, for example `rwxr-xr-x`. Setuid, setgid and sticky bits
/// show as `s` and `t`, upper case when the execute bit underneath is missing.
fn mode_string(mode: u32) -> String {
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

/// Permission bits in octal as `chmod` takes them, for example `755` or `4755`.
fn mode_octal(mode: u32) -> String {
    format!("{:o}", mode & 0o7777)
}

/// Paths given as arguments from `from` on, with globs expanded like bash does.
fn arg_paths(ctx: &Ctx, from: usize) -> Result<Vec<String>> {
    Ok(ctx.rest_strings(from)?.iter().flat_map(|arg| expand_glob(arg)).collect())
}

/// Paths of piped values. Records give their `name` (fs.entry) or `path` (fs.change,
/// fs.stat) field. Strings work as well, so the lines of `^find` do too.
fn piped_paths(ctx: &Ctx, input: PipelineData) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    for value in input.into_stream() {
        collect_path(ctx, value?, &mut paths)?;
    }
    Ok(paths)
}

fn collect_path(ctx: &Ctx, value: Value, out: &mut Vec<String>) -> Result<()> {
    match value {
        Value::String(s) => out.push(s),
        Value::List(items) => {
            for item in items {
                collect_path(ctx, item, out)?;
            }
        }
        Value::Record(r) => out.push(record_path(ctx, &r)?),
        other => return Err(ctx.err(format!("cannot use a {} as a path", other.type_name()))),
    }
    Ok(())
}

fn record_path(ctx: &Ctx, record: &Record) -> Result<String> {
    match record.get("name").or_else(|| record.get("path")) {
        Some(Value::String(s)) => Ok(s.clone()),
        _ => Err(ctx.err("expected records with a name or path field, such as the output of ls or find")),
    }
}

fn change(path: &Path) -> Record {
    Record::typed("fs.change").with("path", path.display().to_string())
}

fn table(rows: Vec<Record>) -> PipelineData {
    PipelineData::Value(Value::List(rows.into_iter().map(Value::Record).collect()))
}

fn record(record: Record) -> PipelineData {
    PipelineData::Value(Value::Record(record))
}
