use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use meersh_parser::{Arg, CommandCall, Expr, Redirect};
use meersh_value::protocol;
use meersh_value::{Record, Value};

use crate::authority::{Cap, Permit};
use crate::util::expand_glob;
use crate::{ByteSource, ByteStream, Engine, Job, PipelineData, Result, ShellError, Stack};

pub(crate) fn expand_args(engine: &Arc<Engine>, stack: &Stack, call: &CommandCall) -> Result<Vec<String>> {
    let mut args = Vec::new();
    for arg in &call.args {
        match arg {
            Arg::Flag { raw, .. } => args.push(raw.clone()),
            Arg::Positional(Expr::Word(word)) => args.extend(expand_glob(word)),
            Arg::Positional(expr) => match engine.eval_expr(stack, expr, None)? {
                Value::List(items) => args.extend(items.iter().map(Value::to_text)),
                value => args.push(value.to_text()),
            },
        }
    }
    Ok(args)
}

/// The process for a program, wrapped in `meersh __exec` when the scope has a sandbox or
/// restrictions, and with the shell's environment changes applied.
fn build(engine: &Engine, stack: &Stack, program: &str, args: &[String]) -> Result<Command> {
    let mut command = if stack.sandbox().is_some() || stack.restrictions().is_some() {
        let helper = engine.exec_helper.as_ref().ok_or_else(|| {
            ShellError::Other("sandboxes and `with` blocks need the meersh binary to start programs".into())
        })?;
        let mut c = Command::new(helper);
        c.arg("__exec");
        if let Some(sandbox) = stack.sandbox() {
            c.args(sandbox.helper_args());
        }
        if let Some(r) = stack.restrictions() {
            if let Some(roots) = &r.write {
                c.arg("--restrict-write");
                for root in roots {
                    c.arg("--write").arg(root);
                }
            }
            if r.no_net {
                c.arg("--no-net");
            }
        }
        c.arg("--cwd").arg(std::env::current_dir()?).arg("--").arg(program).args(args);
        c
    } else {
        let mut c = Command::new(program);
        c.args(args);
        c
    };
    for (key, value) in engine.env_overlay() {
        match value {
            Some(value) => command.env(key, value),
            None => command.env_remove(key),
        };
    }
    Ok(command)
}

fn not_found(program: &str) -> impl FnOnce(std::io::Error) -> ShellError + '_ {
    move |e| match e.kind() {
        std::io::ErrorKind::NotFound => ShellError::CommandNotFound(program.to_string()),
        _ => e.into(),
    }
}

