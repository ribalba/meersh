use std::cmp::Ordering;

use chrono::{DateTime, FixedOffset};

use crate::{Record, Type, format_duration, format_filesize};

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Nothing,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// A size in bytes. Kept apart from `Int` so it renders as `4.2 KiB` and compares with `10mb`.
    Filesize(i64),
    /// A span of time in nanoseconds.
    Duration(i64),
    Date(DateTime<FixedOffset>),
    String(String),
    Binary(Vec<u8>),
    List(Vec<Value>),
    Record(Record),
}

impl Value {
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Nothing => "nothing",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Filesize(_) => "filesize",
            Value::Duration(_) => "duration",
            Value::Date(_) => "date",
            Value::String(_) => "string",
            Value::Binary(_) => "binary",
            Value::List(items) if is_table(items) => "table",
            Value::List(_) => "list",
            Value::Record(_) => "record",
        }
    }

    /// The structural type of this value. A table reports the columns of its first row.
    pub fn ty(&self) -> Type {
        match self {
            Value::Nothing => Type::Nothing,
            Value::Bool(_) => Type::Bool,
            Value::Int(_) => Type::Int,
            Value::Float(_) => Type::Float,
            Value::Filesize(_) => Type::Filesize,
            Value::Duration(_) => Type::Duration,
            Value::Date(_) => Type::Date,
            Value::String(_) => Type::String,
            Value::Binary(_) => Type::Binary,
            Value::Record(r) => Type::Record {
                name: r.type_name().map(str::to_string),
                fields: r.fields.iter().map(|(k, v)| (k.clone(), v.ty())).collect(),
            },
            Value::List(items) => match items.first() {
                None => Type::List(Box::new(Type::Any)),
                Some(Value::Record(first)) if is_table(items) => Type::Table {
                    name: first.type_name().map(str::to_string),
                    fields: first.fields.iter().map(|(k, v)| (k.clone(), v.ty())).collect(),
                },
                Some(first) if items.iter().all(|v| v.type_name() == first.type_name()) => {
                    Type::List(Box::new(first.ty()))
                }
                Some(_) => Type::List(Box::new(Type::Any)),
            },
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) | Value::Filesize(i) => Some(*i),
            Value::Float(f) => Some(*f as i64),
            _ => None,
        }
    }

    pub fn as_record(&self) -> Option<&Record> {
        match self {
            Value::Record(r) => Some(r),
            _ => None,
        }
    }

    fn as_number(&self) -> Option<f64> {
        match self {
            Value::Int(i) | Value::Filesize(i) => Some(*i as f64),
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }

    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Nothing => false,
            Value::Bool(b) => *b,
            Value::String(s) => !s.is_empty(),
            Value::List(l) => !l.is_empty(),
            _ => true,
        }
    }

    /// Follows one member of a cell path. On a table, `name` yields the whole column,
    /// the same as `$table.name` in the language.
    pub fn follow(&self, member: &str) -> Option<Value> {
        match self {
            Value::Record(r) => r.get(member).cloned(),
            Value::List(items) => {
                if let Ok(i) = member.parse::<usize>() {
                    return items.get(i).cloned();
                }
                let has_column = items.iter().any(|v| v.as_record().is_some_and(|r| r.get(member).is_some()));
                has_column
                    .then(|| Value::List(items.iter().map(|v| v.follow(member).unwrap_or(Value::Nothing)).collect()))
            }
            _ => None,
        }
    }

    pub fn compare(&self, other: &Value) -> Option<Ordering> {
        use Value::*;
        match (self, other) {
            (Int(a), Int(b)) | (Filesize(a), Filesize(b)) | (Duration(a), Duration(b)) => a.partial_cmp(b),
            (String(a), String(b)) => a.partial_cmp(b),
            (Date(a), Date(b)) => a.partial_cmp(b),
            (Bool(a), Bool(b)) => a.partial_cmp(b),
            (Nothing, Nothing) => Some(Ordering::Equal),
            (Nothing, _) => Some(Ordering::Less),
            (_, Nothing) => Some(Ordering::Greater),
            (a, b) => a.as_number()?.partial_cmp(&b.as_number()?),
        }
    }

    /// Human oriented rendering of a single value, as used in table cells.
    pub fn to_display(&self) -> String {
        match self {
            Value::Nothing => String::new(),
            Value::Bool(b) => b.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) if f.fract() == 0.0 && f.abs() < 1e15 => format!("{f:.1}"),
            Value::Float(f) => format!("{:.}", (f * 1000.0).round() / 1000.0),
            Value::Filesize(b) => format_filesize(*b),
            Value::Duration(n) => format_duration(*n),
            Value::Date(d) => d.format("%Y-%m-%d %H:%M:%S").to_string(),
            Value::String(s) => s.clone(),
            Value::Binary(b) => format!("<{} bytes>", b.len()),
            Value::List(items) if is_table(items) => format!("[table {} rows]", items.len()),
            Value::List(items) => format!("[list {} items]", items.len()),
            Value::Record(r) => format!("{{record {} fields}}", r.fields.len()),
        }
    }

    /// Text used when a value leaves the object world, for example as an argument or as
    /// input of an external program. Structured values become compact JSON.
    pub fn to_text(&self) -> String {
        match self {
            Value::String(s) => s.clone(),
            Value::List(_) | Value::Record(_) => self.to_json().to_string(),
            Value::Date(d) => d.to_rfc3339(),
            other => other.to_display(),
        }
    }
}

/// A list counts as a table when every item is a record.
pub(crate) fn is_table(items: &[Value]) -> bool {
    !items.is_empty() && items.iter().all(|v| matches!(v, Value::Record(_)))
}

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}

impl From<i64> for Value {
    fn from(i: i64) -> Self {
        Value::Int(i)
    }
}

impl From<f64> for Value {
    fn from(f: f64) -> Self {
        Value::Float(f)
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::String(s.to_string())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::String(s)
    }
}

impl From<Record> for Value {
    fn from(r: Record) -> Self {
        Value::Record(r)
    }
}

impl From<Vec<Value>> for Value {
    fn from(l: Vec<Value>) -> Self {
        Value::List(l)
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(o: Option<T>) -> Self {
        o.map_or(Value::Nothing, Into::into)
    }
}
