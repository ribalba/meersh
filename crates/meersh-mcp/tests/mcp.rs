use std::path::{Path, PathBuf};
use std::sync::Arc;

use meersh_engine::authority::Rule;
use meersh_engine::value::{Record, Type, Value};
use meersh_engine::{Cap, Command, Ctx, Effect, Engine, Permit, PipelineData, Result, Signature, Verdict};
use serde_json::{Value as Json, json};

/// `make <path>` creates an empty file through the authority API.
struct Make;

impl Command for Make {
    fn signature(&self) -> Signature {
        Signature::build("make").input(Type::Nothing).required("path", Type::String, "").effect(Effect::Write)
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let path = PathBuf::from(ctx.req_string(0, "path")?);
        let action = match ctx.authorize(Cap::FsCreate { path: path.clone() })? {
            Permit::Execute => {
                std::fs::write(&path, "")?;
                ctx.performed(Cap::FsCreate { path })?;
                "created".to_string()
            }
            Permit::Plan => "would create".into(),
            Permit::Skip(why) => format!("skipped, {why}"),
        };
        Ok(PipelineData::Value(Value::Record(Record::new().with("action", action))))
    }
}

/// `remove <path>` deletes a file for good.
struct Remove;

impl Command for Remove {
    fn signature(&self) -> Signature {
        Signature::build("remove").input(Type::Nothing).required("path", Type::String, "").effect(Effect::Destructive)
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let path = PathBuf::from(ctx.req_string(0, "path")?);
        let cap = || Cap::FsDelete { path: path.clone(), permanent: true };
        let action = match ctx.authorize(cap())? {
            Permit::Execute => {
                std::fs::remove_file(&path)?;
                ctx.performed(cap())?;
                "deleted".to_string()
            }
            Permit::Plan => "would delete".into(),
            Permit::Skip(why) => format!("skipped, {why}"),
        };
        Ok(PipelineData::Value(Value::Record(Record::new().with("action", action))))
    }
}

