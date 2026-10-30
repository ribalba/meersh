use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use meersh_engine::value::{Record, Type, Value};
use meersh_engine::{Cap, Command, Ctx, Effect, Engine, PipelineData, Result, Signature, ValueStream};
use serde_json::{Value as Json, json};

use crate::LlmRuntime;
use crate::backend::Request;
use crate::encode::encode;

pub fn register(engine: &mut Engine, runtime: Arc<LlmRuntime>) {
    engine.register(Llm(runtime.clone(), crate::actor::Actor::new(runtime.clone())));
    engine.register(crate::actor::LlmEdit(runtime.clone()));
    engine.register(crate::agent::LlmAgent);
    engine.register(LlmMap(runtime.clone()));
    engine.register(LlmWhere(runtime.clone()));
    engine.register(LlmParse(runtime.clone()));
    engine.register(LlmBackends(runtime.clone()));
    engine.register(LlmUse(runtime.clone()));
    engine.register(Ai(runtime));
}

pub(crate) const STAGE_SYSTEM: &str = "You are one stage in a pipeline of the meersh shell. You receive data from the \
previous stage and an instruction. Apply the instruction to the data. Reply with the result only, without \
preamble, explanation or markdown fences, unless the instruction asks for them.";

pub(crate) fn with_llm_flags(sig: Signature) -> Signature {
    sig.named("backend", Type::String, Some('b'), "backend name from `llm backends`")
        .named("model", Type::String, Some('m'), "override the backend's model")
        .effect(Effect::Llm)
}

/// One model call with the common flags applied.
pub(crate) fn ask(ctx: &Ctx, runtime: &LlmRuntime, request: &Request) -> Result<crate::Response> {
    check_send(ctx, runtime)?;
    let backend = ctx.named_string("backend")?;
    let model = ctx.named_string("model")?;
    runtime.complete(request, backend.as_deref(), model.as_deref()).map_err(|e| ctx.err(e))
}

/// Sending data to a model is a capability too. Under a model principal, data may go to a
/// cloud backend only when a policy rule says so.
pub(crate) fn check_send(ctx: &Ctx, runtime: &LlmRuntime) -> Result<()> {
    let (backend, local) = runtime.locality(ctx.named_string("backend")?.as_deref());
    ctx.check_now(Cap::LlmSend { backend, local })
}

fn schema_flag(ctx: &Ctx) -> Result<Option<Type>> {
    ctx.named("schema")?.map(|spec| Type::from_spec(&spec).map_err(|e| ctx.err(e))).transpose()
}

/// Every backend accepts an object at the root of a schema. Other types are wrapped.
fn object_schema(ty: &Type) -> (Json, bool) {
    match ty {
        Type::Record { .. } => (ty.to_json_schema(), false),
        other => (
            json!({"type": "object", "properties": {"value": other.to_json_schema()}, "required": ["value"], "additionalProperties": false}),
            true,
        ),
    }
}

struct Llm(Arc<LlmRuntime>, crate::actor::Actor);

