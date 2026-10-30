//! Methods of fs.entry and proc. A method is a command named `<type>.<method>` that takes the
//! object as input, so `$f.delete()` runs `fs.entry.delete` with `$f`. Methods that act, and
//! all methods of proc, first check that the object is still the one that was listed.

use std::path::{Component, Path, PathBuf};

use meersh_engine::authority::absolute;
use meersh_engine::util::expand_path;
use meersh_engine::value::{Record, Type, Value};
use meersh_engine::{Command, Ctx, Effect, Engine, PipelineData, Result, Signature};
use sysinfo::Pid;

use crate::nav::{self, Owners};
use crate::{files, net, open, proc};

pub fn register(engine: &mut Engine) {
    let entry = |name: &str| Signature::build(name).input(Type::record("fs.entry"));
    let process = |name: &str| Signature::build(name).input(Type::record("proc"));

    engine.register(Method::verified(
        entry("fs.entry.delete")
            .desc("Move the file or directory to the trash, or delete it for good with --permanent. Directories go with their contents.")
            .output(Type::record("fs.change"))
            .switch("permanent", Some('p'), "delete for good instead of moving to the trash")
            .effect(Effect::Destructive)
            .example("ls *.tmp | each { $it.delete() }", "trash every .tmp file"),
        |ctx, entry| files::remove_one(ctx, &path_of(ctx, entry)?, ctx.switch("permanent")).map(Value::Record),
    ));
    engine.register(Method::verified(
        entry("fs.entry.trash")
            .desc("Move the file or directory to the trash, from where undo can bring it back.")
            .output(Type::record("fs.change"))
            .effect(Effect::Destructive)
            .example("ls | where size > 1gb | each { $it.trash() }", "trash files larger than 1 GiB"),
        |ctx, entry| files::remove_one(ctx, &path_of(ctx, entry)?, false).map(Value::Record),
    ));
    engine.register(Method::verified(
        entry("fs.entry.move")
            .desc(
                "Move or rename the file or directory. An existing directory as target receives it under its own name.",
            )
            .output(Type::record("fs.change"))
            .required("to", Type::String, "new path or existing directory")
            .effect(Effect::Write)
            .example("ls *.png | each { $it.move(images/) }", "move every PNG into images/"),
        |ctx, entry| {
            let to = expand_path(&ctx.req_string(0, "to")?);
            files::move_one(ctx, &path_of(ctx, entry)?, &to).map(Value::Record)
        },
    ));
    engine.register(Method::verified(
        entry("fs.entry.copy")
            .desc("Copy the file, or the directory with its contents. An existing directory as target receives the copy under the same name.")
            .output(Type::record("fs.change"))
            .required("to", Type::String, "path of the copy or existing directory")
            .effect(Effect::Write)
            .example("ls Cargo.toml | first | $in.copy(\"Cargo.toml.bak\")", "back up one file"),
        |ctx, entry| {
            let to = expand_path(&ctx.req_string(0, "to")?);
            files::copy_one(ctx, &path_of(ctx, entry)?, &to).map(Value::Record)
        },
    ));
    engine.register(Method::plain(
        entry("fs.entry.parent")
            .desc("The directory that contains the entry.")
            .output(Type::record("fs.entry"))
            .effect(Effect::Read)
            .example("ls **/*.log | where size > 1gb | each { $it.parent() } | uniq", "where the big logs live"),
        |ctx, entry| {
            let raw = path_of(ctx, entry)?;
            let parent = parent(&expand_path(&raw)).ok_or_else(|| ctx.err(format!("'{raw}' has no parent")))?;
            Ok(Value::Record(nav::entry_at(ctx, parent.display().to_string(), &parent)?))
        },
    ));
    engine.register(Method::plain(
        entry("fs.entry.children")
            .desc("The entries of a directory, like ls of it.")
            .output(Type::table("fs.entry"))
            .switch("all", Some('a'), "include hidden entries whose name starts with a dot")
            .effect(Effect::Read)
            .example(
                "ls | where type == dir | each { $it.children() | length }",
                "count the entries of every directory",
            ),
        |ctx, entry| {
            let raw = path_of(ctx, entry)?;
            let path = expand_path(&raw);
            if !path.is_dir() {
                return Err(ctx.err(format!("'{raw}' is not a directory")));
            }
            let mut rows = Vec::new();
            nav::list_dir(ctx, &raw, &path, ctx.switch("all"), false, &mut Owners::default(), &mut rows)?;
            nav::sorted(rows).into_value()
        },
    ));
    engine.register(Method::plain(
        entry("fs.entry.open")
            .desc("Read the file like open. JSON and TOML are parsed, other text becomes a string.")
            .output(Type::Any)
            .switch("raw", Some('r'), "return the text without parsing it")
            .effect(Effect::Read)
            .example("ls *.json | first | $in.open()", "parse the first JSON file"),
        |ctx, entry| open::read_file(ctx, &path_of(ctx, entry)?, ctx.switch("raw")),
    ));

    engine.register(Method::verified(
        process("proc.kill")
            .desc("Send a signal to the process, TERM by default. Refused when the pid now belongs to another process.")
            .output(Type::record("proc.signal"))
            .named("signal", Type::String, Some('s'), "signal name or number such as TERM, KILL or 9")
            .effect(Effect::Destructive)
            .example("ps | where name == sleep | each { $it.kill() }", "stop every sleep process"),
        |ctx, record| {
            let pid = pid_of(ctx, record)?;
            let sig = proc::signal_flag(ctx)?;
            let permit = ctx.authorize(proc::signal_cap(pid, sig))?;
            let name = record.get("name").cloned().unwrap_or(Value::Nothing);
            Ok(Value::Record(proc::send(ctx, pid, name, sig, &permit)?))
        },
    ));
    engine.register(Method::verified(
        process("proc.parent")
            .desc("The parent process.")
            .output(Type::record("proc"))
            .effect(Effect::Read)
            .example("ps | where name == bash | first | $in.parent()", "what started this shell"),
        |ctx, record| {
            let pid = pid_of(ctx, record)?;
            let system = proc::sample(ctx)?;
            let ppid = system.process(Pid::from_u32(pid as u32)).and_then(|p| p.parent());
            let parent = ppid.and_then(|ppid| proc::records(&system, |p| p.pid() == ppid).pop());
            parent.map(Value::Record).ok_or_else(|| ctx.err(format!("process {pid} has no parent")))
        },
    ));
    engine.register(Method::verified(
        process("proc.children")
            .desc("The direct child processes.")
            .output(Type::table("proc"))
            .effect(Effect::Read)
            .example("ps | where name == make | first | $in.children()", "what make is running right now"),
        |ctx, record| {
            let pid = Pid::from_u32(pid_of(ctx, record)? as u32);
            let rows = proc::records(&proc::sample(ctx)?, |p| p.parent() == Some(pid));
            Ok(Value::List(rows.into_iter().map(Value::Record).collect()))
        },
    ));
    engine.register(Method::verified(
        process("proc.files")
            .desc("The regular files the process has open, as fs.entry. Needs permission to read its descriptors.")
            .output(Type::table("fs.entry"))
            .effect(Effect::Read)
            .example(
                "ps | where name == postgres | first | $in.files() | where name =~ '\\.db$'",
                "the database files postgres has open",
            ),
        |ctx, record| {
            let rows = proc::open_files(ctx, pid_of(ctx, record)? as u32)?;
            nav::sorted(rows).into_value()
        },
    ));
    engine.register(Method::verified(
        process("proc.ports")
            .desc("The TCP and UDP sockets of the process that listen or are connected, as net.socket.")
            .output(Type::table("net.socket"))
            .effect(Effect::Read)
            .example("ps | where name == node | first | $in.ports()", "what this node process is listening on"),
        |ctx, record| {
            let rows = net::sockets(ctx, pid_of(ctx, record)? as u32)?;
            Ok(Value::List(rows.into_iter().map(Value::Record).collect()))
        },
    ));
}

