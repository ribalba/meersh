//! Authority. Who may do what to which object.
//!
//! Every command that changes the world calls [`crate::Ctx::authorize`] right before it acts,
//! with the concrete target. The decision depends on the principal that runs the pipeline,
//! on the target (protected, owned, reversible, inside the project, leased) and on who chose
//! the target. In plan mode nothing happens and the request is recorded instead. In apply
//! mode only the approved requests go through. AUTHORITY.md explains the design.

mod identity;
mod journal;
mod ledger;
mod policy;
mod sandbox;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

pub use identity::{absolute, file_id, proc_id, project_root};
pub use journal::{Inverse, JournalEntry};
pub use ledger::{Ledger, LedgerEntry, Ownership};
use meersh_value::{Record, Value};
pub use policy::{Rule, Verdict};
pub use sandbox::Sandbox;

use crate::{Engine, Result, ShellError, Stack};

/// Whoever is responsible for a pipeline. The same `rm` is judged differently depending on
/// whether a human typed it or a language model wrote it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Principal {
    User,
    Script(String),
    /// A pipeline written by `?`.
    Plan,
    /// A language model acting on piped objects, as in `ls | llm "delete ..."`.
    Actor,
    /// An external agent using meersh as its tool, for example over MCP.
    Agent(String),
}

impl Principal {
    pub fn is_model(&self) -> bool {
        matches!(self, Principal::Plan | Principal::Actor | Principal::Agent(_))
    }

    pub fn name(&self) -> String {
        match self {
            Principal::User => "user".into(),
            Principal::Script(path) => format!("script:{path}"),
            Principal::Plan => "plan".into(),
            Principal::Actor => "actor".into(),
            Principal::Agent(name) => format!("agent:{name}"),
        }
    }

    pub fn parse(text: &str) -> Option<Principal> {
        Some(match text {
            "user" => Principal::User,
            "plan" => Principal::Plan,
            "actor" => Principal::Actor,
            other => match other.split_once(':') {
                Some(("agent", name)) => Principal::Agent(name.into()),
                Some(("script", path)) => Principal::Script(path.into()),
                _ => return None,
            },
        })
    }
}

/// A right to do one kind of thing to one concrete target.
#[derive(Debug, Clone)]
pub enum Cap {
    FsCreate {
        path: PathBuf,
    },
    /// Overwrite an existing file.
    FsWrite {
        path: PathBuf,
    },
    FsDelete {
        path: PathBuf,
        permanent: bool,
    },
    FsMove {
        from: PathBuf,
        to: PathBuf,
    },
    FsChmod {
        path: PathBuf,
        mode: u32,
    },
    ProcSpawn {
        program: String,
    },
    ProcSignal {
        pid: u32,
        signal: String,
    },
    /// Run an external program.
    Exec {
        program: String,
        args: Vec<String>,
    },
    LlmSend {
        backend: String,
        local: bool,
    },
}

