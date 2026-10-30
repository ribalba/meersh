//! `plugin add`, `plugin list` and `plugin remove`.

use std::path::PathBuf;
use std::sync::Arc;

use meersh_engine::authority::absolute;
use meersh_engine::util::expand_path;
use meersh_engine::value::{Record, Type, Value};
use meersh_engine::{Cap, Command, Ctx, Effect, Engine, Permit, PipelineData, Result, Signature};

use crate::host::{PluginCommand, signatures_in};
use crate::protocol::PLUGIN_FLAG;
use crate::registry::{ENV_VAR, Entry, Registry, plugin_name, registry_path};

pub fn register(engine: &mut Engine) {
    engine.register(PluginAdd);
    engine.register(PluginList);
    engine.register(PluginRemove);
}

fn registry_file(ctx: &Ctx) -> Result<PathBuf> {
    registry_path(ctx.engine)
        .ok_or_else(|| ctx.err(format!("there is no data directory for the plugin registry, set {ENV_VAR}")))
}

fn load(ctx: &Ctx) -> Result<(PathBuf, Registry)> {
    let file = registry_file(ctx)?;
    let registry = Registry::load(&file).map_err(|e| ctx.err(e))?;
    Ok((file, registry))
}

fn row(entry: &Entry) -> Record {
    Record::typed("plugin")
        .with("plugin", entry.name.as_str())
        .with("path", entry.path.display().to_string())
        .with("commands", Value::List(entry.commands.iter().map(|c| Value::from(c.as_str())).collect()))
}

fn record(record: Record) -> PipelineData {
    PipelineData::Value(Value::Record(record))
}

/// An absolute path for the plugin. A bare name that is not a file here is looked up on PATH,
/// as a shell would.
fn locate(ctx: &Ctx, raw: &str) -> Option<PathBuf> {
    let path = expand_path(raw);
    if path.is_file() {
        return Some(absolute(&path));
    }
    if raw.contains('/') {
        return None;
    }
    let paths = ctx.engine.env_vars().remove("PATH")?;
    std::env::split_paths(&paths).map(|dir| dir.join(raw)).find(|p| p.is_file())
}

struct PluginAdd;

impl Command for PluginAdd {
    fn signature(&self) -> Signature {
        Signature::build("plugin add")
            .desc(
                "Add the commands of a plugin executable and register it, so the shell loads it at every start. \
                 Adding a plugin again updates its commands.",
            )
            .input(Type::Nothing)
            .output(Type::record("plugin"))
            .required("path", Type::String, "the plugin executable, a path or a name on PATH")
            .effect(Effect::Write)
            .example("plugin add ./target/debug/meersh_plugin_example", "adds words and stamp")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let raw = ctx.req_string(0, "path")?;
        let path = locate(ctx, &raw).ok_or_else(|| ctx.err(format!("no plugin executable at '{raw}'")))?;
        let name = plugin_name(&path);
        let planned = Entry { name: name.clone(), path: path.clone(), commands: vec![] };
        let cap = || Cap::Exec { program: path.display().to_string(), args: vec![PLUGIN_FLAG.into()] };
        match ctx.authorize(cap())? {
            Permit::Execute => {}
            Permit::Plan => return Ok(record(row(&planned).with("action", "would add"))),
            Permit::Skip(reason) => {
                return Ok(record(row(&planned).with("action", "skipped").with("because", reason)));
            }
        }
        let signatures = signatures_in(ctx.engine, Some(ctx.stack), &path)
            .map_err(|e| ctx.err(format!("cannot add {} ({e})", path.display())))?;
        ctx.performed(cap())?;

        let (file, mut registry) = load(ctx)?;
        let old = registry.position(&name, Some(&path));
        for signature in &signatures {
            let command = &signature.name;
            let other =
                registry.plugins.iter().enumerate().find(|(i, e)| Some(*i) != old && e.commands.contains(command));
            if let Some((_, other)) = other {
                return Err(ctx.err(format!("{command} is already provided by plugin {}", other.name)));
            }
            let own = old.is_some_and(|i| registry.plugins[i].commands.contains(command));
            if ctx.engine.is_command(command) && !own {
                return Err(ctx.err(format!("{command} is already a command, plugin {name} cannot replace it")));
            }
        }
        let entry = Entry { name, path, commands: signatures.iter().map(|s| s.name.clone()).collect() };
        let replaced = match old {
            Some(i) => Some(std::mem::replace(&mut registry.plugins[i], entry.clone())),
            None => {
                registry.plugins.push(entry.clone());
                None
            }
        };
        registry.save(&file).map_err(|e| ctx.err(e))?;
        for command in replaced.iter().flat_map(|old| &old.commands) {
            ctx.engine.undefine(command);
        }
        for signature in signatures {
            ctx.engine.define(Arc::new(PluginCommand::new(&entry.name, &entry.path, signature)));
        }
        let action = if replaced.is_some() { "updated" } else { "added" };
        Ok(record(row(&entry).with("action", action)))
    }
}

struct PluginList;

impl Command for PluginList {
    fn signature(&self) -> Signature {
        Signature::build("plugin list")
            .desc("The registered plugins with their paths and the commands they add.")
            .input(Type::Nothing)
            .output(Type::table("plugin"))
            .effect(Effect::Read)
            .example("plugin list | get commands", "every command that comes from a plugin")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let (_, registry) = load(ctx)?;
        Ok(PipelineData::Value(Value::List(registry.plugins.iter().map(|e| Value::Record(row(e))).collect())))
    }
}

struct PluginRemove;

impl Command for PluginRemove {
    fn signature(&self) -> Signature {
        Signature::build("plugin remove")
            .desc("Remove the commands of a plugin from the shell and the plugin from the registry.")
            .input(Type::Nothing)
            .output(Type::record("plugin"))
            .required("name", Type::String, "the plugin name that `plugin list` shows, or its path")
            .effect(Effect::Write)
            .example("plugin remove example", "words and stamp are gone")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let name = ctx.req_string(0, "name")?;
        let (file, mut registry) = load(ctx)?;
        let path = absolute(&expand_path(&name));
        let index = registry.position(&name, Some(&path)).ok_or_else(|| ctx.err(format!("no plugin named {name}")))?;
        let entry = registry.plugins.remove(index);
        registry.save(&file).map_err(|e| ctx.err(e))?;
        for command in &entry.commands {
            ctx.engine.undefine(command);
        }
        Ok(record(row(&entry).with("action", "removed")))
    }
}
