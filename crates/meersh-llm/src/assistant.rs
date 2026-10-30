//! The assistant that knows meersh. `?`, `ai` and `llm` without piped input go through it.
//!
//! It answers with meersh pipelines, not bash. The prompt has two parts. The system part is
//! the same for every request (the syntax, the record types and a fixed set of core
//! commands), so a local model can reuse its cache for it. Only the user part changes. It
//! holds the commands that look relevant to the request, the context and the request. The
//! answer is constrained to JSON, which also keeps reasoning models from rambling, and every
//! pipeline is checked before it is shown. A name that is neither a builtin nor an installed
//! program sends it back to the model once, with the error.

use std::sync::Arc;

use meersh_engine::parser::Statement;
use meersh_engine::value::{Record, Value};
use meersh_engine::{Ctx, Effect, Engine, Result, Signature};
use serde_json::json;

use crate::LlmRuntime;
use crate::backend::Request;

const PRIMER: &str = "You are the assistant built into meersh, an object shell for Linux. Its commands pass \
typed records instead of text. When the user asks for a command, answer with a meersh pipeline, never with bash, \
unless bash is asked for explicitly.
Syntax:
- Stages are separated by |. Flags look like --reverse or -r.
- where keeps the rows for which a condition over the columns holds, for example `where size > 10mb and type == file`. \
Operators are == != < <= > >= =~ (regex) in and or not, always with spaces around them.
- Sizes are written 10kb 5mb 2gib. Durations are written 30sec 10min 2hr 3day 2wk 6mo 1yr. `(date now) - 6mo` is \
the date six months ago.
- A block { ... } runs once per row with the row in $it, for example `each { $it.name }`. Records look like {a: 1}, \
lists like [a b].
- External programs need a ^ in front, for example `^git log`. Their output arrives as lines of text. Prefer \
builtins, they return records.
Values have types such as int, float, string, bool, filesize (a byte count, written 10kb), duration (written \
3day), date, list, record and table.
Records:
- ls and find return fs.entry with name, type (file, dir or symlink), size and modified.
- ps returns proc with pid, ppid, name, user, cpu, mem, status and command.
Examples:
- directories without changes for six months: `ls | where type == dir and modified < ((date now) - 6mo)`
- the same in all subdirectories: `find . --type d | where modified < ((date now) - 6mo)`
- the five largest files: `ls | where type == file | sort-by size -r | first 5`
- processes above 10 percent CPU: `ps | where cpu > 10 | select pid name cpu`
- Rust files changed today: `find . --name '*.rs' | where modified > ((date now) - 1day)`";

/// Always in the stable part of the prompt, in this order.
const CORE: [&str; 15] = [
    "ls",
    "find",
    "ps",
    "where",
    "sort-by",
    "first",
    "select",
    "get",
    "each",
    "length",
    "group-by",
    "open",
    "du",
    "rm",
    "llm where",
];
const RELEVANT: usize = 8;

fn describe(sig: &Signature) -> String {
    let mut line = format!("- {}: {}", sig.usage(), sig.description);
    if let Some((example, _)) = sig.examples.first() {
        line.push_str(&format!("\n  e.g. {example}"));
    }
    line
}

fn system_prompt(engine: &Engine) -> String {
    let core: Vec<String> = CORE.iter().filter_map(|name| engine.signature(name)).map(|s| describe(&s)).collect();
    format!("{PRIMER}\nCore commands:\n{}", core.join("\n"))
}

