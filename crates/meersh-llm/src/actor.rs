//! Models that act on objects, and models that edit data.
//!
//! `ls | llm "delete all files larger than 5mb"` does not send the files to the model. The
//! model sees only the type of the input and the methods of that type, and it writes a
//! pipeline over the input, here `where size > 5mb | each { $it.delete() }`. The shell
//! checks that the pipeline can only filter the objects it was handed and call their
//! methods, then runs it under the actor principal. Every action therefore goes through the
//! plan, ownership and policy. This follows CaMeL. The planner never sees untrusted data,
//! and data is judged only by quarantined calls such as `llm where`, which cannot act.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use meersh_engine::authority::{PlanNote, Review};
use meersh_engine::parser::{Element, Statement};
use meersh_engine::value::{Record, Type, Value, parse_literal};
use meersh_engine::{Command, Ctx, Effect, Engine, PipelineData, Principal, Result, Signature};
use serde_json::json;

use crate::LlmRuntime;
use crate::backend::Request;
use crate::commands::{STAGE_SYSTEM, ask, check_send, indexed_table, with_llm_flags};
use crate::encode::encode;

/// What an actor pipeline may use besides the methods of its input type. None of these can
/// reach anything that was not piped in.
const ACTOR_COMMANDS: [&str; 16] = [
    "where",
    "sort-by",
    "first",
    "last",
    "select",
    "reject",
    "each",
    "insert",
    "uniq",
    "reverse",
    "length",
    "get",
    "group-by",
    "sum",
    "llm where",
    "llm map",
];

/// Words that ask for a method. The method has to exist on the input type.
const VERBS: [(&str, &[&str]); 5] = [
    ("delete", &["delete", "remove", "erase", "purge", "clean", "trash", "rm", "wipe", "drop", "discard"]),
    ("kill", &["kill", "stop", "terminate", "end", "quit", "signal"]),
    ("move", &["move", "rename", "mv", "relocate", "put"]),
    ("copy", &["copy", "duplicate", "backup", "cp"]),
    ("chmod", &["chmod", "permission", "permissions"]),
];

fn record_type(value: &Value) -> Option<String> {
    let record = match value {
        Value::Record(r) => r,
        Value::List(items) => items.first()?.as_record()?,
        _ => return None,
    };
    record.type_name().map(str::to_string)
}

fn method_name(full: &str) -> &str {
    full.rsplit('.').next().unwrap_or(full)
}

/// Whether `llm` should act on its input instead of transforming it. That is the case when
/// the input has methods that act and the first words of the instruction ask for one.
pub fn wants_action(engine: &Engine, instruction: &str, input: &Value) -> bool {
    let Some(type_name) = record_type(input) else { return false };
    let acting: Vec<String> = engine
        .methods_of(&type_name)
        .iter()
        .filter(|s| s.effect >= Effect::Write)
        .map(|s| method_name(&s.name).to_string())
        .collect();
    let words: Vec<String> = instruction
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .take(4)
        .map(str::to_lowercase)
        .collect();
    words.iter().any(|word| {
        acting.contains(word)
            || VERBS.iter().any(|(method, words)| acting.iter().any(|a| a == method) && words.contains(&word.as_str()))
    })
}

const ACTOR_SYSTEM: &str = "You turn an instruction into one meersh pipeline that operates on the input table. \
You never see the rows, only their type. The pipeline receives the rows as its input.
Rules:
- Use only the methods and commands listed below. External programs are not available.
- Methods act on one object. Call them inside each, for example `each { $it.delete() }`.
- Filter with where over the columns, for example `where size > 5mb and type == file`. Operators are \
== != < <= > >= =~ (regex) in and or not, with spaces around them. Literals include 10kb 5mb 2gib, 30sec 2hr 3day, \"strings\".
- Write the criterion as a where condition whenever the columns can express it. Use `llm where \"<condition>\"` only \
for criteria that need judgement, such as \"looks like an invoice\".
- Do exactly what the instruction asks. Never act on more objects than it asks for.
Examples:
- `delete all files larger than 5mb` on table<fs.entry> gives `where size > 5mb and type == file | each { $it.delete() }`
- `stop the sleep processes` on table<proc> gives `where name == sleep | each { $it.kill() }`
- `trash the files that look like invoices` gives `llm where \"looks like an invoice\" | each { $it.delete() }`";

