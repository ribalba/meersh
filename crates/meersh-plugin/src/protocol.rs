//! The wire format between meersh and a plugin. The host and plugin authors share these types.
//!
//! meersh starts the plugin with the single argument `--meersh-plugin` and writes one request
//! to its standard input, as a JSON object on one line.
//!
//! - `{"request":"signatures"}` asks for the commands. The plugin answers with one line
//!   `{"signatures":[...]}` and exits.
//! - `{"request":"call","name":"words","positional":[...],"flags":{...}}` runs a command. The
//!   input values follow as an RFC 7464 JSON text sequence written with
//!   [`meersh_value::protocol::Encoder`], and meersh closes standard input after the last one.
//!   The plugin answers with its output as a JSON text sequence on standard output. An error
//!   is the JSON text `{"@error":"message"}` in that sequence.
//!
//! Records keep their type and their filesize, duration and date fields through the
//! declarations of the sequence. A bare filesize, duration or date travels as a number or a
//! string, and the receiver gives it its type back from the signature with [`restore`].

use std::io::{self, BufRead, Write};

use meersh_engine::{Effect, Flag, Param, Signature};
use meersh_value::protocol::{Decoder, RS};
use meersh_value::{Record, Type, Value};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json};

/// The argument meersh starts a plugin with.
pub const PLUGIN_FLAG: &str = "--meersh-plugin";
/// The key of an error text in an output sequence.
pub const ERROR_KEY: &str = "@error";

/// The first line meersh writes to a plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "lowercase")]
pub enum Request {
    Signatures,
    Call {
        name: String,
        #[serde(default)]
        positional: Vec<Json>,
        #[serde(default)]
        flags: Map<String, Json>,
    },
}

/// The answer to `{"request":"signatures"}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Signatures {
    pub signatures: Vec<SignatureSpec>,
}

