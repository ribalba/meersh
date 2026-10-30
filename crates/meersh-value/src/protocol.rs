//! The meersh object protocol for external programs.
//!
//! When meersh reads the standard output of an external program it sets
//! `MEERSH_OBJECTS=json-seq` in that program's environment. A meersh aware program then
//! writes an RFC 7464 JSON text sequence instead of text. Every JSON text starts with the
//! ASCII record separator 0x1E, so meersh can tell objects from text by the first byte.
//!
//! Plain JSON cannot tell an int from a filesize or a string from a date. A writer therefore
//! declares the lossy fields of each record type once, before the first record of that type.
//!
//! ```text
//! <RS>{"@decl":"fs.entry","fields":{"size":"filesize","modified":"date"}}
//! <RS>{"@type":"fs.entry","name":"Cargo.toml","size":812,"modified":"2026-10-29T19:01:00+01:00"}
//! ```
//!
//! The stream stays readable for `jq --seq`. Programs that know nothing about meersh never
//! look at the variable and keep writing text, which meersh then splits into lines.

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};

use serde_json::{Value as Json, json};

use crate::{Type, Value};

pub const ENV_VAR: &str = "MEERSH_OBJECTS";
/// Protocol v2. The number of the file descriptor that takes the objects, so standard output
/// can stay readable for humans. meersh passes descriptor 3.
pub const FD_VAR: &str = "MEERSH_OBJECT_FD";
pub const FORMAT: &str = "json-seq";
pub const RS: u8 = 0x1e;
const DECL_KEY: &str = "@decl";

/// True when the current process was started by meersh and should emit objects.
pub fn wants_objects() -> bool {
    std::env::var(ENV_VAR).is_ok_and(|v| v == FORMAT)
}

/// Where a meersh aware program should write its objects. Descriptor 3 when meersh offers
/// it (protocol v2), standard output when it asks for objects there (v1), otherwise `None`
/// and the program prints text.
pub fn object_writer() -> Option<Box<dyn Write>> {
    use std::os::fd::FromRawFd;
    if let Some(fd) = std::env::var(FD_VAR).ok().and_then(|v| v.parse::<i32>().ok()) {
        // SAFETY: F_GETFD only asks whether the descriptor is open. It changes nothing.
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0 {
            // SAFETY: meersh opened this descriptor for us and nothing else in the process owns it.
            return Some(Box::new(unsafe { std::fs::File::from_raw_fd(fd) }));
        }
    }
    wants_objects().then(|| Box::new(std::io::stdout()) as Box<dyn Write>)
}

pub struct Encoder<W: Write> {
    out: W,
    declared: HashSet<String>,
}

impl<W: Write> Encoder<W> {
    pub fn new(out: W) -> Self {
        Encoder { out, declared: HashSet::new() }
    }

    pub fn write(&mut self, value: &Value) -> io::Result<()> {
        if let Value::Record(r) = value
            && let Some(name) = r.type_name()
            && self.declared.insert(name.to_string())
        {
            let fields: serde_json::Map<String, Json> = r
                .fields
                .iter()
                .filter_map(|(k, v)| lossy_tag(v).map(|t| (k.clone(), Json::String(t.into()))))
                .collect();
            self.text(&json!({ DECL_KEY: name, "fields": fields }))?;
        }
        self.text(&value.to_json_with(true))
    }

    fn text(&mut self, json: &Json) -> io::Result<()> {
        self.out.write_all(&[RS])?;
        serde_json::to_writer(&mut self.out, json)?;
        self.out.write_all(b"\n")
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

fn lossy_tag(value: &Value) -> Option<&'static str> {
    match value {
        Value::Filesize(_) => Some("filesize"),
        Value::Duration(_) => Some("duration"),
        Value::Date(_) => Some("date"),
        _ => None,
    }
}

#[derive(Default)]
pub struct Decoder {
    decls: HashMap<String, Vec<(String, Type)>>,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decodes one JSON text of the sequence. Declarations return `Ok(None)`.
    pub fn decode(&mut self, text: &[u8]) -> Result<Option<Value>, String> {
        let json: Json = serde_json::from_slice(text).map_err(|e| format!("invalid object stream: {e}"))?;
        if let Some(name) = json.get(DECL_KEY).and_then(Json::as_str) {
            let fields = json
                .get("fields")
                .and_then(Json::as_object)
                .map(|m| m.iter().filter_map(|(k, t)| Some((k.clone(), Type::from_name(t.as_str()?)?))).collect())
                .unwrap_or_default();
            self.decls.insert(name.to_string(), fields);
            return Ok(None);
        }
        let mut value = Value::from_json(&json);
        if let Value::Record(r) = &mut value
            && let Some(fields) = r.type_name().and_then(|n| self.decls.get(n))
        {
            for (key, ty) in fields {
                if let Some(raw) = json.get(key) {
                    r.insert(
                        key.clone(),
                        match (ty, raw) {
                            (Type::Duration, Json::Number(n)) => Value::Duration(n.as_i64().unwrap_or(0)),
                            _ => ty.coerce(raw),
                        },
                    );
                }
            }
        }
        Ok(Some(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Record;

    #[test]
    fn roundtrip_keeps_types() {
        let entry = Value::Record(
            Record::typed("fs.entry")
                .with("name", "a.txt")
                .with("size", Value::Filesize(2048))
                .with("age", Value::Duration(5_000_000_000)),
        );
        let mut buf = Vec::new();
        let mut enc = Encoder::new(&mut buf);
        enc.write(&entry).unwrap();
        enc.write(&entry).unwrap();

        let mut dec = Decoder::new();
        let values: Vec<Value> = buf
            .split(|b| *b == RS)
            .filter(|chunk| !chunk.iter().all(u8::is_ascii_whitespace))
            .filter_map(|chunk| dec.decode(chunk).unwrap())
            .collect();
        assert_eq!(values, vec![entry.clone(), entry]);
    }
}
