//! The tools every agent gets, and the one path all pipelines take: plan, decide, apply.

use std::sync::{Arc, Mutex};

use meersh_engine::authority::{Plan, absolute};
use meersh_engine::parser::Statement;
use meersh_engine::util::expand_path;
use meersh_engine::{Consent, Engine, ExecMode, PlannedAction, Signature, TypeInfo, Verdict};
use serde_json::{Map, Value as Json, json};

use crate::typed::{self, TypedTool};
use crate::{Outcome, Server};

/// Commands that change who may do what. They do not authorize anything themselves, so an
/// agent could otherwise grant itself rights. They stay with the user.
pub(crate) const USER_ONLY: &[&str] = &["policy allow", "policy ask", "policy deny", "policy remove", "policy budget"];

const MAX_LEASE_MINUTES: i64 = 24 * 60;

/// How many blocked actions the hint names before it summarises the rest.
const HINT_ACTIONS: usize = 10;

fn object(properties: Json, required: &[&str]) -> Json {
    let mut schema = json!({"type": "object", "properties": properties, "additionalProperties": false});
    if !required.is_empty() {
        schema["required"] = json!(required);
    }
    schema
}

fn pipeline_schema() -> Json {
    object(
        json!({"pipeline": {
            "type": "string",
            "description": "meersh source, for example `ls src | where size > 10kb | sort-by size`"
        }}),
        &["pipeline"],
    )
}

fn definitions(me: &str) -> Vec<Json> {
    vec![
        json!({
            "name": "run",
            "description": format!(
                "Run a meersh pipeline as {me} and return its result as JSON. Several statements return a list of \
                 results. Every pipeline is planned first. When an action needs the user, nothing changes and the \
                 result is {{applied: false, plan, hint}}. Use help to look up commands."
            ),
            "inputSchema": pipeline_schema(),
        }),
        json!({
            "name": "plan",
            "description": "Show the actions a pipeline would take, without changing anything. Each action has a \
                            decision (allow, ask or deny) and the reason. needs_user is true when run would stop \
                            and return the plan.",
            "inputSchema": pipeline_schema(),
        }),
        json!({
            "name": "help",
            "description": "Without a name, list the commands and record types. With a name, describe one command \
                            (params, flags, effect, examples) or one record type (fields and methods).",
            "inputSchema": object(
                json!({"name": {"type": "string", "description": "a command such as `sort-by` or `llm map`, or a type such as `fs.entry`"}}),
                &[],
            ),
        }),
        json!({
            "name": "ledger",
            "description": format!(
                "What meersh recorded as created, split into mine ({me}) and others. state owned means unchanged \
                 since it was created. Also lists the live leases."
            ),
            "inputSchema": object(json!({}), &[]),
        }),
        json!({
            "name": "journal",
            "description": "Every action this server applied, with principal, target and whether it can be undone.",
            "inputSchema": object(json!({}), &[]),
        }),
        json!({
            "name": "lease",
            "description": "Lease a file or directory. Other principals are asked before they change, move or delete \
                            it. Lease what you edit and release it when you are done.",
            "inputSchema": object(
                json!({
                    "path": {"type": "string", "description": "file or directory, absolute or relative to the working directory"},
                    "minutes": {"type": "integer", "minimum": 1, "maximum": MAX_LEASE_MINUTES, "default": 30, "description": "how long the lease lasts"},
                }),
                &["path"],
            ),
        }),
        json!({
            "name": "release",
            "description": "Give up a lease that you hold.",
            "inputSchema": object(json!({"path": {"type": "string", "description": "the leased path"}}), &["path"]),
        }),
        json!({
            "name": "read_file",
            "description": "Read a text file and return its content.",
            "inputSchema": object(json!({"path": {"type": "string", "description": "file to read"}}), &["path"]),
        }),
        json!({
            "name": "write_file",
            "description": format!(
                "Create a file, or overwrite one. Creating inside the project is allowed. Overwriting keeps a \
                 snapshot that undo restores, and overwriting a file that {me} did not create asks the user unless a \
                 policy allows it."
            ),
            "inputSchema": object(
                json!({
                    "path": {"type": "string", "description": "file to write"},
                    "content": {"type": "string", "description": "the complete new content"},
                }),
                &["path", "content"],
            ),
        }),
        json!({
            "name": "edit_file",
            "description": "Replace text in a file. old must occur exactly once unless all is true. Every edit keeps \
                            a snapshot that undo restores.",
            "inputSchema": object(
                json!({
                    "path": {"type": "string", "description": "file to edit"},
                    "old": {"type": "string", "description": "the exact text to replace, with enough context to be unique"},
                    "new": {"type": "string", "description": "the replacement"},
                    "all": {"type": "boolean", "default": false, "description": "replace every occurrence"},
                }),
                &["path", "old", "new"],
            ),
        }),
    ]
}

