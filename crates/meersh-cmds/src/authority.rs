//! Commands that make authority visible and adjustable. Plans, the ledger, the journal,
//! policies, leases, tasks, restricted blocks and sandboxes are all plain data.

use std::path::PathBuf;
use std::sync::Arc;

use meersh_engine::authority::{Restrictions, Rule, Sandbox, absolute, project_root};
use meersh_engine::parser::Expr;
use meersh_engine::util::expand_path;
use meersh_engine::value::{Record, Type, Value};
use meersh_engine::{Cap, Command, Ctx, Effect, Engine, Permit, PipelineData, Principal, Result, Signature, Verdict};

pub fn register(engine: &mut Engine) {
    engine.register(PlanCmd);
    engine.register(PolicyList);
    for verdict in [Verdict::Allow, Verdict::Ask, Verdict::Deny] {
        engine.register(PolicyAdd(verdict));
    }
    engine.register(PolicyRemove);
    engine.register(PolicyBudget);
    engine.register(PolicyExplain);
    engine.register(LedgerCmd);
    engine.register(Leases);
    engine.register(Journal);
    engine.register(Lease);
    engine.register(Release);
    engine.register(Task);
    engine.register(TaskStop);
    engine.register(TaskList);
    engine.register(With);
    engine.register(SandboxCmd);
    engine.register(SandboxCommit);
    engine.register(SandboxDiscard);
    engine.register(SandboxList);
}

/// Commands that change the rules or the session for everyone. A model must never grant
/// itself rights, so they refuse every model principal, also when called through `source`.
pub(crate) fn humans_only(ctx: &Ctx) -> Result<()> {
    if ctx.principal().is_model() {
        return Err(
            ctx.err(format!("{} may not change policies or the session, only the user can", ctx.principal().name()))
        );
    }
    Ok(())
}

fn block<'a>(ctx: &'a Ctx, index: usize) -> Result<&'a meersh_engine::parser::Pipeline> {
    match ctx.positional(index) {
        Some(Expr::Block(pipeline)) => Ok(pipeline),
        _ => Err(ctx.err("expects a block { ... }")),
    }
}

fn list(values: Vec<Value>) -> PipelineData {
    PipelineData::Value(Value::List(values))
}

struct PlanCmd;

impl Command for PlanCmd {
    fn signature(&self) -> Signature {
        Signature::build("plan")
            .desc("Run a block without changing anything and return the actions it would take, with their decisions.")
            .input(Type::Nothing)
            .output(Type::table("plan.action"))
            .required("block", Type::Block, "the pipeline to plan")
            .example("plan { ls | where size > 5mb | rm }", "what would be removed, and whether it is allowed")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let expr = ctx.positional(0).ok_or_else(|| ctx.err("expects a block { ... }"))?;
        let plan = ctx.engine.plan_block(ctx.stack, expr)?;
        Ok(list(plan.actions.iter().map(|a| a.to_value()).collect()))
    }
}

struct PolicyList;

const DEFAULTS: [(&str, &str); 7] = [
    (
        "protected paths",
        "deny for models, ask the user (/, home and its parents, .git, ~/.ssh, ~/.gnupg, system directories)",
    ),
    ("owned", "allow what this session created and nobody changed since"),
    ("chosen by a model", "ask the user before acting on targets an LLM stage picked"),
    ("model, inside the project", "ask for reversible actions, allow new files"),
    ("model, outside the project", "deny"),
    ("model, irreversible", "deny unless owned, ask for signals"),
    ("model budget", "at most 50 actions per plan, see policy budget"),
];

