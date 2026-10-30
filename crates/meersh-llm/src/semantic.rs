//! The rest of the LOTUS style operators and similarity search.
//!
//! `llm rank`, `llm group` and `llm join` play the part of `sem_topk`, `sem_cluster_by` and
//! `sem_join`. Like `llm map` and `llm where` they send rows in batches through `RowAsk` and
//! ask for one answer per row, because small local models judge single rows well but fail at
//! picking items out of a list. `llm embed` and `llm search` add vectors and cosine similarity.

use std::cmp::Ordering;
use std::fmt::Display;
use std::sync::Arc;

use meersh_engine::value::{Record, Type, Value};
use meersh_engine::{Command, Ctx, Engine, PipelineData, Result, ShellError, Signature};
use serde_json::{Value as Json, json};

use crate::backend::Request;
use crate::commands::{RowAsk, STAGE_SYSTEM, ask, batch_size, batched, check_send, with_llm_flags};
use crate::encode::encode;
use crate::{LlmRuntime, embed};

pub fn register(engine: &mut Engine, runtime: Arc<LlmRuntime>) {
    engine.register(LlmRank(runtime.clone()));
    engine.register(LlmGroup(runtime.clone()));
    engine.register(LlmJoin(runtime.clone()));
    engine.register(LlmEmbed(runtime.clone()));
    engine.register(LlmSearch(runtime));
}

/// `llm join` judges every pair of rows, so its cost grows with the product of both tables.
const MAX_PAIRS: usize = 2500;
/// Rows that the first pass of `llm group` sees when it proposes the categories.
const GROUP_SAMPLE: usize = 50;
/// Texts per embedding request. Embedding endpoints take far more input than a chat prompt.
const EMBED_BATCH: usize = 64;

const CASCADE: &str = "backend for the rows the first model is unsure about";

/// A non negative count from a flag such as `--top 3`.
fn count_flag(ctx: &Ctx, name: &str) -> Result<Option<usize>> {
    Ok(ctx.named(name)?.and_then(|v| v.as_int()).map(|n| n.max(0) as usize))
}

/// The backend settings of `llm embed` and `llm search`. Batches run lazily after `run` has
/// returned, so they cannot borrow the `Ctx`.
struct Embedder {
    runtime: Arc<LlmRuntime>,
    command: String,
    backend: Option<String>,
    model: Option<String>,
}

impl Embedder {
    fn new(ctx: &Ctx, runtime: &Arc<LlmRuntime>) -> Result<Embedder> {
        // An embedding request sends the rows to the backend just like a chat request.
        check_send(ctx, runtime)?;
        Ok(Embedder {
            runtime: runtime.clone(),
            command: ctx.call.name.clone(),
            backend: ctx.named_string("backend")?,
            model: ctx.named_string("model")?,
        })
    }

    fn err(&self, msg: impl Display) -> ShellError {
        ShellError::Command { cmd: self.command.clone(), msg: msg.to_string() }
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let config = self.runtime.config();
        let name = self.backend.as_deref().unwrap_or(&config.default);
        let mut backend = config
            .backends
            .get(name)
            .cloned()
            .ok_or_else(|| self.err(format!("no backend named '{name}', see `llm backends`")))?;
        if let Some(model) = self.model.clone().or_else(|| backend.embed_model.clone()) {
            backend.model = Some(model);
        }
        let vectors = if config.cache { embed::embed(&backend, texts) } else { embed::fetch(&backend, texts) };
        vectors.map_err(|e| self.err(e))
    }
}

/// A row as a record. A plain value becomes `{value: ...}`, as in `llm map`.
fn into_record(row: Value) -> Record {
    match row {
        Value::Record(r) => r,
        other => Record::new().with("value", other),
    }
}

/// Sorts rows by a numeric column, highest first. The sort is stable and rows without a
/// number go to the end.
fn sort_desc(rows: &mut [Value], column: &str) {
    let key = |row: &Value| match row.as_record().and_then(|r| r.get(column)) {
        Some(Value::Int(n)) => Some(*n as f64),
        Some(Value::Float(f)) => Some(*f),
        _ => None,
    };
    rows.sort_by(|a, b| match (key(a), key(b)) {
        (Some(x), Some(y)) => y.total_cmp(&x),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    });
}

