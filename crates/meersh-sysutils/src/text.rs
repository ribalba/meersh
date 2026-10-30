use std::collections::VecDeque;
use std::fs;
use std::io::Read;

use meersh_engine::util::expand_path;
use meersh_engine::{Command, Ctx, Effect, Engine, PipelineData, Result, Signature};
use meersh_value::{Record, Type, Value};

use crate::{arg_paths, reason, record, record_path, table};

pub fn register(engine: &mut Engine) {
    engine.register(Cat);
    engine.register(Head);
    engine.register(Tail);
    engine.register(Wc);
}

/// Text or files, whichever was piped in. Records name files. Strings, binary and the raw
/// output of external programs are content.
#[derive(Default)]
struct Piped {
    content: Option<Vec<u8>>,
    files: Vec<String>,
}

impl Piped {
    fn read(ctx: &Ctx, input: PipelineData) -> Result<Self> {
        let mut piped = Piped::default();
        if let PipelineData::Bytes(bytes) = input {
            let mut content = Vec::new();
            bytes.into_reader().read_to_end(&mut content)?;
            piped.content = Some(content);
            return Ok(piped);
        }
        match input.into_value()? {
            Value::Nothing => {}
            Value::List(items) => {
                for item in items {
                    match item {
                        Value::Record(r) => piped.files.push(record_path(ctx, &r)?),
                        // Items of a list of strings are lines.
                        Value::String(line) => {
                            let content = piped.content.get_or_insert_with(Vec::new);
                            content.extend_from_slice(line.as_bytes());
                            content.push(b'\n');
                        }
                        other => return Err(ctx.err(format!("cannot read a {} as text or a file", other.type_name()))),
                    }
                }
            }
            value => piped.push(ctx, value)?,
        }
        Ok(piped)
    }

    fn push(&mut self, ctx: &Ctx, value: Value) -> Result<()> {
        let bytes = match value {
            Value::Record(r) => {
                self.files.push(record_path(ctx, &r)?);
                return Ok(());
            }
            Value::String(s) => s.into_bytes(),
            Value::Binary(b) => b,
            other => return Err(ctx.err(format!("cannot read a {} as text or a file", other.type_name()))),
        };
        self.content.get_or_insert_with(Vec::new).extend(bytes);
        Ok(())
    }
}

fn read_file(ctx: &Ctx, raw: &str) -> Result<Vec<u8>> {
    fs::read(expand_path(raw)).map_err(|e| ctx.err(format!("cannot read '{raw}' ({})", reason(&e))))
}

fn decode(bytes: Vec<u8>) -> Value {
    match String::from_utf8(bytes) {
        Ok(text) => Value::String(text),
        Err(e) => Value::Binary(e.into_bytes()),
    }
}

struct Cat;

impl Command for Cat {
    fn signature(&self) -> Signature {
        Signature::build("cat")
            .desc(
                "Concatenate files into one string, or binary when the bytes are not valid UTF-8. Piped fs.entry \
                 records are read as files. Piped text and the output of external programs pass through, as with \
                 Unix cat.",
            )
            .input(Type::OneOf(vec![Type::Nothing, Type::table("fs.entry"), Type::String, Type::Binary]))
            .output(Type::OneOf(vec![Type::String, Type::Binary]))
            .rest("paths", Type::String, "files or glob patterns to read in this order")
            .effect(Effect::Read)
            .example("cat notes.md todo.md | lines | length", "count the lines of two files together")
            .example("find src --name *.rs | cat | lines | where $it =~ TODO", "TODO lines in all Rust files")
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let piped = Piped::read(ctx, input)?;
        let mut files = piped.files;
        files.extend(arg_paths(ctx, 0)?);
        if piped.content.is_none() && files.is_empty() {
            return Err(ctx.err("nothing to read. Give paths or pipe in rows from find or ls"));
        }
        let mut bytes = piped.content.unwrap_or_default();
        for raw in &files {
            bytes.extend(read_file(ctx, raw)?);
        }
        Ok(PipelineData::Value(decode(bytes)))
    }
}

fn count_flag(ctx: &Ctx) -> Result<usize> {
    match ctx.named("lines")? {
        None => Ok(10),
        Some(v) => v
            .as_int()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| ctx.err("-n needs a whole number of zero or more")),
    }
}

fn text_input() -> Type {
    Type::OneOf(vec![Type::String, Type::Binary, Type::list(Type::Any)])
}

/// Lines with their line ends, so that taking all of them gives back the input.
fn split_lines(bytes: &[u8]) -> Vec<&[u8]> {
    bytes.split_inclusive(|b| *b == b'\n').collect()
}

fn not_text(ctx: &Ctx, value: &Value) -> Result<PipelineData> {
    Err(ctx.err(format!("works on text, binary or lists, not on a {}", value.type_name())))
}

struct Head;