impl Command for PolicyList {
    fn signature(&self) -> Signature {
        Signature::build("policy")
            .desc(
                "List the policy rules. User rules come first and the first match decides, then the built in defaults.",
            )
            .input(Type::Nothing)
            .output(Type::table("policy.rule"))
            .effect(Effect::Read)
            .example("policy | where verdict == deny", "everything that is refused")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let mut rows: Vec<Value> = ctx
            .engine
            .authority
            .rules()
            .iter()
            .enumerate()
            .map(|(i, r)| {
                Value::Record(
                    Record::typed("policy.rule")
                        .with("index", i as i64)
                        .with("verdict", r.verdict.as_str())
                        .with("principal", r.principal.as_str())
                        .with("cap", r.cap.as_str())
                        .with("condition", r.condition.as_ref().map(ToString::to_string))
                        .with("task", r.task.clone())
                        .with("source", "user"),
                )
            })
            .collect();
        for (name, what) in DEFAULTS {
            rows.push(Value::Record(
                Record::typed("policy.rule")
                    .with("index", Value::Nothing)
                    .with("verdict", what)
                    .with("principal", name)
                    .with("cap", "*")
                    .with("condition", Value::Nothing)
                    .with("task", Value::Nothing)
                    .with("source", "builtin"),
            ));
        }
        Ok(list(rows))
    }
}

struct PolicyAdd(Verdict);

const PRINCIPALS: &str = "*, model, user, plan, actor, agent:* or agent:<name>";

fn valid_principal(text: &str) -> bool {
    matches!(text, "*" | "model" | "agent:*") || Principal::parse(text).is_some()
}

impl Command for PolicyAdd {
    fn signature(&self) -> Signature {
        let verdict = self.0.as_str();
        Signature::build(&format!("policy {verdict}"))
            .desc(&format!(
                "Add a rule that decides {verdict} for matching requests. The condition sees the request as a row with owned, \
                 changed, reversible, in_project, protected, chosen_by_llm, leased_by, size, path, to, pid, program and task."
            ))
            .input(Type::Nothing)
            .output(Type::record("policy.rule"))
            .required("principal", Type::String, PRINCIPALS)
            .required("cap", Type::String, "fs.create, fs.write, fs.delete, fs.move, fs.chmod, proc.spawn, proc.signal, exec, llm.send, fs.* or *")
            .optional("condition", Type::Any, "a row condition such as `owned` or `reversible and in_project`")
            .named("task", Type::String, None, "the rule ends with this task")
            .effect(Effect::Write)
            .example("policy allow actor fs.delete reversible and in_project", "let a model trash inside the project")
            .example("policy deny actor llm.send not local", "no data to cloud models from actors")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        humans_only(ctx)?;
        let principal = ctx.req_string(0, "principal")?;
        if !valid_principal(&principal) {
            return Err(ctx.err(format!("unknown principal '{principal}', use {PRINCIPALS}")));
        }
        let cap = ctx.req_string(1, "cap")?;
        let condition = ctx.positional(2).cloned();
        let task = ctx.named_string("task")?;
        let mut text = format!("{} {principal} {cap}", self.0.as_str());
        if let Some(c) = &condition {
            text.push_str(&format!(" {c}"));
        }
        let rule = Rule {
            verdict: self.0,
            principal: principal.clone(),
            cap: cap.clone(),
            condition,
            task,
            text: text.clone(),
        };
        ctx.engine.authority.add_rule(rule);
        let index = ctx.engine.authority.rules().len() - 1;
        Ok(PipelineData::Value(Value::Record(
            Record::typed("policy.rule").with("index", index as i64).with("rule", text),
        )))
    }
}

struct PolicyRemove;

impl Command for PolicyRemove {
    fn signature(&self) -> Signature {
        Signature::build("policy remove")
            .desc("Remove a user rule by its index from `policy`.")
            .input(Type::Nothing)
            .output(Type::Nothing)
            .required("index", Type::Int, "rule index")
            .effect(Effect::Write)
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        humans_only(ctx)?;
        let index = ctx.req(0, "index")?.as_int().ok_or_else(|| ctx.err("index must be an int"))?;
        if !ctx.engine.authority.remove_rule(index as usize) {
            return Err(ctx.err(format!("no rule {index}")));
        }
        Ok(PipelineData::Empty)
    }
}

