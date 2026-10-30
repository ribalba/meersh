//! The host side. A plugin command is an ordinary command of the engine that starts the
//! plugin for every call.

use std::fmt::Display;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command as Process, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use meersh_engine::value::protocol::Encoder;
use meersh_engine::value::{Record, Type, Value};
use meersh_engine::{Cap, Command, Ctx, Effect, Engine, Permit, PipelineData, Result, ShellError, Signature, Stack};
use serde_json::{Map, Value as Json};

use crate::protocol::{ERROR_KEY, PLUGIN_FLAG, Request, Sequence, Signatures, item_type};
use crate::registry::{Registry, registry_path};

/// How long a plugin may take to list its commands. The shell waits for it at startup.
const SIGNATURES_TIMEOUT: Duration = Duration::from_secs(10);

/// A command that a plugin provides.
pub struct PluginCommand {
    plugin: String,
    path: PathBuf,
    signature: Signature,
}

impl PluginCommand {
    pub fn new(plugin: &str, path: &Path, signature: Signature) -> Self {
        PluginCommand { plugin: plugin.into(), path: path.into(), signature }
    }

    /// Evaluates the arguments for the plugin. Strings come back as typed, so `stamp 2d`
    /// means the file `2d`. Records keep their type tags on the wire.
    fn arguments(&self, ctx: &Ctx) -> Result<(Vec<Json>, Map<String, Json>)> {
        let sig = &self.signature;
        let wire = |v: Value| v.to_json_with(true);
        let mut positional = Vec::new();
        for (i, param) in sig.params.iter().enumerate() {
            let value = if param.ty == Type::String { ctx.opt_string(i)?.map(Value::String) } else { ctx.arg(i)? };
            match value {
                Some(value) => positional.push(wire(value)),
                None if param.optional => break,
                None => return Err(ctx.err(format!("missing argument <{}>, see `help {}`", param.name, sig.name))),
            }
        }
        let given = ctx.call.positional.len();
        match &sig.rest {
            Some(rest) if rest.ty == Type::String => {
                positional.extend(ctx.rest_strings(sig.params.len())?.into_iter().map(Json::String));
            }
            Some(_) => {
                for i in sig.params.len()..given {
                    positional.extend(ctx.arg(i)?.map(wire));
                }
            }
            None if given > sig.params.len() => {
                return Err(ctx.err(format!("too many arguments, see `help {}`", sig.name)));
            }
            None => {}
        }
        let mut flags = Map::new();
        for flag in &sig.flags {
            let value = match &flag.arg {
                None => ctx.switch(&flag.long).then_some(Value::Bool(true)),
                Some(Type::String) => ctx.named_string(&flag.long)?.map(Value::String),
                Some(_) => ctx.named(&flag.long)?,
            };
            if let Some(value) = value {
                flags.insert(flag.long.clone(), wire(value));
            }
        }
        Ok((positional, flags))
    }

    /// Starts the plugin for one call. A list or table comes back as a lazy stream, anything
    /// else is collected, so `stamp x | get path` sees a record and not a list of one.
    fn call(
        &self,
        ctx: &Ctx,
        positional: Vec<Json>,
        flags: Map<String, Json>,
        input: PipelineData,
    ) -> Result<PipelineData> {
        let mut child = process(ctx.engine, Some(ctx.stack), &self.path)?
            .spawn()
            .map_err(|e| ctx.err(format!("cannot start plugin {} ({e})", self.plugin)))?;
        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        let request = Request::Call { name: self.signature.name.clone(), positional, flags };
        let feeder = std::thread::spawn(move || feed(stdin, &request, input));
        let output = Output {
            sequence: Some(Sequence::new(BufReader::new(stdout), item_type(&self.signature.output))),
            child,
            feeder: Some(feeder),
            command: self.signature.name.clone(),
        };
        if matches!(self.signature.output, Type::List(_) | Type::Table { .. }) {
            return Ok(PipelineData::stream(output));
        }
        let mut values = output.collect::<Result<Vec<_>>>()?;
        Ok(match values.len() {
            0 => PipelineData::Empty,
            1 => PipelineData::Value(values.remove(0)),
            _ => PipelineData::Value(Value::List(values)),
        })
    }
}

