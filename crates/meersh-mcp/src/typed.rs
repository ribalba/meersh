//! Typed tools generated from builtin commands.
//!
//! Every command that needs no pipeline input and only reads or transforms becomes a tool
//! named `cmd_<name>`. Its input schema comes from the signature. A call turns the arguments
//! back into meersh source with every value quoted, and that source runs like `run`.

use std::collections::HashSet;
use std::sync::Arc;

use meersh_engine::{Effect, Engine, Signature};
use meersh_value::{Type, Value, parse_literal};
use serde_json::{Map, Value as Json, json};

use crate::tools::USER_ONLY;

pub(crate) struct TypedTool {
    pub(crate) name: String,
    signature: Arc<Signature>,
}

/// Where a property of the input schema goes in the command line.
enum Slot {
    Param(usize),
    Rest,
    Flag(usize),
}

fn takes_no_input(ty: &Type) -> bool {
    match ty {
        Type::Nothing => true,
        Type::OneOf(types) => types.contains(&Type::Nothing),
        _ => false,
    }
}

fn qualifies(sig: &Signature) -> bool {
    takes_no_input(&sig.input)
        && matches!(sig.effect, Effect::Pure | Effect::Read)
        && !USER_ONLY.contains(&sig.name.as_str())
}

/// `date now` becomes `cmd_date_now`. Anything an MCP client might reject in a tool name
/// becomes an underscore as well.
fn tool_name(command: &str) -> String {
    let body: String =
        command.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
    let mut name = format!("cmd_{body}");
    name.truncate(64);
    name
}

pub(crate) fn tools(engine: &Engine) -> Vec<TypedTool> {
    let mut seen = HashSet::new();
    engine
        .signatures()
        .into_iter()
        .filter(|sig| qualifies(sig))
        .filter_map(|signature| {
            let name = tool_name(&signature.name);
            seen.insert(name.clone()).then_some(TypedTool { name, signature })
        })
        .collect()
}

pub(crate) fn find(engine: &Engine, name: &str) -> Option<TypedTool> {
    tools(engine).into_iter().find(|tool| tool.name == name)
}

/// JSON Schema for a value that a model writes as an argument. It follows
/// [`Type::to_json_schema`] where that fits and is looser where an argument needs it. A
/// record without declared fields takes any object, and a union offers each of its members.
pub(crate) fn schema(ty: &Type) -> Json {
    match ty {
        Type::Record { fields, .. } if fields.is_empty() => json!({"type": "object"}),
        Type::Record { fields, .. } => {
            let properties: Map<String, Json> = fields.iter().map(|(k, t)| (k.clone(), schema(t))).collect();
            json!({"type": "object", "properties": properties})
        }
        Type::Table { fields, .. } => {
            json!({"type": "array", "items": schema(&Type::Record { name: None, fields: fields.clone() })})
        }
        Type::List(inner) => json!({"type": "array", "items": schema(inner)}),
        Type::OneOf(types) => {
            let mut options: Vec<Json> = types.iter().filter(|t| **t != Type::Nothing).map(schema).collect();
            match options.len() {
                0 => json!({"type": "null"}),
                1 => options.remove(0),
                _ => json!({"anyOf": options}),
            }
        }
        Type::Block => json!({
            "type": "string",
            "description": "meersh code for a block, without the braces, for example `$it.size > 1mb`"
        }),
        Type::Date => json!({"type": "string", "format": "date-time", "description": "RFC 3339 timestamp"}),
        other => other.to_json_schema(),
    }
}

/// Adds the description of a parameter. A unit that the schema already explains is kept.
fn described(mut schema: Json, desc: &str) -> Json {
    if desc.is_empty() {
        return schema;
    }
    let text = match schema.get("description").and_then(Json::as_str) {
        Some(unit) => format!("{desc} ({unit})"),
        None => desc.to_string(),
    };
    schema["description"] = json!(text);
    schema
}

/// A string literal that the meersh lexer reads back exactly.
pub(crate) fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn integer(n: &serde_json::Number) -> Option<i64> {
    n.as_i64().or_else(|| n.as_f64().filter(|f| f.fract() == 0.0 && f.abs() < 9.0e15).map(|f| f as i64))
}