/// A score from 0 to 100. Models sometimes answer `"85"` or `85.5`, and some overshoot.
fn score(json: &Json) -> Option<i64> {
    let n = json.as_f64().or_else(|| json.as_str()?.trim().trim_end_matches('%').trim().parse().ok())?;
    n.is_finite().then(|| n.round().clamp(0.0, 100.0) as i64)
}

struct LlmRank(Arc<LlmRuntime>);

impl Command for LlmRank {
    fn signature(&self) -> Signature {
        with_llm_flags(Signature::build("llm rank"))
            .desc("Score every row from 0 to 100 against a criterion with a language model, add the column `score` and sort by it, best first. Each row gets its own score.")
            .input(Type::any_table())
            .output(Type::any_table())
            .required("criterion", Type::String, "what a high score means, in plain language")
            .named("top", Type::Int, Some('t'), "keep only the N best rows")
            .named("batch", Type::Int, None, "rows per request")
            .named("cascade", Type::String, None, CASCADE)
            .example("[{dish: lemon} {dish: honey} {dish: bread}] | llm rank \"how sweet\"", "a table sorted by score")
            .example("ls *.md | llm rank \"explains how to install meersh\" --top 3", "the three most relevant files")
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let criterion = ctx.req_string(0, "criterion")?;
        let top = count_flag(ctx, "top")?;
        let size = batch_size(ctx, &self.0)?;
        let judge = RowAsk::from_ctx(ctx, &self.0)?;
        let task = format!(
            "Criterion: {criterion}\nRate every row from 0 to 100 by how strongly it has the quality that the criterion \
             names. 0 means not at all, 50 means somewhat and 100 means extremely. Give a short reason first."
        );
        // A reason before the number makes a small model think about the row. With a bare
        // integer `qwen3:4b` scored every row 0.
        let schema = json!({
            "type": "object",
            "properties": {
                "reason": {"type": "string", "description": "one short sentence"},
                "score": {"type": "integer", "description": "from 0 to 100"},
            },
            "required": ["reason", "score"],
            "additionalProperties": false,
        });
        let scored = batched(input.into_stream(), size, ctx.engine.clone(), move |rows| {
            let ratings = judge.per_row(&task, &rows, "rating", schema.clone())?;
            Ok(rows
                .into_iter()
                .enumerate()
                .map(|(i, row)| {
                    let rating = ratings.get(&i).and_then(|r| score(&r["score"]).or_else(|| score(r)));
                    let mut record = into_record(row);
                    record.insert("score", Value::from(rating));
                    Value::Record(record)
                })
                .collect())
        });
        // Sorting needs every score, so this stage is not lazy.
        let mut rows = scored.collect::<Result<Vec<_>>>()?;
        sort_desc(&mut rows, "score");
        if let Some(n) = top {
            rows.truncate(n);
        }
        Ok(PipelineData::Value(Value::List(rows)))
    }
}

/// Up to `n` rows spread evenly over the input. Inputs are often sorted, so the first `n`
/// rows would show the model only one end of the data.
fn sample(rows: &[Value], n: usize) -> Vec<Value> {
    if rows.len() <= n {
        return rows.to_vec();
    }
    (0..n).map(|k| rows[k * rows.len() / n].clone()).collect()
}

/// The proposed category names, trimmed, without duplicates and at most `max` of them.
fn categories(json: &Json, max: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in json.as_array().into_iter().flatten().filter_map(Json::as_str).map(str::trim) {
        if out.len() < max && !name.is_empty() && !out.iter().any(|c| c.eq_ignore_ascii_case(name)) {
            out.push(name.to_string());
        }
    }
    out
}

/// The category an answer names. Backends without constrained decoding may change the case.
fn matching<'a>(categories: &'a [String], answer: &str) -> Option<&'a String> {
    let answer = answer.trim();
    categories.iter().find(|c| *c == answer).or_else(|| categories.iter().find(|c| c.eq_ignore_ascii_case(answer)))
}

struct LlmGroup(Arc<LlmRuntime>);

