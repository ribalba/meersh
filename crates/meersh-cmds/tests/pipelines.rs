use std::sync::Arc;

use meersh_engine::value::{Record, Value};
use meersh_engine::{Engine, Stack};

fn run(src: &str) -> Value {
    let mut engine = Engine::new();
    meersh_cmds::register(&mut engine);
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

const PEOPLE: &str =
    "let people = [{name: ada, age: 36, size: 2kb} {name: bob, age: 17, size: 1mb} {name: cy, age: 51, size: 30b}]";

#[test]
fn row_conditions_use_columns_and_literals() {
    assert_eq!(
        run(&format!("{PEOPLE}; $people | where age > 18 and size < 1kib | get name")),
        Value::List(vec!["cy".into()])
    );
    assert_eq!(run(&format!("{PEOPLE}; $people | where name =~ '^b' | length")), Value::Int(1));
    assert_eq!(run(&format!("{PEOPLE}; $people | where {{ $it.age > 40 }} | first | get name")), "cy".into());
}

#[test]
fn sorting_and_projection() {
    let names = run(&format!("{PEOPLE}; $people | sort-by size -r | select name | get name"));
    assert_eq!(names, Value::List(vec!["bob".into(), "ada".into(), "cy".into()]));
    assert_eq!(run(&format!("{PEOPLE}; $people | get size | sum")), Value::Filesize(1_002_030));
}

#[test]
fn each_builds_records() {
    let out = run(&format!("{PEOPLE}; $people | each {{ {{who: $it.name, adult: ($it.age >= 18)}} }} | first"));
    assert_eq!(out, Value::Record(Record::new().with("who", "ada").with("adult", true)));
}

#[test]
fn group_by_and_describe() {
    let groups = run(&format!("{PEOPLE}; $people | insert grown {{ $it.age >= 18 }} | group-by grown | get count"));
    assert_eq!(groups, Value::List(vec![Value::Int(2), Value::Int(1)]));
    assert_eq!(run("[{a: 1}] | describe"), "table{a: int}".into());
}

#[test]
fn json_roundtrip() {
    assert_eq!(run("[{a: 1, b: [x y]}] | to json -r | from json | get b.1"), Value::List(vec!["y".into()]));
}

#[test]
fn help_is_a_table() {
    assert_eq!(run("help | where name == sort-by | get effect | first"), "pure".into());
}
