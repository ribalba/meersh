//! Showing plans and asking the human. This is the only place where a person approves what
//! a model or a model chosen target is about to do.

use std::io::{BufRead, IsTerminal, Write};
use std::sync::Arc;

use meersh_engine::authority::{Plan, Rule};
use meersh_engine::render::{RenderOptions, render};
use meersh_engine::value::{Value, format_filesize};
use meersh_engine::{Consent, Engine, Stack, Verdict};

/// How a command line was started, which decides who can answer questions.
#[derive(Clone)]
pub enum Asker {
    Interactive,
    /// `-c`. Nobody can answer, so `--approve` or `--approve-only` decide up front.
    Batch {
        approve: bool,
        only: Option<Vec<usize>>,
    },
}

/// Row numbers such as `0 2-4` or `1,3`. `None` when the text is something else.
pub fn parse_selection(text: &str) -> Option<Vec<usize>> {
    let mut picked = Vec::new();
    for part in text.split(|c: char| c == ',' || c.is_whitespace()).filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                let (a, b): (usize, usize) = (a.parse().ok()?, b.parse().ok()?);
                if a > b {
                    return None;
                }
                picked.extend(a..=b);
            }
            None => picked.push(part.parse().ok()?),
        }
    }
    (!picked.is_empty()).then_some(picked)
}

/// Checks picked rows against the plan. Only rows that ask can be picked. Allowed rows are
/// in anyway and refused rows can never be approved.
fn check_selection(plan: &Plan, picked: &[usize]) -> Result<Vec<usize>, String> {
    let mut out = Vec::new();
    for &i in picked {
        match plan.actions.get(i).map(|a| a.verdict) {
            None => return Err(format!("there is no row {i}")),
            Some(Verdict::Deny) => return Err(format!("row {i} is refused and cannot be approved")),
            Some(Verdict::Allow) => {}
            Some(Verdict::Ask) => out.push(i),
        }
    }
    Ok(out)
}

