use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use meersh_engine::util::expand_path;
use meersh_engine::value::{Record, Type, Value};
use meersh_engine::{Command, Ctx, Effect, Engine, PipelineData, Result, Signature};

pub fn register(engine: &mut Engine) {
    engine.register(Which);
}

struct Which;

impl Command for Which {
    fn signature(&self) -> Signature {
        let fields = ["name", "kind", "path"].map(|f| (f.to_string(), Type::String)).to_vec();
        Signature::build("which")
            .desc(
                "Tell whether a name runs a builtin command or an external program. kind is builtin, external or \
                 not found, and path is the program file for externals.",
            )
            .input(Type::Nothing)
            .output(Type::Record { name: None, fields })
            .required("name", Type::String, "command or program name")
            .effect(Effect::Read)
            .example("which git", "where git is installed")
            .example("which where", "where is a builtin, not a program")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let name = ctx.req_string(0, "name")?;
        let (kind, path) = if ctx.engine.is_command(&name) {
            ("builtin", None)
        } else if let Some(path) = find_program(&name) {
            ("external", Some(path.display().to_string()))
        } else {
            ("not found", None)
        };
        let record = Record::new().with("name", name).with("kind", kind).with("path", path);
        Ok(PipelineData::Value(Value::Record(record)))
    }
}

fn find_program(name: &str) -> Option<PathBuf> {
    let executable = |p: &Path| fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
    if name.contains('/') {
        let path = expand_path(name);
        return executable(&path).then_some(path);
    }
    std::env::split_paths(&std::env::var_os("PATH")?).map(|dir| dir.join(name)).find(|p| executable(p))
}
