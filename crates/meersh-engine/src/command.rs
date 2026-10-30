use std::collections::HashMap;
use std::fmt::Display;
use std::sync::Arc;

use meersh_parser::Expr;
use meersh_value::{Type, Value};

use crate::authority::{Cap, Permit, PlanNote, Principal, Review, file_id, proc_id};
use crate::{Engine, ExecMode, PipelineData, Result, ShellError, Stack};

/// What running a command can do to the world. The shell uses it to decide whether a
/// pipeline that an LLM generated may run right away or needs confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Effect {
    /// Only transforms its input.
    Pure,
    /// Reads the system (files, processes) without changing it.
    Read,
    /// Sends data to a language model.
    Llm,
    /// Creates or changes files or state.
    Write,
    /// Deletes data or stops processes.
    Destructive,
    /// An external program. meersh cannot know what it does.
    Unknown,
}

impl Effect {
    pub fn as_str(self) -> &'static str {
        match self {
            Effect::Pure => "pure",
            Effect::Read => "read",
            Effect::Llm => "llm",
            Effect::Write => "write",
            Effect::Destructive => "destructive",
            Effect::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Param {
    pub name: String,
    pub ty: Type,
    pub desc: String,
    pub optional: bool,
}

#[derive(Debug, Clone)]
pub struct Flag {
    pub long: String,
    pub short: Option<char>,
    /// `None` for a switch, the value type for a named flag such as `--into <string>`.
    pub arg: Option<Type>,
    pub desc: String,
}

/// Everything the shell knows about a command without running it. Signatures drive
/// argument binding, `help`, completion and the command catalogue that `?` hands to an LLM.
#[derive(Debug, Clone)]
pub struct Signature {
    pub name: String,
    pub description: String,
    pub input: Type,
    pub output: Type,
    pub params: Vec<Param>,
    pub rest: Option<Param>,
    pub flags: Vec<Flag>,
    pub effect: Effect,
    pub examples: Vec<(String, String)>,
}

impl Signature {
    pub fn build(name: &str) -> Self {
        Signature {
            name: name.into(),
            description: String::new(),
            input: Type::Any,
            output: Type::Any,
            params: vec![],
            rest: None,
            flags: vec![],
            effect: Effect::Pure,
            examples: vec![],
        }
    }

    pub fn desc(mut self, description: &str) -> Self {
        self.description = description.into();
        self
    }

    pub fn input(mut self, ty: Type) -> Self {
        self.input = ty;
        self
    }

    pub fn output(mut self, ty: Type) -> Self {
        self.output = ty;
        self
    }

    pub fn required(mut self, name: &str, ty: Type, desc: &str) -> Self {
        self.params.push(Param { name: name.into(), ty, desc: desc.into(), optional: false });
        self
    }

    pub fn optional(mut self, name: &str, ty: Type, desc: &str) -> Self {
        self.params.push(Param { name: name.into(), ty, desc: desc.into(), optional: true });
        self
    }

    pub fn rest(mut self, name: &str, ty: Type, desc: &str) -> Self {
        self.rest = Some(Param { name: name.into(), ty, desc: desc.into(), optional: true });
        self
    }

    pub fn switch(mut self, long: &str, short: Option<char>, desc: &str) -> Self {
        self.flags.push(Flag { long: long.into(), short, arg: None, desc: desc.into() });
        self
    }

    pub fn named(mut self, long: &str, ty: Type, short: Option<char>, desc: &str) -> Self {
        self.flags.push(Flag { long: long.into(), short, arg: Some(ty), desc: desc.into() });
        self
    }

    pub fn effect(mut self, effect: Effect) -> Self {
        self.effect = effect;
        self
    }

    pub fn example(mut self, example: &str, desc: &str) -> Self {
        self.examples.push((example.into(), desc.into()));
        self
    }

    /// One line usage such as `sort-by <column> [--reverse(-r)]`.
    pub fn usage(&self) -> String {
        let mut parts = vec![self.name.clone()];
        for p in &self.params {
            parts.push(if p.optional { format!("[{}]", p.name) } else { format!("<{}>", p.name) });
        }
        if let Some(rest) = &self.rest {
            parts.push(format!("[...{}]", rest.name));
        }
        for f in &self.flags {
            let short = f.short.map(|c| format!("(-{c})")).unwrap_or_default();
            let value = f.arg.as_ref().map(|t| format!(" <{t}>")).unwrap_or_default();
            parts.push(format!("[--{}{short}{value}]", f.long));
        }
        parts.join(" ")
    }
}

pub trait Command: Send + Sync {
    fn signature(&self) -> Signature;
    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData>;
}

/// A call after binding. Positional arguments stay unevaluated expressions, because some
/// commands (`where`, `each`) evaluate them once per row instead of once per call.
#[derive(Debug, Clone)]
pub struct Call {
    pub name: String,
    pub positional: Vec<Expr>,
    pub flags: HashMap<String, Option<Expr>>,
}

/// What a running command can see.
pub struct Ctx<'a> {
    pub engine: &'a Arc<Engine>,
    pub stack: &'a Stack,
    pub call: &'a Call,
    /// True for the last stage of a pipeline whose output goes to the terminal.
    pub to_terminal: bool,
}

impl Ctx<'_> {
    pub fn eval(&self, expr: &Expr) -> Result<Value> {
        self.engine.eval_expr(self.stack, expr, None)
    }

