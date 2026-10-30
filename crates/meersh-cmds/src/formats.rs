use std::io::Write;

use meersh_engine::render::{RenderOptions, render};
use meersh_engine::util::expand_path;
use meersh_engine::value::{Record, Type, Value};
use meersh_engine::{Cap, Command, Ctx, Effect, Engine, Permit, PipelineData, Result, Signature};

pub fn register(engine: &mut Engine) {
    engine.register(ToJson);
    engine.register(FromJson);
    engine.register(Lines);
    engine.register(Table);
    engine.register(Save);
    engine.register(Edit);
}

struct ToJson;

impl Command for ToJson {
    fn signature(&self) -> Signature {
        Signature::build("to json")
            .desc("Serialize the input as JSON text. Filesizes become bytes and dates RFC 3339 strings.")
            .input(Type::Any)
            .output(Type::String)
            .switch("raw", Some('r'), "compact output on one line")
            .example("ps | first 3 | to json", "processes as JSON")
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let json = input.into_value()?.to_json();
        let text =
            if ctx.switch("raw") { json.to_string() } else { serde_json::to_string_pretty(&json).unwrap_or_default() };
        Ok(PipelineData::Value(Value::String(text)))
    }
}

struct FromJson;

impl Command for FromJson {
    fn signature(&self) -> Signature {
        Signature::build("from json")
            .desc("Parse JSON text into values. Also accepts one JSON document per line.")
            .input(Type::String)
            .output(Type::Any)
            .example(
                "^curl -s https://api.github.com/repos/nushell/nushell | from json | get stargazers_count",
                "a field of an API response",
            )
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let text = match input.into_value()? {
            Value::String(s) => s,
            Value::List(lines) => lines.iter().map(Value::to_text).collect::<Vec<_>>().join("\n"),
            other => return Err(ctx.err(format!("expected text, found {}", other.type_name()))),
        };
        if let Ok(json) = serde_json::from_str(&text) {
            return Ok(PipelineData::Value(Value::from_json(&json)));
        }
        let values = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                serde_json::from_str(l).map(|j| Value::from_json(&j)).map_err(|e| ctx.err(format!("invalid JSON: {e}")))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(PipelineData::Value(Value::List(values)))
    }
}

struct Lines;

impl Command for Lines {
    fn signature(&self) -> Signature {
        Signature::build("lines")
            .desc("Split text into a list of lines. Output of external programs already arrives as lines.")
            .input(Type::String)
            .output(Type::list(Type::String))
            .example("open notes.txt | lines | where $it =~ TODO", "lines that mention TODO")
    }

    fn run(&self, _ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        Ok(PipelineData::stream(input.into_stream().flat_map(|v| {
            let lines: Vec<Result<Value>> = match v {
                Ok(Value::String(s)) => s.lines().map(|l| Ok(Value::String(l.to_string()))).collect(),
                other => vec![other],
            };
            lines
        })))
    }
}

struct Table;

impl Command for Table {
    fn signature(&self) -> Signature {
        Signature::build("table")
            .desc("Render the input as text, the same way the shell shows results.")
            .input(Type::Any)
            .output(Type::String)
            .example("ps | table | ^less -R", "page through a rendered table")
    }

    fn run(&self, _ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        Ok(PipelineData::Value(Value::String(render(&input.into_value()?, &RenderOptions::detect()))))
    }
}

struct Save;

impl Command for Save {
    fn signature(&self) -> Signature {
        Signature::build("save")
            .desc("Write the input to a file. Text is written as is, structured data as JSON.")
            .input(Type::Any)
            .output(Type::record("fs.change"))
            .required("path", Type::String, "target file")
            .switch("force", Some('f'), "overwrite an existing file")
            .effect(Effect::Write)
            .example("ps | where cpu > 5 | save busy.json", "keep a snapshot")
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let path = expand_path(&ctx.req_string(0, "path")?);
        let exists = path.exists();
        if exists && !ctx.switch("force") {
            return Err(ctx.err(format!("{} exists, use --force to overwrite", path.display())));
        }
        // Overwriting keeps a snapshot of the old content in the journal, so it can be undone.
        let cap = || if exists { Cap::FsWrite { path: path.clone() } } else { Cap::FsCreate { path: path.clone() } };
        let change =
            |action: &str| Record::typed("fs.change").with("path", path.display().to_string()).with("action", action);
        match ctx.authorize(cap())? {
            Permit::Execute => {}
            Permit::Plan => return Ok(PipelineData::Value(Value::Record(change("would save")))),
            Permit::Skip(why) => return Ok(PipelineData::Value(Value::Record(change("refused").with("because", why)))),
        }
        let mut file = std::fs::File::create(&path)?;
        match input {
            PipelineData::Bytes(bytes) => {
                std::io::copy(&mut bytes.into_reader(), &mut file)?;
            }
            other => match other.into_value()? {
                Value::String(s) => file.write_all(s.as_bytes())?,
                Value::Binary(b) => file.write_all(&b)?,
                value => {
                    file.write_all(serde_json::to_string_pretty(&value.to_json()).unwrap_or_default().as_bytes())?
                }
            },
        }
        let size = file.metadata()?.len() as i64;
        drop(file);
        ctx.performed(cap())?;
        Ok(PipelineData::Value(Value::Record(change("saved").with("size", Value::Filesize(size)))))
    }
}

struct Edit;

impl Command for Edit {
    fn signature(&self) -> Signature {
        Signature::build("edit")
            .desc(
                "Replace text in a file. The old text must occur exactly once unless --all is given. The edit is an \
                 fs.write, so the old content is kept as a snapshot and `undo` restores it.",
            )
            .input(Type::Nothing)
            .output(Type::record("fs.change"))
            .required("path", Type::String, "the file")
            .required("old", Type::String, "the text to replace")
            .required("new", Type::String, "the replacement")
            .switch("all", Some('a'), "replace every occurrence")
            .effect(Effect::Write)
            .example("edit Cargo.toml 'version = \"0.1.0\"' 'version = \"0.2.0\"'", "bump a version")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let raw = ctx.req_string(0, "path")?;
        let path = expand_path(&raw);
        let (old, new) = (ctx.req_string(1, "old")?, ctx.req_string(2, "new")?);
        if old.is_empty() {
            return Err(ctx.err("the old text is empty"));
        }
        let text = std::fs::read_to_string(&path).map_err(|e| ctx.err(format!("cannot read {raw} ({e})")))?;
        let count = text.matches(old.as_str()).count();
        if count == 0 {
            return Err(ctx.err(format!("the old text does not occur in {raw}")));
        }
        if count > 1 && !ctx.switch("all") {
            return Err(ctx.err(format!("the old text occurs {count} times in {raw}. Give more context or use --all")));
        }
        let change = |action: &str| {
            Record::typed("fs.change")
                .with("path", raw.as_str())
                .with("action", action)
                .with("replacements", count as i64)
        };
        let cap = || Cap::FsWrite { path: path.clone() };
        match ctx.authorize(cap())? {
            Permit::Execute => {}
            Permit::Plan => return Ok(PipelineData::Value(Value::Record(change("would edit")))),
            Permit::Skip(why) => return Ok(PipelineData::Value(Value::Record(change("refused").with("because", why)))),
        }
        let edited = if ctx.switch("all") { text.replace(&old, &new) } else { text.replacen(&old, &new, 1) };
        std::fs::write(&path, edited)?;
        ctx.performed(cap())?;
        Ok(PipelineData::Value(Value::Record(change("edited"))))
    }
}