impl Cap {
    pub fn kind(&self) -> &'static str {
        match self {
            Cap::FsCreate { .. } => "fs.create",
            Cap::FsWrite { .. } => "fs.write",
            Cap::FsDelete { .. } => "fs.delete",
            Cap::FsMove { .. } => "fs.move",
            Cap::FsChmod { .. } => "fs.chmod",
            Cap::ProcSpawn { .. } => "proc.spawn",
            Cap::ProcSignal { .. } => "proc.signal",
            Cap::Exec { .. } => "exec",
            Cap::LlmSend { .. } => "llm.send",
        }
    }

    fn verb(&self) -> &'static str {
        match self {
            Cap::FsCreate { .. } => "create",
            Cap::FsWrite { .. } => "overwrite",
            Cap::FsDelete { permanent: false, .. } => "trash",
            Cap::FsDelete { permanent: true, .. } => "delete",
            Cap::FsMove { .. } => "move",
            Cap::FsChmod { .. } => "chmod",
            Cap::ProcSpawn { .. } => "start",
            Cap::ProcSignal { .. } => "signal",
            Cap::Exec { .. } => "run",
            Cap::LlmSend { .. } => "send",
        }
    }

    /// The path the action is about, if it is about a path.
    pub fn path(&self) -> Option<&Path> {
        match self {
            Cap::FsCreate { path } | Cap::FsWrite { path } | Cap::FsDelete { path, .. } | Cap::FsChmod { path, .. } => {
                Some(path)
            }
            Cap::FsMove { from, .. } => Some(from),
            _ => None,
        }
    }

    fn target(&self) -> String {
        match self {
            Cap::ProcSignal { pid, .. } => format!("pid {pid}"),
            Cap::ProcSpawn { program } | Cap::Exec { program, .. } => program.clone(),
            Cap::LlmSend { backend, .. } => backend.clone(),
            other => other.path().map(display_path).unwrap_or_default(),
        }
    }

    fn detail(&self) -> Value {
        match self {
            Cap::FsMove { to, .. } => Value::String(format!("to {}", display_path(to))),
            Cap::FsChmod { mode, .. } => Value::String(format!("{mode:o}")),
            Cap::ProcSignal { signal, .. } => Value::String(signal.clone()),
            Cap::Exec { args, .. } => Value::String(args.join(" ")),
            _ => Value::Nothing,
        }
    }

    fn absolute(self) -> Cap {
        match self {
            Cap::FsCreate { path } => Cap::FsCreate { path: absolute(&path) },
            Cap::FsWrite { path } => Cap::FsWrite { path: absolute(&path) },
            Cap::FsDelete { path, permanent } => Cap::FsDelete { path: absolute(&path), permanent },
            Cap::FsMove { from, to } => Cap::FsMove { from: absolute(&from), to: absolute(&to) },
            Cap::FsChmod { path, mode } => Cap::FsChmod { path: absolute(&path), mode },
            other => other,
        }
    }

    /// Identifies the request together with the identity of its target, so an approval for
    /// `trash out.bin` does not cover a different file that later appears under that name.
    fn key(&self, principal: &Principal) -> String {
        let identity = match self {
            Cap::ProcSignal { pid, .. } => proc_id(*pid).unwrap_or_default(),
            other => other.path().and_then(file_id).unwrap_or_else(|| "new".into()),
        };
        format!("{}|{}|{}|{}|{}", principal.name(), self.kind(), self.target(), self.detail().to_text(), identity)
    }
}

/// Paths inside the working directory are shown relative to it.
fn display_path(path: &Path) -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    path.strip_prefix(&cwd).map_or_else(|_| path.display().to_string(), |p| p.display().to_string())
}

/// What a command may do with a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Permit {
    /// Act now.
    Execute,
    /// Plan mode. Do not act, report what would happen.
    Plan,
    /// Do not act and report why, then continue with the next target.
    Skip(String),
}

#[derive(Debug, Clone)]
pub struct PlannedAction {
    pub key: String,
    pub principal: String,
    pub kind: String,
    pub action: String,
    pub target: String,
    pub detail: Value,
    pub size: Value,
    pub reversible: bool,
    pub verdict: Verdict,
    pub reason: String,
}

impl PlannedAction {
    pub fn to_value(&self) -> Value {
        Value::Record(
            Record::typed("plan.action")
                .with("principal", self.principal.as_str())
                .with("action", self.action.as_str())
                .with("target", self.target.as_str())
                .with("detail", self.detail.clone())
                .with("size", self.size.clone())
                .with("reversible", self.reversible)
                .with("decision", self.verdict.as_str())
                .with("because", self.reason.as_str()),
        )
    }
}

/// Context that a model attached to a plan, shown above the actions.
#[derive(Debug, Clone)]
pub struct PlanNote {
    pub principal: String,
    pub pipeline: String,
    pub explanation: String,
}

/// A change to data that the user should see before it continues, such as the patches of
/// `llm edit`.
#[derive(Debug, Clone)]
pub struct Review {
    pub title: String,
    pub diff: Value,
}