/// Any JSON value as a meersh literal.
fn literal(value: &Json) -> String {
    match value {
        Json::Null => "null".into(),
        Json::Bool(b) => b.to_string(),
        Json::Number(n) => n.to_string(),
        Json::String(s) => quote(s),
        Json::Array(items) => format!("[{}]", items.iter().map(literal).collect::<Vec<_>>().join(" ")),
        Json::Object(map) => {
            let fields: Vec<String> = map.iter().map(|(k, v)| format!("{}: {}", quote(k), literal(v))).collect();
            format!("{{{}}}", fields.join(", "))
        }
    }
}

/// A JSON argument as meersh source for a value of type `ty`. Strings are always quoted, so
/// an argument can never turn into a flag, a variable or a second command.
pub(crate) fn render(value: &Json, ty: &Type) -> Result<String, String> {
    let wrong = || Err(format!("must be {}", expected(ty)));
    match (ty, value) {
        (_, Json::Null) => Ok("null".into()),
        (Type::Any, v) => Ok(literal(v)),
        (Type::String | Type::Binary, Json::String(s)) => Ok(quote(s)),
        (Type::String | Type::Binary, Json::Number(_) | Json::Bool(_)) => Ok(quote(&value.to_string())),
        (Type::Int, Json::Number(n)) => integer(n).map(|i| i.to_string()).map_or_else(wrong, Ok),
        (Type::Int, Json::String(s)) if s.trim().parse::<i64>().is_ok() => Ok(s.trim().to_string()),
        // Debug keeps a fraction or an exponent, so 3.0 stays a float.
        (Type::Float, Json::Number(n)) => Ok(format!("{:?}", n.as_f64().unwrap_or_default())),
        (Type::Number, Json::Number(n)) => Ok(n.to_string()),
        (Type::Bool, Json::Bool(b)) => Ok(b.to_string()),
        (Type::Filesize, Json::Number(n)) => {
            integer(n).or_else(|| n.as_f64().map(|f| f as i64)).map(|i| format!("{i}b")).map_or_else(wrong, Ok)
        }
        (Type::Filesize, Json::String(s)) => match parse_literal(s.trim()) {
            Some(Value::Filesize(_)) => Ok(s.trim().to_string()),
            Some(Value::Int(i)) => Ok(format!("{i}b")),
            _ => wrong(),
        },
        // f64 Display never uses an exponent, which the duration parser would not accept.
        (Type::Duration, Json::Number(n)) => Ok(format!("{}sec", n.as_f64().unwrap_or_default())),
        (Type::Duration, Json::String(s)) if matches!(parse_literal(s.trim()), Some(Value::Duration(_))) => {
            Ok(s.trim().to_string())
        }
        (Type::Date, Json::String(s)) if matches!(Type::Date.coerce(value), Value::Date(_)) => Ok(quote(s)),
        (Type::List(inner), Json::Array(items)) => list(items, inner),
        (Type::List(inner), single) => list(std::slice::from_ref(single), inner),
        (Type::Table { fields, .. }, Json::Array(items)) => {
            list(items, &Type::Record { name: None, fields: fields.clone() })
        }
        (Type::Record { fields, .. }, Json::Object(map)) => {
            let mut parts = Vec::with_capacity(map.len());
            for (key, v) in map {
                let field_ty = fields.iter().find(|(name, _)| name == key).map_or(&Type::Any, |(_, t)| t);
                let rendered = render(v, field_ty).map_err(|e| format!("field '{key}' {e}"))?;
                parts.push(format!("{}: {rendered}", quote(key)));
            }
            Ok(format!("{{{}}}", parts.join(", ")))
        }
        (Type::OneOf(types), v) => types.iter().find_map(|t| render(v, t).ok()).map_or_else(wrong, Ok),
        (Type::Block, Json::String(code)) => Ok(format!("{{ {code} }}")),
        _ => wrong(),
    }
}

