use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::MetadataExt;

use meersh_engine::authority::proc_id;
use meersh_engine::value::{Record, Type, Value};
use meersh_engine::{Cap, Command, Ctx, Effect, Engine, Permit, PipelineData, Result, Signature};
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use sysinfo::{Process, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System, ThreadKind, UpdateKind, Users};

use crate::nav::{self, Owners};
use crate::{reason, report, table, user_name};

pub fn register(engine: &mut Engine) {
    engine.register(Ps);
    engine.register(Kill);
}

struct Ps;

impl Command for Ps {
    fn signature(&self) -> Signature {
        Signature::build("ps")
            .desc(
                "List running processes as proc records, sorted by pid. cpu is the percentage of one core over a \
                 short sample, so a busy multithreaded process can exceed 100.",
            )
            .input(Type::Nothing)
            .output(Type::table("proc"))
            .effect(Effect::Read)
            .example("ps | where name =~ firefox | sort-by mem -r | first 3", "the three largest firefox processes")
            .example("ps | where cpu > 50 | kill", "stop processes that use more than half a core")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        Ok(table(records(&sample(ctx)?, |_| true)))
    }
}

/// The process table, sampled twice because CPU usage is the difference between two samples.
pub(crate) fn sample(ctx: &Ctx) -> Result<System> {
    let kind = ProcessRefreshKind::nothing()
        .with_cpu()
        .with_memory()
        .with_user(UpdateKind::OnlyIfNotSet)
        .with_cmd(UpdateKind::OnlyIfNotSet);
    let mut system = System::new_with_specifics(RefreshKind::nothing().with_processes(kind))
        .map_err(|e| ctx.err(format!("cannot read the process list ({e})")))?;
    std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
    Ok(system)
}

/// proc records of the sampled processes that `keep` selects, sorted by pid. Each carries
/// its identity, pid and start time.
pub(crate) fn records(system: &System, keep: impl Fn(&Process) -> bool) -> Vec<Record> {
    let users = Users::new_with_refreshed_list().ok();
    let mut names: HashMap<u32, String> = HashMap::new();
    // On Linux sysinfo lists every thread of a process as well. Kernel threads stay,
    // because `ps -e` shows them too.
    let mut processes: Vec<_> =
        system.processes().values().filter(|p| p.thread_kind() != Some(ThreadKind::Userland) && keep(p)).collect();
    processes.sort_by_key(|p| p.pid());
    processes
        .into_iter()
        .map(|p| {
            let user = p.user_id().map(|uid| {
                let known = users.as_ref().and_then(|u| u.get_user_by_id(uid)).map(|u| u.name().to_string());
                names.entry(**uid).or_insert_with(|| known.unwrap_or_else(|| user_name(**uid))).clone()
            });
            let command: Vec<_> = p.cmd().iter().map(|arg| arg.to_string_lossy()).collect();
            let pid = p.pid().as_u32();
            let record = Record::typed("proc")
                .with("pid", i64::from(pid))
                .with("ppid", p.parent().map(|parent| i64::from(parent.as_u32())))
                .with("name", p.name().to_string_lossy().into_owned())
                .with("user", user)
                .with("cpu", (f64::from(p.cpu_usage()) * 100.0).round() / 100.0)
                .with("mem", Value::Filesize(p.memory() as i64))
                .with("status", p.status().to_string().to_lowercase())
                .with("command", command.join(" "));
            match proc_id(pid) {
                Some(id) => record.with_id(id),
                None => record,
            }
        })
        .collect()
}

struct Kill;

impl Command for Kill {
    fn signature(&self) -> Signature {
        Signature::build("kill")
            .desc(
                "Send a signal to processes, given as pids or as piped proc records from ps. A piped record must \
                 still describe the same process. Returns one proc.signal record per process and keeps going \
                 when a single process fails.",
            )
            .input(Type::OneOf(vec![Type::Nothing, Type::table("proc"), Type::list(Type::Int)]))
            .output(Type::table("proc.signal"))
            .rest("pids", Type::Int, "process ids")
            .named(
                "signal",
                Type::String,
                Some('s'),
                "signal name or number such as TERM, KILL, HUP or 9, TERM by default",
            )
            .effect(Effect::Destructive)
            .example("ps | where name == sleep | kill", "stop every sleep process")
            .example("kill -s HUP 1234", "ask a daemon to reload its configuration")
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let sig = signal_flag(ctx)?;
        let mut targets = Vec::new();
        for value in input.into_stream() {
            collect_pids(ctx, value?, &mut targets)?;
        }
        for index in 0..ctx.call.positional.len() {
            if let Some(value) = ctx.arg(index)? {
                collect_pids(ctx, value, &mut targets)?;
            }
        }
        if targets.is_empty() {
            return Err(ctx.err("no process ids given. Pass pids or pipe in rows from ps"));
        }
        if let Some((pid, _)) = targets.iter().find(|(pid, _)| !single_process(*pid)) {
            return Err(ctx.err(format!("refusing pid {pid}, which is not a single process. Use -s to pick a signal")));
        }
        let permits =
            targets.iter().map(|(pid, _)| ctx.authorize(signal_cap(*pid, sig))).collect::<Result<Vec<_>>>()?;
        let rows = targets
            .into_iter()
            .zip(&permits)
            .map(|((pid, name), permit)| send(ctx, pid, name, sig, permit))
            .collect::<Result<_>>()?;
        Ok(table(rows))
    }
}

