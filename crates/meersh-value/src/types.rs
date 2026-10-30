use std::fmt;

use serde_json::{Map, Value as Json, json};

use crate::{Record, Value, parse_literal};

/// Types are used in three places. Command signatures declare their input and output types,
/// `describe` reports the structural type of a value, and `llm --schema` turns a type into a
/// JSON Schema so the model has to answer with data the next stage can consume.
#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    Any,
    Nothing,
    Bool,
    Int,
    Float,
    Number,
    Filesize,
    Duration,
    Date,
    String,
    Binary,
    Block,
    List(Box<Type>),
    Record { name: Option<String>, fields: Vec<(String, Type)> },
    Table { name: Option<String>, fields: Vec<(String, Type)> },
    OneOf(Vec<Type>),
}

impl Type {
    pub fn record(name: &str) -> Type {
        Type::Record { name: Some(name.into()), fields: vec![] }
    }

    pub fn table(name: &str) -> Type {
        Type::Table { name: Some(name.into()), fields: vec![] }
    }

    pub fn any_table() -> Type {
        Type::Table { name: None, fields: vec![] }
    }

    pub fn list(inner: Type) -> Type {
        Type::List(Box::new(inner))
    }

    /// Parses a type written as a meersh value, for example `int`, `[string]` or
    /// `{name: string, size: filesize, tags: [string]}`.
    pub fn from_spec(spec: &Value) -> Result<Type, String> {
        match spec {
            Value::String(s) => Self::from_name(s).ok_or_else(|| format!("unknown type '{s}'")),
            Value::List(items) if items.len() == 1 => Ok(Type::list(Self::from_spec(&items[0])?)),
            Value::List(_) => Err("a list type needs exactly one element type, for example [string]".into()),
            Value::Record(r) => Ok(Type::Record {
                name: r.type_name().map(str::to_string),
                fields: r
                    .fields
                    .iter()
                    .map(|(k, v)| Ok((k.clone(), Self::from_spec(v)?)))
                    .collect::<Result<_, String>>()?,
            }),
            other => Err(format!("cannot use a {} as a type", other.type_name())),
        }
    }

    pub fn from_name(name: &str) -> Option<Type> {
        Some(match name {
            "any" => Type::Any,
            "nothing" => Type::Nothing,
            "bool" => Type::Bool,
            "int" => Type::Int,
            "float" => Type::Float,
            "number" => Type::Number,
            "filesize" => Type::Filesize,
            "duration" => Type::Duration,
            "date" | "datetime" => Type::Date,
            "string" | "str" => Type::String,
            "binary" => Type::Binary,
            _ => return None,
        })
    }

    /// JSON Schema for structured LLM output. Units that JSON cannot express are explained
    /// in `description` so the model knows that a filesize is a byte count.
    pub fn to_json_schema(&self) -> Json {
        match self {
            Type::Any | Type::OneOf(_) | Type::Block => json!({}),
            Type::Nothing => json!({"type": "null"}),
            Type::Bool => json!({"type": "boolean"}),
            Type::Int => json!({"type": "integer"}),
            Type::Float | Type::Number => json!({"type": "number"}),
            Type::Filesize => json!({"type": "integer", "description": "size in bytes"}),
            Type::Duration => json!({"type": "number", "description": "duration in seconds"}),
            Type::Date => json!({"type": "string", "description": "RFC 3339 timestamp"}),
            Type::String | Type::Binary => json!({"type": "string"}),
            Type::List(inner) => json!({"type": "array", "items": inner.to_json_schema()}),
            Type::Table { fields, .. } => {
                json!({"type": "array", "items": Type::Record { name: None, fields: fields.clone() }.to_json_schema()})
            }
            Type::Record { fields, .. } => {
                let properties: Map<String, Json> =
                    fields.iter().map(|(k, t)| (k.clone(), t.to_json_schema())).collect();
                let required: Vec<&str> = fields.iter().map(|(k, _)| k.as_str()).collect();
                json!({"type": "object", "properties": properties, "required": required, "additionalProperties": false})
            }
        }
    }

    /// Turns JSON produced by a model or a foreign program into a value of this type.
    /// It is lenient on purpose. Models like to answer `"42"` where `42` was asked for.
    pub fn coerce(&self, json: &Json) -> Value {
        match (self, json) {
            (_, Json::Null) => Value::Nothing,
            (Type::Int, Json::Number(n)) => {
                n.as_i64().map_or_else(|| Value::Float(n.as_f64().unwrap_or(0.0)), Value::Int)
            }
            (Type::Float | Type::Number, Json::Number(n)) => Value::Float(n.as_f64().unwrap_or(0.0)),
            (Type::Filesize, Json::Number(n)) => Value::Filesize(n.as_f64().unwrap_or(0.0) as i64),
            (Type::Duration, Json::Number(n)) => Value::Duration((n.as_f64().unwrap_or(0.0) * 1e9) as i64),
            (Type::Bool, Json::String(s)) => Value::Bool(s.eq_ignore_ascii_case("true")),
            (Type::Date, Json::String(s)) => {
                chrono::DateTime::parse_from_rfc3339(s).map_or_else(|_| Value::String(s.clone()), Value::Date)
            }
            (Type::String, Json::String(s)) => Value::String(s.clone()),
            (Type::String, other) => Value::String(other.to_string()),
            (Type::Int | Type::Float | Type::Number | Type::Filesize | Type::Duration, Json::String(s)) => {
                parse_literal(s.trim()).unwrap_or_else(|| Value::String(s.clone()))
            }
            (Type::List(inner), Json::Array(items)) => Value::List(items.iter().map(|j| inner.coerce(j)).collect()),
            (Type::Table { fields, .. }, Json::Array(items)) => {
                let row = Type::Record { name: None, fields: fields.clone() };
                Value::List(items.iter().map(|j| row.coerce(j)).collect())
            }
            (Type::Record { name, fields }, Json::Object(map)) if !fields.is_empty() => {
                let mut record = Record { type_name: name.as_deref().map(Into::into), ..Record::default() };
                for (key, ty) in fields {
                    record.insert(key.clone(), map.get(key).map_or(Value::Nothing, |j| ty.coerce(j)));
                }
                Value::Record(record)
            }
            (_, other) => Value::from_json(other),
        }
    }
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let fields_text =
            |fields: &[(String, Type)]| fields.iter().map(|(k, t)| format!("{k}: {t}")).collect::<Vec<_>>().join(", ");
        match self {
            Type::List(inner) => write!(f, "list<{inner}>"),
            Type::Record { name, fields } | Type::Table { name, fields } => {
                f.write_str(if matches!(self, Type::Record { .. }) { "record" } else { "table" })?;
                if let Some(name) = name {
                    write!(f, "<{name}>")?;
                }
                if !fields.is_empty() {
                    write!(f, "{{{}}}", fields_text(fields))?;
                }
                Ok(())
            }
            Type::OneOf(types) => {
                let names: Vec<String> = types.iter().map(ToString::to_string).collect();
                f.write_str(&names.join(" | "))
            }
            simple => f.write_str(match simple {
                Type::Any => "any",
                Type::Nothing => "nothing",
                Type::Bool => "bool",
                Type::Int => "int",
                Type::Float => "float",
                Type::Number => "number",
                Type::Filesize => "filesize",
                Type::Duration => "duration",
                Type::Date => "date",
                Type::String => "string",
                Type::Binary => "binary",
                Type::Block => "block",
                _ => unreachable!(),
            }),
        }
    }
}