impl Command for LlmGroup {
    fn signature(&self) -> Signature {
        with_llm_flags(Signature::build("llm group"))
            .desc("Put every row into a category that a language model finds. A first request proposes the categories from a sample of up to 50 rows. Then every row gets one of them, in batches.")
            .input(Type::any_table())
            .output(Type::any_table())
            .required("criterion", Type::String, "what the categories should tell apart")
            .named("into", Type::String, Some('i'), "name of the new column (default `group`)")
            .named("max", Type::Int, None, "most categories to use (default 8)")
            .named("batch", Type::Int, None, "rows per request")
            .named("cascade", Type::String, None, CASCADE)
            .example("[{name: apple} {name: carrot} {name: pear}] | llm group \"fruit or vegetable\" --max 2", "adds a group column")
            .example(
                "^journalctl -b -p err | last 100 | llm group \"kind of problem\" --into kind | group-by kind",
                "sort log lines into topics",
            )
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let criterion = ctx.req_string(0, "criterion")?;
        let column = ctx.named_string("into")?.unwrap_or_else(|| "group".into());
        let max = count_flag(ctx, "max")?.unwrap_or(8).max(1);
        let size = batch_size(ctx, &self.0)?;
        let judge = RowAsk::from_ctx(ctx, &self.0)?;
        let rows = input.into_stream().collect::<Result<Vec<_>>>()?;
        if rows.is_empty() {
            return Ok(PipelineData::Value(Value::List(vec![])));
        }
        let prompt = format!(
            "Criterion: {criterion}\nPropose at most {max} categories that divide the rows below by the criterion. \
             Every row has to fit exactly one category. Use short names of one to three words.\n\nRows:\n{}",
            encode(&Value::List(sample(&rows, GROUP_SAMPLE)), self.0.config().max_input_chars)
        );
        let schema = json!({
            "type": "object",
            "properties": {"categories": {"type": "array", "items": {"type": "string"}}},
            "required": ["categories"],
            "additionalProperties": false,
        });
        let request = Request { system: STAGE_SYSTEM.into(), prompt, schema: Some(schema) };
        let proposed = ask(ctx, &self.0, &request)?.json.unwrap_or_default();
        let names = categories(&proposed["categories"], max);
        if names.is_empty() {
            return Err(ctx.err("the model proposed no categories"));
        }
        let task = format!(
            "Criterion: {criterion}\nCategories: {}\nPut every row into exactly one of the categories.",
            serde_json::to_string(&names).unwrap_or_default()
        );
        let schema = json!({"type": "string", "enum": names});
        let stream = batched(Box::new(rows.into_iter().map(Ok)), size, ctx.engine.clone(), move |rows| {
            let answers = judge.per_row(&task, &rows, "category", schema.clone())?;
            Ok(rows
                .into_iter()
                .enumerate()
                .map(|(i, row)| {
                    let category = answers.get(&i).and_then(Json::as_str).and_then(|a| matching(&names, a));
                    let mut record = into_record(row);
                    record.insert(column.clone(), Value::from(category.cloned()));
                    Value::Record(record)
                })
                .collect())
        });
        Ok(PipelineData::Stream(stream))
    }
}

/// Refuses joins whose pairs would take too many requests.
fn check_pairs(left: usize, right: usize) -> std::result::Result<usize, String> {
    let pairs = left.saturating_mul(right);
    if pairs > MAX_PAIRS {
        return Err(format!(
            "{left} x {right} rows make {pairs} pairs, more than the {MAX_PAIRS} that llm join judges. Narrow both \
             tables first, for example with `where` or `first`."
        ));
    }
    Ok(pairs)
}

/// One merged record per matching pair. The left fields come first. A right column whose
/// name the left row already uses gets the suffix `_right`. The result describes neither
/// object, so it carries no type tag and no identity.
fn merge(left: Value, right: Value) -> Value {
    let mut merged = Record { fields: into_record(left).fields, ..Record::default() };
    for (key, value) in into_record(right).fields {
        let key = if merged.fields.contains_key(&key) { format!("{key}_right") } else { key };
        merged.fields.insert(key, value);
    }
    Value::Record(merged)
}

struct LlmJoin(Arc<LlmRuntime>);

