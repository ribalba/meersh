use serde_json::{Map, Number, Value as Json};

use crate::{Record, Value};

/// Key that carries a record's nominal type when it is written as JSON.
pub const TYPE_KEY: &str = "@type";
/// Key that carries a record's identity.
pub const ID_KEY: &str = "@id";

impl Value {
    /// Plain JSON, as `to json` writes it. Filesizes become byte counts, durations become
    /// nanoseconds and dates become RFC 3339 strings. Type tags are dropped.
    pub fn to_json(&self) -> Json {
        self.to_json_with(false)
    }

    /// JSON that keeps record type tags under `@type`. Used by the object protocol.
    pub fn to_json_with(&self, type_tags: bool) -> Json {
        match self {
            Value::Nothing => Json::Null,
            Value::Bool(b) => Json::Bool(*b),
            Value::Int(i) | Value::Filesize(i) | Value::Duration(i) => Json::Number((*i).into()),
            Value::Float(f) => Number::from_f64(*f).map_or(Json::Null, Json::Number),
            Value::Date(d) => Json::String(d.to_rfc3339()),
            Value::String(s) => Json::String(s.clone()),
            Value::Binary(b) => Json::Array(b.iter().map(|byte| Json::Number((*byte).into())).collect()),
            Value::List(items) => Json::Array(items.iter().map(|v| v.to_json_with(type_tags)).collect()),
            Value::Record(r) => {
                let mut map = Map::new();
                if type_tags && let Some(name) = r.type_name() {
                    map.insert(TYPE_KEY.into(), Json::String(name.into()));
                }
                if type_tags && let Some(id) = r.id() {
                    map.insert(ID_KEY.into(), Json::String(id.into()));
                }
                for (k, v) in &r.fields {
                    map.insert(k.clone(), v.to_json_with(type_tags));
                }
                Json::Object(map)
            }
        }
    }

    pub fn from_json(json: &Json) -> Value {
        match json {
            Json::Null => Value::Nothing,
            Json::Bool(b) => Value::Bool(*b),
            Json::Number(n) => n.as_i64().map_or_else(|| Value::Float(n.as_f64().unwrap_or(f64::NAN)), Value::Int),
            Json::String(s) => Value::String(s.clone()),
            Json::Array(items) => Value::List(items.iter().map(Value::from_json).collect()),
            Json::Object(map) => {
                let mut record = Record::new();
                for (k, v) in map {
                    if k == TYPE_KEY
                        && let Json::String(name) = v
                    {
                        record.type_name = Some(name.as_str().into());
                    } else if k == ID_KEY
                        && let Json::String(id) = v
                    {
                        record.id = Some(id.as_str().into());
                    } else {
                        record.insert(k.clone(), Value::from_json(v));
                    }
                }
                Value::Record(record)
            }
        }
    }
}