fn list(items: &[Json], inner: &Type) -> Result<String, String> {
    let rendered = items.iter().map(|item| render(item, inner)).collect::<Result<Vec<_>, _>>()?;
    Ok(format!("[{}]", rendered.join(" ")))
}

fn expected(ty: &Type) -> String {
    match ty {
        Type::Int => "an integer".into(),
        Type::Float | Type::Number => "a number".into(),
        Type::Bool => "true or false".into(),
        Type::String | Type::Binary => "a string".into(),
        Type::Filesize => "a size in bytes or a string such as 10mb".into(),
        Type::Duration => "a number of seconds or a string such as 10min".into(),
        Type::Date => "an RFC 3339 timestamp".into(),
        Type::Block => "a string of meersh code".into(),
        Type::Nothing => "null".into(),
        Type::List(_) | Type::Table { .. } => "an array".into(),
        Type::Record { .. } => "an object".into(),
        other => format!("a value of type {other}"),
    }
}

impl TypedTool {
    pub(crate) fn command(&self) -> &str {
        &self.signature.name
    }

    /// The input schema properties in command line order. Positional parameters come first,
    /// then the rest parameter, then the flags. A flag named like a parameter gets `_flag`.
    fn slots(&self) -> Vec<(String, Slot)> {
        let sig = &self.signature;
        let mut slots: Vec<(String, Slot)> =
            sig.params.iter().enumerate().map(|(i, p)| (p.name.clone(), Slot::Param(i))).collect();
        if let Some(rest) = &sig.rest {
            slots.push((rest.name.clone(), Slot::Rest));
        }
        for (i, flag) in sig.flags.iter().enumerate() {
            let taken = slots.iter().any(|(name, _)| *name == flag.long);
            slots.push((if taken { format!("{}_flag", flag.long) } else { flag.long.clone() }, Slot::Flag(i)));
        }
        slots
    }

    fn description(&self) -> String {
        let sig = &self.signature;
        let mut text = sig.description.trim().to_string();
        if text.is_empty() {
            text = format!("The meersh command `{}`.", sig.name);
        } else if !text.ends_with(['.', '!', '?']) {
            text.push('.');
        }
        if !matches!(sig.output, Type::Any | Type::Nothing) {
            text.push_str(&format!(" Returns {}.", sig.output));
        }
        text
    }

    pub(crate) fn definition(&self) -> Json {
        let sig = &self.signature;
        let mut properties = Map::new();
        let mut required = Vec::new();
        for (property, slot) in self.slots() {
            let schema = match slot {
                Slot::Param(i) => {
                    let param = &sig.params[i];
                    if !param.optional {
                        required.push(property.clone());
                    }
                    described(schema(&param.ty), &param.desc)
                }
                Slot::Rest => {
                    let rest = sig.rest.as_ref().expect("a rest slot has a rest parameter");
                    described(json!({"type": "array", "items": schema(&rest.ty)}), &rest.desc)
                }
                Slot::Flag(i) => {
                    let flag = &sig.flags[i];
                    let schema = flag.arg.as_ref().map_or_else(|| json!({"type": "boolean"}), schema);
                    described(schema, &flag.desc)
                }
            };
            properties.insert(property, schema);
        }
        let mut input = json!({"type": "object", "properties": properties, "additionalProperties": false});
        if !required.is_empty() {
            input["required"] = json!(required);
        }
        json!({"name": self.name, "description": self.description(), "inputSchema": input})
    }