fn engine() -> Arc<Engine> {
    let mut engine = Engine::new();
    meersh_cmds::register(&mut engine);
    engine.register(Make);
    engine.register(Remove);
    Arc::new(engine)
}

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("meersh-mcp-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Sends the messages as one session and returns the replies.
fn session(engine: &Arc<Engine>, agent: &str, messages: &[Json]) -> Vec<Json> {
    let input: String = messages.iter().map(|m| format!("{m}\n")).collect();
    let mut output = Vec::new();
    meersh_mcp::serve_io(engine.clone(), agent, input.as_bytes(), &mut output).unwrap();
    String::from_utf8(output).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect()
}

fn request(id: i64, method: &str, params: Json) -> Json {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

fn call(id: i64, tool: &str, arguments: Json) -> Json {
    request(id, "tools/call", json!({"name": tool, "arguments": arguments}))
}

/// The JSON inside the text content of a tool result.
fn content(reply: &Json) -> Json {
    let result = &reply["result"];
    assert_eq!(result["isError"], json!(false), "{reply}");
    serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
}

fn quoted(path: &Path) -> String {
    format!("'{}'", path.display())
}

#[test]
fn initialize_and_list_the_tools() {
    let replies = session(
        &engine(),
        "test",
        &[
            request(
                1,
                "initialize",
                json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t"}}),
            ),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            request(2, "tools/list", json!({})),
            request(3, "ping", json!({})),
            request(4, "resources/list", json!({})),
        ],
    );
    assert_eq!(replies.len(), 4, "the notification gets no answer: {replies:?}");

    let init = &replies[0]["result"];
    assert_eq!(init["protocolVersion"], "2025-06-18");
    assert_eq!(init["serverInfo"]["name"], "meersh");
    assert!(init["capabilities"]["tools"].is_object());

    let tools = replies[1]["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for expected in
        ["run", "plan", "help", "ledger", "journal", "lease", "release", "cmd_help", "cmd_echo", "cmd_date_now"]
    {
        assert!(names.contains(&expected), "{expected} missing in {names:?}");
    }
    // Commands that need input, write or change the policy get no typed tool.
    for absent in ["cmd_where", "cmd_length", "cmd_make", "cmd_remove", "cmd_policy_allow"] {
        assert!(!names.contains(&absent), "{absent} should not be a tool");
    }
    for tool in tools {
        assert_eq!(tool["inputSchema"]["type"], "object", "{tool}");
        assert!(!tool["description"].as_str().unwrap().is_empty(), "{tool}");
    }
    let help = tools.iter().find(|t| t["name"] == "cmd_help").unwrap();
    assert_eq!(help["inputSchema"]["properties"]["command"]["type"], "array");

    assert_eq!(replies[2]["result"], json!({}));
    assert_eq!(replies[3]["error"]["code"], -32601);
}

#[test]
fn run_returns_values_as_json() {
    let replies = session(&engine(), "test", &[call(1, "run", json!({"pipeline": "[1 2 3] | length"}))]);
    assert_eq!(content(&replies[0]), json!(3));
}

#[test]
fn typed_tools_quote_their_arguments() {
    let values = json!(["a b", "say \"hi\"\n", "--all", "$env", 2, true, [1, "x"], {"key": "v"}]);
    let replies = session(
        &engine(),
        "test",
        &[
            call(1, "cmd_echo", json!({"values": values})),
            call(2, "cmd_help", json!({"command": ["length"]})),
            call(3, "cmd_echo", json!({"bogus": 1})),
        ],
    );
    assert_eq!(content(&replies[0]), values);
    assert_eq!(content(&replies[1])["name"], "length");
    assert_eq!(replies[2]["result"]["isError"], json!(true));
}

#[test]
fn an_action_that_needs_the_user_returns_the_plan() {
    let engine = engine();
    let dir = temp("approval");
    let foreign = dir.join("foreign.txt");
    std::fs::write(&foreign, "written outside meersh").unwrap();
    let pipeline = format!("remove {}", quoted(&foreign));

    let replies = session(
        &engine,
        "test",
        &[call(1, "run", json!({"pipeline": pipeline})), call(2, "plan", json!({"pipeline": pipeline}))],
    );
    let run = content(&replies[0]);
    assert_eq!(run["applied"], json!(false), "{run}");
    let action = &run["plan"][0];
    assert_eq!(action["action"], "delete");
    assert_eq!(action["cap"], "fs.delete");
    assert_eq!(action["principal"], "agent:test");
    assert_ne!(action["decision"], "allow", "{action}");
    let hint = run["hint"].as_str().unwrap();
    assert!(hint.contains("policy allow agent:test fs.delete"), "{hint}");
    assert!(foreign.exists(), "a rejected plan changes nothing");

    let plan = content(&replies[1]);
    assert_eq!(plan["needs_user"], json!(true));
    assert_eq!(plan["plan"].as_array().unwrap().len(), 1);
    assert!(foreign.exists());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn an_allowed_plan_is_applied_and_owned_by_the_agent() {
    let engine = engine();
    engine.authority.add_rule(Rule {
        verdict: Verdict::Allow,
        principal: "agent:*".into(),
        cap: "fs.create".into(),
        condition: None,
        task: None,
        text: "agents may create files in tests".into(),
    });
    let dir = temp("apply");
    let file = dir.join("new.txt");
    let replies = session(
        &engine,
        "test",
        &[
            call(1, "run", json!({"pipeline": format!("make {}", quoted(&file))})),
            call(2, "ledger", json!({})),
            call(3, "journal", json!({})),
        ],
    );
    assert_eq!(content(&replies[0])["action"], "created");
    assert!(file.exists());
    let ledger = content(&replies[1]);
    let mine = ledger["mine"].as_array().unwrap();
    assert!(mine.iter().any(|e| e["object"] == file.display().to_string() && e["state"] == "owned"), "{ledger}");
    let journal = content(&replies[2]);
    assert_eq!(journal[0]["principal"], "agent:test");
    assert_eq!(journal[0]["action"], "create");

    // The user's own policy commands are not open to the agent.
    assert!(engine.is_command("policy allow"));
    let replies = session(&engine, "test", &[call(1, "run", json!({"pipeline": "policy allow agent:test fs.*"}))]);
    assert_eq!(replies[0]["result"]["isError"], json!(true), "{}", replies[0]);
    assert_eq!(engine.authority.rules().len(), 1);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_lease_shows_in_the_ledger_and_stops_other_agents() {
    let engine = engine();
    let dir = temp("lease");
    let file = dir.join("parser.rs");
    std::fs::write(&file, "fn main() {}").unwrap();
    let path = file.display().to_string();

    let replies = session(
        &engine,
        "test",
        &[call(1, "lease", json!({"path": path, "minutes": 5})), call(2, "ledger", json!({}))],
    );
    let lease = content(&replies[0]);
    assert_eq!(lease["leased"], json!(true), "{lease}");
    assert_eq!(lease["holder"], "agent:test");
    let ledger = content(&replies[1]);
    let leases = ledger["leases"].as_array().unwrap();
    assert!(leases.iter().any(|l| l["path"] == path && l["holder"] == "agent:test"), "{ledger}");

    // Another agent is asked before it touches the leased file, and cannot take the lease.
    let replies = session(
        &engine,
        "other",
        &[
            call(1, "run", json!({"pipeline": format!("remove {}", quoted(&file))})),
            call(2, "lease", json!({"path": path})),
        ],
    );
    let run = content(&replies[0]);
    assert_eq!(run["applied"], json!(false));
    assert_eq!(run["plan"][0]["decision"], "ask");
    assert!(run["plan"][0]["because"].as_str().unwrap().contains("leased by agent:test"), "{run}");
    assert_eq!(content(&replies[1])["leased"], json!(false));
    assert!(file.exists());

    let replies = session(&engine, "test", &[call(1, "release", json!({"path": path}))]);
    assert_eq!(content(&replies[0])["released"], json!(true));
    assert!(engine.authority.ledger.lease_rows().is_empty());
    std::fs::remove_dir_all(dir).unwrap();
}