struct PolicyBudget;

impl Command for PolicyBudget {
    fn signature(&self) -> Signature {
        Signature::build("policy budget")
            .desc("Limit how many actions one plan of a principal may contain. Models default to 50.")
            .input(Type::Nothing)
            .output(Type::Nothing)
            .required("principal", Type::String, "plan, actor, agent:<name> or model")
            .required("actions", Type::Int, "maximum number of actions per plan")
            .effect(Effect::Write)
            .example("policy budget actor 10", "an actor may change at most ten things at once")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        humans_only(ctx)?;
        let principal = ctx.req_string(0, "principal")?;
        let actions = ctx.req(1, "actions")?.as_int().ok_or_else(|| ctx.err("actions must be an int"))?;
        ctx.engine.authority.set_budget(&principal, actions.max(0) as usize);
        Ok(PipelineData::Empty)
    }
}

struct PolicyExplain;

impl Command for PolicyExplain {
    fn signature(&self) -> Signature {
        Signature::build("policy explain")
            .desc("Show how a request would be decided and which rule or default decides it.")
            .input(Type::Nothing)
            .output(Type::record("policy.decision"))
            .required(
                "cap",
                Type::String,
                "fs.create, fs.write, fs.delete, fs.move, fs.chmod, proc.signal, proc.spawn or exec",
            )
            .required("target", Type::String, "a path, a pid or a program")
            .named("principal", Type::String, Some('p'), "whose request, the current principal by default")
            .named("to", Type::String, None, "destination for fs.move")
            .switch("permanent", None, "fs.delete without the trash")
            .example("policy explain fs.delete thesis.tex --principal actor", "may a model trash my thesis?")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let kind = ctx.req_string(0, "cap")?;
        let target = ctx.req_string(1, "target")?;
        let path = || expand_path(&target);
        let cap = match kind.as_str() {
            "fs.create" => Cap::FsCreate { path: path() },
            "fs.write" => Cap::FsWrite { path: path() },
            "fs.delete" => Cap::FsDelete { path: path(), permanent: ctx.switch("permanent") },
            "fs.move" => Cap::FsMove {
                from: path(),
                to: expand_path(&ctx.named_string("to")?.ok_or_else(|| ctx.err("fs.move needs --to"))?),
            },
            "fs.chmod" => Cap::FsChmod { path: path(), mode: 0o644 },
            "proc.signal" => Cap::ProcSignal {
                pid: target.parse().map_err(|_| ctx.err("the target of proc.signal is a pid"))?,
                signal: "SIGTERM".into(),
            },
            "proc.spawn" => Cap::ProcSpawn { program: target.clone() },
            "exec" => Cap::Exec { program: target.clone(), args: vec![] },
            other => return Err(ctx.err(format!("unknown capability '{other}'"))),
        };
        let stack = match ctx.named_string("principal")? {
            Some(p) => ctx
                .stack
                .with_principal(Principal::parse(&p).ok_or_else(|| ctx.err(format!("unknown principal '{p}'")))?),
            None => ctx.stack.clone(),
        };
        Ok(PipelineData::Value(ctx.engine.explain(&stack, cap)?))
    }
}

struct LedgerCmd;

impl Command for LedgerCmd {
    fn signature(&self) -> Signature {
        Signature::build("ledger")
            .desc("Everything meersh sessions and agents created that still exists, with who created it and whether it changed since.")
            .input(Type::Nothing)
            .output(Type::table("ledger.entry"))
            .effect(Effect::Read)
            .example("ledger | where principal =~ agent", "what agents left behind")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        Ok(list(ctx.engine.authority.ledger.rows()))
    }
}

struct Leases;

impl Command for Leases {
    fn signature(&self) -> Signature {
        Signature::build("leases")
            .desc("Live leases on files, so several agents in one project do not overwrite each other.")
            .input(Type::Nothing)
            .output(Type::table("lease"))
            .effect(Effect::Read)
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        Ok(list(ctx.engine.authority.ledger.lease_rows()))
    }
}

