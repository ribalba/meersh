//! The plugin side. A plugin implements [`Plugin`] and hands it to [`serve`] in `main`.

use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};
use std::process::ExitCode;

use meersh_engine::Signature;
use meersh_value::protocol::Encoder;
use meersh_value::{Type, Value};
use serde_json::{Map, Value as Json};

use crate::protocol::{PLUGIN_FLAG, Request, Sequence, SignatureSpec, Signatures, item_type, restore, write_error};

/// The commands of a plugin.
pub trait Plugin {
    /// The commands, built like the shell's own with [`Signature::build`].
    fn signatures(&self) -> Vec<Signature>;

    /// Runs one command. Arguments, flags and input arrive with the types the signature
    /// declares. A switch that was given is `true`, flags that were not given are missing.
    /// The input holds the piped values, and a piped list arrives as its items. The returned
    /// values form the output stream. An error reaches the user as the error of the command.
    fn call(
        &self,
        name: &str,
        positional: Vec<Value>,
        flags: BTreeMap<String, Value>,
        input: Vec<Value>,
    ) -> Result<Vec<Value>, String>;
}

/// Serves one request on standard input and output. Return its exit code from `main`.
/// Started by hand without `--meersh-plugin`, the plugin explains itself instead.
pub fn serve(plugin: &impl Plugin) -> ExitCode {
    if std::env::args().nth(1).as_deref() != Some(PLUGIN_FLAG) {
        let program = std::env::args().next().unwrap_or_else(|| "<path>".into());
        eprintln!("This is a meersh plugin. Add it in meersh with `plugin add {program}`. It provides these commands:");
        for signature in plugin.signatures() {
            eprintln!("  {:<30} {}", signature.usage(), signature.description);
        }
        return ExitCode::from(2);
    }
    match serve_io(plugin, io::stdin().lock(), io::stdout().lock()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

/// The protocol on any reader and writer. [`serve`] uses standard input and output, tests
/// use buffers.
pub fn serve_io(plugin: &impl Plugin, mut input: impl BufRead, mut output: impl Write) -> io::Result<()> {
    let mut line = String::new();
    input.read_line(&mut line)?;
    match serde_json::from_str::<Request>(&line) {
        Ok(Request::Signatures) => {
            let signatures = plugin.signatures().iter().map(SignatureSpec::from).collect();
            serde_json::to_writer(&mut output, &Signatures { signatures })?;
            output.write_all(b"\n")?;
        }
        Ok(Request::Call { name, positional, flags }) => match call(plugin, &name, &positional, &flags, input) {
            Ok(values) => {
                let mut encoder = Encoder::new(&mut output);
                for value in &values {
                    encoder.write(value)?;
                }
            }
            Err(message) => write_error(&mut output, &message)?,
        },
        Err(e) => write_error(&mut output, &format!("cannot read the request ({e})"))?,
    }
    output.flush()
}

fn call(
    plugin: &impl Plugin,
    name: &str,
    positional: &[Json],
    flags: &Map<String, Json>,
    input: impl BufRead,
) -> Result<Vec<Value>, String> {
    let signature = plugin
        .signatures()
        .into_iter()
        .find(|s| s.name == name)
        .ok_or_else(|| format!("this plugin has no command named '{name}'"))?;
    let positional = positional
        .iter()
        .enumerate()
        .map(|(i, json)| {
            let ty = signature.params.get(i).or(signature.rest.as_ref()).map_or(&Type::Any, |p| &p.ty);
            restore(ty, Value::from_json(json))
        })
        .collect();
    let flags = flags
        .iter()
        .map(|(long, json)| {
            let ty =
                signature.flags.iter().find(|f| f.long == *long).and_then(|f| f.arg.as_ref()).unwrap_or(&Type::Any);
            (long.clone(), restore(ty, Value::from_json(json)))
        })
        .collect();
    let input = Sequence::new(input, item_type(&signature.input)).collect::<Result<Vec<_>, _>>()?;
    let values = plugin.call(name, positional, flags, input)?;
    // A list returned for a list or table output goes out as its items, so the shell can
    // stream them and give each item its type back.
    if matches!(signature.output, Type::List(_) | Type::Table { .. }) {
        return Ok(values
            .into_iter()
            .flat_map(|v| match v {
                Value::List(items) => items,
                other => vec![other],
            })
            .collect());
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use meersh_engine::Effect;
    use meersh_value::Record;

    use super::*;

    /// `sizes` doubles filesizes. `fail` always fails.
    struct Sizes;

    impl Plugin for Sizes {
        fn signatures(&self) -> Vec<Signature> {
            vec![
                Signature::build("sizes")
                    .input(Type::list(Type::Filesize))
                    .output(Type::list(Type::Filesize))
                    .optional("factor", Type::Int, "")
                    .switch("tag", None, "")
                    .named("since", Type::Date, None, "")
                    .effect(Effect::Pure),
                Signature::build("fail").effect(Effect::Read),
            ]
        }

        fn call(
            &self,
            name: &str,
            positional: Vec<Value>,
            flags: BTreeMap<String, Value>,
            input: Vec<Value>,
        ) -> Result<Vec<Value>, String> {
            if name == "fail" {
                return Err("it failed".into());
            }
            assert_eq!(flags.get("tag"), Some(&Value::Bool(true)));
            assert!(matches!(flags.get("since"), Some(Value::Date(_))));
            let factor = positional.first().and_then(Value::as_int).unwrap_or(2);
            let doubled = input
                .iter()
                .map(|v| match v {
                    Value::Filesize(n) => Ok(Value::Filesize(n * factor)),
                    other => Err(format!("expected a filesize, found {}", other.type_name())),
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(vec![Value::List(doubled), Value::Record(Record::typed("note").with("size", Value::Filesize(1)))])
        }
    }

    fn exchange(request: &str, input: &[Value]) -> Vec<Result<Value, String>> {
        let mut stdin = format!("{request}\n").into_bytes();
        let mut encoder = Encoder::new(&mut stdin);
        for value in input {
            encoder.write(value).unwrap();
        }
        let mut stdout = Vec::new();
        serve_io(&Sizes, stdin.as_slice(), &mut stdout).unwrap();
        Sequence::new(stdout.as_slice(), Type::Any).collect()
    }

    #[test]
    fn signatures_are_one_line() {
        let mut stdout = Vec::new();
        serve_io(&Sizes, &b"{\"request\":\"signatures\"}\n"[..], &mut stdout).unwrap();
        let text = String::from_utf8(stdout).unwrap();
        assert_eq!(text.lines().count(), 1);
        let answer: Signatures = serde_json::from_str(&text).unwrap();
        assert_eq!(answer.signatures[0].input, "list<filesize>");
        assert_eq!(answer.signatures[1].effect, "read");
    }

    #[test]
    fn arguments_and_input_arrive_typed() {
        let request = r#"{"request":"call","name":"sizes","positional":[3],"flags":{"tag":true,"since":"2026-10-29T19:01:00+01:00"}}"#;
        let out = exchange(request, &[Value::Filesize(10), Value::Filesize(20)]);
        let values: Vec<Value> = out.into_iter().collect::<Result<_, _>>().unwrap();
        // The list went out as its items. The typed record kept its filesize by itself.
        assert_eq!(values[..2], [Value::Int(30), Value::Int(60)]);
        assert_eq!(values[2], Value::Record(Record::typed("note").with("size", Value::Filesize(1))));
    }

    #[test]
    fn errors_are_error_texts() {
        assert_eq!(exchange(r#"{"request":"call","name":"fail"}"#, &[]), vec![Err("it failed".into())]);
        let unknown = exchange(r#"{"request":"call","name":"nope"}"#, &[]);
        assert!(matches!(&unknown[0], Err(e) if e.contains("no command named 'nope'")));
        let garbage = exchange("hello", &[]);
        assert!(matches!(&garbage[0], Err(e) if e.contains("cannot read the request")));
    }
}