/// Commands outside the core whose name, description or examples share words with the request.
fn relevant(engine: &Engine, request: &str) -> String {
    let words: Vec<String> =
        request.split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() > 2).map(str::to_lowercase).collect();
    let mut scored: Vec<(usize, Arc<Signature>)> = engine
        .signatures()
        .into_iter()
        .filter(|s| !CORE.contains(&s.name.as_str()) && !s.name.contains('.') && s.name != "ai")
        .filter_map(|s| {
            let examples: Vec<&str> = s.examples.iter().map(|e| e.0.as_str()).collect();
            let text = format!("{} {} {}", s.name, s.description, examples.join(" ")).to_lowercase();
            let score = words.iter().filter(|w| text.contains(w.as_str())).count();
            (score > 0).then_some((score, s))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
    scored.into_iter().take(RELEVANT).map(|(_, s)| describe(&s)).collect::<Vec<_>>().join("\n")
}

fn on_path(program: &str) -> bool {
    if program.contains('/') {
        return std::path::Path::new(program).is_file();
    }
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

/// Parses the pipeline and checks that every command exists. Returns its effect.
fn check(engine: &Engine, pipeline: &str) -> std::result::Result<Effect, String> {
    let script = engine.parse(pipeline).map_err(|e| e.to_string())?;
    let mut effect = Effect::Pure;
    let mut problem = None;
    for statement in &script.statements {
        let (Statement::Pipeline(p) | Statement::Background(p) | Statement::Let { pipeline: p, .. }) = statement;
        effect = effect.max(engine.effect_of(p));
        p.walk_commands(&mut |call| {
            if problem.is_none() && call.external && !on_path(&call.name) {
                problem = Some(format!("`{}` is neither a meersh command nor an installed program", call.name));
            }
        });
    }
    problem.map_or(Ok(effect), Err)
}

/// `--backend` wins, then the `assistant` setting, then the default backend.
fn ask_assistant(ctx: &Ctx, runtime: &LlmRuntime, request: &Request) -> Result<crate::Response> {
    let backend = ctx.named_string("backend")?.or_else(|| runtime.config().assistant);
    let (name, local) = runtime.locality(backend.as_deref());
    ctx.check_now(meersh_engine::Cap::LlmSend { backend: name, local })?;
    let model = ctx.named_string("model")?;
    runtime.complete(request, backend.as_deref(), model.as_deref()).map_err(|e| ctx.err(e))
}

/// How the assistant should answer.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Want {
    /// Always a pipeline, for `?` and `ai`.
    Pipeline,
    /// A pipeline when the request asks for one, otherwise a text answer, for `llm` without input.
    Answer,
}

/// Asks the assistant. A pipeline comes back as an `ai.pipeline` record, a plain answer as a string.
pub fn assist(ctx: &Ctx, runtime: &LlmRuntime, request_text: &str, want: Want) -> Result<Value> {
    let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
    let previous = ctx.stack.get("_").map_or_else(|| "nothing".to_string(), |v| v.ty().to_string());
    let extra = relevant(ctx.engine, request_text);
    let context = format!(
        "More commands that may help:\n{extra}\n\nWorking directory: {cwd}\nType of $_ (the previous result): {previous}"
    );
    let (schema, task) = match want {
        Want::Pipeline => (
            json!({
                "type": "object",
                "properties": {"pipeline": {"type": "string"}, "explanation": {"type": "string"}},
                "required": ["pipeline", "explanation"],
                "additionalProperties": false,
            }),
            "Write exactly one meersh pipeline for this request and say in explanation, in one sentence, what it \
             does. Do not repeat the pipeline there.",
        ),
        Want::Answer => (
            json!({
                "type": "object",
                "properties": {"answer": {"type": "string"}, "pipeline": {"type": "string"}},
                "required": ["answer", "pipeline"],
                "additionalProperties": false,
            }),
            "Answer briefly. If the request asks for a command or for something the shell can do, put one meersh \
             pipeline into pipeline and say in answer, in one sentence, what it does. Do not repeat the pipeline in \
             answer. Otherwise leave pipeline empty.",
        ),
    };
    let system = system_prompt(ctx.engine);
    let mut prompt = format!("{context}\n\n{task}\nRequest: {request_text}");
    for attempt in 0..2 {
        let request = Request { system: system.clone(), prompt: prompt.clone(), schema: Some(schema.clone()) };
        let json = ask_assistant(ctx, runtime, &request)?.json.unwrap_or_default();
        let pipeline = json["pipeline"].as_str().unwrap_or_default().trim().trim_matches('`').to_string();
        let mut explanation =
            json["explanation"].as_str().or(json["answer"].as_str()).unwrap_or_default().trim().to_string();
        // Small models like to repeat the pipeline or the request instead of explaining.
        if !pipeline.is_empty()
            && (explanation.trim_matches('`') == pipeline || explanation.eq_ignore_ascii_case(request_text.trim()))
        {
            explanation.clear();
        }
        if pipeline.is_empty() && want == Want::Answer {
            return Ok(Value::String(explanation));
        }
        match check(ctx.engine, &pipeline) {
            Ok(effect) => {
                let out = Record::typed("ai.pipeline")
                    .with("pipeline", pipeline)
                    .with("explanation", explanation)
                    .with("effect", effect.as_str());
                return Ok(Value::Record(out));
            }
            Err(problem) if attempt == 0 => {
                prompt = format!(
                    "{context}\n\n{task}\nRequest: {request_text}\n\nYour previous pipeline `{pipeline}` was rejected: \
                     {problem}. Use only the listed meersh commands or installed programs with ^."
                );
            }
            Err(problem) => {
                return Err(ctx.err(format!("the model wrote `{pipeline}`, which does not work: {problem}")));
            }
        }
    }
    unreachable!("the loop returns on its second attempt")
}