type Run = fn(&Ctx, &Record) -> Result<Value>;

/// A method takes exactly one object and returns one value for it.
struct Method {
    signature: Signature,
    /// Check the identity of the object before running.
    verify: bool,
    run: Run,
}

impl Method {
    fn verified(signature: Signature, run: Run) -> Self {
        Method { signature, verify: true, run }
    }

    fn plain(signature: Signature, run: Run) -> Self {
        Method { signature, verify: false, run }
    }
}

impl Command for Method {
    fn signature(&self) -> Signature {
        self.signature.clone()
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let value = input.into_value()?;
        let Value::Record(record) = &value else {
            let short = self.signature.name.rsplit('.').next().unwrap_or_default();
            return Err(ctx.err(format!(
                "takes one record, not a {}. Use each {{ $it.{short}() }} for a table",
                value.type_name()
            )));
        };
        if self.verify {
            ctx.verify(&value)?;
        }
        Ok(PipelineData::Value((self.run)(ctx, record)?))
    }
}

fn path_of(ctx: &Ctx, entry: &Record) -> Result<String> {
    match entry.get("name").or_else(|| entry.get("path")) {
        Some(Value::String(path)) => Ok(path.clone()),
        _ => Err(ctx.err("expected a record with a name or path field, such as a row of ls")),
    }
}

fn pid_of(ctx: &Ctx, record: &Record) -> Result<i64> {
    let pid = record.get("pid").and_then(Value::as_int).ok_or_else(|| ctx.err("expected a record with a pid field"))?;
    if !proc::single_process(pid) {
        return Err(ctx.err(format!("refusing pid {pid}, which is not a single process")));
    }
    Ok(pid)
}

/// The parent as it would be written next to the entry. `notes.md` has the parent `.`.
/// Names that end in `.` or `..` are resolved first, so the parent of `..` is two levels up.
fn parent(path: &Path) -> Option<PathBuf> {
    match path.components().next_back() {
        Some(Component::Normal(_)) => {
            path.parent().map(|p| if p.as_os_str().is_empty() { PathBuf::from(".") } else { p.to_path_buf() })
        }
        _ => absolute(path).parent().map(Path::to_path_buf),
    }
}