/// Runs a program that is not a builtin.
///
/// Values going into the program are written as text. Strings become lines and structured
/// values become JSON lines, so `ls | ^jq .name` works out of the box. Bytes coming out of
/// the program stay bytes until a builtin consumes them. Two externals in a row are joined
/// by a plain OS pipe. A program that a model wrote, or whose input a model chose, needs
/// authority first.
pub fn run(
    engine: &Arc<Engine>,
    stack: &Stack,
    call: &CommandCall,
    input: PipelineData,
    to_terminal: bool,
    feeds_builtin: bool,
) -> Result<PipelineData> {
    let args = expand_args(engine, stack, call)?;
    if stack.principal().is_model() || stack.llm_chosen() {
        match engine.authorize(stack, Cap::Exec { program: call.name.clone(), args: args.clone() })? {
            Permit::Execute => {}
            Permit::Plan => return Ok(PipelineData::Empty),
            Permit::Skip(reason) => {
                let skipped = Record::typed("exec.skipped").with("program", call.name.as_str()).with("because", reason);
                return Ok(PipelineData::Value(Value::Record(skipped)));
            }
        }
    }

    let adapter = engine
        .adapter(&call.name)
        .filter(|_| feeds_builtin && !to_terminal && !call.redirects.iter().any(|r| r.stdout))
        .and_then(|a| a.rewrite(&args).map(|new_args| (a, new_args)));
    let run_args = adapter.as_ref().map_or_else(|| args.clone(), |(_, a)| a.clone());
    let mut command = build(engine, stack, &call.name, &run_args)?;
    let mut upstream = Vec::new();
    let mut feed = None;
    match input {
        PipelineData::Empty => {}
        PipelineData::Bytes(ByteStream { source: ByteSource::Child(stdout), children, .. }) => {
            command.stdin(Stdio::from(stdout));
            upstream = children;
        }
        other => {
            command.stdin(Stdio::piped());
            feed = Some(other);
        }
    }
    let mut objects = None;
    let mut objects_write = None;
    let mut to_terminal = to_terminal;
    let mut redirected_paths = Vec::new();
    for redirect in &call.redirects {
        let (file, path) = open_redirect(engine, stack, redirect)?;
        redirected_paths.extend(file.as_ref().map(|_| path));
        let stdio = |f: &Option<File>| -> Result<Stdio> {
            Ok(match f {
                Some(f) => Stdio::from(f.try_clone()?),
                None => Stdio::null(),
            })
        };
        if redirect.stderr {
            command.stderr(stdio(&file)?);
        }
        if redirect.stdout {
            command.stdout(stdio(&file)?);
            // The program writes straight into the file, the pipeline gets nothing.
            to_terminal = true;
        }
    }
    let stdout_redirected = call.redirects.iter().any(|r| r.stdout);
    if stdout_redirected {
        // Set above.
    } else if to_terminal {
        command.stdout(Stdio::inherit());
    } else {
        command.stdout(Stdio::piped());
        // Ask meersh aware programs for objects, on stdout (v1) or on descriptor 3 (v2).
        if feeds_builtin {
            command.env(protocol::ENV_VAR, protocol::FORMAT);
        }
        if adapter.is_none() && feeds_builtin {
            let (read, write) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).map_err(std::io::Error::from)?;
            let raw = write.as_raw_fd();
            // SAFETY: dup2 and fcntl are async signal safe, nothing else runs between fork and exec.
            unsafe {
                command.pre_exec(move || {
                    let ok = if raw == 3 { libc::fcntl(3, libc::F_SETFD, 0) } else { libc::dup2(raw, 3) };
                    if ok == -1 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
                });
            }
            command.env(protocol::FD_VAR, "3");
            objects = Some(File::from(read));
            objects_write = Some(write);
        }
    }

    let mut child = command.spawn().map_err(not_found(&call.name))?;
    drop(objects_write);
    if let Some(data) = feed {
        let stdin = child.stdin.take().expect("stdin was piped");
        std::thread::spawn(move || feed_input(data, stdin));
    }

    if to_terminal {
        let status = child.wait()?;
        for mut up in upstream {
            let _ = up.wait();
        }
        // The program wrote into files this session owns. They stay owned with that content.
        for path in &redirected_paths {
            engine.authority.ledger.refresh(path, stack.principal(), &engine.authority.session);
        }
        engine.last_exit.store(status.code().unwrap_or(-1), Ordering::Relaxed);
        return Ok(PipelineData::Empty);
    }
    if let Some((adapter, _)) = adapter {
        let mut text = String::new();
        child.stdout.take().expect("stdout was piped").read_to_string(&mut text)?;
        child.wait()?;
        for mut up in upstream {
            let _ = up.wait();
        }
        let rows = adapter
            .parse(&text)
            .map_err(|e| ShellError::Other(format!("^{}: cannot read its output ({e})", call.name)))?;
        return Ok(PipelineData::Value(Value::List(rows)));
    }
    let stdout = child.stdout.take().expect("stdout was piped");
    upstream.push(child);
    Ok(PipelineData::Bytes(ByteStream { source: ByteSource::Child(stdout), children: upstream, objects }))
}

/// Opens the target of a redirection with authority. In plan mode nothing is opened and the
/// stream goes nowhere.
fn open_redirect(
    engine: &Arc<Engine>,
    stack: &Stack,
    redirect: &Redirect,
) -> Result<(Option<File>, std::path::PathBuf)> {
    let target = match &redirect.target {
        Expr::Word(w) => crate::util::expand_path(w),
        other => crate::util::expand_path(&engine.eval_expr(stack, other, None)?.to_text()),
    };
    let cap =
        if target.exists() { Cap::FsWrite { path: target.clone() } } else { Cap::FsCreate { path: target.clone() } };
    match engine.authorize(stack, cap.clone())? {
        Permit::Execute => {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .append(redirect.append)
                .truncate(!redirect.append)
                .open(&target)?;
            engine.authority.performed(stack, cap)?;
            Ok((Some(file), crate::authority::absolute(&target)))
        }
        Permit::Plan | Permit::Skip(_) => Ok((None, target)),
    }
}

