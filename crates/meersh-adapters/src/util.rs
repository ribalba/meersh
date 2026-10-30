//! Helpers shared by the adapters. Command line scanning in the style of getopt, JSON field
//! access that tolerates older output formats, and date conversion.

use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use meersh_value::Value;
use serde_json::{Map, Value as Json};

pub(crate) type Object = Map<String, Json>;

/// One element of a command line, split the way getopt_long splits it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Opt<'a> {
    /// A short option such as `n` in `-tn`, with its value when it takes one.
    Short(char, Option<&'a str>),
    /// A long option without its dashes, with its value when it takes one.
    Long(&'a str, Option<&'a str>),
    /// A word that is not an option, including every word after `--`.
    Operand(&'a str),
}

/// Which options of a program take a value.
pub(crate) struct Spec {
    /// Short options with a required value, written attached (`-n5`) or as the next word.
    pub short: &'static str,
    /// Short options with an optional value, which is only taken when attached (`-uno`).
    pub short_optional: &'static str,
    /// Long options (without dashes) with a required value, after `=` or as the next word.
    pub long: &'static [&'static str],
}

impl Spec {
    pub(crate) const NONE: Spec = Spec { short: "", short_optional: "", long: &[] };
}

/// Splits `args` into options and operands. Each item carries the index of the word it
/// starts at, so callers can find the subcommand of git or docker.
pub(crate) fn scan<'a>(args: &'a [String], spec: &Spec) -> Vec<(usize, Opt<'a>)> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut only_operands = false;
    while i < args.len() {
        let arg = args[i].as_str();
        let start = i;
        i += 1;
        if only_operands || arg == "-" || !arg.starts_with('-') {
            out.push((start, Opt::Operand(arg)));
            continue;
        }
        if arg == "--" {
            only_operands = true;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            if let Some((name, value)) = long.split_once('=') {
                out.push((start, Opt::Long(name, Some(value))));
            } else if spec.long.contains(&long) {
                let value = args.get(i).map(String::as_str);
                i += usize::from(value.is_some());
                out.push((start, Opt::Long(long, value)));
            } else {
                out.push((start, Opt::Long(long, None)));
            }
            continue;
        }
        let cluster = &arg[1..];
        for (pos, c) in cluster.char_indices() {
            let rest = &cluster[pos + c.len_utf8()..];
            if spec.short.contains(c) {
                if rest.is_empty() {
                    let value = args.get(i).map(String::as_str);
                    i += usize::from(value.is_some());
                    out.push((start, Opt::Short(c, value)));
                } else {
                    out.push((start, Opt::Short(c, Some(rest))));
                }
                break;
            }
            if spec.short_optional.contains(c) {
                out.push((start, Opt::Short(c, (!rest.is_empty()).then_some(rest))));
                break;
            }
            out.push((start, Opt::Short(c, None)));
        }
    }
    out
}

/// True when one of the scanned items is the short option `short` or the long option `long`.
pub(crate) fn has(opts: &[(usize, Opt)], short: Option<char>, long: &[&str]) -> bool {
    value_of(opts, short, long).is_some()
}

/// The item for the short option `short` or one of the long options `long`, with its value.
/// The last one wins, as with most programs.
pub(crate) fn value_of<'a>(opts: &[(usize, Opt<'a>)], short: Option<char>, long: &[&str]) -> Option<Option<&'a str>> {
    opts.iter().rev().find_map(|(_, opt)| match *opt {
        Opt::Short(c, value) if Some(c) == short => Some(value),
        Opt::Long(name, value) if long.contains(&name) => Some(value),
        _ => None,
    })
}

/// Arguments with `extra` inserted at `at`.
pub(crate) fn insert_at(args: &[String], at: usize, extra: &[&str]) -> Vec<String> {
    let mut out = args[..at].to_vec();
    out.extend(extra.iter().map(|s| s.to_string()));
    out.extend_from_slice(&args[at..]);
    out
}

/// Arguments with `extra` added before a `--`, or at the end when there is none, so that they
/// win over earlier options of the user.
pub(crate) fn insert_last(args: &[String], extra: &[&str]) -> Vec<String> {
    let at = args.iter().position(|a| a == "--").unwrap_or(args.len());
    insert_at(args, at, extra)
}

/// Every top level JSON value in `text`. Accepts one document, concatenated documents and
/// JSON lines, and flattens top level arrays.
pub(crate) fn json_values(text: &str) -> Result<Vec<Json>, String> {
    let mut out = Vec::new();
    for value in serde_json::Deserializer::from_str(text).into_iter::<Json>() {
        match value.map_err(|e| format!("not valid JSON ({e})"))? {
            Json::Array(items) => out.extend(items),
            other => out.push(other),
        }
    }
    Ok(out)
}

/// Every top level JSON object in `text`, or an error naming what was found instead.
pub(crate) fn json_objects(text: &str) -> Result<Vec<Object>, String> {
    json_values(text)?
        .into_iter()
        .map(|value| match value {
            Json::Object(map) => Ok(map),
            other => Err(format!("expected JSON objects, found {}", kind(&other))),
        })
        .collect()
}

fn kind(json: &Json) -> &'static str {
    match json {
        Json::Null => "null",
        Json::Bool(_) => "a boolean",
        Json::Number(_) => "a number",
        Json::String(_) => "a string",
        Json::Array(_) => "an array",
        Json::Object(_) => "an object",
    }
}