impl Command for LlmJoin {
    fn signature(&self) -> Signature {
        with_llm_flags(Signature::build("llm join"))
            .desc("Join the input with another table on a condition in plain language. A language model judges every pair of rows, so the two tables may form at most 2500 pairs.")
            .input(Type::any_table())
            .output(Type::any_table())
            .required("right", Type::any_table(), "the table to join with, for example $cities")
            .required("condition", Type::String, "how a row of the input relates to a row of the right table")
            .named("batch", Type::Int, None, "pairs per request")
            .named("cascade", Type::String, None, CASCADE)
            .example("$people | llm join $cities \"the person lives in the city\"", "one merged record per matching pair")
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let right = match ctx.req(0, "right")? {
            Value::List(items) => items,
            Value::Nothing => vec![],
            other => vec![other],
        };
        let condition = ctx.req_string(1, "condition")?;
        let size = batch_size(ctx, &self.0)?;
        let judge = RowAsk::from_ctx(ctx, &self.0)?;
        let left = input.into_stream().collect::<Result<Vec<_>>>()?;
        check_pairs(left.len(), right.len()).map_err(|e| ctx.err(e))?;
        let pairs: Vec<Value> = left
            .iter()
            .flat_map(|l| {
                right.iter().map(|r| Value::Record(Record::new().with("left", l.clone()).with("right", r.clone())))
            })
            .collect();
        let task = format!(
            "Condition: {condition}\nEvery row pairs a record of the left table with a record of the right table. The \
             condition says how the left record relates to the right record. Decide for every pair whether it satisfies \
             the condition."
        );
        let schema = json!({"type": "boolean"});
        let stream = batched(Box::new(pairs.into_iter().map(Ok)), size, ctx.engine.clone(), move |pairs| {
            let verdicts = judge.per_row(&task, &pairs, "matches", schema.clone())?;
            Ok(pairs
                .into_iter()
                .enumerate()
                .filter(|(i, _)| verdicts.get(i).and_then(Json::as_bool) == Some(true))
                .map(|(_, pair)| {
                    let mut pair = into_record(pair);
                    let mut take = |key| pair.fields.swap_remove(key).unwrap_or(Value::Nothing);
                    let left = take("left");
                    merge(left, take("right"))
                })
                .collect())
        });
        Ok(PipelineData::Stream(stream))
    }
}

/// The text that stands for a row. Without a column it is the whole row as compact JSON,
/// minus the column `skip` so that a stored vector is never embedded as text. A plain value
/// is its own text.
fn text_of(row: &Value, column: Option<&str>, skip: &str) -> std::result::Result<String, String> {
    let Value::Record(record) = row else { return Ok(row.to_text()) };
    match column {
        Some(c) => record.get(c).map(Value::to_text).ok_or_else(|| format!("a row has no column '{c}'")),
        None => {
            let mut record = record.clone();
            record.fields.shift_remove(skip);
            Ok(Value::Record(record).to_text())
        }
    }
}

fn vector_value(v: &[f32]) -> Value {
    Value::List(v.iter().map(|x| Value::Float(f64::from(*x))).collect())
}

/// A vector stored by `llm embed`, if the row has one.
fn stored_vector(row: &Value) -> Option<Vec<f32>> {
    let Some(Value::List(items)) = row.as_record()?.get("embedding") else { return None };
    let vector: Option<Vec<f32>> = items
        .iter()
        .map(|v| match v {
            Value::Float(f) => Some(*f as f32),
            Value::Int(n) => Some(*n as f32),
            _ => None,
        })
        .collect();
    vector.filter(|v| !v.is_empty())
}