impl Command for Llm {
    fn signature(&self) -> Signature {
        with_llm_flags(Signature::build("llm"))
            .desc("Apply an instruction to the whole input with a language model. With --schema the answer is typed data.")
            .input(Type::Any)
            .output(Type::Any)
            .required("instruction", Type::String, "what the model should do with the input")
            .named("schema", Type::Any, Some('s'), "type of the answer, for example [{name: string, risk: int}]")
            .switch("act", None, "act on the piped objects through their methods")
            .switch("no-act", None, "never act, even when the instruction asks for it")
            .example("\"Der Hund bellt\" | llm \"translate to English\"", "plain text in, plain text out")
            .example("ls | llm \"delete all files that are larger than 5mb\"", "the model writes a plan over the objects")
            .example(
                "^journalctl -b -p err | last 40 | llm \"group the problems\" --schema [{unit: string, problem: string, count: int}]",
                "unstructured logs in, a table out",
            )
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let instruction = ctx.req_string(0, "instruction")?;
        let schema = schema_flag(ctx)?;
        let input = input.into_value()?;
        let act = !ctx.switch("no-act")
            && schema.is_none()
            && (ctx.switch("act") || crate::actor::wants_action(ctx.engine, &instruction, &input));
        if act {
            return self.1.run(ctx, &instruction, input);
        }
        // Without data the question is about the shell itself, so the assistant answers. It
        // knows meersh and returns a checked pipeline when the request asks for a command.
        if matches!(input, Value::Nothing) && schema.is_none() {
            let answer = crate::assistant::assist(ctx, &self.0, &instruction, crate::assistant::Want::Answer)?;
            return Ok(PipelineData::Value(answer));
        }
        let mut prompt = format!("Instruction: {instruction}");
        if !matches!(input, Value::Nothing) {
            let data = encode(&input, self.0.config().max_input_chars);
            prompt.push_str(&format!("\n\nInput of type {}:\n{data}", input.ty()));
        }
        let wrapped = schema.as_ref().map(object_schema);
        let request = Request { system: STAGE_SYSTEM.into(), prompt, schema: wrapped.as_ref().map(|(s, _)| s.clone()) };
        // Text for the terminal is shown while it is generated.
        if schema.is_none() && ctx.to_terminal && std::io::IsTerminal::is_terminal(&std::io::stdout()) {
            check_send(ctx, &self.0)?;
            let (backend, model) = (ctx.named_string("backend")?, ctx.named_string("model")?);
            let mut out = std::io::stdout();
            self.0
                .complete_streaming(&request, backend.as_deref(), model.as_deref(), &mut |token| {
                    let _ = std::io::Write::write_all(&mut out, token.as_bytes());
                    let _ = std::io::Write::flush(&mut out);
                })
                .map_err(|e| ctx.err(e))?;
            println!();
            return Ok(PipelineData::Empty);
        }
        let response = ask(ctx, &self.0, &request)?;
        let value = match (schema, wrapped, response.json) {
            (Some(ty), Some((_, unwrap)), Some(json)) => ty.coerce(if unwrap { &json["value"] } else { &json }),
            _ => Value::String(response.text.trim().to_string()),
        };
        Ok(PipelineData::Value(value))
    }
}

/// Splits a stream into batches and runs `f` on each batch lazily, so `first 3` after an
/// LLM stage costs only one request.
pub(crate) fn batched(
    input: ValueStream,
    size: usize,
    engine: Arc<Engine>,
    f: impl FnMut(Vec<Value>) -> Result<Vec<Value>> + Send + 'static,
) -> ValueStream {
    struct Batches<F> {
        input: ValueStream,
        size: usize,
        engine: Arc<Engine>,
        f: F,
        ready: VecDeque<Value>,
    }
    impl<F: FnMut(Vec<Value>) -> Result<Vec<Value>>> Iterator for Batches<F> {
        type Item = Result<Value>;

        fn next(&mut self) -> Option<Self::Item> {
            loop {
                if let Some(v) = self.ready.pop_front() {
                    return Some(Ok(v));
                }
                if let Err(e) = self.engine.check_interrupt() {
                    return Some(Err(e));
                }
                let mut batch = Vec::with_capacity(self.size);
                for item in self.input.by_ref().take(self.size) {
                    match item {
                        Ok(v) => batch.push(v),
                        Err(e) => return Some(Err(e)),
                    }
                }
                if batch.is_empty() {
                    return None;
                }
                match (self.f)(batch) {
                    Ok(out) => self.ready.extend(out),
                    Err(e) => return Some(Err(e)),
                }
            }
        }
    }
    Box::new(Batches { input, size: size.max(1), engine, f, ready: VecDeque::new() })
}

pub(crate) fn indexed_table(rows: &[Value]) -> Value {
    Value::List(
        rows.iter()
            .enumerate()
            .map(|(i, row)| {
                let mut record = Record::new().with("i", i as i64);
                match row {
                    Value::Record(r) => record.fields.extend(r.fields.clone()),
                    other => record.insert("value", other.clone()),
                }
                Value::Record(record)
            })
            .collect(),
    )
}

pub(crate) fn batch_size(ctx: &Ctx, runtime: &LlmRuntime) -> Result<usize> {
    Ok(ctx.named("batch")?.and_then(|v| v.as_int()).map_or(runtime.config().batch_size, |n| n.max(1) as usize))
}

struct LlmMap(Arc<LlmRuntime>);

