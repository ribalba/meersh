use std::collections::HashMap;
use std::fs;
use std::mem::{offset_of, size_of};
use std::path::Path;

use chrono::{DateTime, Local, Utc};
use meersh_engine::{Command, Ctx, Effect, Engine, PipelineData, Result, Signature};
use meersh_value::{Record, Type, Value};
use nix::sys::utsname::uname;
use nix::unistd::geteuid;

use crate::{reason, record, user_name};

pub fn register(engine: &mut Engine) {
    engine.register(Free);
    engine.register(Uptime);
    engine.register(Whoami);
    engine.register(Hostname);
    engine.register(Uname);
}

fn read_proc(ctx: &Ctx, path: &str) -> Result<String> {
    fs::read_to_string(path).map_err(|e| ctx.err(format!("cannot read {path} ({})", reason(&e))))
}

/// The `Key: value kB` lines of `/proc/meminfo`, in bytes.
fn meminfo(text: &str) -> HashMap<&str, i64> {
    text.lines()
        .filter_map(|line| {
            let (key, rest) = line.split_once(':')?;
            let mut parts = rest.split_whitespace();
            let number: i64 = parts.next()?.parse().ok()?;
            let factor = if parts.next() == Some("kB") { 1024 } else { 1 };
            Some((key, number * factor))
        })
        .collect()
}

struct Free;

impl Command for Free {
    fn signature(&self) -> Signature {
        Signature::build("free")
            .desc(
                "Memory and swap as a mem.info record from /proc/meminfo. used is total minus available, as in \
                 current procps. cached includes reclaimable kernel memory.",
            )
            .input(Type::Nothing)
            .output(Type::record("mem.info"))
            .effect(Effect::Read)
            .example("free | get available", "memory that programs can still get")
            .example("free | select total used swap_total swap_free", "memory and swap at a glance")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let text = read_proc(ctx, "/proc/meminfo")?;
        let info = meminfo(&text);
        let get = |key: &str| info.get(key).copied().unwrap_or(0);
        let total = get("MemTotal");
        let free = get("MemFree");
        let buffers = get("Buffers");
        let cached = get("Cached") + get("SReclaimable");
        // Kernels before 3.14 have no MemAvailable.
        let available = info.get("MemAvailable").copied().unwrap_or(free + buffers + cached);
        let size = Value::Filesize;
        Ok(record(
            Record::typed("mem.info")
                .with("total", size(total))
                .with("used", size((total - available).max(0)))
                .with("free", size(free))
                .with("available", size(available))
                .with("buffers", size(buffers))
                .with("cached", size(cached))
                .with("swap_total", size(get("SwapTotal")))
                .with("swap_free", size(get("SwapFree"))),
        ))
    }
}

/// Counts logged in users the way procps `uptime` does. On a systemd machine that is the
/// number of logind sessions, which `sd_get_sessions` takes from the file names in
/// `/run/systemd/sessions`. Elsewhere it is the user processes in utmp, read as glibc's
/// `struct utmpx` at the offsets libc declares.
fn logged_in_users() -> Option<i64> {
    if Path::new("/run/systemd/system").exists()
        && let Ok(sessions) = fs::read_dir("/run/systemd/sessions")
    {
        let count = sessions
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()) && !e.file_name().to_string_lossy().starts_with('.'))
            .count();
        return Some(count as i64);
    }
    let data = fs::read("/run/utmp").or_else(|_| fs::read("/var/run/utmp")).ok()?;
    let size = size_of::<libc::utmpx>();
    let kind = offset_of!(libc::utmpx, ut_type);
    let user = offset_of!(libc::utmpx, ut_user);
    let count = data
        .chunks_exact(size)
        .filter(|r| i16::from_ne_bytes([r[kind], r[kind + 1]]) == libc::USER_PROCESS && r[user] != 0)
        .count();
    Some(count as i64)
}

struct Uptime;

impl Command for Uptime {
    fn signature(&self) -> Signature {
        Signature::build("uptime")
            .desc(
                "Time since boot, the boot time and the load averages as a sys.uptime record. users counts \
                 sessions like procps uptime, logind sessions under systemd and utmp entries elsewhere. It is \
                 left out when neither can be read.",
            )
            .input(Type::Nothing)
            .output(Type::record("sys.uptime"))
            .effect(Effect::Read)
            .example("uptime | get boot", "when the machine booted")
            .example("uptime | get load1", "the load average of the last minute")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let uptime = read_proc(ctx, "/proc/uptime")?;
        let seconds: f64 = uptime
            .split_whitespace()
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| ctx.err("cannot parse /proc/uptime"))?;
        let loadavg = read_proc(ctx, "/proc/loadavg")?;
        let loads: Vec<f64> = loadavg.split_whitespace().take(3).filter_map(|s| s.parse().ok()).collect();
        let &[load1, load5, load15] = loads.as_slice() else { return Err(ctx.err("cannot parse /proc/loadavg")) };
        // Whole seconds, so the boot time does not jitter from one call to the next.
        let boot = DateTime::<Utc>::from_timestamp(Utc::now().timestamp() - seconds.round() as i64, 0)
            .map(|d| Value::Date(d.with_timezone(&Local).fixed_offset()));
        let mut row = Record::typed("sys.uptime")
            .with("uptime", Value::Duration((seconds * 1e9) as i64))
            .with("boot", boot)
            .with("load1", load1)
            .with("load5", load5)
            .with("load15", load15);
        if let Some(users) = logged_in_users() {
            row = row.with("users", users);
        }
        Ok(record(row))
    }
}

struct Whoami;

impl Command for Whoami {
    fn signature(&self) -> Signature {
        Signature::build("whoami")
            .desc("The name of the effective user as a string.")
            .input(Type::Nothing)
            .output(Type::String)
            .effect(Effect::Read)
            .example("whoami", "the user name")
            .example("ps | where user == (whoami)", "my own processes")
    }

    fn run(&self, _ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        Ok(PipelineData::Value(Value::String(user_name(geteuid().as_raw()))))
    }
}

struct Hostname;

impl Command for Hostname {
    fn signature(&self) -> Signature {
        Signature::build("hostname")
            .desc("The host name of the machine as a string.")
            .input(Type::Nothing)
            .output(Type::String)
            .effect(Effect::Read)
            .example("hostname", "the name of this machine")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let name = uname().map_err(|e| ctx.err(format!("cannot read the host name ({})", e.desc())))?;
        Ok(PipelineData::Value(Value::String(name.nodename().to_string_lossy().into_owned())))
    }
}

struct Uname;

impl Command for Uname {
    fn signature(&self) -> Signature {
        Signature::build("uname")
            .desc("The kernel and the machine as a sys.uname record.")
            .input(Type::Nothing)
            .output(Type::record("sys.uname"))
            .effect(Effect::Read)
            .example("uname | get release", "the kernel release")
            .example("uname | get machine", "the hardware architecture, such as x86_64")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let name = uname().map_err(|e| ctx.err(format!("cannot read the kernel name ({})", e.desc())))?;
        let text = |s: &std::ffi::OsStr| s.to_string_lossy().into_owned();
        Ok(record(
            Record::typed("sys.uname")
                .with("sysname", text(name.sysname()))
                .with("release", text(name.release()))
                .with("version", text(name.version()))
                .with("machine", text(name.machine())),
        ))
    }
}
