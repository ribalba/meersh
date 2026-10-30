use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, RwLock};

use meersh_parser::{Arg, BinOp, CommandCall, Element, Expr, Pipeline, Script, Statement};
use meersh_value::{Record, Type, Value, parse_literal};
use regex::Regex;

use crate::authority::{Authority, ExecMode, Ledger, Plan, Principal, Restrictions, Sandbox};
use crate::command::{Call, Command, Ctx, Effect, Signature};
use crate::{PipelineData, Result, ShellError, external};

#[derive(Clone)]
struct Registered {
    command: Arc<dyn Command>,
    signature: Arc<Signature>,
}

/// A record type the shell knows about. `help <type>` shows its fields and methods, and the
/// completer offers its fields after `where`, `select` and `sort-by`.
#[derive(Debug, Clone)]
pub struct TypeInfo {
    pub name: String,
    pub description: String,
    pub fields: Vec<(String, Type, String)>,
}

/// Turns the output of a well known program into typed records, in the spirit of jc. It
/// applies when the output of the program feeds a builtin, never when it goes to the terminal.
pub trait Adapter: Send + Sync {
    fn program(&self) -> &str;
    /// The arguments to run instead, or `None` when this invocation is not supported and
    /// the output should stay text.
    fn rewrite(&self, args: &[String]) -> Option<Vec<String>>;
    fn parse(&self, stdout: &str) -> std::result::Result<Vec<Value>, String>;
}

/// A program started with `&` or `spawn`.
pub struct Job {
    pub id: usize,
    pub command: String,
    pub pid: u32,
    pub child: Option<std::process::Child>,
    /// The systemd scope of a job started inside a task.
    pub unit: Option<String>,
    pub task: Option<String>,
}

/// The answer to a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Consent {
    /// Every action that is not refused.
    Approve,
    /// Only the actions at these positions of `plan.actions` that needed approval. Actions
    /// the policy allows need no approval and always stay in. Refused ones never go in.
    Only(Vec<usize>),
    Reject,
}

/// Shared state of a shell session. Built once at startup and then put behind an `Arc`.
/// What changes at run time (definitions, aliases, environment, authority) sits behind locks.
pub struct Engine {
    commands: BTreeMap<String, Registered>,
    /// Commands defined at run time with `def` or by plugins.
    dynamic: RwLock<BTreeMap<String, Registered>>,
    aliases: RwLock<BTreeMap<String, String>>,
    /// Changes to the environment made in the shell. Edition 2024 makes `set_var` unsafe,
    /// and a shell should not change its own environment while threads run anyway. `None`
    /// removes a variable.
    env: RwLock<BTreeMap<String, Option<String>>>,
    types: BTreeMap<String, TypeInfo>,
    adapters: BTreeMap<String, Arc<dyn Adapter>>,
    pub authority: Authority,
    /// The meersh binary. It runs external programs inside sandboxes and Landlock.
    pub exec_helper: Option<PathBuf>,
    pub jobs: Mutex<Vec<Job>>,
    /// Set by the Ctrl-C handler. Long running stages check it between rows.
    pub interrupt: Arc<AtomicBool>,
    pub last_exit: AtomicI32,
}

/// Variables and the authority context of the current scope. Cloned into lazy streams, so
/// keep it small.
#[derive(Debug, Clone)]
pub struct Stack {
    vars: HashMap<String, Value>,
    principal: Principal,
    mode: ExecMode,
    /// Set for every stage downstream of an LLM stage. The targets there were chosen by a model.
    llm_chosen: bool,
    task: Option<String>,
    restrictions: Option<Arc<Restrictions>>,
    sandbox: Option<Arc<Sandbox>>,
}

impl Default for Stack {
    fn default() -> Self {
        Stack {
            vars: HashMap::new(),
            principal: Principal::User,
            mode: ExecMode::Execute,
            llm_chosen: false,
            task: None,
            restrictions: None,
            sandbox: None,
        }
    }
}