    /// The meersh source for a call with these arguments.
    pub(crate) fn pipeline(&self, args: &Json) -> Result<String, String> {
        let sig = &self.signature;
        let slots = self.slots();
        if let Json::Object(map) = args
            && let Some(unknown) = map.keys().find(|key| !slots.iter().any(|(name, _)| name == *key))
        {
            let known: Vec<&str> = slots.iter().map(|(name, _)| name.as_str()).collect();
            return Err(if known.is_empty() {
                format!("unknown argument '{unknown}'. {} takes no arguments", self.name)
            } else {
                format!("unknown argument '{unknown}'. {} takes {}", self.name, known.join(", "))
            });
        }
        let mut parts = vec![sig.name.clone()];
        // Arguments are positional, so a later one cannot be given without the earlier ones.
        let mut gap: Option<&str> = None;
        for (property, slot) in &slots {
            let value = args.get(property).filter(|v| !v.is_null());
            let field = |e: String| format!("'{property}' {e}");
            match slot {
                Slot::Param(i) => {
                    let param = &sig.params[*i];
                    match value {
                        Some(v) => {
                            if let Some(gap) = gap {
                                return Err(format!("'{property}' needs '{gap}' as well, because both are positional"));
                            }
                            parts.push(render(v, &param.ty).map_err(field)?);
                        }
                        None if !param.optional => return Err(format!("missing argument '{property}'")),
                        None => {
                            gap.get_or_insert(property);
                        }
                    }
                }
                Slot::Rest => {
                    let rest = sig.rest.as_ref().expect("a rest slot has a rest parameter");
                    let items = match value {
                        None => &[][..],
                        Some(Json::Array(items)) => items.as_slice(),
                        Some(single) => std::slice::from_ref(single),
                    };
                    if let (Some(gap), false) = (gap, items.is_empty()) {
                        return Err(format!("'{property}' needs '{gap}' as well, because both are positional"));
                    }
                    for item in items {
                        parts.push(render(item, &rest.ty).map_err(field)?);
                    }
                }
                Slot::Flag(i) => {
                    let flag = &sig.flags[*i];
                    match (&flag.arg, value) {
                        (_, None) | (None, Some(Json::Bool(false))) => {}
                        (None, Some(Json::Bool(true))) => parts.push(format!("--{}", flag.long)),
                        (None, Some(_)) => return Err(field("must be true or false".into())),
                        (Some(ty), Some(v)) => {
                            parts.push(format!("--{}", flag.long));
                            parts.push(render(v, ty).map_err(field)?);
                        }
                    }
                }
            }
        }
        Ok(parts.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_quoted_so_they_stay_values() {
        assert_eq!(quote("a \"b\" \\ c\nd"), r#""a \"b\" \\ c\nd""#);
        assert_eq!(render(&json!("--all; rm -rf /"), &Type::String).unwrap(), r#""--all; rm -rf /""#);
        assert_eq!(render(&json!(3), &Type::Float).unwrap(), "3.0");
        assert_eq!(render(&json!(-5), &Type::Int).unwrap(), "-5");
        assert_eq!(render(&json!(2048), &Type::Filesize).unwrap(), "2048b");
        assert_eq!(render(&json!("10mb"), &Type::Filesize).unwrap(), "10mb");
        assert_eq!(render(&json!(1.5), &Type::Duration).unwrap(), "1.5sec");
        assert_eq!(render(&json!(["a", "b"]), &Type::list(Type::String)).unwrap(), r#"["a" "b"]"#);
        assert_eq!(render(&json!({"k": [1, null]}), &Type::Any).unwrap(), r#"{"k": [1 null]}"#);
        assert!(render(&json!("x"), &Type::Int).is_err());
        assert!(render(&json!("yesterday"), &Type::Date).is_err());
    }

    #[test]
    fn literals_round_trip_through_the_parser() {
        for (text, ty) in [("10mb", Type::Filesize), ("1.5sec", Type::Duration), ("3.0", Type::Float)] {
            let value = parse_literal(text).unwrap();
            assert_eq!(value.ty(), ty, "{text}");
        }
    }

    #[test]
    fn schemas_accept_what_a_command_accepts() {
        assert_eq!(schema(&Type::Record { name: None, fields: vec![] }), json!({"type": "object"}));
        assert_eq!(
            schema(&Type::OneOf(vec![Type::Nothing, Type::Int, Type::String])),
            json!({"anyOf": [{"type": "integer"}, {"type": "string"}]})
        );
        assert_eq!(schema(&Type::list(Type::Int)), json!({"type": "array", "items": {"type": "integer"}}));
        assert_eq!(tool_name("date now"), "cmd_date_now");
        assert_eq!(tool_name("sort-by"), "cmd_sort-by");
    }
}