impl Command for LlmMap {
    fn signature(&self) -> Signature {
        with_llm_flags(Signature::build("llm map"))
            .desc("Add a column whose value a language model computes for every row. Rows are sent in batches.")
            .input(Type::any_table())
            .output(Type::any_table())
            .required("instruction", Type::String, "how to compute the new column from a row")
            .named("into", Type::String, Some('i'), "name of the new column (default: result)")
            .named("schema", Type::Any, Some('s'), "type of the new column (default: string)")
            .named("batch", Type::Int, None, "rows per request")
            .named("cascade", Type::String, None, "backend for the rows the first model is unsure about")
            .example("ls *.md | llm map \"topic in three words\" --into topic", "label files")
            .example(
                "ps | first 20 | llm map \"is this a system daemon\" --into daemon --schema bool",
                "a typed column",
            )
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let instruction = ctx.req_string(0, "instruction")?;
        let column = ctx.named_string("into")?.unwrap_or_else(|| "result".into());
        let ty = schema_flag(ctx)?.unwrap_or(Type::String);
        let size = batch_size(ctx, &self.0)?;
        let ask = RowAsk::from_ctx(ctx, &self.0)?;
        let task = format!("For every input row compute `{column}`. {instruction}");
        let stream = batched(input.into_stream(), size, ctx.engine.clone(), move |rows| {
            let results = ask.per_row(&task, &rows, &column, ty.to_json_schema())?;
            Ok(rows
                .into_iter()
                .enumerate()
                .map(|(i, row)| {
                    let value = results.get(&i).map_or(Value::Nothing, |j| ty.coerce(j));
                    let mut record = match row {
                        Value::Record(r) => r,
                        other => Record::new().with("value", other),
                    };
                    record.insert(column.clone(), value);
                    Value::Record(record)
                })
                .collect())
        });
        Ok(PipelineData::Stream(stream))
    }
}

/// Rows below this self rated confidence go to the cascade backend.
const CASCADE_THRESHOLD: f64 = 0.7;

/// The model settings of a row wise command, shared by its batches.
pub(crate) struct RowAsk {
    runtime: Arc<LlmRuntime>,
    backend: Option<String>,
    model: Option<String>,
    cascade: Option<String>,
}

impl RowAsk {
    pub(crate) fn from_ctx(ctx: &Ctx, runtime: &Arc<LlmRuntime>) -> Result<RowAsk> {
        check_send(ctx, runtime)?;
        let cascade = ctx.named_string("cascade")?;
        if let Some(strong) = &cascade {
            let (backend, local) = runtime.locality(Some(strong));
            ctx.check_now(Cap::LlmSend { backend, local })?;
        }
        Ok(RowAsk {
            runtime: runtime.clone(),
            backend: ctx.named_string("backend")?,
            model: ctx.named_string("model")?,
            cascade,
        })
    }

    /// One request for a set of rows. `on` picks the backend and model, `rate` asks for a confidence.
    fn ask(
        &self,
        task: &str,
        rows: &[(usize, &Value)],
        field: &str,
        schema: &Json,
        rate: bool,
        on: (Option<&str>, Option<&str>),
    ) -> Result<HashMap<usize, Json>> {
        let (backend, model) = on;
        let mut properties = json!({"i": {"type": "integer"}, field: schema});
        let mut required = vec!["i", field];
        if rate {
            properties["confidence"] = json!({"type": "number", "description": "0 to 1, how sure you are"});
            required.push("confidence");
        }
        let schema = json!({
            "type": "object",
            "properties": {"results": {"type": "array", "items": {
                "type": "object", "properties": properties, "required": required, "additionalProperties": false,
            }}},
            "required": ["results"],
            "additionalProperties": false,
        });
        let table = Value::List(
            rows.iter()
                .map(|(i, row)| {
                    let mut record = Record::new().with("i", *i as i64);
                    match row {
                        Value::Record(r) => record.fields.extend(r.fields.clone()),
                        other => record.insert("value", (*other).clone()),
                    }
                    Value::Record(record)
                })
                .collect(),
        );
        let rate_note = if rate { " and your confidence" } else { "" };
        let prompt = format!(
            "{task}\nJudge every row on its own. Return exactly one result per row, with the row's index i{rate_note}.\n\nRows:\n{}",
            encode(&table, self.runtime.config().max_input_chars)
        );
        let request = Request { system: STAGE_SYSTEM.into(), prompt, schema: Some(schema) };
        let response = self.runtime.complete(&request, backend, model).map_err(meersh_engine::ShellError::Other)?;
        Ok(response
            .json
            .as_ref()
            .and_then(|j| j["results"].as_array())
            .map(|items| items.iter().filter_map(|item| Some((item["i"].as_u64()? as usize, item.clone()))).collect())
            .unwrap_or_default())
    }