/// The pipeline behind one of the file tools. All arguments are quoted, so content can
/// never become code.
fn file_pipeline(name: &str, args: &Json) -> Result<String, Outcome> {
    let path = typed::quote(required_string(args, "path")?);
    Ok(match name {
        "read_file" => format!("open --raw {path}"),
        "write_file" => {
            let content = typed::quote(required_string(args, "content")?);
            format!("{content} | save --force {path}")
        }
        _ => {
            let (old, new) = (typed::quote(required_string(args, "old")?), typed::quote(required_string(args, "new")?));
            let all = if args.get("all").and_then(Json::as_bool) == Some(true) { " --all" } else { "" };
            format!("edit {path} {old} {new}{all}")
        }
    })
}

fn string_arg<'a>(args: &'a Json, name: &str) -> Result<Option<&'a str>, Outcome> {
    match args.get(name) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::String(s)) => Ok(Some(s)),
        Some(_) => Err(Outcome::error(format!("'{name}' must be a string"))),
    }
}

fn required_string<'a>(args: &'a Json, name: &str) -> Result<&'a str, Outcome> {
    string_arg(args, name)?.ok_or_else(|| Outcome::error(format!("missing argument '{name}'")))
}

/// The consent of an absent human. Only a plan in which every action is allowed goes through.
fn needs_user(plan: &Plan) -> bool {
    plan.actions.iter().any(|a| a.verdict != Verdict::Allow)
}

fn action_json(action: &PlannedAction) -> Json {
    let mut json = action.to_value().to_json();
    if let Json::Object(map) = &mut json {
        map.insert("cap".into(), json!(action.kind));
    }
    json
}

/// The plan as JSON, with notes and reviews only when there are some.
fn plan_json(plan: &Plan) -> Map<String, Json> {
    let mut map = Map::new();
    map.insert("plan".into(), plan.actions.iter().map(action_json).collect());
    if !plan.notes.is_empty() {
        let notes = plan
            .notes
            .iter()
            .map(|n| json!({"principal": n.principal, "pipeline": n.pipeline, "explanation": n.explanation}))
            .collect();
        map.insert("notes".into(), Json::Array(notes));
    }
    if !plan.reviews.is_empty() {
        let reviews = plan.reviews.iter().map(|r| json!({"title": r.title, "diff": r.diff.to_json()})).collect();
        map.insert("reviews".into(), Json::Array(reviews));
    }
    map
}

fn hint(plan: &Plan, me: &str) -> String {
    let blocked: Vec<&PlannedAction> = plan.actions.iter().filter(|a| a.verdict != Verdict::Allow).collect();
    let mut named: Vec<String> = blocked
        .iter()
        .take(HINT_ACTIONS)
        .map(|a| match a.reason.as_str() {
            "" => format!("{} {} ({})", a.action, a.target, a.verdict.as_str()),
            why => format!("{} {} ({}, {why})", a.action, a.target, a.verdict.as_str()),
        })
        .collect();
    if blocked.len() > HINT_ACTIONS {
        named.push(format!("{} more", blocked.len() - HINT_ACTIONS));
    }
    let (subject, verb) = if blocked.len() == 1 { ("This action", "needs") } else { ("These actions", "need") };
    let kind = blocked.first().map_or("fs.delete", |a| a.kind.as_str());
    let mut text = format!(
        "Nothing was changed. {subject} {verb} the user: {}. Show the plan to the user. The user can run the \
         pipeline in meersh and approve it there, or allow it for you with a rule such as \
         `policy allow {me} {kind} <condition>`.",
        named.join(", ")
    );
    if blocked.iter().any(|a| a.reason.starts_with("protected")) {
        text.push_str(" No rule opens a protected path.");
    }
    if blocked.iter().any(|a| a.reason.contains("budget")) {
        text.push_str(
            " A rule does not help with actions over the budget. The user can raise it with `policy budget`.",
        );
    }
    text
}

/// The first command in the source that only the user may run.
fn user_only_command(engine: &Engine, src: &str) -> Option<String> {
    // A parse error is reported by the run itself.
    let script = engine.parse(src).ok()?;
    let mut found = None;
    for statement in &script.statements {
        let pipeline = match statement {
            Statement::Let { pipeline, .. } | Statement::Pipeline(pipeline) | Statement::Background(pipeline) => {
                pipeline
            }
        };
        pipeline.walk_commands(&mut |call| {
            if !call.external && USER_ONLY.contains(&call.name.as_str()) {
                found.get_or_insert_with(|| call.name.clone());
            }
        });
    }
    found
}

