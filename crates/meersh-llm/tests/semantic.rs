//! `llm rank`, `llm group`, `llm join`, `llm embed` and `llm search`.
//!
//! The tests marked `#[ignore]` talk to a real model. Run them with
//! `cargo test -p meersh-llm --test semantic -- --ignored --test-threads=1` while Ollama is
//! running. `MEERSH_TEST_MODEL` picks the chat model, otherwise the first installed one is
//! used. `MEERSH_TEST_EMBED_MODEL` picks the embedding model, otherwise the first installed
//! model that can embed. The embedding test is skipped when there is none.
//!
//! The other tests need no model. They point the backend at the discard port, so any request
//! would fail.

use std::sync::Arc;

use meersh_engine::value::Value;
use meersh_engine::{Engine, Stack};
use meersh_llm::{LlmConfig, LlmRuntime};

fn engine(config: LlmConfig) -> Arc<Engine> {
    let mut engine = Engine::new();
    meersh_cmds::register(&mut engine);
    meersh_llm::register(&mut engine, LlmRuntime::new(config));
    Arc::new(engine)
}

fn try_run_with(config: LlmConfig, src: &str) -> Result<Value, String> {
    let engine = engine(config);
    let mut out = Value::Nothing;
    engine
        .run_script(&mut Stack::default(), src, false, &mut |data| {
            out = data.into_value()?;
            Ok(())
        })
        .map_err(|e| e.to_string())?;
    Ok(out)
}

fn local_config() -> LlmConfig {
    let mut config = LlmConfig { cache: false, ..LlmConfig::default() };
    if let Ok(model) = std::env::var("MEERSH_TEST_MODEL") {
        config.backends.get_mut("local").expect("default backend").model = Some(model);
    }
    config
}

fn offline_config() -> LlmConfig {
    let mut config = LlmConfig { cache: false, ..LlmConfig::default() };
    let local = config.backends.get_mut("local").expect("default backend");
    local.url = Some("http://127.0.0.1:9".into());
    local.model = Some("none".into());
    local.timeout_secs = Some(2);
    config
}

fn run(src: &str) -> Value {
    try_run_with(local_config(), src).unwrap_or_else(|e| panic!("{src}: {e}"))
}

fn offline(src: &str) -> Result<Value, String> {
    try_run_with(offline_config(), src)
}

fn texts(value: &Value) -> Vec<String> {
    match value {
        Value::List(items) => items.iter().map(Value::to_text).collect(),
        other => panic!("expected a list, got {other:?}"),
    }
}

fn rows(n: usize) -> String {
    let rows: Vec<String> = (0..n).map(|i| format!("{{n: {i}}}")).collect();
    format!("[{}]", rows.join(" "))
}

#[test]
fn join_refuses_too_many_pairs() {
    let err =
        offline(&format!("let right = {}\n{} | llm join $right \"same number\"", rows(50), rows(51))).unwrap_err();
    assert!(err.contains("2550 pairs") && err.contains("2500"), "{err}");
}

#[test]
fn join_at_the_limit_is_accepted() {
    // 2500 pairs pass the check and then need the model, which the discard port refuses.
    let err =
        offline(&format!("let right = {}\n{} | llm join $right \"same number\"", rows(50), rows(50))).unwrap_err();
    assert!(err.contains("127.0.0.1:9/api/chat"), "{err}");
}

#[test]
fn empty_input_makes_no_request() {
    for src in [
        "[] | llm rank \"how sweet\"",
        "[] | llm group \"fruit or vegetable\"",
        "[] | llm join [{city: Paris}] \"lives in\"",
        "[{name: Anna}] | llm join [] \"lives in\"",
        "[] | llm embed",
        "[] | llm search \"fruit\"",
    ] {
        assert_eq!(offline(src), Ok(Value::List(vec![])), "{src}");
    }
}

#[test]
fn embed_explains_that_claude_code_has_no_endpoint() {
    let err = offline("[{name: apple}] | llm embed --backend claude").unwrap_err();
    assert!(err.contains("no embedding endpoint"), "{err}");
}