struct Lease;

impl Command for Lease {
    fn signature(&self) -> Signature {
        Signature::build("lease")
            .desc("Lease a file or directory. Other principals are asked before they change, move or delete it.")
            .input(Type::Nothing)
            .output(Type::record("lease"))
            .required("path", Type::String, "file or directory")
            .named("minutes", Type::Int, Some('m'), "how long, 30 by default")
            .effect(Effect::Write)
            .example("lease src/parser.rs --minutes 60", "keep other agents off the parser for an hour")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let path = absolute(&expand_path(&ctx.req_string(0, "path")?));
        let minutes = ctx.named("minutes")?.and_then(|v| v.as_int()).unwrap_or(30);
        let until = ctx.engine.authority.ledger.lease(&path, ctx.principal(), minutes);
        let record = Record::typed("lease")
            .with("path", path.display().to_string())
            .with("holder", ctx.principal().name())
            .with("until", chrono::DateTime::parse_from_rfc3339(&until).map_or(Value::Nothing, Value::Date));
        Ok(PipelineData::Value(Value::Record(record)))
    }
}

struct Release;

impl Command for Release {
    fn signature(&self) -> Signature {
        Signature::build("release")
            .desc("Give up a lease.")
            .input(Type::Nothing)
            .output(Type::Bool)
            .required("path", Type::String, "the leased path")
            .effect(Effect::Write)
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let path = absolute(&expand_path(&ctx.req_string(0, "path")?));
        Ok(PipelineData::Value(Value::Bool(ctx.engine.authority.ledger.release(&path, ctx.principal()))))
    }
}

struct Journal;

impl Command for Journal {
    fn signature(&self) -> Signature {
        Signature::build("journal")
            .desc("Every action of this session with its principal and whether it can be undone. Pipe rows into undo.")
            .input(Type::Nothing)
            .output(Type::table("journal.entry"))
            .effect(Effect::Read)
            .example("journal | where principal == actor | undo", "take back everything a model did")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        Ok(list(ctx.engine.authority.journal().iter().map(|e| e.to_value()).collect()))
    }
}

struct Task;

impl Command for Task {
    fn signature(&self) -> Signature {
        Signature::build("task")
            .desc("Run a block as a named task. What it creates and starts is recorded under the task and can be stopped or cleaned up together.")
            .input(Type::Any)
            .output(Type::Any)
            .required("name", Type::String, "task name")
            .required("block", Type::Block, "what the task does")
            .example("task dev { spawn python3 -m http.server }", "start a server as part of the task dev")
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let name = ctx.req_string(0, "name")?;
        let stack = ctx.stack.with_task(&name);
        ctx.engine.run_pipeline(&stack, block(ctx, 1)?, input, ctx.to_terminal)
    }
}

struct TaskStop;