/// Cosine similarity, from -1 for opposite to 1 for the same direction. `None` when the
/// lengths differ or a vector is zero.
fn cosine(a: &[f32], b: &[f32]) -> Option<f64> {
    if a.len() != b.len() || a.is_empty() {
        return None;
    }
    let (mut dot, mut norm_a, mut norm_b) = (0.0_f64, 0.0_f64, 0.0_f64);
    for (x, y) in a.iter().zip(b) {
        let (x, y) = (f64::from(*x), f64::from(*y));
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    (norm_a > 0.0 && norm_b > 0.0).then(|| dot / (norm_a.sqrt() * norm_b.sqrt()))
}

struct LlmEmbed(Arc<LlmRuntime>);

impl Command for LlmEmbed {
    fn signature(&self) -> Signature {
        with_llm_flags(Signature::build("llm embed"))
            .desc("Add an embedding vector to every row. Without --column the whole row is embedded as compact JSON. Needs a backend with an embedding endpoint, which Ollama and OpenAI compatible servers have.")
            .input(Type::any_table())
            .output(Type::any_table())
            .named("column", Type::String, Some('c'), "the text column to embed (default the whole row)")
            .named("into", Type::String, Some('i'), "name of the new column (default `embedding`)")
            .example("ls ~/notes | llm embed --column name --model nomic-embed-text", "a list of floats per row")
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let column = ctx.named_string("column")?;
        let into = ctx.named_string("into")?.unwrap_or_else(|| "embedding".into());
        let embedder = Embedder::new(ctx, &self.0)?;
        let stream = batched(input.into_stream(), EMBED_BATCH, ctx.engine.clone(), move |rows| {
            let texts = rows
                .iter()
                .map(|row| text_of(row, column.as_deref(), &into))
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| embedder.err(e))?;
            let vectors = embedder.embed(&texts)?;
            Ok(rows
                .into_iter()
                .zip(vectors)
                .map(|(row, vector)| {
                    let mut record = into_record(row);
                    record.insert(into.clone(), vector_value(&vector));
                    Value::Record(record)
                })
                .collect())
        });
        Ok(PipelineData::Stream(stream))
    }
}

struct LlmSearch(Arc<LlmRuntime>);

impl Command for LlmSearch {
    fn signature(&self) -> Signature {
        with_llm_flags(Signature::build("llm search"))
            .desc("Sort rows by how close their meaning is to a query, closest first. Rows and query are embedded and `similarity` is their cosine similarity. Without --column, rows that already carry an `embedding` from `llm embed` are not embedded again.")
            .input(Type::any_table())
            .output(Type::any_table())
            .required("query", Type::String, "what to look for, in plain language")
            .named("column", Type::String, Some('c'), "the text column to compare (default the whole row)")
            .named("top", Type::Int, Some('t'), "keep only the N closest rows")
            .example("ls ~/notes | llm search \"tax return\" --column name --top 5", "the five closest file names")
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let query = ctx.req_string(0, "query")?;
        let column = ctx.named_string("column")?;
        let top = count_flag(ctx, "top")?;
        let embedder = Embedder::new(ctx, &self.0)?;
        let rows = input.into_stream().collect::<Result<Vec<_>>>()?;
        if rows.is_empty() {
            return Ok(PipelineData::Value(Value::List(vec![])));
        }
        let mut vectors: Vec<Option<Vec<f32>>> =
            rows.iter().map(|row| if column.is_none() { stored_vector(row) } else { None }).collect();
        let missing: Vec<usize> = (0..rows.len()).filter(|i| vectors[*i].is_none()).collect();
        let mut texts = vec![query];
        for i in &missing {
            texts.push(text_of(&rows[*i], column.as_deref(), "embedding").map_err(|e| ctx.err(e))?);
        }
        let mut embedded = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(EMBED_BATCH) {
            ctx.engine.check_interrupt()?;
            embedded.extend(embedder.embed(chunk)?);
        }
        let mut embedded = embedded.into_iter();
        let query = embedded.next().unwrap_or_default();
        for (i, vector) in missing.into_iter().zip(embedded) {
            vectors[i] = Some(vector);
        }
        let mut out = Vec::with_capacity(rows.len());
        for (row, vector) in rows.into_iter().zip(vectors) {
            let vector = vector.unwrap_or_default();
            if vector.len() != query.len() {
                return Err(ctx.err(format!(
                    "a stored embedding has {} dimensions and the query {}. Embed both with the same model.",
                    vector.len(),
                    query.len()
                )));
            }
            let mut record = into_record(row);
            record.insert("similarity", Value::from(cosine(&query, &vector)));
            out.push(Value::Record(record));
        }
        sort_desc(&mut out, "similarity");
        if let Some(n) = top {
            out.truncate(n);
        }
        Ok(PipelineData::Value(Value::List(out)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: Option<f64>, b: f64) -> bool {
        a.is_some_and(|a| (a - b).abs() < 1e-9)
    }

    #[test]
    fn cosine_similarity() {
        assert!(close(cosine(&[1.0, 2.0, 3.0], &[2.0, 4.0, 6.0]), 1.0));
        assert!(close(cosine(&[1.0, 0.0], &[0.0, 5.0]), 0.0));
        assert!(close(cosine(&[1.0, 1.0], &[-1.0, -1.0]), -1.0));
        assert!(close(cosine(&[3.0, 4.0], &[4.0, 3.0]), 24.0 / 25.0));
        assert_eq!(cosine(&[1.0, 2.0], &[1.0]), None);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), None);
        assert_eq!(cosine(&[], &[]), None);
    }