    pub fn positional(&self, index: usize) -> Option<&Expr> {
        self.call.positional.get(index)
    }

    pub fn arg(&self, index: usize) -> Result<Option<Value>> {
        self.positional(index).map(|e| self.eval(e)).transpose()
    }

    pub fn req(&self, index: usize, what: &str) -> Result<Value> {
        self.arg(index)?.ok_or_else(|| self.err(format!("missing argument <{what}>")))
    }

    /// A string argument. Bare words come back exactly as typed, so `cd 2d` means the
    /// directory `2d` and not a duration of two days.
    pub fn req_string(&self, index: usize, what: &str) -> Result<String> {
        self.opt_string(index)?.ok_or_else(|| self.err(format!("missing argument <{what}>")))
    }

    pub fn opt_string(&self, index: usize) -> Result<Option<String>> {
        self.positional(index).map(|e| self.expr_string(e)).transpose()
    }

    /// All positional arguments from `from` on, as strings. Lists are flattened.
    pub fn rest_strings(&self, from: usize) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for expr in self.call.positional.iter().skip(from) {
            match expr {
                Expr::Word(_) | Expr::Str(_) => out.push(self.expr_string(expr)?),
                other => match self.eval(other)? {
                    Value::List(items) => out.extend(items.iter().map(Value::to_text)),
                    v => out.push(v.to_text()),
                },
            }
        }
        Ok(out)
    }

    fn expr_string(&self, expr: &Expr) -> Result<String> {
        match expr {
            Expr::Word(w) => Ok(w.clone()),
            Expr::Str(s) => Ok(s.clone()),
            other => Ok(self.eval(other)?.to_text()),
        }
    }

    pub fn switch(&self, long: &str) -> bool {
        self.call.flags.contains_key(long)
    }

    pub fn named(&self, long: &str) -> Result<Option<Value>> {
        match self.call.flags.get(long) {
            Some(Some(expr)) => Ok(Some(self.eval(expr)?)),
            _ => Ok(None),
        }
    }

    pub fn named_string(&self, long: &str) -> Result<Option<String>> {
        match self.call.flags.get(long) {
            Some(Some(expr)) => Ok(Some(self.expr_string(expr)?)),
            _ => Ok(None),
        }
    }

    pub fn err(&self, msg: impl Display) -> ShellError {
        ShellError::Command { cmd: self.call.name.clone(), msg: msg.to_string() }
    }

    pub fn principal(&self) -> &Principal {
        self.stack.principal()
    }

    pub fn is_planning(&self) -> bool {
        self.stack.is_planning()
    }

    /// Ask for the right to act, right before acting and with the concrete target. Paths may
    /// be relative. `Permit::Execute` means act now and then call [`Ctx::performed`].
    /// `Permit::Plan` means report what would happen without doing it. `Permit::Skip` means
    /// leave this target alone, report the reason and go on with the next one.
    pub fn authorize(&self, cap: Cap) -> Result<Permit> {
        self.engine.authorize(self.stack, cap)
    }

    /// Report a completed action, so the ledger and the journal stay right.
    pub fn performed(&self, cap: Cap) -> Result<()> {
        self.engine.authority.performed(self.stack, cap)
    }

    /// Report a process this session started, so it can be stopped later as its own.
    pub fn started(&self, pid: u32, program: &str) {
        let authority = &self.engine.authority;
        authority.ledger.record_process(pid, program, self.principal(), &authority.session, self.stack.task());
    }

    /// For requests that cannot wait for a plan, such as sending data to a model.
    pub fn check_now(&self, cap: Cap) -> Result<()> {
        self.engine.check_now(self.stack, cap)
    }

    /// Checks that a record still describes the same object as when it was produced. A file
    /// must have the same inode, size and modification time, a process the same start time.
    pub fn verify(&self, value: &Value) -> Result<()> {
        let Some(record) = value.as_record() else { return Ok(()) };
        let Some(id) = record.id() else { return Ok(()) };
        let current = if id.starts_with("file:") {
            let path = record.get("name").or_else(|| record.get("path")).and_then(Value::as_str).unwrap_or_default();
            file_id(&crate::util::expand_path(path))
        } else if let Some(rest) = id.strip_prefix("proc:") {
            rest.split(':').next().and_then(|pid| pid.parse().ok()).and_then(proc_id)
        } else {
            return Ok(());
        };
        if current.as_deref() == Some(id) {
            Ok(())
        } else {
            let what = record.get("name").or_else(|| record.get("pid")).map(Value::to_text).unwrap_or_default();
            Err(self.err(format!("{what} changed or disappeared since it was listed")))
        }
    }

    /// Attaches context to the plan being computed, such as a pipeline a model wrote.
    pub fn plan_note(&self, note: PlanNote) {
        if let ExecMode::Plan(plan) = self.stack.mode() {
            plan.lock().expect("plan lock").notes.push(note);
        }
    }

    /// Asks the user to look at a change to data before the plan is applied.
    pub fn review(&self, review: Review) {
        if let ExecMode::Plan(plan) = self.stack.mode() {
            plan.lock().expect("plan lock").reviews.push(review);
        }
    }
}