/// The actor half of `llm`. It remembers what it compiled, so the planning run and the
/// applying run execute the same pipeline even without the response cache.
pub struct Actor {
    runtime: Arc<LlmRuntime>,
    compiled: Mutex<HashMap<String, (String, String)>>,
}

impl Actor {
    pub fn new(runtime: Arc<LlmRuntime>) -> Self {
        Actor { runtime, compiled: Mutex::default() }
    }

    pub fn run(&self, ctx: &Ctx, instruction: &str, input: Value) -> Result<PipelineData> {
        let type_name =
            record_type(&input).ok_or_else(|| ctx.err("acting needs typed objects, such as the output of ls or ps"))?;
        let input_type = input.ty();
        let key = format!("{type_name}|{input_type}|{instruction}");
        let cached = self.compiled.lock().expect("actor lock").get(&key).cloned();
        let (pipeline, explanation) = match cached {
            Some(found) => found,
            None => {
                let compiled = self.compile(ctx, instruction, &type_name, &input_type)?;
                self.compiled.lock().expect("actor lock").insert(key, compiled.clone());
                compiled
            }
        };
        let script = ctx.engine.parse(&pipeline)?;
        let Some(Statement::Pipeline(parsed)) = script.statements.first() else {
            return Err(ctx.err("the model did not produce a pipeline"));
        };
        ctx.plan_note(PlanNote { principal: "actor".into(), pipeline: pipeline.clone(), explanation });
        let stack = ctx.stack.with_principal(Principal::Actor);
        ctx.engine.run_pipeline(&stack, parsed, PipelineData::Value(input), false)
    }

    fn compile(&self, ctx: &Ctx, instruction: &str, type_name: &str, input_type: &Type) -> Result<(String, String)> {
        let mut catalogue = format!("Input type: {input_type}\n\nMethods of {type_name}:\n");
        for sig in ctx.engine.methods_of(type_name) {
            let usage = sig.usage().replacen(&sig.name, &format!("$it.{}", method_name(&sig.name)), 1);
            catalogue.push_str(&format!("- {usage} ({}): {}\n", sig.effect.as_str(), sig.description));
        }
        catalogue.push_str("\nCommands:\n");
        for name in ACTOR_COMMANDS {
            if let Some(sig) = ctx.engine.signature(name) {
                catalogue.push_str(&format!("- {}: {}\n", sig.usage(), sig.description));
            }
        }
        let schema = json!({
            "type": "object",
            "properties": {"pipeline": {"type": "string"}, "explanation": {"type": "string"}},
            "required": ["pipeline", "explanation"],
            "additionalProperties": false,
        });
        let system = format!("{ACTOR_SYSTEM}\n\n{catalogue}");
        let mut prompt = format!("Instruction: {instruction}");
        for attempt in 0..2 {
            let request = Request { system: system.clone(), prompt: prompt.clone(), schema: Some(schema.clone()) };
            let json = ask(ctx, &self.runtime, &request)?.json.unwrap_or_default();
            let pipeline = json["pipeline"].as_str().unwrap_or_default().trim().trim_matches('`').to_string();
            let explanation = json["explanation"].as_str().unwrap_or_default().to_string();
            match validate(ctx.engine, &pipeline, type_name) {
                Ok(()) => return Ok((pipeline, explanation)),
                Err(problem) if attempt == 0 => {
                    prompt = format!(
                        "Instruction: {instruction}\n\nYour answer `{pipeline}` was rejected: {problem}. Fix it."
                    );
                }
                Err(problem) => {
                    return Err(ctx.err(format!("the model wrote `{pipeline}`, which is not allowed: {problem}")));
                }
            }
        }
        unreachable!("the loop returns on its second attempt")
    }
}