#[derive(Debug, Default)]
pub struct Plan {
    pub actions: Vec<PlannedAction>,
    pub notes: Vec<PlanNote>,
    pub reviews: Vec<Review>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty() && self.reviews.is_empty()
    }

    /// True when the plan can be applied without asking. That is the case when every action
    /// is allowed and comes from the user, and no review is pending.
    pub fn is_routine(&self) -> bool {
        self.reviews.is_empty()
            && self.notes.is_empty()
            && self.actions.iter().all(|a| a.verdict == Verdict::Allow && a.principal == "user")
    }

    pub fn approvable_keys(&self) -> HashSet<String> {
        self.actions.iter().filter(|a| a.verdict != Verdict::Deny).map(|a| a.key.clone()).collect()
    }

    /// The keys the applying run may execute for an answer. Allowed actions are always in,
    /// refused ones never, and `Only` picks among the ones that asked.
    pub fn keys_for(&self, consent: &crate::Consent) -> HashSet<String> {
        match consent {
            crate::Consent::Approve => self.approvable_keys(),
            crate::Consent::Reject => HashSet::new(),
            crate::Consent::Only(picked) => self
                .actions
                .iter()
                .enumerate()
                .filter(|(i, a)| a.verdict == Verdict::Allow || (a.verdict == Verdict::Ask && picked.contains(i)))
                .map(|(_, a)| a.key.clone())
                .collect(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum ExecMode {
    Execute,
    Plan(Arc<Mutex<Plan>>),
    Apply(Arc<HashSet<String>>),
}

/// Limits a block puts on everything inside it, as in `with {fs.write: ./out, net: none} {...}`.
#[derive(Debug, Clone, Default)]
pub struct Restrictions {
    /// Directories that may be written. `None` means no limit.
    pub write: Option<Vec<PathBuf>>,
    pub no_net: bool,
}

impl Restrictions {
    fn allows_write(&self, path: &Path) -> bool {
        self.write.as_ref().is_none_or(|roots| roots.iter().any(|root| path.starts_with(root)))
    }
}

/// The facts a decision is based on. Rules see them as the fields of `$it`.
#[derive(Debug, Clone)]
pub(crate) struct Facts {
    pub protected: Option<String>,
    pub ownership: Ownership,
    pub in_project: bool,
    pub reversible: bool,
    pub leased_by: Option<String>,
    pub outside_scope: bool,
    pub size: Value,
}

pub struct Authority {
    pub ledger: Ledger,
    journal: Mutex<Vec<JournalEntry>>,
    rules: RwLock<Vec<Rule>>,
    budgets: RwLock<HashMap<String, usize>>,
    /// Inverses that must be captured before acting, such as the old content of a file.
    pending: Mutex<HashMap<String, Inverse>>,
    sandboxes: Mutex<Vec<Arc<Sandbox>>>,
    /// The directory that counts as the project. It is fixed when the session starts and
    /// only the user moves it, so a model cannot widen its reach with `cd /`.
    project: RwLock<PathBuf>,
    /// Identifies this shell session in the shared ledger.
    pub session: String,
}

pub const DEFAULT_MODEL_BUDGET: usize = 50;

impl Authority {
    pub fn new(ledger: Ledger) -> Self {
        let session = format!("{}-{}", std::process::id(), chrono::Local::now().timestamp_millis());
        Authority {
            ledger,
            journal: Mutex::default(),
            rules: RwLock::default(),
            budgets: RwLock::default(),
            pending: Mutex::default(),
            sandboxes: Mutex::default(),
            project: RwLock::new(project_root()),
            session,
        }
    }

    pub fn project(&self) -> PathBuf {
        self.project.read().expect("project lock").clone()
    }

    /// Moves the project root. The REPL calls this after lines the user ran.
    pub fn set_project(&self, root: PathBuf) {
        *self.project.write().expect("project lock") = root;
    }

    pub fn add_rule(&self, rule: Rule) {
        self.rules.write().expect("rules lock").push(rule);
    }

    pub fn remove_rule(&self, index: usize) -> bool {
        let mut rules = self.rules.write().expect("rules lock");
        if index < rules.len() {
            rules.remove(index);
            true
        } else {
            false
        }
    }

    pub fn rules(&self) -> Vec<Rule> {
        self.rules.read().expect("rules lock").clone()
    }

    pub fn set_budget(&self, principal: &str, actions: usize) {
        self.budgets.write().expect("budget lock").insert(principal.into(), actions);
    }

    pub fn budget(&self, principal: &Principal) -> Option<usize> {
        let budgets = self.budgets.read().expect("budget lock");
        let name = principal.name();
        budgets
            .get(&name)
            .or_else(|| budgets.get("model").filter(|_| principal.is_model()))
            .copied()
            .or(principal.is_model().then_some(DEFAULT_MODEL_BUDGET))
    }

    pub fn journal(&self) -> Vec<JournalEntry> {
        self.journal.lock().expect("journal lock").clone()
    }

    pub fn add_sandbox(&self, sandbox: Arc<Sandbox>) {
        self.sandboxes.lock().expect("sandbox lock").push(sandbox);
    }

    pub fn take_sandboxes(&self) -> Vec<Arc<Sandbox>> {
        std::mem::take(&mut *self.sandboxes.lock().expect("sandbox lock"))
    }

    pub fn pending_sandboxes(&self) -> Vec<Arc<Sandbox>> {
        self.sandboxes.lock().expect("sandbox lock").clone()
    }

    fn facts(&self, stack: &Stack, cap: &Cap) -> Facts {
        let principal = stack.principal();
        let root = self.project();
        let paths: Vec<&Path> = match cap {
            Cap::FsMove { from, to } => vec![from.as_path(), to.as_path()],
            other => other.path().into_iter().collect(),
        };
        let protected = paths.iter().find_map(|p| identity::protected(p));
        let in_project = !paths.is_empty() && paths.iter().all(|p| p.starts_with(&root));
        let ownership = match cap {
            Cap::ProcSignal { pid, .. } => self.ledger.process_ownership(*pid, principal, &self.session),
            other => other.path().map_or(Ownership::NotOwned, |p| self.ledger.ownership(p, principal, &self.session)),
        };
        let reversible = match cap {
            Cap::FsDelete { permanent: false, .. } | Cap::FsMove { .. } | Cap::FsCreate { .. } => true,
            Cap::FsWrite { .. } | Cap::FsChmod { .. } | Cap::ProcSpawn { .. } => true,
            Cap::FsDelete { path, permanent: true } => identity::git_restorable(path),
            Cap::Exec { .. } => stack.sandbox().is_some(),
            Cap::ProcSignal { .. } | Cap::LlmSend { .. } => false,
        };
        let leased_by = paths
            .iter()
            .find_map(|p| self.ledger.lease_holder(p))
            .filter(|(holder, _)| *holder != principal.name())
            .map(|(holder, until)| format!("{holder} until {until}"));
        let outside_scope = match (stack.restrictions(), cap) {
            (Some(r), Cap::FsCreate { .. } | Cap::FsWrite { .. } | Cap::FsDelete { .. } | Cap::FsChmod { .. }) => {
                paths.iter().any(|p| !r.allows_write(p))
            }
            (Some(r), Cap::FsMove { from, to }) => !r.allows_write(from) || !r.allows_write(to),
            _ => false,
        };
        let size = cap
            .path()
            .and_then(|p| std::fs::symlink_metadata(p).ok())
            .filter(|m| m.is_file())
            .map_or(Value::Nothing, |m| Value::Filesize(m.len() as i64));
        Facts { protected, ownership, in_project, reversible, leased_by, outside_scope, size }
    }

    /// The record that rule conditions see as `$it`.
    fn request_record(stack: &Stack, cap: &Cap, facts: &Facts) -> Value {
        let mut record = Record::typed("policy.request")
            .with("principal", stack.principal().name())
            .with("cap", cap.kind())
            .with("target", cap.target())
            .with("owned", facts.ownership == Ownership::Owned)
            .with("changed", matches!(facts.ownership, Ownership::Changed(_)))
            .with("reversible", facts.reversible)
            .with("in_project", facts.in_project)
            .with("protected", facts.protected.is_some())
            .with("chosen_by_llm", stack.llm_chosen())
            .with("leased_by", facts.leased_by.clone())
            .with("size", facts.size.clone())
            .with("task", stack.task().map(str::to_string));
        if let Some(path) = cap.path() {
            record.insert("path", path.display().to_string());
        }
        match cap {
            Cap::FsMove { to, .. } => record.insert("to", to.display().to_string()),
            Cap::ProcSignal { pid, signal } => {
                record.insert("pid", i64::from(*pid));
                record.insert("signal", signal.as_str());
            }
            Cap::Exec { program, args } => {
                record.insert("program", program.as_str());
                record.insert("args", Value::List(args.iter().map(|a| Value::from(a.as_str())).collect()));
            }
            Cap::LlmSend { local, .. } => record.insert("local", *local),
            _ => {}
        }
        Value::Record(record)
    }

    /// Records what is needed to undo an action before it happens.
    fn prepare(&self, cap: &Cap) -> Result<()> {
        let inverse = match cap {
            Cap::FsWrite { path } if path.is_file() => {
                Inverse::RestoreSnapshot { path: path.clone(), snapshot: journal::snapshot(path)? }
            }
            Cap::FsChmod { path, .. } => {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(path)?.permissions().mode() & 0o7777;
                Inverse::Chmod { path: path.clone(), mode }
            }
            _ => return Ok(()),
        };
        self.pending.lock().expect("pending lock").insert(cap.target(), inverse);
        Ok(())
    }

    /// Reports a completed action. Creations become owned, moves keep their ownership,
    /// and the journal gets the inverse.
    pub(crate) fn performed(&self, stack: &Stack, cap: Cap) -> Result<()> {
        let cap = cap.absolute();
        let principal = stack.principal();
        let task = stack.task().map(str::to_string);
        let pending = self.pending.lock().expect("pending lock").remove(&cap.target());
        let inverse = match &cap {
            Cap::FsCreate { path } => {
                self.ledger.record_path(path, principal, &self.session, task.as_deref());
                Inverse::RemoveCreated { path: path.clone() }
            }
            Cap::FsWrite { path } => {
                self.ledger.refresh(path, principal, &self.session);
                pending.unwrap_or(Inverse::None)
            }
            Cap::FsDelete { path, permanent } => {
                self.ledger.forget(path);
                if *permanent {
                    Inverse::None
                } else {
                    Inverse::RestoreTrash { path: path.clone(), after: chrono::Local::now().timestamp() - 2 }
                }
            }
            Cap::FsMove { from, to } => {
                self.ledger.moved(from, to);
                Inverse::MoveBack { original: from.clone(), current: to.clone() }
            }
            Cap::FsChmod { .. } => pending.unwrap_or(Inverse::None),
            Cap::ProcSignal { .. } | Cap::ProcSpawn { .. } | Cap::Exec { .. } | Cap::LlmSend { .. } => Inverse::None,
        };
        let mut journal = self.journal.lock().expect("journal lock");
        let id = journal.len() as u64 + 1;
        journal.push(JournalEntry {
            id,
            at: chrono::Local::now(),
            principal: principal.name(),
            task,
            action: cap.verb().into(),
            target: cap.target(),
            detail: cap.detail().to_text(),
            inverse,
            undone: false,
        });
        Ok(())
    }

    /// Reverses one journal entry.
    pub fn undo(&self, id: u64) -> Result<Value> {
        let entry = self
            .journal()
            .into_iter()
            .find(|e| e.id == id)
            .ok_or_else(|| ShellError::Other(format!("no journal entry {id}")))?;
        if entry.undone {
            return Err(ShellError::Other(format!("journal entry {id} was already undone")));
        }
        let result = journal::reverse(&entry.inverse, &self.ledger, &self.session)?;
        if let Some(e) = self.journal.lock().expect("journal lock").iter_mut().find(|e| e.id == id) {
            e.undone = true;
        }
        Ok(Value::Record(
            Record::typed("journal.undo")
                .with("id", id as i64)
                .with("action", entry.action)
                .with("target", entry.target)
                .with("result", result),
        ))
    }
}

impl Engine {
    /// The decision without side effects. `policy explain` and the plan use it.
    pub(crate) fn decide(self: &Arc<Self>, stack: &Stack, cap: &Cap) -> Result<(Verdict, String, Facts)> {
        let facts = self.authority.facts(stack, cap);
        let principal = stack.principal();
        if let Some(reason) = &facts.protected {
            let verdict = if principal.is_model() { Verdict::Deny } else { Verdict::Ask };
            return Ok((verdict, format!("protected, {reason}"), facts));
        }
        if facts.outside_scope {
            return Ok((Verdict::Deny, "outside the write scope of this block".into(), facts));
        }
        let rules = self.authority.rules();
        if !rules.is_empty() {
            let request = Authority::request_record(stack, cap, &facts);
            for (i, rule) in rules.iter().enumerate() {
                if rule.matches(principal, cap.kind(), stack.task()) && rule.condition_holds(self, stack, &request)? {
                    return Ok((rule.verdict, format!("rule {i}, {}", rule.text), facts));
                }
            }
        }
        if let Some(holder) = &facts.leased_by {
            return Ok((Verdict::Ask, format!("leased by {holder}"), facts));
        }
        let (verdict, reason) = policy::default_decision(principal, stack.llm_chosen(), cap, &facts);
        Ok((verdict, reason, facts))
    }

    pub(crate) fn authorize(self: &Arc<Self>, stack: &Stack, cap: Cap) -> Result<Permit> {
        let cap = cap.absolute();
        let principal = stack.principal();
        let (mut verdict, mut reason, facts) = self.decide(stack, &cap)?;
        let key = cap.key(principal);
        match stack.mode() {
            ExecMode::Plan(plan) => {
                let mut plan = plan.lock().expect("plan lock");
                if verdict != Verdict::Deny
                    && let Some(budget) = self.authority.budget(principal)
                {
                    let used = plan.actions.iter().filter(|a| a.principal == principal.name()).count();
                    if used >= budget {
                        verdict = Verdict::Deny;
                        reason = format!("over the budget of {budget} actions");
                    }
                }
                plan.actions.push(PlannedAction {
                    key,
                    principal: principal.name(),
                    kind: cap.kind().into(),
                    action: cap.verb().into(),
                    target: cap.target(),
                    detail: cap.detail(),
                    size: facts.size,
                    reversible: facts.reversible,
                    verdict,
                    reason: reason.clone(),
                });
                Ok(if verdict == Verdict::Deny { Permit::Skip(reason) } else { Permit::Plan })
            }
            ExecMode::Apply(approved) => {
                if approved.contains(&key) {
                    self.authority.prepare(&cap)?;
                    Ok(Permit::Execute)
                } else {
                    Ok(Permit::Skip("not approved, or changed since the plan".into()))
                }
            }
            ExecMode::Execute => {
                if principal.is_model() {
                    return Err(ShellError::NeedsApproval(format!(
                        "{} {} by {}. Actions of a language model run through a plan",
                        cap.verb(),
                        cap.target(),
                        principal.name()
                    )));
                }
                match verdict {
                    Verdict::Allow => {
                        self.authority.prepare(&cap)?;
                        Ok(Permit::Execute)
                    }
                    Verdict::Ask => {
                        Err(ShellError::NeedsApproval(format!("{} {}, {reason}", cap.verb(), cap.target())))
                    }
                    Verdict::Deny => Err(ShellError::Refused(format!("{} {}, {reason}", cap.verb(), cap.target()))),
                }
            }
        }
    }

    /// For requests that cannot wait for a plan, such as sending data to a model while the
    /// plan is being computed. Only a deny or an unanswered ask for a model stops them.
    pub(crate) fn check_now(self: &Arc<Self>, stack: &Stack, cap: Cap) -> Result<()> {
        let (verdict, reason, _) = self.decide(stack, &cap)?;
        match verdict {
            Verdict::Allow => Ok(()),
            Verdict::Ask if !stack.principal().is_model() => Ok(()),
            _ => Err(ShellError::Refused(format!("{} {}, {reason}", cap.verb(), cap.target()))),
        }
    }

    /// `policy explain`. The decision for a request, without acting or recording it.
    pub fn explain(self: &Arc<Self>, stack: &Stack, cap: Cap) -> Result<Value> {
        let cap = cap.absolute();
        let (verdict, reason, facts) = self.decide(stack, &cap)?;
        let Value::Record(mut record) = Authority::request_record(stack, &cap, &facts) else { unreachable!() };
        record.type_name = Some("policy.decision".into());
        record.insert("decision", verdict.as_str());
        record.insert("because", reason);
        Ok(Value::Record(record))
    }
}
