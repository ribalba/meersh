//! An example meersh plugin with two commands. `words` counts words and is pure. `stamp`
//! appends a timestamp to a file, so it writes and runs only when the authority allows it.
//!
//! ```sh
//! cargo build -p meersh-plugin-example
//! meersh -c 'plugin add ./target/debug/meersh_plugin_example'
//! meersh -c 'open README.md | words | first 5'
//! ```

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::process::ExitCode;

use meersh_plugin::{Effect, Plugin, Record, Signature, Type, Value};

struct Example;

impl Plugin for Example {
    fn signatures(&self) -> Vec<Signature> {
        vec![
            Signature::build("words")
                .desc("Count the words of the input text, the most frequent first.")
                .input(Type::String)
                .output(Type::table("word.count"))
                .example("open README.md | words | first 5", "the five most common words"),
            Signature::build("stamp")
                .desc("Append the current time as a line to a file.")
                .input(Type::Nothing)
                .output(Type::record("fs.change"))
                .required("path", Type::String, "the file to stamp")
                .effect(Effect::Write)
                .example("stamp notes.txt", "notes.txt ends with the current time"),
        ]
    }

    fn call(
        &self,
        name: &str,
        args: Vec<Value>,
        _: BTreeMap<String, Value>,
        input: Vec<Value>,
    ) -> Result<Vec<Value>, String> {
        match name {
            "words" => words(input),
            "stamp" => stamp(args.first().and_then(Value::as_str).ok_or("missing argument <path>")?),
            other => Err(format!("no command named {other}")),
        }
    }
}

fn words(input: Vec<Value>) -> Result<Vec<Value>, String> {
    let mut counts: BTreeMap<String, i64> = BTreeMap::new();
    for value in input {
        let Value::String(text) = value else { return Err(format!("expected text, found {}", value.type_name())) };
        for word in text.split_whitespace() {
            let word = word.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
            if !word.is_empty() {
                *counts.entry(word).or_default() += 1;
            }
        }
    }
    // The map is sorted by word and the sort is stable, so equal counts stay alphabetical.
    let mut rows: Vec<(String, i64)> = counts.into_iter().collect();
    rows.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    let row = |(word, count): (String, i64)| {
        Value::Record(Record::typed("word.count").with("word", word).with("count", count))
    };
    Ok(rows.into_iter().map(row).collect())
}

fn stamp(path: &str) -> Result<Vec<Value>, String> {
    let fail = |e: std::io::Error| format!("cannot stamp {path} ({e})");
    let now = chrono::Local::now().fixed_offset();
    let mut file = OpenOptions::new().create(true).append(true).open(path).map_err(fail)?;
    writeln!(file, "{}", now.to_rfc3339()).map_err(fail)?;
    let size = file.metadata().map_err(fail)?.len() as i64;
    let change = Record::typed("fs.change")
        .with("path", path)
        .with("action", "stamped")
        .with("size", Value::Filesize(size))
        .with("at", Value::Date(now));
    Ok(vec![Value::Record(change)])
}

fn main() -> ExitCode {
    meersh_plugin::serve(&Example)
}