/// An actor pipeline may only filter what it received and call methods of that type.
pub fn validate(engine: &Engine, src: &str, type_name: &str) -> std::result::Result<(), String> {
    let script = engine.parse(src).map_err(|e| e.to_string())?;
    let [Statement::Pipeline(pipeline)] = script.statements.as_slice() else {
        return Err("it must be exactly one pipeline".into());
    };
    if !matches!(pipeline.elements.first(), Some(Element::Command(_))) {
        return Err("it must start with a command that reads the input, such as where or each".into());
    }
    let mut problem = None;
    pipeline.walk_commands(&mut |call| {
        if problem.is_some() {
            return;
        }
        if call.external {
            problem = Some(format!("external programs are not allowed, `{}` is not a builtin", call.name));
        } else if !ACTOR_COMMANDS.contains(&call.name.as_str())
            && !engine.is_command(&format!("{type_name}.{}", call.name))
        {
            problem =
                Some(format!("`{}` is not available. Use the listed methods of {type_name} and commands", call.name));
        }
    });
    problem.map_or(Ok(()), Err)
}

/// `llm edit`. The model returns patches instead of a new table, and the patches are shown
/// as a diff before the plan is applied.
pub struct LlmEdit(pub Arc<LlmRuntime>);

impl Command for LlmEdit {
    fn signature(&self) -> Signature {
        with_llm_flags(Signature::build("llm edit"))
            .desc(
                "Change field values of the input with a language model. The changes are shown as a diff for approval.",
            )
            .input(Type::any_table())
            .output(Type::any_table())
            .required("instruction", Type::String, "what to change")
            .example(
                "open contacts.json | llm edit \"normalise all phone numbers to E.164\" | save -f contacts.json",
                "reviewable edits",
            )
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let instruction = ctx.req_string(0, "instruction")?;
        check_send(ctx, &self.0)?;
        let mut rows: Vec<Value> = input.into_stream().collect::<Result<_>>()?;
        let schema = json!({
            "type": "object",
            "properties": {"patches": {"type": "array", "items": {
                "type": "object",
                "properties": {"i": {"type": "integer"}, "field": {"type": "string"}, "value": {"type": "string"}},
                "required": ["i", "field", "value"],
                "additionalProperties": false,
            }}},
            "required": ["patches"],
            "additionalProperties": false,
        });
        let prompt = format!(
            "Instruction: {instruction}\nApply the instruction by changing field values. Return one patch per changed field \
             with the row index i, the field name and the new value as text. Leave out everything that needs no change.\n\nRows:\n{}",
            encode(&indexed_table(&rows), self.0.config().max_input_chars)
        );
        let response = ask(ctx, &self.0, &Request { system: STAGE_SYSTEM.into(), prompt, schema: Some(schema) })?;
        let patches = response.json.as_ref().and_then(|j| j["patches"].as_array()).cloned().unwrap_or_default();
        let mut diff = Vec::new();
        for patch in patches {
            let (Some(i), Some(field), Some(text)) =
                (patch["i"].as_u64(), patch["field"].as_str(), patch["value"].as_str())
            else {
                continue;
            };
            let Some(Value::Record(record)) = rows.get_mut(i as usize) else { continue };
            let Some(old) = record.get(field).cloned() else { continue };
            let new = like(&old, text);
            if new == old {
                continue;
            }
            record.insert(field, new.clone());
            diff.push(Value::Record(
                Record::typed("llm.patch").with("row", i as i64).with("field", field).with("old", old).with("new", new),
            ));
        }
        if !diff.is_empty() {
            ctx.review(Review { title: format!("llm edit changes {} values", diff.len()), diff: Value::List(diff) });
        }
        Ok(PipelineData::Value(Value::List(rows)))
    }
}

/// A new value of the same type as the old one, where the text allows it.
fn like(old: &Value, text: &str) -> Value {
    match old {
        Value::String(_) | Value::Nothing => Value::String(text.to_string()),
        Value::Bool(_) => Value::Bool(text.eq_ignore_ascii_case("true")),
        Value::Date(_) => {
            chrono::DateTime::parse_from_rfc3339(text).map_or_else(|_| Value::String(text.to_string()), Value::Date)
        }
        _ => parse_literal(text.trim()).unwrap_or_else(|| Value::String(text.to_string())),
    }
}
