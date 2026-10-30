//! Commands that shape the session, such as definitions, aliases, the environment and sourcing scripts.

use std::sync::Arc;

use meersh_engine::parser::{Expr, Pipeline};
use meersh_engine::value::{Record, Type, Value};
use meersh_engine::{Command, Ctx, Effect, Engine, PipelineData, Result, Signature};

use crate::authority::humans_only;

pub fn register(engine: &mut Engine) {
    engine.register(Def);
    engine.register(Alias);
    engine.register(Unalias);
    engine.register(Env);
    engine.register(EnvSet);
    engine.register(EnvUnset);
    engine.register(Source);
}

/// A command written in meersh with `def`.
struct Custom {
    signature: Signature,
    /// Name, type and whether the parameter is a switch.
    params: Vec<(String, Type, bool)>,
    body: Pipeline,
}

impl Command for Custom {
    fn signature(&self) -> Signature {
        self.signature.clone()
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let mut stack = ctx.stack.clone();
        let mut position = 0;
        for (name, ty, switch) in &self.params {
            let value = if *switch {
                Value::Bool(ctx.switch(name))
            } else {
                let value = ctx.arg(position)?.unwrap_or(Value::Nothing);
                position += 1;
                check_type(ctx, name, ty, value)?
            };
            stack.set(name, value);
        }
        ctx.engine.run_pipeline(&stack, &self.body, input, ctx.to_terminal)
    }
}

/// Parameters of `def` are typed, and a mismatch is an error before the body runs.
fn check_type(ctx: &Ctx, name: &str, ty: &Type, value: Value) -> Result<Value> {
    let ok = match (ty, &value) {
        (Type::Any, _) | (_, Value::Nothing) => true,
        (Type::Int, Value::Int(_)) | (Type::Float | Type::Number, Value::Int(_) | Value::Float(_)) => true,
        (Type::Filesize, Value::Filesize(_)) | (Type::Duration, Value::Duration(_)) | (Type::Bool, Value::Bool(_)) => {
            true
        }
        (Type::Date, Value::Date(_)) | (Type::List(_), Value::List(_)) | (Type::Record { .. }, Value::Record(_)) => {
            true
        }
        (Type::String, _) => return Ok(Value::String(value.to_text())),
        _ => false,
    };
    if ok { Ok(value) } else { Err(ctx.err(format!("parameter {name} expects {ty}, got {}", value.type_name()))) }
}

struct Def;

impl Command for Def {
    fn signature(&self) -> Signature {
        Signature::build("def")
            .desc(
                "Define a command. Parameters are a record of name and type. The type switch makes a flag. The \
                 parameters become variables in the body, and the input is the body's input.",
            )
            .input(Type::Nothing)
            .output(Type::Nothing)
            .required("name", Type::String, "command name")
            .required(
                "params",
                Type::Record { name: None, fields: vec![] },
                "for example {path: string, count: int, verbose: switch}",
            )
            .required("body", Type::Block, "the pipeline")
            .named("desc", Type::String, Some('d'), "description for help and for the model")
            .effect(Effect::Write)
            .example(
                "def big {min: filesize} { ls | where size > $min | sort-by size -r }",
                "big 10mb lists large files",
            )
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        humans_only(ctx)?;
        let name = ctx.req_string(0, "name")?;
        if ctx.engine.is_command(&name) && !ctx.engine.is_defined(&name) {
            return Err(ctx.err(format!("{name} is a builtin and cannot be redefined")));
        }
        let params = match ctx.req(1, "params")? {
            Value::Record(r) => r,
            other => return Err(ctx.err(format!("params must be a record, got {}", other.type_name()))),
        };
        let Some(Expr::Block(body)) = ctx.positional(2) else {
            return Err(ctx.err("the body must be a block { ... }"));
        };
        let mut signature = Signature::build(&name)
            .desc(&ctx.named_string("desc")?.unwrap_or_else(|| format!("defined as {{ {body} }}")))
            .effect(ctx.engine.effect_of(body));
        let mut typed = Vec::new();
        for (param, spec) in &params.fields {
            let spec = spec.to_text();
            if spec == "switch" {
                signature = signature.switch(param, None, "");
                typed.push((param.clone(), Type::Bool, true));
                continue;
            }
            let (spec, optional) = spec.strip_suffix('?').map_or((spec.as_str(), false), |s| (s, true));
            let ty = Type::from_name(spec).ok_or_else(|| ctx.err(format!("unknown type '{spec}' for {param}")))?;
            signature = if optional {
                signature.optional(param, ty.clone(), "")
            } else {
                signature.required(param, ty.clone(), "")
            };
            typed.push((param.clone(), ty, false));
        }
        ctx.engine.define(Arc::new(Custom { signature, params: typed, body: body.clone() }));
        Ok(PipelineData::Empty)
    }
}