/// `cmd o> file` for builtins. Text is written as is, structured values as JSON.
pub(crate) fn write_redirect(
    engine: &Arc<Engine>,
    stack: &Stack,
    redirect: &Redirect,
    data: PipelineData,
) -> Result<()> {
    let (Some(mut file), path) = open_redirect(engine, stack, redirect)? else { return Ok(()) };
    match data {
        PipelineData::Bytes(bytes) => {
            std::io::copy(&mut bytes.into_reader(), &mut file)?;
        }
        other => match other.into_value()? {
            Value::Nothing => {}
            Value::String(s) => writeln!(file, "{s}")?,
            Value::Binary(b) => file.write_all(&b)?,
            value => writeln!(file, "{}", serde_json::to_string_pretty(&value.to_json()).unwrap_or_default())?,
        },
    }
    drop(file);
    engine.authority.ledger.refresh(&path, stack.principal(), &engine.authority.session);
    Ok(())
}

fn feed_input(data: PipelineData, stdin: ChildStdin) {
    let mut out = BufWriter::new(stdin);
    if let PipelineData::Bytes(bytes) = data {
        let _ = std::io::copy(&mut bytes.into_reader(), &mut out);
        return;
    }
    for value in data.into_stream() {
        // A write error means the program stopped reading, which is fine.
        let Ok(value) = value else { break };
        if writeln!(out, "{}", value.to_text()).is_err() {
            break;
        }
    }
}

static JOB_IDS: AtomicUsize = AtomicUsize::new(1);

fn has_program(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(name).is_file()))
}

impl Engine {
    /// Starts a program in the background, for `cmd &` and `spawn`. It gets its own process
    /// group, so Ctrl-C in the shell does not reach it, and it enters the ledger, so this
    /// session may stop it later. Inside a task it runs in its own systemd scope, which
    /// lets `task stop` end it together with everything it started.
    pub fn spawn_job(self: &Arc<Self>, stack: &Stack, program: &str, args: &[String]) -> Result<Value> {
        let text = std::iter::once(program.to_string()).chain(args.iter().cloned()).collect::<Vec<_>>().join(" ");
        match self.authorize(stack, Cap::ProcSpawn { program: program.into() })? {
            Permit::Execute => {}
            Permit::Plan => {
                return Ok(Value::Record(Record::typed("job").with("command", text).with("state", "would start")));
            }
            Permit::Skip(reason) => {
                return Ok(Value::Record(
                    Record::typed("job").with("command", text).with("state", "skipped").with("because", reason),
                ));
            }
        }
        let id = JOB_IDS.fetch_add(1, Ordering::Relaxed);
        let unit = stack.task().filter(|_| has_program("systemd-run")).map(|task| {
            let clean: String = task.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
            format!("meersh-{clean}-{id}")
        });
        let mut command = match &unit {
            Some(unit) => {
                let mut c = Command::new("systemd-run");
                c.args(["--user", "--scope", "--quiet", "--collect", "--unit", unit, "--"]).arg(program).args(args);
                c
            }
            None => build(self, stack, program, args)?,
        };
        command.stdin(Stdio::null()).process_group(0);
        let child = command.spawn().map_err(not_found(program))?;
        let pid = child.id();
        self.authority.performed(stack, Cap::ProcSpawn { program: program.into() })?;
        self.authority.ledger.record_process(pid, program, stack.principal(), &self.authority.session, stack.task());
        self.jobs.lock().expect("jobs lock").push(Job {
            id,
            command: text.clone(),
            pid,
            child: Some(child),
            unit: unit.clone(),
            task: stack.task().map(str::to_string),
        });
        let mut record = Record::typed("job").with("id", id as i64).with("pid", i64::from(pid)).with("command", text);
        record.insert("state", "running");
        record.insert("unit", unit);
        Ok(Value::Record(record.with_id(crate::authority::proc_id(pid).unwrap_or_default())))
    }
}
