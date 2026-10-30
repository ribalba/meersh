//! Tab completion that knows types. After `where`, `select`, `sort-by` and friends it offers
//! the columns of the data flowing in, taken from the output type of the upstream command
//! and the registered record types. Elsewhere it completes commands, flags and paths.

use std::sync::Arc;

use meersh_engine::Engine;
use meersh_engine::value::Type;
use reedline::{Completer, CompletionResult, Span, Suggestion};

pub struct MeershCompleter {
    pub engine: Arc<Engine>,
}

const COLUMN_COMMANDS: [&str; 9] =
    ["where", "select", "reject", "sort-by", "get", "group-by", "insert", "llm map", "uniq"];

fn suggestion(value: String, description: Option<String>, start: usize, end: usize) -> Suggestion {
    Suggestion { value, description, span: Span { start, end }, append_whitespace: true, ..Suggestion::default() }
}

impl MeershCompleter {
    /// The record type flowing into the stage that is being typed. Filters keep their input
    /// type, so the walk goes back until a stage names a record type.
    fn upstream_type(&self, stages: &[&str]) -> Option<String> {
        for stage in stages.iter().rev() {
            let words: Vec<&str> = stage.split_whitespace().collect();
            let name = match words.as_slice() {
                [first, second, ..] if self.engine.is_command(&format!("{first} {second}")) => {
                    format!("{first} {second}")
                }
                [first, ..] => first.to_string(),
                [] => continue,
            };
            let named = |ty: &Type| match ty {
                Type::Table { name: Some(n), .. } | Type::Record { name: Some(n), .. } => Some(n.clone()),
                _ => None,
            };
            if let Some(found) = self.engine.signature(&name).and_then(|s| named(&s.output)) {
                return Some(found);
            }
        }
        None
    }

    fn columns(&self, stages: &[&str]) -> Vec<(String, String)> {
        let Some(ty) = self.upstream_type(stages) else { return vec![] };
        let Some(info) = self.engine.type_info(&ty) else { return vec![] };
        info.fields.iter().map(|(name, ty, desc)| (name.clone(), format!("{ty}  {desc}"))).collect()
    }
}

impl Completer for MeershCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> CompletionResult {
        CompletionResult::fresh(self.suggestions(line, pos))
    }
}

impl MeershCompleter {
    fn suggestions(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        let before = &line[..pos];
        let start = before.rfind(|c: char| c.is_whitespace() || "|({[".contains(c)).map_or(0, |i| i + 1);
        let word = &before[start..];
        let stages: Vec<&str> = before.split('|').collect();
        let current = stages.last().copied().unwrap_or_default().trim_start();
        let previous = &stages[..stages.len().saturating_sub(1)];
        let head: Vec<&str> = current.split_whitespace().collect();
        let first_word = current.len() == word.len();

        if first_word {
            let mut out: Vec<Suggestion> = self
                .engine
                .signatures()
                .iter()
                .filter(|s| s.name.starts_with(word) && !s.name.contains(' '))
                .map(|s| suggestion(s.name.clone(), Some(s.description.clone()), start, pos))
                .collect();
            out.extend(
                self.engine
                    .aliases()
                    .into_iter()
                    .filter(|(name, _)| name.starts_with(word))
                    .map(|(name, exp)| suggestion(name, Some(format!("alias for {exp}")), start, pos)),
            );
            return out;
        }

        let command = match head.as_slice() {
            [a, b, ..] if self.engine.is_command(&format!("{a} {b}")) => format!("{a} {b}"),
            [a, ..] => a.to_string(),
            [] => String::new(),
        };
        if head.len() == 1 || (head.len() == 2 && !word.is_empty() && !self.engine.is_command(&command)) {
            let prefix = format!("{} {word}", head[0]);
            let subs: Vec<Suggestion> = self
                .engine
                .signatures()
                .iter()
                .filter(|s| s.name.starts_with(&prefix))
                .filter_map(|s| {
                    s.name
                        .split_once(' ')
                        .map(|(_, sub)| suggestion(sub.to_string(), Some(s.description.clone()), start, pos))
                })
                .collect();
            if !subs.is_empty() {
                return subs;
            }
        }
        if word.starts_with('-')
            && let Some(sig) = self.engine.signature(&command)
        {
            return sig
                .flags
                .iter()
                .map(|f| (format!("--{}", f.long), f.desc.clone()))
                .filter(|(flag, _)| flag.starts_with(word))
                .map(|(flag, desc)| suggestion(flag, Some(desc), start, pos))
                .collect();
        }
        if COLUMN_COMMANDS.contains(&command.as_str()) {
            let columns: Vec<Suggestion> = self
                .columns(previous)
                .into_iter()
                .filter(|(name, _)| name.starts_with(word))
                .map(|(name, desc)| suggestion(name, Some(desc), start, pos))
                .collect();
            if !columns.is_empty() {
                return columns;
            }
        }
        paths(word, start, pos)
    }
}

fn paths(word: &str, start: usize, end: usize) -> Vec<Suggestion> {
    let expanded = meersh_engine::util::expand_tilde(word);
    let pattern = format!("{expanded}*");
    let Ok(matches) = glob::glob(&pattern) else { return vec![] };
    matches
        .filter_map(Result::ok)
        .take(200)
        .map(|path| {
            let mut text = path.display().to_string();
            if word.starts_with('~')
                && let Some(home) = dirs::home_dir()
            {
                text = text.replacen(&home.display().to_string(), "~", 1);
            }
            let dir = path.is_dir();
            if dir {
                text.push('/');
            }
            Suggestion { append_whitespace: !dir, ..suggestion(text, None, start, end) }
        })
        .collect()
}