struct Alias;

impl Command for Alias {
    fn signature(&self) -> Signature {
        Signature::build("alias")
            .desc("Define an alias, or list them without arguments. The expansion replaces the name at the start of a command and may be a whole pipeline.")
            .input(Type::Nothing)
            .output(Type::table("alias"))
            .optional("name", Type::String, "alias name")
            .optional("expansion", Type::String, "what it stands for, as a string")
            .effect(Effect::Write)
            .example("alias ll \"ls -l\"", "ll src lists src with details")
            .example("alias big \"ls | sort-by size -r\"", "big | first 3")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        match (ctx.opt_string(0)?, ctx.opt_string(1)?) {
            (Some(name), Some(expansion)) => {
                humans_only(ctx)?;
                ctx.engine.set_alias(&name, &expansion);
                Ok(PipelineData::Empty)
            }
            (None, _) => Ok(PipelineData::Value(Value::List(
                ctx.engine
                    .aliases()
                    .into_iter()
                    .map(|(name, expansion)| {
                        Value::Record(Record::typed("alias").with("name", name).with("expansion", expansion))
                    })
                    .collect(),
            ))),
            (Some(name), None) => {
                Ok(PipelineData::Value(ctx.engine.alias(&name).map_or(Value::Nothing, Value::String)))
            }
        }
    }
}

struct Unalias;

impl Command for Unalias {
    fn signature(&self) -> Signature {
        Signature::build("unalias")
            .desc("Remove an alias.")
            .input(Type::Nothing)
            .output(Type::Nothing)
            .required("name", Type::String, "alias name")
            .effect(Effect::Write)
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        humans_only(ctx)?;
        let name = ctx.req_string(0, "name")?;
        if !ctx.engine.remove_alias(&name) {
            return Err(ctx.err(format!("no alias named {name}")));
        }
        Ok(PipelineData::Empty)
    }
}

struct Env;

impl Command for Env {
    fn signature(&self) -> Signature {
        Signature::build("env")
            .desc("The environment as a table. External programs see it including changes made with env set.")
            .input(Type::Nothing)
            .output(Type::table("env.var"))
            .example("env | where name =~ '^LC_'", "locale settings")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        Ok(PipelineData::Value(Value::List(
            ctx.engine
                .env_vars()
                .into_iter()
                .map(|(name, value)| Value::Record(Record::typed("env.var").with("name", name).with("value", value)))
                .collect(),
        )))
    }
}

struct EnvSet;

impl Command for EnvSet {
    fn signature(&self) -> Signature {
        Signature::build("env set")
            .desc("Set an environment variable for this session and the programs it starts.")
            .input(Type::Nothing)
            .output(Type::Nothing)
            .required("name", Type::String, "variable name")
            .required("value", Type::String, "new value")
            .effect(Effect::Write)
            .example("env set RUST_LOG debug", "more logging from Rust programs")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        humans_only(ctx)?;
        ctx.engine.set_env(&ctx.req_string(0, "name")?, Some(ctx.req_string(1, "value")?));
        Ok(PipelineData::Empty)
    }
}

struct EnvUnset;

impl Command for EnvUnset {
    fn signature(&self) -> Signature {
        Signature::build("env unset")
            .desc("Remove an environment variable.")
            .input(Type::Nothing)
            .output(Type::Nothing)
            .required("name", Type::String, "variable name")
            .effect(Effect::Write)
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        humans_only(ctx)?;
        ctx.engine.set_env(&ctx.req_string(0, "name")?, None);
        Ok(PipelineData::Empty)
    }
}

struct Source;

impl Command for Source {
    fn signature(&self) -> Signature {
        Signature::build("source")
            .desc("Run a script file. Its definitions, aliases and environment changes stay. Its variables do not.")
            .input(Type::Nothing)
            .output(Type::Any)
            .required("path", Type::String, "script file")
            .effect(Effect::Unknown)
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let path = meersh_engine::util::expand_path(&ctx.req_string(0, "path")?);
        let text = std::fs::read_to_string(&path).map_err(|e| ctx.err(format!("{}: {e}", path.display())))?;
        let mut stack = ctx.stack.clone();
        let mut last = Value::Nothing;
        ctx.engine.run_script(&mut stack, &text, ctx.to_terminal, &mut |data| {
            last = data.into_value()?;
            Ok(())
        })?;
        Ok(PipelineData::Value(last))
    }
}