impl Command for PluginCommand {
    fn signature(&self) -> Signature {
        self.signature.clone()
    }

    /// Pure and read commands just run. Commands that write or delete run only after the
    /// authority allows running the plugin for this command.
    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let (positional, flags) = self.arguments(ctx)?;
        if !matches!(self.signature.effect, Effect::Write | Effect::Destructive) {
            return self.call(ctx, positional, flags, input);
        }
        let cap = || Cap::Exec { program: self.path.display().to_string(), args: vec![self.signature.name.clone()] };
        match ctx.authorize(cap())? {
            Permit::Execute => {}
            Permit::Plan => return Ok(PipelineData::Empty),
            Permit::Skip(reason) => {
                let skipped = Record::typed("plugin.skipped")
                    .with("plugin", self.plugin.as_str())
                    .with("command", self.signature.name.as_str())
                    .with("because", reason);
                return Ok(PipelineData::Value(Value::Record(skipped)));
            }
        }
        let output = self.call(ctx, positional, flags, input)?;
        ctx.performed(cap())?;
        Ok(output)
    }
}

/// The plugin process. Inside a sandbox or a `with` block it runs through `meersh __exec`,
/// the same way the engine starts external programs, so a plugin gets no more rights than
/// any other program in that scope.
fn process(engine: &Engine, stack: Option<&Stack>, path: &Path) -> Result<Process> {
    let confined = stack.filter(|s| s.sandbox().is_some() || s.restrictions().is_some());
    let mut command = match confined {
        Some(stack) => {
            let helper = engine.exec_helper.as_ref().ok_or_else(|| {
                ShellError::Other("sandboxes and `with` blocks need the meersh binary to start plugins".into())
            })?;
            let mut c = Process::new(helper);
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
            c.arg("--cwd").arg(std::env::current_dir()?).arg("--").arg(path).arg(PLUGIN_FLAG);
            c
        }
        None => {
            let mut c = Process::new(path);
            c.arg(PLUGIN_FLAG);
            c
        }
    };
    for (key, value) in engine.env_overlay() {
        match value {
            Some(value) => command.env(key, value),
            None => command.env_remove(key),
        };
    }
    command.stdin(Stdio::piped()).stdout(Stdio::piped());
    Ok(command)
}

/// Writes the request line and the input as a JSON text sequence, then closes standard input.
/// A failed write means the plugin stopped reading, which it may. A failed input stream is
/// reported back.
fn feed(stdin: ChildStdin, request: &Request, input: PipelineData) -> Option<ShellError> {
    let mut out = BufWriter::new(stdin);
    let line = serde_json::to_string(request).expect("a request serializes");
    if writeln!(out, "{line}").is_err() {
        return None;
    }
    let mut encoder = Encoder::new(&mut out);
    for value in input.into_stream() {
        match value {
            Ok(value) => {
                if encoder.write(&value).is_err() {
                    return None;
                }
            }
            Err(e) => return Some(e),
        }
    }
    let _ = encoder.flush();
    None
}

/// The output of one call, read lazily. Dropping it closes the pipe and reaps the process.
struct Output {
    sequence: Option<Sequence<BufReader<ChildStdout>>>,
    child: Child,
    feeder: Option<JoinHandle<Option<ShellError>>>,
    command: String,
}

impl Output {
    fn err(&self, msg: impl Display) -> ShellError {
        ShellError::Command { cmd: self.command.clone(), msg: msg.to_string() }
    }

