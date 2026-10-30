//! Methods of `fs.entry` that belong to the tools of this crate.

use std::io::Read;

use meersh_engine::util::expand_path;
use meersh_engine::value::{Type, Value};
use meersh_engine::{Command, Ctx, Effect, Engine, PipelineData, Result, Signature};
use sha2::{Digest, Sha256};

use crate::files::Chmod;

pub fn register(engine: &mut Engine) {
    engine.register(EntryChmod);
    engine.register(EntryHash);
}

fn one_entry(ctx: &Ctx, input: PipelineData) -> Result<Value> {
    let value = input.into_value()?;
    if value.as_record().is_none() {
        let method = ctx.call.name.rsplit('.').next().unwrap_or_default();
        return Err(ctx.err(format!("takes one record. Use each {{ $it.{method}() }} for a table")));
    }
    ctx.verify(&value)?;
    Ok(value)
}

struct EntryChmod;

impl Command for EntryChmod {
    fn signature(&self) -> Signature {
        Signature::build("fs.entry.chmod")
            .desc("Change the mode of this file or directory, octal or symbolic. The old mode goes into the journal.")
            .input(Type::record("fs.entry"))
            .output(Type::record("fs.change"))
            .required("mode", Type::String, "for example 644, u+x or go-w")
            .effect(Effect::Write)
            .example("ls *.sh | each { $it.chmod(u+x) }", "make scripts executable")
    }

    /// The same as `chmod <mode>` with this entry piped in, but one record out.
    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let entry = one_entry(ctx, input)?;
        let rows = Chmod.run(ctx, PipelineData::Value(entry))?.into_value()?;
        Ok(PipelineData::Value(match rows {
            Value::List(mut items) if items.len() == 1 => items.remove(0),
            other => other,
        }))
    }
}

struct EntryHash;

impl Command for EntryHash {
    fn signature(&self) -> Signature {
        Signature::build("fs.entry.hash")
            .desc("The SHA-256 of the file's content, as hex.")
            .input(Type::record("fs.entry"))
            .output(Type::String)
            .effect(Effect::Read)
            .example(
                "ls | where type == file | insert sha256 { $it.hash() } | group-by sha256 | where count > 1",
                "find duplicate files",
            )
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let entry = one_entry(ctx, input)?;
        let name = entry.follow("name").or_else(|| entry.follow("path")).map(|v| v.to_text()).unwrap_or_default();
        let mut file =
            std::fs::File::open(expand_path(&name)).map_err(|e| ctx.err(format!("cannot read {name} ({e})")))?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
        }
        let hex: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
        Ok(PipelineData::Value(Value::String(hex)))
    }
}