/// A command signature on the wire. Types are written the way meersh prints them, such as
/// `string`, `list<int>` or `table<fs.entry>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignatureSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "any")]
    pub input: String,
    #[serde(default = "any")]
    pub output: String,
    #[serde(default)]
    pub params: Vec<ParamSpec>,
    #[serde(default)]
    pub rest: Option<ParamSpec>,
    #[serde(default)]
    pub flags: Vec<FlagSpec>,
    /// `pure`, `read`, `write` or `destructive`. There is no default, a plugin has to say what
    /// its commands do.
    pub effect: String,
    #[serde(default)]
    pub examples: Vec<ExampleSpec>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParamSpec {
    pub name: String,
    #[serde(rename = "type", default = "any")]
    pub ty: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub optional: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlagSpec {
    pub long: String,
    #[serde(default)]
    pub short: Option<char>,
    /// The type of the flag's value, or null for a switch.
    #[serde(rename = "type", default)]
    pub ty: Option<String>,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExampleSpec {
    pub example: String,
    #[serde(default)]
    pub description: String,
}

fn any() -> String {
    "any".into()
}

impl From<&Signature> for SignatureSpec {
    fn from(s: &Signature) -> Self {
        let param = |p: &Param| ParamSpec {
            name: p.name.clone(),
            ty: p.ty.to_string(),
            description: p.desc.clone(),
            optional: p.optional,
        };
        SignatureSpec {
            name: s.name.clone(),
            description: s.description.clone(),
            input: s.input.to_string(),
            output: s.output.to_string(),
            params: s.params.iter().map(param).collect(),
            rest: s.rest.as_ref().map(param),
            flags: s
                .flags
                .iter()
                .map(|f| FlagSpec {
                    long: f.long.clone(),
                    short: f.short,
                    ty: f.arg.as_ref().map(ToString::to_string),
                    description: f.desc.clone(),
                })
                .collect(),
            effect: s.effect.as_str().into(),
            examples: s
                .examples
                .iter()
                .map(|(e, d)| ExampleSpec { example: e.clone(), description: d.clone() })
                .collect(),
        }
    }
}

impl SignatureSpec {
    /// The signature the shell binds calls against. Unknown types and effects are errors.
    pub fn to_signature(&self) -> Result<Signature, String> {
        if self.name.trim().is_empty() {
            return Err("a command without a name".into());
        }
        let named = |e: String| format!("command {} has {e}", self.name);
        let param = |p: &ParamSpec| -> Result<Param, String> {
            Ok(Param {
                name: p.name.clone(),
                ty: parse_type(&p.ty)?,
                desc: p.description.clone(),
                optional: p.optional,
            })
        };
        let flag = |f: &FlagSpec| -> Result<Flag, String> {
            Ok(Flag {
                long: f.long.clone(),
                short: f.short,
                arg: f.ty.as_deref().map(parse_type).transpose()?,
                desc: f.description.clone(),
            })
        };
        Ok(Signature {
            name: self.name.clone(),
            description: self.description.clone(),
            input: parse_type(&self.input).map_err(named)?,
            output: parse_type(&self.output).map_err(named)?,
            params: self.params.iter().map(param).collect::<Result<_, _>>().map_err(named)?,
            rest: self
                .rest
                .as_ref()
                .map(|p| param(p).map(|p| Param { optional: true, ..p }))
                .transpose()
                .map_err(named)?,
            flags: self.flags.iter().map(flag).collect::<Result<_, _>>().map_err(named)?,
            effect: parse_effect(&self.effect).map_err(named)?,
            examples: self.examples.iter().map(|e| (e.example.clone(), e.description.clone())).collect(),
        })
    }
}

/// The effects a plugin command may declare.
pub fn parse_effect(text: &str) -> Result<Effect, String> {
    match text {
        "pure" => Ok(Effect::Pure),
        "read" => Ok(Effect::Read),
        "write" => Ok(Effect::Write),
        "destructive" => Ok(Effect::Destructive),
        other => Err(format!("an unknown effect '{other}', expected pure, read, write or destructive")),
    }
}

/// Parses a type the way [`Type`] prints it, such as `string`, `list<int>`, `table<fs.entry>`,
/// `record{name: string, size: filesize}` or `int | string`.
pub fn parse_type(text: &str) -> Result<Type, String> {
    let mut parser = TypeParser { text, pos: 0 };
    let ty = parser.union()?;
    parser.skip_space();
    if parser.pos < text.len() {
        return Err(format!("an unexpected '{}' in the type '{text}'", &text[parser.pos..]));
    }
    Ok(ty)
}

/// All syntax characters of a type are ASCII, so the parser walks bytes and only ever slices
/// at ASCII positions.
struct TypeParser<'a> {
    text: &'a str,
    pos: usize,
}

impl<'a> TypeParser<'a> {
    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.pos).copied()
    }

    fn skip_space(&mut self) {
        while self.peek().is_some_and(|b| b.is_ascii_whitespace()) {
            self.pos += 1;
        }
    }

    fn eat(&mut self, byte: u8) -> bool {
        self.skip_space();
        let found = self.peek() == Some(byte);
        if found {
            self.pos += 1;
        }
        found
    }

    fn expect(&mut self, byte: u8) -> Result<(), String> {
        if self.eat(byte) { Ok(()) } else { Err(format!("a missing '{}' in the type '{}'", byte as char, self.text)) }
    }

    fn word(&mut self) -> Result<&'a str, String> {
        self.skip_space();
        let start = self.pos;
        while self.peek().is_some_and(|b| !b.is_ascii_whitespace() && !b"<>{}:,|".contains(&b)) {
            self.pos += 1;
        }
        if start == self.pos {
            return Err(format!("a missing name in the type '{}'", self.text));
        }
        Ok(&self.text[start..self.pos])
    }

    fn union(&mut self) -> Result<Type, String> {
        let mut types = vec![self.single()?];
        while self.eat(b'|') {
            types.push(self.single()?);
        }
        Ok(if types.len() == 1 { types.remove(0) } else { Type::OneOf(types) })
    }

    fn single(&mut self) -> Result<Type, String> {
        let word = self.word()?;
        match word {
            "list" => {
                self.expect(b'<')?;
                let inner = self.union()?;
                self.expect(b'>')?;
                Ok(Type::list(inner))
            }
            "record" | "table" => {
                let name = if self.eat(b'<') {
                    let name = self.word()?.to_string();
                    self.expect(b'>')?;
                    Some(name)
                } else {
                    None
                };
                let mut fields = Vec::new();
                if self.eat(b'{') && !self.eat(b'}') {
                    loop {
                        let key = self.word()?.to_string();
                        self.expect(b':')?;
                        fields.push((key, self.union()?));
                        if self.eat(b'}') {
                            break;
                        }
                        self.expect(b',')?;
                    }
                }
                Ok(if word == "record" { Type::Record { name, fields } } else { Type::Table { name, fields } })
            }
            "block" => Ok(Type::Block),
            other => Type::from_name(other).ok_or_else(|| format!("an unknown type '{other}'")),
        }
    }
}

