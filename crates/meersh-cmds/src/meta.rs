use meersh_engine::value::{Record, Type, Value};
use meersh_engine::{Command, Ctx, Effect, Engine, PipelineData, Result, Signature};

pub fn register(engine: &mut Engine) {
    engine.register(Help);
    engine.register(Describe);
    engine.register(Echo);
    engine.register(DateNow);
}

struct Help;

impl Command for Help {
    fn signature(&self) -> Signature {
        Signature::build("help")
            .desc("List all commands as a table, or show one command in detail. Help is data too.")
            .input(Type::Nothing)
            .output(Type::table("command"))
            .rest("command", Type::String, "a command name such as `sort-by` or `llm map`")
            .example("help | where effect == destructive", "commands that delete or kill")
            .example("help llm map", "details of one command")
            .example("help fs.entry", "fields and methods of a record type")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let name = ctx.rest_strings(0)?.join(" ");
        if name.is_empty() {
            let rows = ctx
                .engine
                .signatures()
                .iter()
                .map(|s| {
                    Value::Record(
                        Record::typed("command")
                            .with("name", s.name.as_str())
                            .with("input", s.input.to_string())
                            .with("output", s.output.to_string())
                            .with("effect", s.effect.as_str())
                            .with("description", s.description.as_str()),
                    )
                })
                .collect();
            return Ok(PipelineData::Value(Value::List(rows)));
        }
        if ctx.engine.signature(&name).is_none()
            && let Some(info) = ctx.engine.type_info(&name)
        {
            return Ok(PipelineData::Value(type_help(ctx, info)));
        }
        let s = ctx.engine.signature(&name).ok_or_else(|| ctx.err(format!("no command or type named '{name}'")))?;
        let params = s
            .params
            .iter()
            .chain(&s.rest)
            .map(|p| {
                Value::Record(
                    Record::new()
                        .with("name", p.name.as_str())
                        .with("type", p.ty.to_string())
                        .with("description", p.desc.as_str()),
                )
            })
            .collect();
        let flags = s
            .flags
            .iter()
            .map(|f| {
                Value::Record(
                    Record::new()
                        .with("flag", format!("--{}", f.long))
                        .with("short", f.short.map(|c| format!("-{c}")))
                        .with("value", f.arg.as_ref().map(ToString::to_string))
                        .with("description", f.desc.as_str()),
                )
            })
            .collect();
        let examples = s
            .examples
            .iter()
            .map(|(e, d)| Value::Record(Record::new().with("example", e.as_str()).with("description", d.as_str())))
            .collect();
        let detail = Record::typed("command")
            .with("name", s.name.as_str())
            .with("usage", s.usage())
            .with("description", s.description.as_str())
            .with("input", s.input.to_string())
            .with("output", s.output.to_string())
            .with("effect", s.effect.as_str())
            .with("params", Value::List(params))
            .with("flags", Value::List(flags))
            .with("examples", Value::List(examples));
        Ok(PipelineData::Value(Value::Record(detail)))
    }
}

/// `help fs.entry`. A type is its fields and its methods, and every method shows the
/// capability it needs.
fn type_help(ctx: &Ctx, info: &meersh_engine::TypeInfo) -> Value {
    let fields = info
        .fields
        .iter()
        .map(|(name, ty, desc)| {
            Value::Record(
                Record::new()
                    .with("field", name.as_str())
                    .with("type", ty.to_string())
                    .with("description", desc.as_str()),
            )
        })
        .collect();
    let methods = ctx
        .engine
        .methods_of(&info.name)
        .iter()
        .map(|s| {
            let method = s.name.rsplit('.').next().unwrap_or_default().to_string();
            let usage = s.usage().replacen(&s.name, &format!("$it.{method}"), 1);
            Value::Record(
                Record::new()
                    .with("method", method)
                    .with("usage", usage)
                    .with("effect", s.effect.as_str())
                    .with("description", s.description.as_str()),
            )
        })
        .collect();
    Value::Record(
        Record::typed("type")
            .with("type", info.name.as_str())
            .with("description", info.description.as_str())
            .with("fields", Value::List(fields))
            .with("methods", Value::List(methods)),
    )
}

struct Describe;

impl Command for Describe {
    fn signature(&self) -> Signature {
        Signature::build("describe")
            .desc("Show the type of the input, including record types and columns.")
            .input(Type::Any)
            .output(Type::String)
            .example("ls | describe", "table<fs.entry>{name: string, type: string, ...}")
    }

    fn run(&self, _ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        Ok(PipelineData::Value(Value::String(input.into_value()?.ty().to_string())))
    }
}

struct Echo;

impl Command for Echo {
    fn signature(&self) -> Signature {
        Signature::build("echo")
            .desc("Return the arguments as values. One argument gives a value, several give a list.")
            .input(Type::Nothing)
            .output(Type::Any)
            .rest("values", Type::Any, "values to return")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let mut values: Vec<Value> = ctx.call.positional.iter().map(|e| ctx.eval(e)).collect::<Result<_>>()?;
        Ok(PipelineData::Value(if values.len() == 1 { values.remove(0) } else { Value::List(values) }))
    }
}

struct DateNow;

impl Command for DateNow {
    fn signature(&self) -> Signature {
        Signature::build("date now")
            .desc("The current local date and time.")
            .input(Type::Nothing)
            .output(Type::Date)
            .effect(Effect::Read)
            .example("ls | where modified > ((date now) - 1day)", "files changed in the last day")
    }

    fn run(&self, _ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        Ok(PipelineData::Value(Value::Date(chrono::Local::now().fixed_offset())))
    }
}