/// A string field, `Nothing` when it is missing, null or empty.
pub(crate) fn text(obj: &Object, key: &str) -> Value {
    match obj.get(key) {
        Some(Json::String(s)) if !s.is_empty() => Value::String(s.clone()),
        Some(Json::Number(n)) => Value::String(n.to_string()),
        Some(Json::Bool(b)) => Value::String(b.to_string()),
        _ => Value::Nothing,
    }
}

/// An integer field. Older tools write numbers as strings, so those are accepted too.
pub(crate) fn int(obj: &Object, key: &str) -> Value {
    obj.get(key).and_then(json_i64).map_or(Value::Nothing, Value::Int)
}

/// A size in bytes, as a number or a numeric string.
pub(crate) fn size(obj: &Object, key: &str) -> Value {
    obj.get(key).and_then(json_i64).map_or(Value::Nothing, Value::Filesize)
}

/// A boolean field, written as `true`, `1` or `"1"` depending on the tool version.
pub(crate) fn boolean(obj: &Object, key: &str) -> Value {
    match obj.get(key) {
        Some(Json::Bool(b)) => Value::Bool(*b),
        Some(Json::Number(n)) => Value::Bool(n.as_i64() != Some(0)),
        Some(Json::String(s)) => match s.as_str() {
            "1" | "true" | "yes" => Value::Bool(true),
            "0" | "false" | "no" | "" => Value::Bool(false),
            _ => Value::Nothing,
        },
        _ => Value::Nothing,
    }
}

/// A list of strings. A single string becomes a list of one, nulls are dropped.
pub(crate) fn strings(obj: &Object, key: &str) -> Value {
    match obj.get(key) {
        Some(Json::Array(items)) => {
            Value::List(items.iter().filter_map(|v| v.as_str()).map(|s| Value::String(s.to_string())).collect())
        }
        Some(Json::String(s)) => Value::List(vec![Value::String(s.clone())]),
        _ => Value::List(vec![]),
    }
}

pub(crate) fn json_i64(json: &Json) -> Option<i64> {
    match json {
        Json::Number(n) => n.as_i64().or_else(|| n.as_u64().map(|u| i64::try_from(u).unwrap_or(i64::MAX))),
        Json::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// A JSON key as a meersh column name. `maj:min` becomes `maj_min` and `use%` becomes `use_pct`.
pub(crate) fn column_name(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for c in key.chars() {
        match c {
            '%' => out.push_str(if out.is_empty() || out.ends_with('_') { "pct" } else { "_pct" }),
            c if c.is_alphanumeric() => out.extend(c.to_lowercase()),
            _ if !out.ends_with('_') => out.push('_'),
            _ => {}
        }
    }
    out.trim_matches('_').to_string()
}

/// Microseconds since the epoch as a date in the local time zone. Zero and `u64::MAX` mean
/// "never" in systemd and become `Nothing`.
pub(crate) fn date_from_micros(micros: i64) -> Value {
    if micros <= 0 || micros == i64::MAX {
        return Value::Nothing;
    }
    DateTime::from_timestamp_micros(micros)
        .map_or(Value::Nothing, |d| Value::Date(d.with_timezone(&Local).fixed_offset()))
}

/// Seconds since the epoch as a date in the local time zone.
pub(crate) fn date_from_secs(secs: i64) -> Value {
    DateTime::from_timestamp(secs, 0).map_or(Value::Nothing, |d| Value::Date(d.with_timezone(&Local).fixed_offset()))
}

/// A wall clock time without a zone, read as local time.
pub(crate) fn local_date(naive: NaiveDateTime) -> Value {
    Local.from_local_datetime(&naive).earliest().map_or(Value::Nothing, |d| Value::Date(d.fixed_offset()))
}

/// The value of a token that is a number, a placeholder such as `*` or `-`, or a word.
pub(crate) fn int_or_text(token: &str) -> Value {
    match token {
        "" | "*" | "-" => Value::Nothing,
        _ => token.parse::<i64>().map_or_else(|_| Value::String(token.to_string()), Value::Int),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn scan_splits_clusters_values_and_operands() {
        let spec = Spec { short: "o", short_optional: "u", long: &["output"] };
        let args = words("-tno json --output x -uall --x=1 file -- -q");
        let opts = scan(&args, &spec);
        assert_eq!(
            opts.iter().map(|(_, o)| *o).collect::<Vec<_>>(),
            vec![
                Opt::Short('t', None),
                Opt::Short('n', None),
                Opt::Short('o', Some("json")),
                Opt::Long("output", Some("x")),
                Opt::Short('u', Some("all")),
                Opt::Long("x", Some("1")),
                Opt::Operand("file"),
                Opt::Operand("-q"),
            ]
        );
        assert_eq!(opts[6].0, 6);
        assert_eq!(value_of(&opts, Some('o'), &["output"]), Some(Some("x")));
    }

    #[test]
    fn column_names_are_plain_words() {
        assert_eq!(column_name("maj:min"), "maj_min");
        assert_eq!(column_name("use%"), "use_pct");
        assert_eq!(column_name("fsuse%"), "fsuse_pct");
        assert_eq!(column_name("buff/cache"), "buff_cache");
        assert_eq!(column_name("Use%"), "use_pct");
    }

    #[test]
    fn json_values_accepts_lines_and_arrays() {
        assert_eq!(json_values("{\"a\":1}\n{\"a\":2}\n").unwrap().len(), 2);
        assert_eq!(json_values("[{\"a\":1},{\"a\":2}]").unwrap().len(), 2);
        assert!(json_values("").unwrap().is_empty());
        assert!(json_values("not json").unwrap_err().contains("not valid JSON"));
        assert!(json_objects("[1]").unwrap_err().contains("a number"));
    }
}