/// The `--signal` flag, TERM by default.
pub(crate) fn signal_flag(ctx: &Ctx) -> Result<Signal> {
    let name = ctx.named_string("signal")?.unwrap_or_else(|| "TERM".into());
    parse_signal(&name).ok_or_else(|| ctx.err(format!("unknown signal '{name}'")))
}

/// Only called with pids that passed [`single_process`].
pub(crate) fn signal_cap(pid: i64, sig: Signal) -> Cap {
    Cap::ProcSignal { pid: pid as u32, signal: sig.as_str().into() }
}

/// Sends one authorized signal and reports it as proc.signal.
pub(crate) fn send(ctx: &Ctx, pid: i64, name: Value, sig: Signal, permit: &Permit) -> Result<Record> {
    let row = Record::typed("proc.signal")
        .with("pid", pid)
        .with("name", name)
        .with("signal", sig.as_str())
        .with("action", "signaled")
        .with("ok", Value::Nothing)
        .with("error", Value::Nothing);
    if *permit != Permit::Execute {
        return Ok(report(row, permit, "signal"));
    }
    match signal::kill(Pid::from_raw(pid as i32), sig) {
        Ok(()) => {
            ctx.performed(signal_cap(pid, sig))?;
            Ok(row.with("ok", true))
        }
        Err(e) => Ok(row.with("action", "failed").with("ok", false).with("error", e.desc())),
    }
}

/// kill(2) reads 0 and negative pids as process groups, and -1 as every process the user
/// owns. A bash habit like `kill -9 1234` arrives here as pid -9 and must not go through.
pub(crate) fn single_process(pid: i64) -> bool {
    pid > 0 && pid <= i64::from(i32::MAX)
}

/// Accepts `TERM`, `SIGTERM`, `term` and `15`.
fn parse_signal(name: &str) -> Option<Signal> {
    if let Ok(number) = name.parse::<i32>() {
        return Signal::try_from(number).ok();
    }
    let upper = name.to_ascii_uppercase();
    let full = if upper.starts_with("SIG") { upper } else { format!("SIG{upper}") };
    full.parse().ok()
}

/// Pairs of pid and process name. The name is only known when a proc record came in, and
/// such a record must still describe the same process.
fn collect_pids(ctx: &Ctx, value: Value, out: &mut Vec<(i64, Value)>) -> Result<()> {
    ctx.verify(&value)?;
    match value {
        Value::Int(pid) => out.push((pid, Value::Nothing)),
        Value::String(s) => {
            let pid = s.trim().parse().map_err(|_| ctx.err(format!("'{s}' is not a process id")))?;
            out.push((pid, Value::Nothing));
        }
        Value::List(items) => {
            for item in items {
                collect_pids(ctx, item, out)?;
            }
        }
        Value::Record(r) => {
            let pid =
                r.get("pid").and_then(Value::as_int).ok_or_else(|| ctx.err("expected records with a pid field"))?;
            out.push((pid, r.get("name").cloned().unwrap_or(Value::Nothing)));
        }
        other => return Err(ctx.err(format!("cannot use a {} as a process id", other.type_name()))),
    }
    Ok(())
}

/// Regular files a process has open, as fs.entry. A descriptor whose path now names a
/// different file, or none at all because the file was deleted, is left out.
pub(crate) fn open_files(ctx: &Ctx, pid: u32) -> Result<Vec<Record>> {
    let fds = fs::read_dir(format!("/proc/{pid}/fd"))
        .map_err(|e| ctx.err(format!("cannot read the open files of pid {pid} ({})", reason(&e))))?;
    let mut seen = HashSet::new();
    let mut owners = Owners::default();
    let mut rows = Vec::new();
    for fd in fds.flatten() {
        let (Ok(target), Ok(open)) = (fs::read_link(fd.path()), fs::metadata(fd.path())) else { continue };
        if !open.is_file() || !target.is_absolute() || !seen.insert(target.clone()) {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(&target) else { continue };
        if meta.dev() == open.dev() && meta.ino() == open.ino() {
            rows.push(nav::entry(target.display().to_string(), &target, &meta, false, &mut owners));
        }
    }
    Ok(rows)
}