impl Command for Head {
    fn signature(&self) -> Signature {
        Signature::build("head")
            .desc(
                "The first n lines of text or binary, or the first n items of a list or stream, 10 by default. \
                 Output of external programs counts as lines and stops being read after n of them.",
            )
            .input(text_input())
            .output(text_input())
            .named("lines", Type::Int, Some('n'), "how many lines or items")
            .effect(Effect::Pure)
            .example("cat server.log | head -n 5", "the first five lines of a log")
            .example("find --type f | head -n 3", "three files, without walking the whole tree")
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let n = count_flag(ctx)?;
        Ok(match input {
            PipelineData::Empty | PipelineData::Value(Value::Nothing) => PipelineData::Empty,
            PipelineData::Value(Value::String(s)) => {
                PipelineData::Value(Value::String(s.split_inclusive('\n').take(n).collect()))
            }
            PipelineData::Value(Value::Binary(b)) => {
                PipelineData::Value(Value::Binary(split_lines(&b).into_iter().take(n).flatten().copied().collect()))
            }
            PipelineData::Value(Value::List(items)) => {
                PipelineData::Value(Value::List(items.into_iter().take(n).collect()))
            }
            PipelineData::Value(other) => return not_text(ctx, &other),
            // `take` stops pulling, so `^yes | head` ends instead of reading forever.
            lazy => PipelineData::stream(lazy.into_stream().take(n)),
        })
    }
}

struct Tail;

impl Command for Tail {
    fn signature(&self) -> Signature {
        Signature::build("tail")
            .desc(
                "The last n lines of text or binary, or the last n items of a list or stream, 10 by default. \
                 Output of external programs counts as lines.",
            )
            .input(text_input())
            .output(text_input())
            .named("lines", Type::Int, Some('n'), "how many lines or items")
            .effect(Effect::Pure)
            .example("cat server.log | tail -n 20", "the last twenty lines of a log")
            .example("ls | sort-by modified | tail -n 3", "the three most recently changed files")
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let n = count_flag(ctx)?;
        Ok(match input {
            PipelineData::Empty | PipelineData::Value(Value::Nothing) => PipelineData::Empty,
            PipelineData::Value(Value::String(s)) => {
                let lines: Vec<&str> = s.split_inclusive('\n').collect();
                PipelineData::Value(Value::String(lines[lines.len().saturating_sub(n)..].concat()))
            }
            PipelineData::Value(Value::Binary(b)) => {
                let lines = split_lines(&b);
                PipelineData::Value(Value::Binary(lines[lines.len().saturating_sub(n)..].concat()))
            }
            PipelineData::Value(Value::List(mut items)) => {
                let last = items.split_off(items.len().saturating_sub(n));
                PipelineData::Value(Value::List(last))
            }
            PipelineData::Value(other) => return not_text(ctx, &other),
            lazy => {
                let mut last = VecDeque::with_capacity(n.min(4096));
                for item in lazy.into_stream() {
                    let item = item?;
                    if n == 0 {
                        continue;
                    }
                    if last.len() == n {
                        last.pop_front();
                    }
                    last.push_back(item);
                }
                PipelineData::Value(Value::List(last.into()))
            }
        })
    }
}

/// Counts like `wc`. Lines are newline characters. Words are runs of non whitespace.
/// Characters are UTF-8 code points, so every byte that does not continue a sequence counts.
fn count(bytes: &[u8]) -> Record {
    let lines = bytes.iter().filter(|b| **b == b'\n').count();
    let words = match std::str::from_utf8(bytes) {
        Ok(text) => text.split_whitespace().count(),
        Err(_) => bytes.split(u8::is_ascii_whitespace).filter(|w| !w.is_empty()).count(),
    };
    let chars = bytes.iter().filter(|b| (**b & 0xC0) != 0x80).count();
    Record::typed("text.count")
        .with("lines", lines as i64)
        .with("words", words as i64)
        .with("bytes", bytes.len() as i64)
        .with("chars", chars as i64)
}

struct Wc;

impl Command for Wc {
    fn signature(&self) -> Signature {
        Signature::build("wc")
            .desc(
                "Count lines, words, bytes and characters. Piped text gives one text.count record. Files given as \
                 arguments or piped as fs.entry records give one row per file with its path. It only reads files \
                 when it is given some, and it is pure on piped text.",
            )
            .input(Type::OneOf(vec![Type::String, Type::Binary, Type::table("fs.entry")]))
            .output(Type::OneOf(vec![Type::record("text.count"), Type::table("text.count")]))
            .rest("paths", Type::String, "files or glob patterns to count instead of the input")
            .effect(Effect::Read)
            .example("open README.md | wc | get words", "the number of words in the README")
            .example("wc src/*.rs | sort-by lines -r | first 3", "the three longest source files")
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let piped = Piped::read(ctx, input)?;
        let mut files = piped.files;
        files.extend(arg_paths(ctx, 0)?);
        match (piped.content, files.is_empty()) {
            (Some(bytes), true) => Ok(record(count(&bytes))),
            (None, false) => {
                let mut rows = Vec::new();
                for raw in files {
                    let counted = count(&read_file(ctx, &raw)?);
                    let mut row = Record::typed("text.count").with("path", raw);
                    row.fields.extend(counted.fields);
                    rows.push(row);
                }
                Ok(table(rows))
            }
            (None, true) => Err(ctx.err("nothing to count. Pipe in text or give files")),
            (Some(_), false) => Err(ctx.err("give either piped text or files, not both")),
        }
    }
}