    /// One answer per row. With a cascade backend the first model also rates its confidence,
    /// and the rows it is unsure about are asked again on the stronger backend.
    pub(crate) fn per_row(
        &self,
        task: &str,
        rows: &[Value],
        field: &str,
        schema: Json,
    ) -> Result<HashMap<usize, Json>> {
        let all: Vec<(usize, &Value)> = rows.iter().enumerate().collect();
        let first = self.ask(
            task,
            &all,
            field,
            &schema,
            self.cascade.is_some(),
            (self.backend.as_deref(), self.model.as_deref()),
        )?;
        let mut out: HashMap<usize, Json> = first.iter().map(|(i, item)| (*i, item[field].clone())).collect();
        if let Some(strong) = &self.cascade {
            let unsure: Vec<(usize, &Value)> = all
                .iter()
                .filter(|(i, _)| {
                    first.get(i).and_then(|it| it["confidence"].as_f64()).is_none_or(|c| c < CASCADE_THRESHOLD)
                })
                .copied()
                .collect();
            if !unsure.is_empty() {
                let second = self.ask(task, &unsure, field, &schema, false, (Some(strong), None))?;
                out.extend(second.into_iter().map(|(i, item)| (i, item[field].clone())));
            }
        }
        // Small models sometimes skip a row or answer one twice. The skipped rows get one
        // more request of their own.
        out.retain(|i, _| *i < rows.len());
        let missing: Vec<(usize, &Value)> = all.iter().filter(|(i, _)| !out.contains_key(i)).copied().collect();
        if !missing.is_empty() {
            let again =
                self.ask(task, &missing, field, &schema, false, (self.backend.as_deref(), self.model.as_deref()))?;
            out.extend(again.into_iter().filter(|(i, _)| *i < rows.len()).map(|(i, item)| (i, item[field].clone())));
        }
        Ok(out)
    }
}

struct LlmWhere(Arc<LlmRuntime>);

impl Command for LlmWhere {
    fn signature(&self) -> Signature {
        with_llm_flags(Signature::build("llm where"))
            .desc("Keep the rows that a language model judges to match a condition written in plain language. Each row gets its own verdict.")
            .input(Type::any_table())
            .output(Type::any_table())
            .required("condition", Type::String, "the condition in plain language")
            .named("batch", Type::Int, None, "rows per request")
            .named("cascade", Type::String, None, "backend for the rows the first model is unsure about")
            .example("ls ~/Downloads | llm where \"looks like an invoice\"", "semantic filter")
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let condition = ctx.req_string(0, "condition")?;
        let size = batch_size(ctx, &self.0)?;
        let ask = RowAsk::from_ctx(ctx, &self.0)?;
        // A verdict per row instead of a list of matching indices. Small local models
        // tend to return every index when asked for a list, but judge single rows well.
        let task = format!("Condition: {condition}\nDecide for every row whether it satisfies the condition.");
        let stream = batched(input.into_stream(), size, ctx.engine.clone(), move |rows| {
            let verdicts = ask.per_row(&task, &rows, "matches", json!({"type": "boolean"}))?;
            Ok(rows
                .into_iter()
                .enumerate()
                .filter(|(i, _)| verdicts.get(i).and_then(Json::as_bool) == Some(true))
                .map(|(_, row)| row)
                .collect())
        });
        Ok(PipelineData::Stream(stream))
    }
}

struct LlmParse(Arc<LlmRuntime>);