impl Stack {
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.vars.get(name)
    }

    pub fn set(&mut self, name: &str, value: Value) {
        self.vars.insert(name.into(), value);
    }

    pub fn with(&self, name: &str, value: Value) -> Stack {
        let mut stack = self.clone();
        stack.set(name, value);
        stack
    }

    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    pub fn mode(&self) -> &ExecMode {
        &self.mode
    }

    pub fn is_planning(&self) -> bool {
        matches!(self.mode, ExecMode::Plan(_))
    }

    pub fn llm_chosen(&self) -> bool {
        self.llm_chosen
    }

    pub fn task(&self) -> Option<&str> {
        self.task.as_deref()
    }

    pub fn restrictions(&self) -> Option<&Restrictions> {
        self.restrictions.as_deref()
    }

    pub fn sandbox(&self) -> Option<&Arc<Sandbox>> {
        self.sandbox.as_ref()
    }

    pub fn with_principal(&self, principal: Principal) -> Stack {
        Stack { principal, ..self.clone() }
    }

    pub fn with_mode(&self, mode: ExecMode) -> Stack {
        Stack { mode, ..self.clone() }
    }

    pub fn with_task(&self, task: &str) -> Stack {
        Stack { task: Some(task.into()), ..self.clone() }
    }

    pub fn with_sandbox(&self, sandbox: Arc<Sandbox>) -> Stack {
        Stack { sandbox: Some(sandbox), ..self.clone() }
    }

    pub fn tainted(&self) -> Stack {
        Stack { llm_chosen: true, ..self.clone() }
    }

    /// Authority only narrows. A nested block can drop rights but never gain them back.
    pub fn with_restrictions(&self, new: Restrictions) -> Stack {
        let merged = match self.restrictions.as_deref() {
            None => new,
            Some(old) => Restrictions {
                write: match (&old.write, new.write) {
                    (None, new) => new,
                    (Some(old_roots), None) => Some(old_roots.clone()),
                    (Some(old_roots), Some(new_roots)) => {
                        Some(new_roots.into_iter().filter(|r| old_roots.iter().any(|o| r.starts_with(o))).collect())
                    }
                },
                no_net: old.no_net || new.no_net,
            },
        };
        Stack { restrictions: Some(Arc::new(merged)), ..self.clone() }
    }

    /// Policy conditions are plain expressions. They run outside any plan.
    pub(crate) fn for_policy(&self) -> Stack {
        Stack { mode: ExecMode::Execute, principal: Principal::User, ..self.clone() }
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    /// An engine with an in-memory ledger. The shell uses [`Engine::with_ledger`] with the
    /// shared one.
    pub fn new() -> Self {
        Self::with_ledger(Ledger::in_memory())
    }

    pub fn with_ledger(ledger: Ledger) -> Self {
        Engine {
            commands: BTreeMap::new(),
            dynamic: RwLock::default(),
            aliases: RwLock::default(),
            env: RwLock::default(),
            types: BTreeMap::new(),
            adapters: BTreeMap::new(),
            authority: Authority::new(ledger),
            exec_helper: None,
            jobs: Mutex::default(),
            interrupt: Arc::default(),
            last_exit: AtomicI32::new(0),
        }
    }

    pub fn register(&mut self, command: impl Command + 'static) {
        let signature = Arc::new(command.signature());
        self.commands.insert(signature.name.clone(), Registered { command: Arc::new(command), signature });
    }

    /// Adds or replaces a command while the shell runs, for `def` and plugins.
    pub fn define(&self, command: Arc<dyn Command>) {
        let signature = Arc::new(command.signature());
        self.dynamic
            .write()
            .expect("definitions lock")
            .insert(signature.name.clone(), Registered { command, signature });
    }

    pub fn undefine(&self, name: &str) -> bool {
        self.dynamic.write().expect("definitions lock").remove(name).is_some()
    }

    fn lookup(&self, name: &str) -> Option<Registered> {
        self.commands.get(name).cloned().or_else(|| self.dynamic.read().expect("definitions lock").get(name).cloned())
    }

    pub fn is_command(&self, name: &str) -> bool {
        self.commands.contains_key(name) || self.dynamic.read().expect("definitions lock").contains_key(name)
    }

    pub fn is_defined(&self, name: &str) -> bool {
        self.dynamic.read().expect("definitions lock").contains_key(name)
    }

    pub fn signature(&self, name: &str) -> Option<Arc<Signature>> {
        self.lookup(name).map(|r| r.signature)
    }

    pub fn signatures(&self) -> Vec<Arc<Signature>> {
        let mut all: BTreeMap<String, Arc<Signature>> =
            self.commands.iter().map(|(k, r)| (k.clone(), r.signature.clone())).collect();
        for (k, r) in self.dynamic.read().expect("definitions lock").iter() {
            all.insert(k.clone(), r.signature.clone());
        }
        all.into_values().collect()
    }

    pub fn register_type(&mut self, info: TypeInfo) {
        self.types.insert(info.name.clone(), info);
    }

    pub fn type_info(&self, name: &str) -> Option<&TypeInfo> {
        self.types.get(name)
    }

    pub fn types(&self) -> impl Iterator<Item = &TypeInfo> {
        self.types.values()
    }

    /// Methods of a record type are the commands named `<type>.<method>`.
    pub fn methods_of(&self, type_name: &str) -> Vec<Arc<Signature>> {
        let prefix = format!("{type_name}.");
        self.signatures()
            .into_iter()
            .filter(|s| s.name.strip_prefix(&prefix).is_some_and(|rest| !rest.contains(' ') && !rest.contains('.')))
            .collect()
    }

    pub fn register_adapter(&mut self, adapter: impl Adapter + 'static) {
        self.adapters.insert(adapter.program().to_string(), Arc::new(adapter));
    }

    pub fn adapter(&self, program: &str) -> Option<Arc<dyn Adapter>> {
        self.adapters.get(program).cloned()
    }

    pub fn adapters(&self) -> Vec<String> {
        self.adapters.keys().cloned().collect()
    }

    pub fn set_alias(&self, name: &str, expansion: &str) {
        self.aliases.write().expect("alias lock").insert(name.into(), expansion.into());
    }

    pub fn remove_alias(&self, name: &str) -> bool {
        self.aliases.write().expect("alias lock").remove(name).is_some()
    }

    pub fn aliases(&self) -> Vec<(String, String)> {
        self.aliases.read().expect("alias lock").iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    pub fn alias(&self, name: &str) -> Option<String> {
        self.aliases.read().expect("alias lock").get(name).cloned()
    }

    pub fn set_env(&self, name: &str, value: Option<String>) {
        self.env.write().expect("env lock").insert(name.into(), value);
    }

    /// Changes to apply to the environment of external programs.
    pub fn env_overlay(&self) -> BTreeMap<String, Option<String>> {
        self.env.read().expect("env lock").clone()
    }

    /// The environment as the shell sees it, process environment plus overlay.
    pub fn env_vars(&self) -> BTreeMap<String, String> {
        let mut vars: BTreeMap<String, String> = std::env::vars().collect();
        for (k, v) in self.env_overlay() {
            match v {
                Some(v) => vars.insert(k, v),
                None => vars.remove(&k),
            };
        }
        vars
    }

    pub fn parse(&self, src: &str) -> Result<Script> {
        Ok(meersh_parser::parse(src, &|name| self.is_command(name), &|name| self.alias(name))?)
    }

    pub fn check_interrupt(&self) -> Result<()> {
        if self.interrupt.swap(false, AtomicOrdering::Relaxed) { Err(ShellError::Interrupted) } else { Ok(()) }
    }

    /// The effect of one call. A method call such as `$it.delete()` is judged by the
    /// strongest command of that name on any type.
    fn call_effect(&self, call: &CommandCall) -> Effect {
        if call.external {
            return Effect::Unknown;
        }
        if let Some(sig) = self.signature(&call.name) {
            return sig.effect;
        }
        let suffix = format!(".{}", call.name);
        self.signatures()
            .iter()
            .filter(|s| s.name.ends_with(&suffix))
            .map(|s| s.effect)
            .max()
            .unwrap_or(Effect::Unknown)
    }

    /// The strongest effect of any command in the pipeline, nested blocks included.
    pub fn effect_of(&self, pipeline: &Pipeline) -> Effect {
        let mut effect = Effect::Pure;
        pipeline.walk_commands(&mut |call| effect = effect.max(self.call_effect(call)));
        effect
    }

    /// Whether a statement has to be planned before it runs. That is the case for anything a
    /// model wrote, and for every statement with a builtin that writes, deletes or calls a
    /// model. External programs typed by the user run directly, as in any other shell.
    pub fn needs_plan(&self, statement: &Statement, stack: &Stack) -> bool {
        if stack.principal().is_model() {
            return true;
        }
        let pipeline = match statement {
            Statement::Let { pipeline, .. } | Statement::Pipeline(pipeline) | Statement::Background(pipeline) => {
                pipeline
            }
        };
        let mut needs = false;
        pipeline.walk_commands(&mut |call| {
            needs |=
                !call.external && self.call_effect(call) >= Effect::Llm && self.call_effect(call) != Effect::Unknown;
        });
        needs
    }

    /// Runs every statement. The output of each pipeline goes to `sink`, which is where the
    /// REPL renders it. `let` statements bind their collected result instead.
    pub fn run_script(
        self: &Arc<Self>,
        stack: &mut Stack,
        src: &str,
        to_terminal: bool,
        sink: &mut dyn FnMut(PipelineData) -> Result<()>,
    ) -> Result<()> {
        for statement in &self.statements(src)? {
            for parsed in &self.parse(statement)?.statements {
                self.run_statement(stack, parsed, to_terminal, sink)?;
            }
        }
        Ok(())
    }

    /// The statements of a script, each parsed only right before it runs.
    fn statements(&self, src: &str) -> Result<Vec<String>> {
        // Parse everything once first, so a syntax error anywhere stops the script before it starts.
        self.parse(src)?;
        Ok(meersh_parser::statement_sources(src)?)
    }

    fn run_statement(
        self: &Arc<Self>,
        stack: &mut Stack,
        statement: &Statement,
        to_terminal: bool,
        sink: &mut dyn FnMut(PipelineData) -> Result<()>,
    ) -> Result<()> {
        match statement {
            Statement::Let { name, pipeline } => {
                let value = self.run_pipeline(stack, pipeline, PipelineData::Empty, false)?.into_value()?;
                stack.set(name, value);
            }
            Statement::Pipeline(pipeline) => {
                sink(self.run_pipeline(stack, pipeline, PipelineData::Empty, to_terminal)?)?
            }
            Statement::Background(pipeline) => {
                let [Element::Command(call)] = pipeline.elements.as_slice() else {
                    return Err(ShellError::Other("only a single external program can run in the background".into()));
                };
                if !call.external {
                    return Err(ShellError::Other(format!(
                        "{} is a builtin, only external programs run in the background",
                        call.name
                    )));
                }
                let args = external::expand_args(self, stack, call)?;
                sink(PipelineData::Value(self.spawn_job(stack, &call.name, &args)?))?;
            }
        }
        Ok(())
    }

    /// Plan first execution, used by the REPL, `-c` and the MCP server. Each statement that
    /// needs it runs once in plan mode, where nothing changes and every request is recorded.
    /// `consent` sees the plan unless it is routine. The approved plan is then applied, and
    /// only requests that are part of it, with unchanged targets, go through.
    pub fn run_planned(
        self: &Arc<Self>,
        stack: &mut Stack,
        src: &str,
        to_terminal: bool,
        consent: &mut dyn FnMut(&Plan) -> Consent,
        sink: &mut dyn FnMut(PipelineData) -> Result<()>,
    ) -> Result<()> {
        for source in &self.statements(src)? {
            let script = self.parse(source)?;
            let Some(statement) = script.statements.first() else { continue };
            if !self.needs_plan(statement, stack) {
                self.run_statement(stack, statement, to_terminal, sink)?;
                continue;
            }
            let plan = Arc::new(Mutex::new(Plan::default()));
            let mut planning = stack.with_mode(ExecMode::Plan(plan.clone()));
            let mut outputs = Vec::new();
            self.run_statement(&mut planning, statement, to_terminal, &mut |data| {
                outputs.push(data.into_value()?);
                Ok(())
            })?;
            let plan = std::mem::take(&mut *plan.lock().expect("plan lock"));
            if plan.is_empty() && plan.notes.is_empty() {
                // Nothing to approve. The planning run was the real run.
                stack.vars = planning.vars;
                for value in outputs {
                    sink(PipelineData::Value(value))?;
                }
                continue;
            }
            let answer = if plan.is_routine() { Consent::Approve } else { consent(&plan) };
            if answer == Consent::Reject {
                return Err(ShellError::Other("nothing was changed".into()));
            }
            let approved = plan.keys_for(&answer);
            if approved.is_empty() && plan.reviews.is_empty() {
                return Err(ShellError::Other("nothing was approved, nothing was changed".into()));
            }
            // External programs of a model. One a human approved runs in an overlay, so its
            // changes can be reviewed before they are kept. One a policy rule allows runs
            // directly, confined by Landlock to writing inside the project and the temp directory.
            let model_exec = |verdict: crate::Verdict| {
                plan.actions.iter().any(|a| {
                    a.kind == "exec" && a.principal != "user" && a.verdict == verdict && approved.contains(&a.key)
                })
            };
            let (reviewed, ruled) = (model_exec(crate::Verdict::Ask), model_exec(crate::Verdict::Allow));
            let mut applying = stack.with_mode(ExecMode::Apply(Arc::new(approved)));
            if reviewed {
                let sandbox = Arc::new(Sandbox::create(&self.authority.project())?);
                self.authority.add_sandbox(sandbox.clone());
                applying = applying.with_sandbox(sandbox);
            } else if ruled {
                let write = Some(vec![self.authority.project(), std::env::temp_dir()]);
                applying = applying.with_restrictions(Restrictions { write, no_net: false });
            }
            self.run_statement(&mut applying, statement, to_terminal, sink)?;
            stack.vars = applying.vars;
        }
        Ok(())
    }

    /// Runs a block in plan mode and returns what it would do, for `plan { ... }`.
    pub fn plan_block(self: &Arc<Self>, stack: &Stack, block: &Expr) -> Result<Plan> {
        let plan = Arc::new(Mutex::new(Plan::default()));
        let planning = stack.with_mode(ExecMode::Plan(plan.clone()));
        match block {
            Expr::Block(pipeline) => {
                self.run_pipeline(&planning, pipeline, PipelineData::Empty, false)?.into_value()?;
            }
            other => {
                self.eval_expr(&planning, other, None)?;
            }
        }
        Ok(std::mem::take(&mut *plan.lock().expect("plan lock")))
    }

    /// `to_terminal` lets a trailing external program write straight to the terminal,
    /// which keeps interactive programs like `vim` or `less` working.
    pub fn run_pipeline(
        self: &Arc<Self>,
        stack: &Stack,
        pipeline: &Pipeline,
        mut data: PipelineData,
        to_terminal: bool,
    ) -> Result<PipelineData> {
        let last = pipeline.elements.len() - 1;
        let mut scope = std::borrow::Cow::Borrowed(stack);
        for (i, element) in pipeline.elements.iter().enumerate() {
            self.check_interrupt()?;
            // Objects and adapters only make sense when a builtin reads the output.
            let feeds_builtin = match pipeline.elements.get(i + 1) {
                Some(Element::Command(next)) => !next.external || self.is_defined(&next.name),
                Some(Element::Expr(_)) => true,
                None => !to_terminal,
            };
            data = match element {
                Element::Command(call) => {
                    let out = self.run_command(&scope, call, data, to_terminal && i == last, feeds_builtin)?;
                    if !scope.llm_chosen && self.call_effect(call) == Effect::Llm {
                        scope = std::borrow::Cow::Owned(scope.tainted());
                    }
                    out
                }
                Element::Expr(expr) => {
                    let local =
                        if data.is_empty() { scope.clone().into_owned() } else { scope.with("in", data.into_value()?) };
                    PipelineData::Value(self.eval_expr(&local, expr, None)?)
                }
            };
        }
        Ok(data)
    }

    fn run_command(
        self: &Arc<Self>,
        stack: &Stack,
        call: &CommandCall,
        input: PipelineData,
        to_terminal: bool,
        feeds_builtin: bool,
    ) -> Result<PipelineData> {
        // A name defined with `def` earlier on the same line was parsed as an external.
        let builtin = self.lookup(&call.name).filter(|_| !call.external || self.is_defined(&call.name));
        let Some(registered) = builtin else {
            return external::run(self, stack, call, input, to_terminal, feeds_builtin).map_err(|e| e.at(call.pos));
        };
        let bound = bind(&registered.signature, call).map_err(|e| e.at(call.pos))?;
        let redirected = call.redirects.iter().find(|r| r.stdout);
        let ctx = Ctx { engine: self, stack, call: &bound, to_terminal: to_terminal && redirected.is_none() };
        let output = registered.command.run(&ctx, input).map_err(|e| e.at(call.pos))?;
        match redirected {
            Some(redirect) => {
                external::write_redirect(self, stack, redirect, output)?;
                Ok(PipelineData::Empty)
            }
            None => Ok(output),
        }
    }

    /// Evaluates an expression once per row, with the row bound to `$it`. A block runs as a
    /// pipeline that receives the row as input. Any other expression sees the row's columns
    /// as bare words, which is what makes `where size > 1mb` read naturally.
    pub fn eval_row(self: &Arc<Self>, stack: &Stack, expr: &Expr, row: &Value) -> Result<Value> {
        let scope = stack.with("it", row.clone());
        match expr {
            Expr::Block(pipeline) => {
                self.run_pipeline(&scope, pipeline, PipelineData::Value(row.clone()), false)?.into_value()
            }
            other => self.eval_expr(&scope, other, Some(row)),
        }
    }

    pub fn eval_expr(self: &Arc<Self>, stack: &Stack, expr: &Expr, row: Option<&Value>) -> Result<Value> {
        match expr {
            Expr::Word(word) => {
                if let Some(Value::Record(r)) = row
                    && let Some(v) = r.get(word)
                {
                    return Ok(v.clone());
                }
                Ok(parse_literal(word).unwrap_or_else(|| Value::String(word.clone())))
            }
            Expr::Str(s) => Ok(Value::String(s.clone())),
            Expr::Var(name) => match name.as_str() {
                "env" => {
                    Ok(Value::Record(self.env_vars().into_iter().fold(Record::typed("env"), |r, (k, v)| r.with(&k, v))))
                }
                _ => stack
                    .get(name)
                    .cloned()
                    .ok_or_else(|| ShellError::Other(format!("variable ${name} is not defined"))),
            },
            Expr::Member(base, member) => {
                let value = self.eval_expr(stack, base, row)?;
                value
                    .follow(member)
                    .ok_or_else(|| ShellError::Other(format!("{} has no member '{member}'", value.type_name())))
            }
            Expr::MethodCall { receiver, method, args } => {
                let receiver = self.eval_expr(stack, receiver, row)?;
                // A type can override a generic command, `proc.kill` before `kill`.
                let typed = receiver.as_record().and_then(|r| r.type_name()).map(|t| format!("{t}.{method}"));
                let name = typed.filter(|n| self.is_command(n)).unwrap_or_else(|| method.clone());
                if !self.is_command(&name) {
                    return Err(ShellError::Other(format!("{} has no method '{method}'", receiver.type_name())));
                }
                let call = CommandCall { name, args: args.clone(), ..CommandCall::default() };
                self.run_command(stack, &call, PipelineData::Value(receiver), false, true)?.into_value()
            }
            Expr::List(items) => {
                Ok(Value::List(items.iter().map(|e| self.eval_expr(stack, e, row)).collect::<Result<_>>()?))
            }
            Expr::Record(fields) => {
                let mut record = Record::new();
                for (key, e) in fields {
                    record.insert(key.clone(), self.eval_expr(stack, e, row)?);
                }
                Ok(Value::Record(record))
            }
            Expr::Block(pipeline) => match row {
                Some(row) => self.run_pipeline(stack, pipeline, PipelineData::Value(row.clone()), false)?.into_value(),
                None => Err(ShellError::Other(
                    "a block is only allowed as an argument of commands like each or where".into(),
                )),
            },
            Expr::Subexpr(pipeline) => self.run_pipeline(stack, pipeline, PipelineData::Empty, false)?.into_value(),
            Expr::Not(e) => Ok(Value::Bool(!self.eval_expr(stack, e, row)?.is_truthy())),
            Expr::Binary(lhs, BinOp::And, rhs) => Ok(Value::Bool(
                self.eval_expr(stack, lhs, row)?.is_truthy() && self.eval_expr(stack, rhs, row)?.is_truthy(),
            )),
            Expr::Binary(lhs, BinOp::Or, rhs) => Ok(Value::Bool(
                self.eval_expr(stack, lhs, row)?.is_truthy() || self.eval_expr(stack, rhs, row)?.is_truthy(),
            )),
            Expr::Binary(lhs, op, rhs) => {
                binary(*op, self.eval_expr(stack, lhs, row)?, self.eval_expr(stack, rhs, row)?)
            }
        }
    }
}

/// Matches parsed arguments against a signature. Unknown flags are an error here, before
/// the command runs, and the message points at `help`.
fn bind(signature: &Signature, call: &CommandCall) -> Result<Call> {
    let mut positional = Vec::new();
    let mut flags = HashMap::new();
    let mut args = call.args.iter().peekable();
    while let Some(arg) = args.next() {
        let (name, short, value, raw) = match arg {
            Arg::Positional(expr) => {
                positional.push(expr.clone());
                continue;
            }
            Arg::Flag { name, short, value, raw } => (name, *short, value, raw),
        };
        let unknown = || ShellError::Command {
            cmd: call.name.clone(),
            msg: format!("unknown flag {raw}, see `help {}`", call.name),
        };
        let matched: Vec<_> = if short {
            name.chars()
                .map(|c| signature.flags.iter().find(|f| f.short == Some(c)).ok_or_else(unknown))
                .collect::<Result<_>>()?
        } else {
            vec![signature.flags.iter().find(|f| f.long == *name).ok_or_else(unknown)?]
        };
        let count = matched.len();
        for (i, flag) in matched.into_iter().enumerate() {
            let bound = match (&flag.arg, value) {
                (None, _) => None,
                (Some(_), Some(v)) => Some(v.clone()),
                (Some(_), None) if i + 1 == count && matches!(args.peek(), Some(Arg::Positional(_))) => {
                    let Some(Arg::Positional(v)) = args.next() else { unreachable!() };
                    Some(v.clone())
                }
                (Some(ty), None) => {
                    return Err(ShellError::Command {
                        cmd: call.name.clone(),
                        msg: format!("flag --{} needs a value of type {ty}", flag.long),
                    });
                }
            };
            flags.insert(flag.long.clone(), bound);
        }
    }
    check_literals(signature, call, &positional)?;
    Ok(Call { name: call.name.clone(), positional, flags })
}

/// Literal arguments are checked against the declared parameter types before the command
/// runs, so `first abc` fails with a message about the parameter instead of somewhere inside.
fn check_literals(signature: &Signature, call: &CommandCall, positional: &[Expr]) -> Result<()> {
    let params =
        signature.params.iter().chain(std::iter::repeat_n(signature.rest.as_ref(), positional.len()).flatten());
    for (param, expr) in params.zip(positional) {
        let Expr::Word(word) = expr else { continue };
        let literal = parse_literal(word);
        let fits = match &param.ty {
            Type::Int => matches!(literal, Some(Value::Int(_))),
            Type::Float | Type::Number => matches!(literal, Some(Value::Int(_) | Value::Float(_))),
            Type::Filesize => matches!(literal, Some(Value::Filesize(_) | Value::Int(_))),
            Type::Duration => matches!(literal, Some(Value::Duration(_))),
            Type::Bool => matches!(literal, Some(Value::Bool(_))),
            _ => true,
        };
        if !fits {
            return Err(ShellError::Command {
                cmd: call.name.clone(),
                msg: format!("<{}> expects {}, got '{word}'", param.name, param.ty),
            });
        }
    }
    Ok(())
}

fn mismatch(op: BinOp, a: &Value, b: &Value) -> ShellError {
    ShellError::Other(format!("cannot apply {} to {} and {}", op.symbol(), a.type_name(), b.type_name()))
}

fn binary(op: BinOp, a: Value, b: Value) -> Result<Value> {
    use Value::*;
    let ordering = |a: &Value, b: &Value| a.compare(b).ok_or_else(|| mismatch(op, a, b));
    Ok(match op {
        BinOp::Eq => Bool(a == b || a.compare(&b) == Some(Ordering::Equal)),
        BinOp::Ne => Bool(!(a == b || a.compare(&b) == Some(Ordering::Equal))),
        BinOp::Lt => Bool(ordering(&a, &b)? == Ordering::Less),
        BinOp::Le => Bool(ordering(&a, &b)? != Ordering::Greater),
        BinOp::Gt => Bool(ordering(&a, &b)? == Ordering::Greater),
        BinOp::Ge => Bool(ordering(&a, &b)? != Ordering::Less),
        BinOp::Match | BinOp::NotMatch => {
            let pattern = b.to_text();
            let found = with_regex(&pattern, |re| re.is_match(&a.to_display()))?;
            Bool(found == (op == BinOp::Match))
        }
        BinOp::In => Bool(match &b {
            List(items) => items.iter().any(|item| *item == a || item.compare(&a) == Some(Ordering::Equal)),
            String(s) => s.contains(&a.to_text()),
            Record(r) => r.get(&a.to_text()).is_some(),
            _ => return Err(mismatch(op, &a, &b)),
        }),
        BinOp::Add => match (a, b) {
            (Int(x), Int(y)) => Int(x + y),
            (Filesize(x), Filesize(y)) => Filesize(x + y),
            (Duration(x), Duration(y)) => Duration(x + y),
            (Date(d), Duration(n)) => Date(d + chrono::Duration::nanoseconds(n)),
            (String(x), y) => String(x + &y.to_text()),
            (List(mut x), List(y)) => {
                x.extend(y);
                List(x)
            }
            (x, y) => {
                let (x, y) = number(op, &x, &y)?;
                Float(x + y)
            }
        },
        BinOp::Sub => match (a, b) {
            (Int(x), Int(y)) => Int(x - y),
            (Filesize(x), Filesize(y)) => Filesize(x - y),
            (Duration(x), Duration(y)) => Duration(x - y),
            (Date(d), Duration(n)) => Date(d - chrono::Duration::nanoseconds(n)),
            (Date(x), Date(y)) => Duration((x - y).num_nanoseconds().unwrap_or(i64::MAX)),
            (x, y) => {
                let (x, y) = number(op, &x, &y)?;
                Float(x - y)
            }
        },
        BinOp::Mul => match (a, b) {
            (Int(x), Int(y)) => Int(x * y),
            (Filesize(x), Int(y)) | (Int(y), Filesize(x)) => Filesize(x * y),
            (Duration(x), Int(y)) | (Int(y), Duration(x)) => Duration(x * y),
            (x, y) => {
                let (x, y) = number(op, &x, &y)?;
                Float(x * y)
            }
        },
        BinOp::Div => match (a, b) {
            (Filesize(x), Int(y)) if y != 0 => Filesize(x / y),
            (Duration(x), Int(y)) if y != 0 => Duration(x / y),
            (Duration(x), Duration(y)) if y != 0 => Float(x as f64 / y as f64),
            (x, y) => {
                let (x, y) = number(op, &x, &y)?;
                Float(x / y)
            }
        },
        BinOp::And | BinOp::Or => unreachable!("short circuited in eval_expr"),
    })
}

fn number(op: BinOp, a: &Value, b: &Value) -> Result<(f64, f64)> {
    let as_f64 = |v: &Value| match v {
        Value::Int(i) | Value::Filesize(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    };
    match (as_f64(a), as_f64(b)) {
        (Some(x), Some(y)) => Ok((x, y)),
        _ => Err(mismatch(op, a, b)),
    }
}

thread_local! {
    // Row conditions evaluate the same pattern for every row, so compile it once.
    static REGEX_CACHE: RefCell<HashMap<String, Regex>> = RefCell::new(HashMap::new());
}

fn with_regex<T>(pattern: &str, f: impl FnOnce(&Regex) -> T) -> Result<T> {
    REGEX_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if !cache.contains_key(pattern) {
            let re = Regex::new(pattern).map_err(|e| ShellError::Other(format!("invalid regex: {e}")))?;
            cache.insert(pattern.to_string(), re);
        }
        Ok(f(&cache[pattern]))
    })
}