impl Command for TaskStop {
    fn signature(&self) -> Signature {
        Signature::build("task stop")
            .desc(
                "Stop every job of a task. Jobs in a systemd scope are stopped together with everything they started.",
            )
            .input(Type::Nothing)
            .output(Type::table("proc.signal"))
            .required("name", Type::String, "task name")
            .effect(Effect::Destructive)
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let name = ctx.req_string(0, "name")?;
        let targets: Vec<(u32, Option<String>, String)> = ctx
            .engine
            .jobs
            .lock()
            .expect("jobs lock")
            .iter_mut()
            .filter(|j| j.task.as_deref() == Some(name.as_str()))
            .filter_map(|j| {
                let running = j.child.as_mut().is_some_and(|c| c.try_wait().ok().flatten().is_none());
                running.then(|| (j.pid, j.unit.clone(), j.command.clone()))
            })
            .collect();
        let mut rows = Vec::new();
        for (pid, unit, command) in targets {
            let cap = || Cap::ProcSignal { pid, signal: "SIGTERM".into() };
            let outcome = match ctx.authorize(cap())? {
                Permit::Execute => {
                    let ok = match &unit {
                        Some(unit) => std::process::Command::new("systemctl")
                            .args(["--user", "stop", &format!("{unit}.scope")])
                            .status()
                            .is_ok_and(|s| s.success()),
                        None => nix::sys::signal::kill(
                            nix::unistd::Pid::from_raw(pid as i32),
                            nix::sys::signal::Signal::SIGTERM,
                        )
                        .is_ok(),
                    };
                    ctx.performed(cap())?;
                    if ok { "stopped".to_string() } else { "failed".to_string() }
                }
                Permit::Plan => "would stop".into(),
                Permit::Skip(why) => format!("refused, {why}"),
            };
            rows.push(Value::Record(
                Record::typed("proc.signal")
                    .with("pid", i64::from(pid))
                    .with("command", command)
                    .with("unit", unit)
                    .with("result", outcome),
            ));
        }
        Ok(list(rows))
    }
}

struct TaskList;

impl Command for TaskList {
    fn signature(&self) -> Signature {
        Signature::build("task list")
            .desc("Tasks with the number of objects they own and the jobs that still run.")
            .input(Type::Nothing)
            .output(Type::table("task"))
            .effect(Effect::Read)
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let mut tasks: Vec<(String, i64, i64)> = Vec::new();
        let mut bump = |name: &str, objects: i64, jobs: i64| match tasks.iter_mut().find(|t| t.0 == name) {
            Some(t) => {
                t.1 += objects;
                t.2 += jobs;
            }
            None => tasks.push((name.to_string(), objects, jobs)),
        };
        for row in ctx.engine.authority.ledger.rows() {
            if let Some(task) = row.follow("task").and_then(|t| t.as_str().map(str::to_string)) {
                bump(&task, 1, 0);
            }
        }
        for job in ctx.engine.jobs.lock().expect("jobs lock").iter_mut() {
            let running = job.child.as_mut().is_some_and(|c| c.try_wait().ok().flatten().is_none());
            if let (Some(task), true) = (&job.task, running) {
                bump(task, 0, 1);
            }
        }
        Ok(list(
            tasks
                .into_iter()
                .map(|(name, objects, jobs)| {
                    Value::Record(
                        Record::typed("task").with("task", name).with("objects", objects).with("running_jobs", jobs),
                    )
                })
                .collect(),
        ))
    }
}

struct With;

impl Command for With {
    fn signature(&self) -> Signature {
        Signature::build("with")
            .desc(
                "Run a block with less authority. fs.write limits where builtins and programs may write, net: none cuts \
                 TCP. Programs inside are confined by Landlock. Nested blocks can only narrow further.",
            )
            .input(Type::Any)
            .output(Type::Any)
            .required("limits", Type::Record { name: None, fields: vec![] }, "{fs.write: <path or list>, net: none}")
            .required("block", Type::Block, "what runs with the limits")
            .example("with {fs.write: ./out, net: none} { ^make }", "make may only write ./out and has no network")
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let limits = ctx.req(0, "limits")?;
        let record = limits.as_record().ok_or_else(|| ctx.err("limits must be a record such as {fs.write: ./out}"))?;
        let write = record.get("fs.write").map(|v| {
            let paths: Vec<String> = match v {
                Value::List(items) => items.iter().map(Value::to_text).collect(),
                other => vec![other.to_text()],
            };
            paths.iter().map(|p| absolute(&expand_path(p))).collect::<Vec<PathBuf>>()
        });
        let no_net = record.get("net").is_some_and(|v| v.to_text() == "none");
        for key in record.fields.keys() {
            if key != "fs.write" && key != "net" {
                return Err(ctx.err(format!("unknown limit '{key}', use fs.write and net")));
            }
        }
        let stack = ctx.stack.with_restrictions(Restrictions { write, no_net });
        ctx.engine.run_pipeline(&stack, block(ctx, 1)?, input, ctx.to_terminal)
    }
}

