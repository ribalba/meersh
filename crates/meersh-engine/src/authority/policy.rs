use std::sync::Arc;

use meersh_parser::Expr;
use meersh_value::Value;

use super::{Cap, Facts, Ownership, Principal};
use crate::{Engine, Result, Stack};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Ask,
    Deny,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Allow => "allow",
            Verdict::Ask => "ask",
            Verdict::Deny => "deny",
        }
    }

    pub fn parse(text: &str) -> Option<Verdict> {
        Some(match text {
            "allow" => Verdict::Allow,
            "ask" => Verdict::Ask,
            "deny" => Verdict::Deny,
            _ => return None,
        })
    }
}

/// A user rule such as `policy allow actor fs.delete { owned }`. Rules are checked in order
/// before the built in defaults, and the first rule that matches decides.
#[derive(Debug, Clone)]
pub struct Rule {
    pub verdict: Verdict,
    /// `*`, `model`, `user`, `plan`, `actor`, `agent:*` or `agent:<name>`.
    pub principal: String,
    /// `*`, a kind such as `fs.delete`, or a prefix such as `fs.*`.
    pub cap: String,
    pub condition: Option<Expr>,
    /// Limits the rule to one task. Such rules end with the task.
    pub task: Option<String>,
    pub text: String,
}

impl Rule {
    pub(crate) fn matches(&self, principal: &Principal, kind: &str, task: Option<&str>) -> bool {
        let name = principal.name();
        let principal_ok = match self.principal.as_str() {
            "*" => true,
            "model" => principal.is_model(),
            "agent:*" => matches!(principal, Principal::Agent(_)),
            exact => exact == name,
        };
        let cap_ok = match self.cap.strip_suffix(".*") {
            _ if self.cap == "*" => true,
            Some(prefix) => kind.starts_with(prefix) && kind[prefix.len()..].starts_with('.'),
            None => self.cap == kind,
        };
        let task_ok = self.task.as_deref().is_none_or(|t| Some(t) == task);
        principal_ok && cap_ok && task_ok
    }

    pub(crate) fn condition_holds(&self, engine: &Arc<Engine>, stack: &Stack, request: &Value) -> Result<bool> {
        match &self.condition {
            None => Ok(true),
            Some(condition) => Ok(engine.eval_row(&stack.for_policy(), condition, request)?.is_truthy()),
        }
    }
}

/// The built in defaults. They implement "a session may remove what it created" and "a
/// model may do reversible things inside the project after asking, and nothing else".
pub(crate) fn default_decision(principal: &Principal, llm_chosen: bool, cap: &Cap, facts: &Facts) -> (Verdict, String) {
    use Verdict::*;
    let owned = facts.ownership == Ownership::Owned;
    let ownership_note = || match &facts.ownership {
        Ownership::Changed(why) => format!("created by this session but {why}"),
        Ownership::Other(who) => format!("created by {who}"),
        _ => "not created by this session".to_string(),
    };
    if !principal.is_model() {
        let picks_target = matches!(
            cap,
            Cap::FsDelete { .. }
                | Cap::FsMove { .. }
                | Cap::FsWrite { .. }
                | Cap::FsChmod { .. }
                | Cap::ProcSignal { .. }
        );
        if llm_chosen && picks_target && !owned {
            return (Ask, "a language model chose this target".into());
        }
        return (Allow, if owned { "created by this session".into() } else { String::new() });
    }
    let r = |v: Verdict, why: &str| (v, why.to_string());
    match cap {
        Cap::FsCreate { .. } if facts.in_project => r(Allow, "new file inside the project"),
        Cap::FsCreate { .. } => r(Ask, "outside the project"),
        Cap::FsMove { .. } if owned && facts.in_project => r(Allow, "created by this session"),
        Cap::FsDelete { .. } | Cap::FsWrite { .. } | Cap::FsChmod { .. } if owned => {
            r(Allow, "created by this session")
        }
        Cap::FsDelete { .. } | Cap::FsWrite { .. } | Cap::FsChmod { .. } | Cap::FsMove { .. } if !facts.in_project => {
            r(Deny, "outside the project")
        }
        Cap::FsDelete { permanent: true, .. } if !facts.reversible => {
            (Deny, format!("{} and cannot be undone", ownership_note()))
        }
        Cap::FsDelete { permanent: true, .. } => (Ask, format!("{}, git can restore it", ownership_note())),
        Cap::FsDelete { .. } => (Ask, format!("{}, can be restored from the trash", ownership_note())),
        Cap::FsWrite { .. } => (Ask, format!("{}, a snapshot of the old content is kept", ownership_note())),
        Cap::FsChmod { .. } | Cap::FsMove { .. } => (Ask, ownership_note()),
        Cap::ProcSignal { .. } if owned => r(Allow, "started by this session"),
        Cap::ProcSignal { .. } => r(Ask, "not started by this session, a signal cannot be undone"),
        Cap::ProcSpawn { .. } => r(Ask, "starts a program"),
        Cap::Exec { .. } if facts.reversible => r(Ask, "runs in a sandbox, its changes are shown before they are kept"),
        Cap::Exec { .. } => r(Ask, "runs an external program that meersh cannot see into"),
        Cap::LlmSend { local: true, .. } => r(Allow, "local model"),
        Cap::LlmSend { .. } => r(Ask, "sends data to a cloud model"),
    }
}