impl Command for LlmParse {
    fn signature(&self) -> Signature {
        with_llm_flags(Signature::build("llm parse"))
            .desc(
                "Turn unstructured text into a table of the given record type. Long input is sent in chunks of lines.",
            )
            .input(Type::String)
            .output(Type::any_table())
            .required("schema", Type::Any, "the record type, for example {time: date, level: string, message: string}")
            .named("batch", Type::Int, None, "lines per request (default 50)")
            .example("^dmesg | last 30 | llm parse {subsystem: string, message: string}", "text in, table out")
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let ty = Type::from_spec(&ctx.req(0, "schema")?).map_err(|e| ctx.err(e))?;
        let size = ctx.named("batch")?.and_then(|v| v.as_int()).map_or(50, |n| n.max(1) as usize);
        let (backend, model) = (ctx.named_string("backend")?, ctx.named_string("model")?);
        let runtime = self.0.clone();
        let schema = json!({
            "type": "object",
            "properties": {"items": {"type": "array", "items": ty.to_json_schema()}},
            "required": ["items"],
            "additionalProperties": false,
        });
        let lines: ValueStream = Box::new(input.into_stream().flat_map(|v| -> Vec<Result<Value>> {
            match v {
                Ok(Value::String(s)) => s.lines().map(|l| Ok(Value::String(l.to_string()))).collect(),
                other => vec![other],
            }
        }));
        let stream = batched(lines, size, ctx.engine.clone(), move |chunk| {
            let text: Vec<String> = chunk.iter().map(Value::to_text).collect();
            let prompt = format!(
                "Extract every record from the text below. Return one item per record with the fields of the schema. \
                 Use null for a field the text does not contain.\n\nText:\n{}",
                encode(&Value::String(text.join("\n")), runtime.config().max_input_chars)
            );
            let request = Request { system: STAGE_SYSTEM.into(), prompt, schema: Some(schema.clone()) };
            let response = runtime
                .complete(&request, backend.as_deref(), model.as_deref())
                .map_err(meersh_engine::ShellError::Other)?;
            Ok(response
                .json
                .as_ref()
                .and_then(|j| j["items"].as_array())
                .map_or_else(Vec::new, |items| items.iter().map(|j| ty.coerce(j)).collect()))
        });
        Ok(PipelineData::Stream(stream))
    }
}

struct LlmBackends(Arc<LlmRuntime>);

impl Command for LlmBackends {
    fn signature(&self) -> Signature {
        Signature::build("llm backends")
            .desc("List the configured language model backends.")
            .input(Type::Nothing)
            .output(Type::table("llm.backend"))
    }

    fn run(&self, _ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let config = self.0.config();
        let rows = config
            .backends
            .iter()
            .map(|(name, b)| {
                Value::Record(
                    Record::typed("llm.backend")
                        .with("name", name.as_str())
                        .with("kind", b.kind.as_str())
                        .with("model", b.model.clone().unwrap_or_else(|| "(default)".into()))
                        .with("url", b.url.clone())
                        .with("default", *name == config.default),
                )
            })
            .collect();
        Ok(PipelineData::Value(Value::List(rows)))
    }
}

struct LlmUse(Arc<LlmRuntime>);

impl Command for LlmUse {
    fn signature(&self) -> Signature {
        Signature::build("llm use")
            .desc("Make a backend the default for this session.")
            .input(Type::Nothing)
            .output(Type::Nothing)
            .required("backend", Type::String, "backend name")
            .effect(Effect::Write)
            .example("llm use claude", "switch from the local model to Claude Code")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        // Switching the default could send later data to a cloud model.
        if ctx.principal().is_model() {
            return Err(ctx.err("only the user may switch the default backend"));
        }
        self.0.set_default(&ctx.req_string(0, "backend")?).map_err(|e| ctx.err(e))?;
        Ok(PipelineData::Empty)
    }
}

struct Ai(Arc<LlmRuntime>);

impl Command for Ai {
    fn signature(&self) -> Signature {
        with_llm_flags(Signature::build("ai"))
            .desc("Translate a request in plain language into a meersh pipeline. The REPL shows it and asks before running it. `? request` is the shortcut.")
            .input(Type::Nothing)
            .output(Type::record("ai.pipeline"))
            .rest("request", Type::String, "what you want, in plain language")
            .example("ai the five biggest files in my home directory", "returns {pipeline, explanation, effect}")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let request_text = ctx.rest_strings(0)?.join(" ");
        if request_text.trim().is_empty() {
            return Err(ctx.err("what should the pipeline do?"));
        }
        let answer = crate::assistant::assist(ctx, &self.0, &request_text, crate::assistant::Want::Pipeline)?;
        Ok(PipelineData::Value(answer))
    }
}