    #[test]
    fn pair_limit() {
        assert_eq!(check_pairs(50, 50), Ok(2500));
        assert_eq!(check_pairs(0, 1_000_000), Ok(0));
        let err = check_pairs(51, 50).unwrap_err();
        assert!(err.contains("2550 pairs") && err.contains("2500"), "{err}");
        assert!(check_pairs(usize::MAX, 2).is_err());
    }

    #[test]
    fn scores_are_clamped_and_lenient() {
        assert_eq!(score(&json!(85)), Some(85));
        assert_eq!(score(&json!(85.6)), Some(86));
        assert_eq!(score(&json!("70")), Some(70));
        assert_eq!(score(&json!(" 40 % ")), Some(40));
        assert_eq!(score(&json!(140)), Some(100));
        assert_eq!(score(&json!(-3)), Some(0));
        assert_eq!(score(&json!("sweet")), None);
        assert_eq!(score(&Json::Null), None);
    }

    #[test]
    fn rows_sort_by_number_with_gaps_last() {
        let row = |name: &str, v: Value| Value::Record(Record::new().with("name", name).with("score", v));
        let mut rows = vec![
            row("a", Value::Int(10)),
            row("b", Value::Nothing),
            row("c", Value::Int(90)),
            row("d", Value::Float(10.0)),
        ];
        sort_desc(&mut rows, "score");
        let names: Vec<String> = rows.iter().map(|r| r.follow("name").unwrap().to_text()).collect();
        assert_eq!(names, ["c", "a", "d", "b"]);
    }

    #[test]
    fn categories_are_cleaned() {
        let names = categories(&json!([" Fruit ", "fruit", "", "Vegetable", "Grain", 3]), 2);
        assert_eq!(names, ["Fruit", "Vegetable"]);
        assert_eq!(matching(&names, "vegetable "), Some(&"Vegetable".to_string()));
        assert_eq!(matching(&names, "meat"), None);
    }

    #[test]
    fn samples_spread_over_the_input() {
        let rows: Vec<Value> = (0..100).map(Value::Int).collect();
        let picked = sample(&rows, 4);
        assert_eq!(picked, [0, 25, 50, 75].map(Value::Int));
        assert_eq!(sample(&rows[..3], 50).len(), 3);
    }

    #[test]
    fn merged_pairs_keep_both_sides() {
        let left = Value::Record(Record::typed("person").with("name", "Anna").with("age", 30));
        let right = Value::Record(Record::new().with("name", "Berlin").with("country", "Germany"));
        let merged = merge(left, right);
        let record = merged.as_record().unwrap();
        assert_eq!(record.type_name(), None);
        let keys: Vec<&str> = record.fields.keys().map(String::as_str).collect();
        assert_eq!(keys, ["name", "age", "name_right", "country"]);
        assert_eq!(merge("x".into(), "y".into()).follow("value_right"), Some("y".into()));
    }

    #[test]
    fn row_text_for_embeddings() {
        let row = Value::Record(Record::new().with("name", "a.md").with("embedding", vec![Value::Float(0.5)]));
        assert_eq!(text_of(&row, Some("name"), "embedding"), Ok("a.md".into()));
        assert_eq!(text_of(&row, None, "embedding"), Ok(r#"{"name":"a.md"}"#.into()));
        assert!(text_of(&row, Some("size"), "embedding").is_err());
        assert_eq!(text_of(&"plain".into(), Some("name"), "embedding"), Ok("plain".into()));
        assert_eq!(stored_vector(&row), Some(vec![0.5]));
        assert_eq!(stored_vector(&"plain".into()), None);
    }
}