struct SandboxCmd;

impl Command for SandboxCmd {
    fn signature(&self) -> Signature {
        Signature::build("sandbox")
            .desc(
                "Run the external programs of a block in a copy on write overlay of the project. Nothing changes until \
                 `sandbox commit`. The result lists what would change.",
            )
            .input(Type::Any)
            .output(Type::table("sandbox.change"))
            .required("block", Type::Block, "the programs to try")
            .effect(Effect::Read)
            .example("sandbox { ^npm install }", "see what npm install would change, then commit or discard")
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let sandbox = Arc::new(Sandbox::create(&project_root())?);
        ctx.engine.authority.add_sandbox(sandbox.clone());
        let stack = ctx.stack.with_sandbox(sandbox.clone());
        let output = ctx.engine.run_pipeline(&stack, block(ctx, 0)?, input, ctx.to_terminal)?.into_value()?;
        let changes = Value::List(sandbox.changes());
        if matches!(output, Value::Nothing) {
            return Ok(PipelineData::Value(changes));
        }
        let record = Record::typed("sandbox.result")
            .with("sandbox", sandbox.id.as_str())
            .with("output", output)
            .with("changes", changes);
        Ok(PipelineData::Value(Value::Record(record)))
    }
}

fn pick_sandboxes(ctx: &Ctx) -> Result<Vec<Arc<Sandbox>>> {
    let id = ctx.opt_string(0)?;
    let all = ctx.engine.authority.take_sandboxes();
    let (picked, keep): (Vec<_>, Vec<_>) = all.into_iter().partition(|s| id.as_deref().is_none_or(|id| s.id == id));
    for s in keep {
        ctx.engine.authority.add_sandbox(s);
    }
    if picked.is_empty() {
        return Err(ctx.err("no pending sandbox"));
    }
    Ok(picked)
}

struct SandboxCommit;

impl Command for SandboxCommit {
    fn signature(&self) -> Signature {
        Signature::build("sandbox commit")
            .desc("Apply the changes of pending sandboxes to the project. New files become owned, deletions go to the trash.")
            .input(Type::Nothing)
            .output(Type::table("fs.change"))
            .optional("id", Type::String, "one sandbox, all pending ones by default")
            .effect(Effect::Write)
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        humans_only(ctx)?;
        let mut rows = Vec::new();
        for sandbox in pick_sandboxes(ctx)? {
            rows.extend(sandbox.commit(&ctx.engine.authority, ctx.stack)?);
        }
        Ok(list(rows))
    }
}

struct SandboxDiscard;

impl Command for SandboxDiscard {
    fn signature(&self) -> Signature {
        Signature::build("sandbox discard")
            .desc("Throw away pending sandboxes.")
            .input(Type::Nothing)
            .output(Type::Int)
            .optional("id", Type::String, "one sandbox, all pending ones by default")
            .effect(Effect::Write)
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        humans_only(ctx)?;
        let picked = pick_sandboxes(ctx)?;
        for sandbox in &picked {
            sandbox.discard();
        }
        Ok(PipelineData::Value(Value::Int(picked.len() as i64)))
    }
}

struct SandboxList;

impl Command for SandboxList {
    fn signature(&self) -> Signature {
        Signature::build("sandbox list")
            .desc("Pending sandboxes and how many changes each holds.")
            .input(Type::Nothing)
            .output(Type::table("sandbox"))
            .effect(Effect::Read)
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        Ok(list(
            ctx.engine
                .authority
                .pending_sandboxes()
                .iter()
                .map(|s| {
                    Value::Record(
                        Record::typed("sandbox")
                            .with("id", s.id.as_str())
                            .with("over", s.lower.display().to_string())
                            .with("changes", s.changes().len() as i64),
                    )
                })
                .collect(),
        ))
    }
}