/// Bold and dim text for the terminal, plain text for pipes, files and `NO_COLOR`.
pub fn styled(code: &str, text: &str, terminal: bool) -> String {
    if terminal && std::env::var_os("NO_COLOR").is_none() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

pub fn show_plan(plan: &Plan, terminal: bool) -> String {
    let mut out = String::new();
    for note in &plan.notes {
        out.push_str(&format!(
            "\n  {}\n  {}\n",
            styled("1", &note.pipeline, terminal),
            styled("2", &format!("# {} wrote this. {}", note.principal, note.explanation), terminal)
        ));
    }
    for review in &plan.reviews {
        out.push_str(&format!("\n  {}\n{}\n", review.title, render(&review.diff, &RenderOptions::detect())));
    }
    if !plan.actions.is_empty() {
        let rows = Value::List(plan.actions.iter().map(|a| a.to_value()).collect());
        out.push('\n');
        out.push_str(&render(&rows, &RenderOptions::detect()));
        out.push('\n');
        let count = |v: Verdict| plan.actions.iter().filter(|a| a.verdict == v).count();
        let bytes: i64 = plan.actions.iter().filter_map(|a| a.size.as_int()).sum();
        let irreversible = plan.actions.iter().filter(|a| !a.reversible && a.verdict != Verdict::Deny).count();
        out.push_str(&format!(
            "  {} actions, {}, {}. {} need your approval, {} refused.\n",
            plan.actions.len(),
            format_filesize(bytes),
            if irreversible == 0 { "all reversible".to_string() } else { format!("{irreversible} cannot be undone") },
            count(Verdict::Ask),
            count(Verdict::Deny),
        ));
    }
    out
}

/// Reads one answer with the line editor, so the terminal is handled the same way as for
/// command lines. Without a terminal it falls back to a plain line from stdin.
pub fn read_answer(prompt: &str) -> String {
    if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        use reedline::{DefaultPrompt, DefaultPromptSegment, Reedline, Signal};
        let prompt = DefaultPrompt::new(DefaultPromptSegment::Basic(prompt.to_string()), DefaultPromptSegment::Empty);
        return match Reedline::create().read_line(&prompt) {
            Ok(Signal::Success(answer)) => answer.trim().to_ascii_lowercase(),
            _ => String::new(),
        };
    }
    print!("{prompt} ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    let _ = std::io::stdin().lock().read_line(&mut answer);
    answer.trim().to_ascii_lowercase()
}

/// The narrowest rule that would have allowed the approved plan. It names the exact targets
/// and ends with the current task, if there is one.
fn narrow_rules(plan: &Plan, stack: &Stack, picked: Option<&[usize]>) -> Vec<Rule> {
    let mut rules: Vec<Rule> = Vec::new();
    let chosen = |i: &usize| picked.is_none_or(|p| p.contains(i));
    for (i, action) in plan.actions.iter().enumerate().filter(|(i, a)| a.verdict == Verdict::Ask && chosen(i)) {
        // Protected paths are checked before every rule, so a rule could never allow them.
        if action.reason.starts_with("protected") {
            println!("  row {i} is a protected path. It is approved this time, but no rule can allow it.");
            continue;
        }
        let existing = rules.iter_mut().find(|r| r.principal == action.principal && r.cap == action.kind);
        let target = Value::String(action.target.clone());
        match existing {
            Some(rule) => {
                if let Some(meersh_engine::parser::Expr::Binary(_, _, list)) = &mut rule.condition
                    && let meersh_engine::parser::Expr::List(items) = list.as_mut()
                {
                    items.push(meersh_engine::parser::Expr::Str(target.to_text()));
                }
            }
            None => {
                use meersh_engine::parser::{BinOp, Expr};
                let condition = Expr::Binary(
                    Box::new(Expr::Word("target".into())),
                    BinOp::In,
                    Box::new(Expr::List(vec![Expr::Str(target.to_text())])),
                );
                rules.push(Rule {
                    verdict: Verdict::Allow,
                    principal: action.principal.clone(),
                    cap: action.kind.clone(),
                    condition: Some(condition),
                    task: stack.task().map(str::to_string),
                    text: String::new(),
                });
            }
        }
    }
    for rule in &mut rules {
        let condition = rule.condition.as_ref().map(ToString::to_string).unwrap_or_default();
        rule.text = format!("allow {} {} {condition}", rule.principal, rule.cap);
        if let Some(task) = &rule.task {
            rule.text.push_str(&format!(" --task {task}"));
        }
    }
    rules
}

pub fn decide(engine: &Arc<Engine>, stack: &Stack, plan: &Plan, asker: &Asker) -> Consent {
    let terminal = match asker {
        Asker::Interactive => std::io::stdout().is_terminal(),
        Asker::Batch { .. } => std::io::stderr().is_terminal(),
    };
    let text = show_plan(plan, terminal);
    let needs_human = plan.actions.iter().any(|a| a.verdict == Verdict::Ask) || !plan.reviews.is_empty();
    let approvable = plan.actions.iter().any(|a| a.verdict != Verdict::Deny) || !plan.reviews.is_empty();
    match asker {
        Asker::Batch { approve, only } => {
            eprint!("{text}");
            if !approvable {
                eprintln!("  nothing in this plan may run.");
                return Consent::Reject;
            }
            if let Some(picked) = only {
                return match check_selection(plan, picked) {
                    Ok(picked) => Consent::Only(picked),
                    Err(e) => {
                        eprintln!("  not applied, {e}.");
                        Consent::Reject
                    }
                };
            }
            if needs_human && !approve {
                eprintln!("  not applied. Run interactively, or add --approve or --approve-only <rows>.");
                return Consent::Reject;
            }
            Consent::Approve
        }
        Asker::Interactive => {
            print!("{text}");
            if !approvable {
                println!("  nothing in this plan may run.");
                return Consent::Reject;
            }
            let prompt = if needs_human {
                "approve? [y]es all / rows such as 0 2-3 / [r]ule [rows] / [N]o"
            } else {
                "approve? [Y]es / [n]o"
            };
            loop {
                let answer = read_answer(prompt);
                let (rule, rest) = match answer.strip_prefix('r') {
                    Some(rest) if rest.is_empty() || rest.starts_with([' ', 'u']) => {
                        (true, rest.trim_start_matches("ule").trim())
                    }
                    _ => (false, answer.as_str()),
                };
                let picked = match rest {
                    "" if rule => None,
                    "y" | "yes" if !rule => return Consent::Approve,
                    "" if !needs_human => return Consent::Approve,
                    "" | "n" | "no" => return Consent::Reject,
                    rows => match parse_selection(rows).map(|p| check_selection(plan, &p)) {
                        Some(Ok(picked)) => Some(picked),
                        Some(Err(e)) => {
                            println!("  {e}");
                            continue;
                        }
                        None => {
                            println!("  answer y, n, r, or row numbers such as 0 2-3");
                            continue;
                        }
                    },
                };
                if rule {
                    for rule in narrow_rules(plan, stack, picked.as_deref()) {
                        println!("  new rule: policy {}", rule.text);
                        engine.authority.add_rule(rule);
                    }
                }
                return match picked {
                    Some(picked) => Consent::Only(picked),
                    None => Consent::Approve,
                };
            }
        }
    }
}

/// After a line ran, pending sandboxes with changes are shown and kept or thrown away.
pub fn review_sandboxes(engine: &Arc<Engine>, stack: &Stack, asker: &Asker) {
    for sandbox in engine.authority.take_sandboxes() {
        let changes = sandbox.changes();
        if changes.is_empty() {
            sandbox.discard();
            continue;
        }
        let table = render(&Value::List(changes), &RenderOptions::detect());
        let keep = match asker {
            Asker::Interactive => {
                println!("\n  the sandbox {} changed this:\n{table}", sandbox.id);
                matches!(read_answer("keep these changes? [y/N]").as_str(), "y" | "yes")
            }
            Asker::Batch { approve, .. } => {
                eprintln!("\n  the sandbox {} changed this:\n{table}", sandbox.id);
                if !approve {
                    eprintln!("  discarded. Add --approve to keep sandbox changes.");
                }
                *approve
            }
        };
        if keep {
            match sandbox.commit(&engine.authority, stack) {
                Ok(rows) => println!("{}", render(&Value::List(rows), &RenderOptions::detect())),
                Err(e) => eprintln!("meersh: cannot commit the sandbox ({e})"),
            }
        } else {
            sandbox.discard();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_selection;

    #[test]
    fn selections() {
        assert_eq!(parse_selection("0 2-4"), Some(vec![0, 2, 3, 4]));
        assert_eq!(parse_selection("1,3"), Some(vec![1, 3]));
        assert_eq!(parse_selection("y"), None);
        assert_eq!(parse_selection("3-1"), None);
        assert_eq!(parse_selection(""), None);
    }
}
