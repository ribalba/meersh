//! These tests talk to a real model, so they are ignored by default. Run them with
//! `cargo test -p meersh-llm -- --ignored` while Ollama is running. `MEERSH_TEST_MODEL`
//! picks the model, otherwise the first installed one is used.

use std::sync::Arc;

use meersh_engine::value::Value;
use meersh_engine::{Engine, Stack};
use meersh_llm::{LlmConfig, LlmRuntime};

fn run(src: &str) -> Value {
    let mut config = LlmConfig { cache: false, ..LlmConfig::default() };
    if let Ok(model) = std::env::var("MEERSH_TEST_MODEL") {
        config.backends.get_mut("local").expect("default backend").model = Some(model);
    }
    let mut engine = Engine::new();
    meersh_cmds::register(&mut engine);
    meersh_llm::register(&mut engine, LlmRuntime::new(config));
    let engine = Arc::new(engine);
    let mut out = Value::Nothing;
    engine
        .run_script(&mut Stack::default(), src, false, &mut |data| {
            out = data.into_value()?;
            Ok(())
        })
        .unwrap_or_else(|e| panic!("{src}: {e}"));
    out
}

#[test]
#[ignore]
fn text_in_text_out() {
    let out = run("\"Der Hund bellt\" | llm \"translate to English\"");
    assert!(out.to_text().to_lowercase().contains("dog"), "{out:?}");
}

#[test]
#[ignore]
fn map_adds_a_typed_column() {
    let out = run("[{name: apple} {name: carrot} {name: banana} {name: potato}] \
         | llm map \"is this a fruit\" --into fruit --schema bool | where fruit == true | get name");
    assert_eq!(out, Value::List(vec!["apple".into(), "banana".into()]));
}

#[test]
#[ignore]
fn where_filters_semantically() {
    let out = run("[{city: Paris} {city: Tokyo} {city: Lyon} {city: Osaka}] | llm where \"is in France\" | get city");
    assert_eq!(out, Value::List(vec!["Paris".into(), "Lyon".into()]));
}

#[test]
#[ignore]
fn parse_turns_text_into_a_table() {
    let out = run("\"Oct 01 10:02 sda1: disk almost full\\nOct 01 10:05 fan0: speed below threshold\" \
         | llm parse {device: string, problem: string} | get device");
    assert_eq!(out, Value::List(vec!["sda1".into(), "fan0".into()]));
}

#[test]
#[ignore]
fn ai_writes_a_parsable_pipeline() {
    let out = run("ai the largest three items in a list of records by their size column");
    let record = out.as_record().expect("a record");
    assert!(record.get("pipeline").and_then(Value::as_str).is_some_and(|p| p.contains("sort-by")), "{record:?}");
}

/// Needs a logged in `claude` CLI. Costs a few tokens of the user's plan.
#[test]
#[ignore]
fn claude_code_backend() {
    let out = run(
        "[{name: apple} {name: carrot}] | llm map \"is this a fruit\" --into fruit --schema bool --backend claude --model haiku | get fruit",
    );
    assert_eq!(out, Value::List(vec![true.into(), false.into()]));
}

/// The example from AUTHORITY.md section 7. The model sees only the type, writes a pipeline
/// over the input, and nothing is deleted while planning.
#[test]
#[ignore]
fn actor_plans_deleting_large_files() {
    use std::sync::Mutex;

    use meersh_engine::{Consent, Plan};

    let dir = std::env::temp_dir().join(format!("meersh-actor-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("big.bin"), vec![0u8; 6_000_000]).unwrap();
    std::fs::write(dir.join("small.txt"), "hello").unwrap();

    let mut config = LlmConfig { cache: false, ..LlmConfig::default() };
    if let Ok(model) = std::env::var("MEERSH_TEST_MODEL") {
        config.backends.get_mut("local").expect("default backend").model = Some(model);
    }
    let mut engine = Engine::new();
    meersh_cmds::register(&mut engine);
    meersh_coreutils::register(&mut engine);
    meersh_llm::register(&mut engine, LlmRuntime::new(config));
    let engine = Arc::new(engine);

    let seen = Mutex::new(None);
    let src = format!("ls '{}' | llm \"delete all files that are larger than 5mb\"", dir.display());
    let result = engine.run_planned(
        &mut Stack::default(),
        &src,
        false,
        &mut |plan: &Plan| {
            let targets: Vec<String> = plan.actions.iter().map(|a| a.target.clone()).collect();
            let pipelines: Vec<String> = plan.notes.iter().map(|n| n.pipeline.clone()).collect();
            *seen.lock().unwrap() = Some((targets, pipelines));
            Consent::Reject
        },
        &mut |_| Ok(()),
    );
    assert!(result.is_err(), "a rejected plan reports that nothing changed");
    let (targets, pipelines) = seen.lock().unwrap().take().expect("the plan was shown");
    assert!(pipelines[0].contains("delete"), "{pipelines:?}");
    assert_eq!(targets.len(), 1, "{targets:?}");
    assert!(targets[0].ends_with("big.bin"), "{targets:?}");
    assert!(dir.join("big.bin").exists() && dir.join("small.txt").exists(), "planning deletes nothing");
    std::fs::remove_dir_all(dir).unwrap();
}

/// Without piped data, `llm` must answer with a meersh pipeline and not with bash.
#[test]
#[ignore]
fn llm_without_input_answers_with_meersh() {
    let out = run("llm \"Give me a command that filters all directories that had no edits in the last 6 months\"");
    let record = out.as_record().unwrap_or_else(|| panic!("a pipeline, got {out:?}"));
    let pipeline = record.get("pipeline").and_then(Value::as_str).unwrap_or_default();
    assert!(pipeline.contains("modified") && !pipeline.contains("-mtime"), "{pipeline}");
    assert!(pipeline.contains("dir") || pipeline.contains("--type d"), "{pipeline}");
}