/// The type of one item of a stream that carries values of `ty`. Lists and tables travel as
/// their items.
pub fn item_type(ty: &Type) -> Type {
    match ty {
        Type::List(inner) => (**inner).clone(),
        Type::Table { name, fields } => Type::Record { name: name.clone(), fields: fields.clone() },
        other => other.clone(),
    }
}

/// Gives a value back the types that JSON loses, guided by the type the signature declares.
/// A filesize or duration arrives as an int and a date as an RFC 3339 string.
pub fn restore(ty: &Type, value: Value) -> Value {
    match (ty, value) {
        (Type::Filesize, Value::Int(n)) => Value::Filesize(n),
        (Type::Duration, Value::Int(n)) => Value::Duration(n),
        (Type::Date, Value::String(s)) => Type::Date.coerce(&Json::String(s)),
        (Type::List(inner), Value::List(items)) => Value::List(items.into_iter().map(|v| restore(inner, v)).collect()),
        (Type::Table { fields, .. }, Value::List(items)) => Value::List(
            items
                .into_iter()
                .map(|v| match v {
                    Value::Record(r) => Value::Record(restore_fields(fields, r)),
                    other => other,
                })
                .collect(),
        ),
        (Type::Record { fields, .. }, Value::Record(r)) => Value::Record(restore_fields(fields, r)),
        (_, value) => value,
    }
}

fn restore_fields(fields: &[(String, Type)], mut record: Record) -> Record {
    for (key, ty) in fields {
        if let Some(slot) = record.fields.get_mut(key) {
            *slot = restore(ty, std::mem::replace(slot, Value::Nothing));
        }
    }
    record
}

/// The message of an error text `{"@error":"message"}`.
fn error_message(value: &Value) -> Option<String> {
    match value {
        Value::Record(r) if r.type_name().is_none() && r.fields.len() == 1 => r.get(ERROR_KEY).map(Value::to_text),
        _ => None,
    }
}

/// Writes an error as one JSON text of the sequence.
pub fn write_error(out: &mut impl Write, message: &str) -> io::Result<()> {
    out.write_all(&[RS])?;
    serde_json::to_writer(&mut *out, &serde_json::json!({ ERROR_KEY: message }))?;
    out.write_all(b"\n")
}

/// Reads a JSON text sequence as values of the declared item type. An error text ends the
/// sequence with its message.
pub struct Sequence<R> {
    reader: R,
    decoder: Decoder,
    item: Type,
    done: bool,
}

impl<R: BufRead> Sequence<R> {
    pub fn new(reader: R, item: Type) -> Self {
        Sequence { reader, decoder: Decoder::new(), item, done: false }
    }
}

impl<R: BufRead> Iterator for Sequence<R> {
    type Item = Result<Value, String>;

