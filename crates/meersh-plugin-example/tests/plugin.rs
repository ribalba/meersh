//! Runs the example plugin binary, once through the raw protocol and once through the shell.
//! Every registry and every stamped file lives in a temp directory, so the user's own
//! registry is never touched.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use meersh_engine::value::protocol::Encoder;
use meersh_engine::value::{Record, Type, Value};
use meersh_engine::{Effect, Engine, ExecMode, Plan, Result, Stack};
use meersh_plugin::protocol::{Sequence, Signatures};
use meersh_plugin::{Entry, Registry};

const PLUGIN: &str = env!("CARGO_BIN_EXE_meersh_plugin_example");

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("meersh-plugin-example-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Speaks the protocol by hand. One request line, then the input as a JSON text sequence.
fn exchange(request: &str, input: &[Value]) -> Vec<u8> {
    let mut child =
        Command::new(PLUGIN).arg("--meersh-plugin").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "{request}").unwrap();
    let mut encoder = Encoder::new(&mut stdin);
    for value in input {
        encoder.write(value).unwrap();
    }
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{out:?}");
    out.stdout
}

#[test]
fn signatures_and_a_call_round_trip() {
    let answer: Signatures = serde_json::from_slice(&exchange(r#"{"request":"signatures"}"#, &[])).unwrap();
    let signatures: Vec<_> = answer.signatures.iter().map(|s| s.to_signature().unwrap()).collect();
    let words = signatures.iter().find(|s| s.name == "words").unwrap();
    assert_eq!((words.effect, &words.input, &words.output), (Effect::Pure, &Type::String, &Type::table("word.count")));
    let stamp = signatures.iter().find(|s| s.name == "stamp").unwrap();
    assert_eq!((stamp.effect, stamp.usage()), (Effect::Write, "stamp <path>".to_string()));

    let request = r#"{"request":"call","name":"words","positional":[],"flags":{}}"#;
    let out = exchange(request, &[Value::from("b a B."), Value::from("c")]);
    let rows: Vec<Value> = Sequence::new(out.as_slice(), Type::Any).collect::<std::result::Result<_, _>>().unwrap();
    let row =
        |word: &str, count: i64| Value::Record(Record::typed("word.count").with("word", word).with("count", count));
    assert_eq!(rows, vec![row("b", 2), row("a", 1), row("c", 1)]);

    let out = exchange(r#"{"request":"call","name":"nope"}"#, &[]);
    let error = Sequence::new(out.as_slice(), Type::Any).next().unwrap().unwrap_err();
    assert!(error.contains("nope"), "{error}");
}

fn shell(registry: &Path) -> Arc<Engine> {
    let mut engine = Engine::new();
    meersh_cmds::register(&mut engine);
    meersh_plugin::register(&mut engine);
    engine.set_env(meersh_plugin::ENV_VAR, Some(registry.display().to_string()));
    Arc::new(engine)
}

fn try_run(engine: &Arc<Engine>, stack: &mut Stack, src: &str) -> Result<Value> {
    let mut out = Value::Nothing;
    engine.run_script(stack, src, false, &mut |data| {
        out = data.into_value()?;
        Ok(())
    })?;
    Ok(out)
}

fn run(engine: &Arc<Engine>, stack: &mut Stack, src: &str) -> Value {
    try_run(engine, stack, src).unwrap_or_else(|e| panic!("{src}: {e}"))
}

fn field(value: &Value, name: &str) -> Value {
    value.as_record().and_then(|r| r.get(name)).cloned().unwrap_or(Value::Nothing)
}

#[test]
fn plugin_commands_run_in_the_shell() {
    let dir = temp("shell");
    let registry = dir.join("plugins.json");
    let engine = shell(&registry);
    let mut user = Stack::default();

    let added = run(&engine, &mut user, &format!("plugin add '{PLUGIN}'"));
    assert_eq!(field(&added, "plugin"), "example".into());
    assert_eq!(field(&added, "commands"), Value::List(vec!["words".into(), "stamp".into()]));
    assert_eq!(Registry::load(&registry).unwrap().plugins.len(), 1);

    assert_eq!(run(&engine, &mut user, r#""a b a" | words | first | get word"#), "a".into());
    assert_eq!(run(&engine, &mut user, "help | where name == stamp | get effect | first"), "write".into());
    assert_eq!(run(&engine, &mut user, "plugin list | get plugin"), Value::List(vec!["example".into()]));

    // In plan mode a write command does not run. The plan holds the request instead.
    let target = dir.join("stamped.txt");
    let stamp = format!("stamp '{}'", target.display());
    let plan = Arc::new(Mutex::new(Plan::default()));
    let mut planning = user.with_mode(ExecMode::Plan(plan.clone()));
    assert_eq!(run(&engine, &mut planning, &stamp), Value::Nothing);
    assert!(!target.exists(), "planning must not write");
    let actions = std::mem::take(&mut plan.lock().unwrap().actions);
    assert_eq!(actions.len(), 1);
    assert_eq!((actions[0].kind.as_str(), actions[0].target.as_str()), ("exec", PLUGIN));

    // A request that is not part of the approved plan is skipped with a reason.
    let mut applying = user.with_mode(ExecMode::Apply(Arc::new(HashSet::new())));
    let skipped = run(&engine, &mut applying, &stamp);
    assert_eq!(skipped.as_record().and_then(|r| r.type_name()), Some("plugin.skipped"));
    assert!(!target.exists());

    // Run by the user it writes, and the filesize and the date survive the protocol.
    let stamped = run(&engine, &mut user, &stamp);
    assert_eq!(field(&stamped, "action"), "stamped".into());
    assert!(matches!(field(&stamped, "size"), Value::Filesize(n) if n > 0));
    assert!(matches!(field(&stamped, "at"), Value::Date(_)));
    assert_eq!(std::fs::read_to_string(&target).unwrap().lines().count(), 1);

    // An error of the plugin is the error of the command.
    let error = try_run(&engine, &mut user, &format!("stamp '{}'", dir.join("missing").join("x").display()));
    assert!(error.unwrap_err().to_string().contains("cannot stamp"));

    // A new shell loads the registered plugin at startup.
    let fresh = shell(&registry);
    assert_eq!(meersh_plugin::load_registered(&fresh), Vec::<String>::new());
    assert!(fresh.is_command("words") && fresh.is_command("stamp"));

    // Adding it again updates it. Removing it takes the commands away.
    assert_eq!(field(&run(&engine, &mut user, &format!("plugin add '{PLUGIN}'")), "action"), "updated".into());
    run(&engine, &mut user, "plugin remove example");
    assert!(!engine.is_command("words"));
    assert_eq!(Registry::load(&registry).unwrap(), Registry::default());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_missing_plugin_is_a_warning_at_startup() {
    let dir = temp("missing");
    let registry = dir.join("plugins.json");
    let gone = Entry { name: "gone".into(), path: dir.join("gone"), commands: vec!["vanish".into()] };
    Registry { plugins: vec![gone] }.save(&registry).unwrap();
    let warnings = meersh_plugin::load_registered(&shell(&registry));
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].starts_with("plugin gone was not loaded"), "{warnings:?}");
    std::fs::remove_dir_all(dir).unwrap();
}