#[test]
fn signatures_declare_the_llm_effect() {
    let engine = engine(offline_config());
    for name in ["llm rank", "llm group", "llm join", "llm embed", "llm search"] {
        let sig = engine.signature(name).unwrap_or_else(|| panic!("{name} is not registered"));
        assert_eq!(sig.effect, meersh_engine::Effect::Llm, "{name}");
        assert!(sig.flags.iter().any(|f| f.long == "backend") && sig.flags.iter().any(|f| f.long == "model"));
        assert!(!sig.examples.is_empty(), "{name}");
    }
}

#[test]
#[ignore]
fn rank_by_sweetness() {
    let out = run("[{item: salt} {item: honey} {item: lemon} {item: \"chocolate cake\"}] | llm rank \"how sweet\"");
    let scores = texts(&out.follow("score").expect("a score column"));
    let items = texts(&out.follow("item").expect("an item column"));
    eprintln!("{items:?} {scores:?}");
    let numbers: Vec<i64> = scores.iter().map(|s| s.parse().expect("an integer score")).collect();
    assert!(numbers.windows(2).all(|w| w[0] >= w[1]), "not sorted: {numbers:?}");
    let mut sweet = items[..2].to_vec();
    sweet.sort();
    assert_eq!(sweet, ["chocolate cake", "honey"], "{items:?}");
}

#[test]
#[ignore]
fn rank_keeps_the_top_rows() {
    let out =
        run("[{item: salt} {item: honey} {item: lemon} {item: sugar}] | llm rank \"how sweet\" --top 2 | get item");
    assert_eq!(texts(&out).len(), 2, "{out:?}");
}

#[test]
#[ignore]
fn group_fruits_and_vegetables() {
    let out = run("[{name: apple} {name: carrot} {name: banana} {name: potato}] \
         | llm group \"fruit or vegetable\" --max 2 | get group");
    let groups = texts(&out);
    eprintln!("{groups:?}");
    assert!(groups.iter().all(|g| !g.is_empty()), "{groups:?}");
    assert_eq!(groups[0], groups[2], "apple and banana: {groups:?}");
    assert_eq!(groups[1], groups[3], "carrot and potato: {groups:?}");
    assert_ne!(groups[0], groups[1], "{groups:?}");
}

#[test]
#[ignore]
fn join_people_with_cities() {
    let out = run(
        "let cities = [{city: Paris, country: France} {city: Tokyo, country: Japan} {city: Berlin, country: Germany}]\n\
         [{name: Anna, address: \"Unter den Linden 5, 10117 Berlin\"} \
          {name: Bob, address: \"12 Rue de Rivoli, 75001 Paris\"} \
          {name: Chen, address: \"2-1 Shibuya, Tokyo 150-0002\"}] \
         | llm join $cities \"the person lives in the city\"",
    );
    let names = texts(&out.follow("name").expect("a name column"));
    let cities = texts(&out.follow("city").expect("a city column"));
    assert_eq!(names, ["Anna", "Bob", "Chen"], "{out:?}");
    assert_eq!(cities, ["Berlin", "Paris", "Tokyo"], "{out:?}");
}

#[test]
#[ignore]
fn embed_and_search() {
    let model = std::env::var("MEERSH_TEST_EMBED_MODEL").map(|m| format!(" --model {m}")).unwrap_or_default();
    let rows = "[{text: \"a ripe banana\"} {text: \"a diesel engine\"} {text: \"a fresh apple\"}]";
    let embedded = match try_run_with(local_config(), &format!("{rows} | llm embed --column text{model}")) {
        Ok(out) => out,
        Err(e) if e.contains("embed") => {
            eprintln!("skipped, the local backend cannot embed: {e}");
            return;
        }
        Err(e) => panic!("{e}"),
    };
    let Value::List(items) = &embedded else { panic!("{embedded:?}") };
    let Some(Value::List(vector)) = items[0].follow("embedding") else { panic!("{embedded:?}") };
    assert!(vector.len() > 10 && vector.iter().all(|v| matches!(v, Value::Float(_))), "{vector:?}");

    let out = run(&format!("{rows} | llm search \"car parts\" --column text --top 2{model}"));
    let found = texts(&out.follow("text").expect("a text column"));
    assert_eq!(found.len(), 2);
    assert_eq!(found[0], "a diesel engine", "{out:?}");

    // A stored embedding is reused, so only the query is embedded.
    let out = run(&format!("{rows} | llm embed --column text{model} | llm search \"fruit\"{model} | get text"));
    assert_ne!(texts(&out)[0], "a diesel engine", "{out:?}");
}