fn refuse_user_only(name: &str) -> Outcome {
    Outcome::error(format!(
        "{name} changes the policy, and only the user may do that. Ask the user to add the rule in meersh."
    ))
}

fn signature_json(sig: &Signature, tool: Option<&str>) -> Json {
    let param = |p: &meersh_engine::Param| json!({"name": p.name, "type": p.ty.to_string(), "optional": p.optional, "description": p.desc});
    let mut json = json!({
        "name": sig.name,
        "usage": sig.usage(),
        "description": sig.description,
        "input": sig.input.to_string(),
        "output": sig.output.to_string(),
        "effect": sig.effect.as_str(),
        "params": sig.params.iter().map(param).collect::<Vec<_>>(),
        "flags": sig.flags.iter().map(|f| json!({
            "flag": format!("--{}", f.long),
            "short": f.short.map(|c| format!("-{c}")),
            "value": f.arg.as_ref().map(ToString::to_string),
            "description": f.desc,
        })).collect::<Vec<_>>(),
        "examples": sig.examples.iter().map(|(e, d)| json!({"example": e, "description": d})).collect::<Vec<_>>(),
    });
    if let Some(rest) = &sig.rest {
        json["rest"] = param(rest);
    }
    if let Some(tool) = tool {
        json["tool"] = json!(tool);
    }
    json
}

fn type_json(info: &TypeInfo, methods: &[Arc<Signature>]) -> Json {
    let prefix = format!("{}.", info.name);
    json!({
        "type": info.name,
        "description": info.description,
        "fields": info.fields.iter().map(|(name, ty, desc)| json!({"name": name, "type": ty.to_string(), "description": desc})).collect::<Vec<_>>(),
        "methods": methods.iter().map(|m| json!({
            "method": m.name.strip_prefix(&prefix).unwrap_or(&m.name),
            "usage": m.usage(),
            "effect": m.effect.as_str(),
            "description": m.description,
        })).collect::<Vec<_>>(),
    })
}

impl Server {
    pub(crate) fn tool_list(&self) -> Vec<Json> {
        let mut tools = definitions(&self.principal.name());
        tools.extend(typed::tools(&self.engine).iter().map(TypedTool::definition));
        tools
    }

    /// `None` when there is no tool of that name.
    pub(crate) fn call(&mut self, name: &str, args: &Json) -> Option<Outcome> {
        let outcome = match name {
            "run" => required_string(args, "pipeline").map(|src| self.execute(src, None)),
            "plan" => required_string(args, "pipeline").map(|src| self.plan(src)),
            "help" => string_arg(args, "name").map(|name| self.help(name)),
            "ledger" => Ok(self.ledger()),
            "journal" => {
                Ok(Outcome::json(self.engine.authority.journal().iter().map(|e| e.to_value().to_json()).collect()))
            }
            "lease" => self.lease(args),
            "release" => required_string(args, "path").map(|path| self.release(path)),
            "read_file" | "write_file" | "edit_file" => file_pipeline(name, args).map(|src| self.execute(&src, None)),
            other => {
                let tool = typed::find(&self.engine, other)?;
                tool.pipeline(args).map(|src| self.execute(&src, Some(&src))).map_err(Outcome::error)
            }
        };
        Some(outcome.unwrap_or_else(|e| e))
    }

    /// Plans the source, applies it when every action is allowed, and returns the plan
    /// otherwise. `generated` is the pipeline a typed tool built, so the agent can show it.
    fn execute(&mut self, src: &str, generated: Option<&str>) -> Outcome {
        if let Some(name) = user_only_command(&self.engine, src) {
            return refuse_user_only(&name);
        }
        let me = self.principal.name();
        let mut outputs = Vec::new();
        let mut rejected = None;
        let result = self.engine.run_planned(
            &mut self.stack,
            src,
            false,
            &mut |plan| {
                if !needs_user(plan) {
                    return Consent::Approve;
                }
                rejected = Some((plan_json(plan), hint(plan, &me)));
                Consent::Reject
            },
            &mut |data| {
                outputs.push(data.into_value()?.to_json());
                Ok(())
            },
        );
        outputs.retain(|v| !v.is_null());
        if let Some((plan, mut hint)) = rejected {
            let mut map = Map::new();
            map.insert("applied".into(), json!(false));
            if let Some(src) = generated {
                map.insert("pipeline".into(), json!(src));
            }
            map.extend(plan);
            if !outputs.is_empty() {
                hint.push_str(" The statements before the stopped one ran. Their results are in output.");
                map.insert("output".into(), Json::Array(outputs));
            }
            map.insert("hint".into(), json!(hint));
            return Outcome::json(Json::Object(map));
        }
        match result {
            Err(e) => match generated {
                Some(src) => Outcome::error(format!("{e} (while running `{src}`)")),
                None => Outcome::error(e.to_string()),
            },
            Ok(()) if outputs.len() == 1 => Outcome::json(outputs.remove(0)),
            Ok(()) if outputs.is_empty() => Outcome::json(Json::Null),
            Ok(()) => Outcome::json(Json::Array(outputs)),
        }
    }

