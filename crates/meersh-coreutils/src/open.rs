use std::fs;

use chrono::DateTime;
use meersh_engine::util::expand_path;
use meersh_engine::value::{Record, Type, Value};
use meersh_engine::{Command, Ctx, Effect, Engine, PipelineData, Result, Signature};

use crate::reason;

pub fn register(engine: &mut Engine) {
    engine.register(Open);
}

struct Open;

impl Command for Open {
    fn signature(&self) -> Signature {
        Signature::build("open")
            .desc(
                "Read a file. Files ending in .json or .toml are parsed into records and lists. Other files \
                 become a string, or binary when they are not valid UTF-8.",
            )
            .input(Type::Nothing)
            .output(Type::Any)
            .required("path", Type::String, "file to read")
            .switch("raw", Some('r'), "return the text without parsing it")
            .effect(Effect::Read)
            .example("open Cargo.toml | get package.version", "a field of a TOML file")
            .example("open notes.txt | lines | length", "count the lines of a text file")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let raw = ctx.req_string(0, "path")?;
        Ok(PipelineData::Value(read_file(ctx, &raw, ctx.switch("raw"))?))
    }
}

/// The content of a file, parsed by its extension unless `raw` is set. `open` and
/// `fs.entry.open` share it.
pub(crate) fn read_file(ctx: &Ctx, raw: &str, keep_text: bool) -> Result<Value> {
    let path = expand_path(raw);
    let bytes = fs::read(&path).map_err(|e| ctx.err(format!("cannot open '{raw}' ({})", reason(&e))))?;
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(e) => return Ok(Value::Binary(e.into_bytes())),
    };
    if keep_text {
        return Ok(Value::String(text));
    }
    let extension = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    Ok(match extension.as_deref() {
        Some("json") => serde_json::from_str(&text)
            .map(|json| Value::from_json(&json))
            .map_err(|e| ctx.err(format!("'{raw}' is not valid JSON ({e})")))?,
        Some("toml") => text
            .parse::<toml::Table>()
            .map(toml_table)
            .map_err(|e| ctx.err(format!("'{raw}' is not valid TOML ({})", e.message())))?,
        _ => Value::String(text),
    })
}

fn toml_table(table: toml::Table) -> Value {
    let mut record = Record::new();
    for (key, value) in table {
        record.insert(key, toml_value(value));
    }
    Value::Record(record)
}

/// TOML datetimes with an offset become dates. Local dates and times have no meersh
/// counterpart and stay strings.
fn toml_value(value: toml::Value) -> Value {
    match value {
        toml::Value::String(s) => Value::String(s),
        toml::Value::Integer(i) => Value::Int(i),
        toml::Value::Float(f) => Value::Float(f),
        toml::Value::Boolean(b) => Value::Bool(b),
        toml::Value::Datetime(d) => {
            let text = d.to_string();
            DateTime::parse_from_rfc3339(&text).map_or(Value::String(text), Value::Date)
        }
        toml::Value::Array(items) => Value::List(items.into_iter().map(toml_value).collect()),
        toml::Value::Table(table) => toml_table(table),
    }
}