    /// After the last value. A failed input or a failed process is an error even when the
    /// output looked complete. The feeder is only joined when it is done, because it may
    /// still wait for a slow upstream stage.
    fn finish(&mut self) -> Option<ShellError> {
        self.sequence = None;
        let status = self.child.wait();
        let input_error = self.feeder.take().filter(JoinHandle::is_finished).and_then(|f| f.join().ok().flatten());
        if let Some(e) = input_error {
            return Some(e);
        }
        match status {
            Ok(status) if status.success() => None,
            Ok(status) => Some(self.err(exit_text(status))),
            Err(e) => Some(e.into()),
        }
    }
}

impl Iterator for Output {
    type Item = Result<Value>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.sequence.as_mut()?.next() {
            Some(Ok(value)) => Some(Ok(value)),
            Some(Err(message)) => {
                self.sequence = None;
                let _ = self.child.wait();
                Some(Err(self.err(message)))
            }
            None => self.finish().map(Err),
        }
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        // Closing the pipe first lets a plugin that is still writing stop with EPIPE.
        self.sequence = None;
        let _ = self.child.wait();
    }
}

fn exit_text(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("the plugin exited with status {code}"),
        None => "the plugin was killed by a signal".into(),
    }
}

/// Starts a plugin and asks for its commands.
pub fn query_signatures(engine: &Engine, path: &Path) -> std::result::Result<Vec<Signature>, String> {
    signatures_in(engine, None, path)
}

pub(crate) fn signatures_in(
    engine: &Engine,
    stack: Option<&Stack>,
    path: &Path,
) -> std::result::Result<Vec<Signature>, String> {
    let mut child = process(engine, stack, path)
        .map_err(|e| e.to_string())?
        .spawn()
        .map_err(|e| format!("cannot start {} ({e})", path.display()))?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let request = serde_json::to_string(&Request::Signatures).expect("a request serializes");
    // A plugin that exits without reading closes the pipe. Its answer, or the missing one,
    // says more than the failed write.
    let _ = writeln!(stdin, "{request}");
    drop(stdin);
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = tx.send(stdout.read_to_string(&mut text).map(|_| text));
    });
    let Ok(answer) = rx.recv_timeout(SIGNATURES_TIMEOUT) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("no answer within {} seconds", SIGNATURES_TIMEOUT.as_secs()));
    };
    let status = child.wait().map_err(|e| e.to_string())?;
    let text = answer.map_err(|e| format!("cannot read the answer ({e})"))?;
    let Some(line) = text.lines().find(|l| !l.trim().is_empty()) else {
        return Err(format!("no answer to the signatures request, {}", exit_text(status)));
    };
    let json: Json = serde_json::from_str(line).map_err(|e| format!("an answer that is not JSON ({e})"))?;
    if let Some(message) = json.get(ERROR_KEY) {
        return Err(message.as_str().map_or_else(|| message.to_string(), str::to_string));
    }
    let answer: Signatures =
        serde_json::from_value(json).map_err(|e| format!("an invalid answer to the signatures request ({e})"))?;
    answer.signatures.iter().map(|s| s.to_signature()).collect()
}

/// Defines the commands of every registered plugin. The shell calls it at startup. It returns
/// a warning for every plugin or command that could not be loaded, and loads the rest.
pub fn load_registered(engine: &Engine) -> Vec<String> {
    let Some(file) = registry_path(engine) else { return vec![] };
    let registry = match Registry::load(&file) {
        Ok(registry) => registry,
        Err(e) => return vec![e],
    };
    let mut warnings = Vec::new();
    for entry in &registry.plugins {
        let signatures = match query_signatures(engine, &entry.path) {
            Ok(signatures) => signatures,
            Err(e) => {
                warnings.push(format!("plugin {} was not loaded, {e}", entry.name));
                continue;
            }
        };
        for signature in signatures {
            if engine.is_command(&signature.name) && !engine.is_defined(&signature.name) {
                warnings.push(format!("plugin {} cannot replace the builtin {}", entry.name, signature.name));
                continue;
            }
            engine.define(Arc::new(PluginCommand::new(&entry.name, &entry.path, signature)));
        }
    }
    warnings
}