    /// Runs the whole source in plan mode on a copy of the stack, so not even a `let` sticks.
    fn plan(&mut self, src: &str) -> Outcome {
        if let Some(name) = user_only_command(&self.engine, src) {
            return refuse_user_only(&name);
        }
        let plan = Arc::new(Mutex::new(Plan::default()));
        let mut planning = self.stack.with_mode(ExecMode::Plan(plan.clone()));
        // Draining the output matters. Lazy streams only ask for authority as rows are pulled.
        if let Err(e) = self.engine.run_script(&mut planning, src, false, &mut |data| data.into_value().map(drop)) {
            return Outcome::error(e.to_string());
        }
        let plan = std::mem::take(&mut *plan.lock().expect("plan lock"));
        let mut map = plan_json(&plan);
        map.insert("needs_user".into(), json!(needs_user(&plan)));
        Outcome::json(Json::Object(map))
    }

    fn help(&self, name: Option<&str>) -> Outcome {
        let engine = &self.engine;
        let tools = typed::tools(engine);
        let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) else {
            let commands: Vec<Json> = engine
                .signatures()
                .iter()
                .map(|s| json!({"name": s.name, "effect": s.effect.as_str(), "description": s.description}))
                .collect();
            let types: Vec<Json> =
                engine.types().map(|t| json!({"name": t.name, "description": t.description})).collect();
            return Outcome::json(json!({"commands": commands, "types": types}));
        };
        // A tool name such as cmd_date_now works too.
        let command = tools.iter().find(|t| t.name == name).map_or(name, |t| t.command());
        if let Some(sig) = engine.signature(command) {
            let tool = tools.iter().find(|t| t.command() == sig.name).map(|t| t.name.as_str());
            return Outcome::json(signature_json(&sig, tool));
        }
        if let Some(info) = engine.type_info(name) {
            return Outcome::json(type_json(info, &engine.methods_of(name)));
        }
        Outcome::error(format!("no command or type named '{name}'. Call help without a name to list them."))
    }

    fn ledger(&self) -> Outcome {
        let ledger = &self.engine.authority.ledger;
        let me = self.principal.name();
        let (mine, others): (Vec<Json>, Vec<Json>) = ledger
            .rows()
            .iter()
            .map(|row| row.to_json())
            .partition(|row| row.get("principal").and_then(Json::as_str) == Some(me.as_str()));
        let leases: Vec<Json> = ledger.lease_rows().iter().map(|row| row.to_json()).collect();
        Outcome::json(json!({"agent": me, "mine": mine, "others": others, "leases": leases}))
    }

    fn lease(&self, args: &Json) -> Result<Outcome, Outcome> {
        let path = absolute(&expand_path(required_string(args, "path")?));
        let minutes = match args.get("minutes") {
            None | Some(Json::Null) => 30,
            Some(m) => m
                .as_i64()
                .filter(|m| (1..=MAX_LEASE_MINUTES).contains(m))
                .ok_or_else(|| Outcome::error(format!("'minutes' must be an integer from 1 to {MAX_LEASE_MINUTES}")))?,
        };
        let ledger = &self.engine.authority.ledger;
        let me = self.principal.name();
        if let Some((holder, until)) = ledger.lease_holder(&path).filter(|(holder, _)| *holder != me) {
            return Ok(Outcome::json(json!({
                "leased": false,
                "path": path.display().to_string(),
                "holder": holder,
                "until": until,
                "hint": "Someone else holds this lease. Work on something else, ask the user, or wait until it ends.",
            })));
        }
        let until = ledger.lease(&path, &self.principal, minutes);
        Ok(Outcome::json(json!({"leased": true, "path": path.display().to_string(), "holder": me, "until": until})))
    }

    fn release(&self, path: &str) -> Outcome {
        let path = absolute(&expand_path(path));
        let released = self.engine.authority.ledger.release(&path, &self.principal);
        Outcome::json(json!({"released": released, "path": path.display().to_string()}))
    }
}