    fn next(&mut self) -> Option<Self::Item> {
        while !self.done {
            let mut chunk = Vec::new();
            let outcome = match self.reader.read_until(RS, &mut chunk) {
                Ok(0) => {
                    self.done = true;
                    return None;
                }
                Ok(_) => {
                    if chunk.last() == Some(&RS) {
                        chunk.pop();
                    }
                    if chunk.iter().all(u8::is_ascii_whitespace) {
                        continue;
                    }
                    self.decoder.decode(&chunk)
                }
                Err(e) => Err(format!("cannot read the plugin's output ({e})")),
            };
            match outcome {
                Ok(None) => {}
                Ok(Some(value)) => match error_message(&value) {
                    Some(message) => {
                        self.done = true;
                        return Some(Err(message));
                    }
                    None => return Some(Ok(restore(&self.item, value))),
                },
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use meersh_value::protocol::Encoder;

    use super::*;

    #[test]
    fn types_survive_printing_and_parsing() {
        let types = [
            Type::Any,
            Type::Nothing,
            Type::String,
            Type::Filesize,
            Type::Block,
            Type::list(Type::Int),
            Type::list(Type::list(Type::Date)),
            Type::record("fs.change"),
            Type::table("fs.entry"),
            Type::any_table(),
            Type::Record { name: None, fields: vec![] },
            Type::Table {
                name: Some("word.count".into()),
                fields: vec![("word".into(), Type::String), ("count".into(), Type::Int)],
            },
            Type::Record { name: None, fields: vec![("tags".into(), Type::list(Type::String))] },
            Type::OneOf(vec![Type::Int, Type::list(Type::String)]),
        ];
        for ty in types {
            assert_eq!(parse_type(&ty.to_string()), Ok(ty.clone()), "{ty}");
        }
        assert_eq!(parse_type(" table < fs.entry > "), Ok(Type::table("fs.entry")));
        assert!(parse_type("tabel<x>").is_err());
        assert!(parse_type("list<int").is_err());
        assert!(parse_type("int string").is_err());
    }

    #[test]
    fn signatures_survive_the_wire() {
        let signature = Signature::build("stamp")
            .desc("Append the time to a file.")
            .input(Type::Nothing)
            .output(Type::record("fs.change"))
            .required("path", Type::String, "the file")
            .rest("more", Type::Filesize, "sizes")
            .switch("quiet", Some('q'), "no output")
            .named("format", Type::String, None, "time format")
            .effect(Effect::Write)
            .example("stamp notes.txt", "stamps notes.txt");
        let spec = SignatureSpec::from(&signature);
        let json = serde_json::to_value(&spec).unwrap();
        assert_eq!(json["params"][0]["type"], "string");
        assert_eq!(json["flags"][0]["type"], Json::Null);
        assert_eq!(json["flags"][0]["short"], "q");
        assert_eq!(json["effect"], "write");
        let back: SignatureSpec = serde_json::from_value(json).unwrap();
        let rebuilt = back.to_signature().unwrap();
        assert_eq!(SignatureSpec::from(&rebuilt), spec);
        assert_eq!(rebuilt.usage(), signature.usage());
        assert_eq!(rebuilt.effect, Effect::Write);
    }

    #[test]
    fn a_signature_must_declare_its_effect() {
        let missing = serde_json::from_str::<SignatureSpec>(r#"{"name":"x"}"#);
        assert!(missing.is_err());
        let spec: SignatureSpec = serde_json::from_str(r#"{"name":"x","effect":"llm"}"#).unwrap();
        assert!(spec.to_signature().unwrap_err().contains("unknown effect"));
        let spec: SignatureSpec = serde_json::from_str(r#"{"name":"x","effect":"read"}"#).unwrap();
        let signature = spec.to_signature().unwrap();
        assert_eq!((signature.input, signature.output), (Type::Any, Type::Any));
    }

    #[test]
    fn requests_are_one_json_object() {
        assert_eq!(serde_json::to_string(&Request::Signatures).unwrap(), r#"{"request":"signatures"}"#);
        let call: Request = serde_json::from_str(r#"{"request":"call","name":"words"}"#).unwrap();
        assert_eq!(call, Request::Call { name: "words".into(), positional: vec![], flags: Map::new() });
    }

    #[test]
    fn sequences_restore_types_and_end_at_an_error() {
        let mut buf = Vec::new();
        let mut encoder = Encoder::new(&mut buf);
        encoder.write(&Value::Filesize(2048)).unwrap();
        encoder.write(&Value::Record(Record::typed("t").with("size", Value::Filesize(1)))).unwrap();
        write_error(&mut buf, "it broke").unwrap();
        let mut encoder = Encoder::new(&mut buf);
        encoder.write(&Value::Int(3)).unwrap();

        let items: Vec<_> = Sequence::new(buf.as_slice(), Type::Filesize).collect();
        assert_eq!(
            items,
            vec![
                Ok(Value::Filesize(2048)),
                Ok(Value::Record(Record::typed("t").with("size", Value::Filesize(1)))),
                Err("it broke".into()),
            ]
        );
    }

    #[test]
    fn restore_follows_lists_and_fields() {
        let ty = parse_type("list<record{size: filesize, at: date}>").unwrap();
        let value = Value::List(vec![Value::Record(
            Record::new()
                .with("size", Value::Int(5))
                .with("at", "2026-10-29T19:01:00+01:00")
                .with("other", Value::Int(1)),
        )]);
        let Value::List(items) = restore(&ty, value) else { panic!("a list stays a list") };
        let record = items[0].as_record().unwrap();
        assert_eq!(record.get("size"), Some(&Value::Filesize(5)));
        assert!(matches!(record.get("at"), Some(Value::Date(_))));
        assert_eq!(record.get("other"), Some(&Value::Int(1)));
    }
}
