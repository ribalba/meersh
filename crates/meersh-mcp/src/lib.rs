//! meersh as the execution layer for coding agents, over the Model Context Protocol.
//!
//! The server speaks JSON-RPC 2.0 with one message per line. Every pipeline runs under the
//! principal `agent:<name>` and is planned first. Nobody can answer a question on this
//! channel, so a plan is applied only when every action in it is allowed. Otherwise nothing
//! changes and the agent gets the plan back, together with a hint on how the user can
//! approve it. AUTHORITY.md, section 8, describes the design.

mod tools;
mod typed;

use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::os::fd::AsFd;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use meersh_engine::{Engine, Principal, Stack};
use serde_json::{Map, Value as Json, json};

/// The protocol versions this server understands, newest first. All of them agree on the
/// parts used here, which are tools and ping.
const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;

/// Serves MCP on standard input and output until the client closes its end.
///
/// The protocol moves to private copies of both descriptors first. Descriptor 0 then reads
/// from `/dev/null` and descriptor 1 writes to stderr, so neither a command that prints on
/// its own nor an external program can read requests or corrupt the replies.
pub fn serve(engine: Arc<Engine>, agent: &str) -> io::Result<()> {
    let (input, output) = take_stdio()?;
    serve_io(engine, agent, BufReader::new(input), BufWriter::new(output))
}

/// Serves MCP on any reader and writer. Each line of `reader` is one JSON-RPC message, and
/// each reply is written as one line and flushed right away.
pub fn serve_io(engine: Arc<Engine>, agent: &str, mut reader: impl BufRead, mut writer: impl Write) -> io::Result<()> {
    let mut server = Server::new(engine, agent);
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            return Ok(());
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        if let Some(reply) = server.handle(&line) {
            serde_json::to_writer(&mut writer, &reply)?;
            writer.write_all(b"\n")?;
            writer.flush()?;
        }
    }
}

/// Copies of stdin and stdout for the protocol. The copies are close-on-exec, so programs
/// that meersh starts do not inherit them.
fn take_stdio() -> io::Result<(File, File)> {
    let input = File::from(io::stdin().as_fd().try_clone_to_owned()?);
    let output = File::from(io::stdout().as_fd().try_clone_to_owned()?);
    let null = File::open("/dev/null")?;
    nix::unistd::dup2_stdin(&null)?;
    nix::unistd::dup2_stdout(io::stderr())?;
    Ok((input, output))
}

/// The answer of a tool. Successful results are JSON, errors are a plain message.
pub(crate) struct Outcome {
    text: String,
    is_error: bool,
}

impl Outcome {
    pub(crate) fn json(value: Json) -> Outcome {
        Outcome { text: value.to_string(), is_error: false }
    }

    pub(crate) fn error(message: impl Into<String>) -> Outcome {
        Outcome { text: message.into(), is_error: true }
    }

    fn into_result(self) -> Json {
        json!({"content": [{"type": "text", "text": self.text}], "isError": self.is_error})
    }
}

pub(crate) struct Server {
    engine: Arc<Engine>,
    principal: Principal,
    /// Variables survive between calls, so a `let` in one call is visible in the next.
    stack: Stack,
}

impl Server {
    fn new(engine: Arc<Engine>, agent: &str) -> Server {
        let principal = Principal::Agent(agent.to_string());
        let stack = Stack::default().with_principal(principal.clone());
        Server { engine, principal, stack }
    }

    fn handle(&mut self, line: &[u8]) -> Option<Json> {
        let message: Json = match serde_json::from_slice(line) {
            Ok(message) => message,
            Err(e) => return Some(error_reply(Json::Null, PARSE_ERROR, format!("parse error, {e}"))),
        };
        match message {
            Json::Array(batch) if batch.is_empty() => {
                Some(error_reply(Json::Null, INVALID_REQUEST, "an empty batch is not a request".into()))
            }
            Json::Array(batch) => {
                let replies: Vec<Json> = batch.into_iter().filter_map(|m| self.handle_message(m)).collect();
                (!replies.is_empty()).then_some(Json::Array(replies))
            }
            other => self.handle_message(other),
        }
    }

    fn handle_message(&mut self, message: Json) -> Option<Json> {
        let Json::Object(map) = message else {
            return Some(error_reply(Json::Null, INVALID_REQUEST, "a message must be a JSON object".into()));
        };
        let id = map.get("id").cloned();
        let Some(method) = map.get("method").and_then(Json::as_str) else {
            // A response to a request of ours. This server sends none, so there is nothing to do.
            if map.contains_key("result") || map.contains_key("error") {
                return None;
            }
            return Some(error_reply(id.unwrap_or(Json::Null), INVALID_REQUEST, "the message has no method".into()));
        };
        // Notifications such as notifications/initialized and notifications/cancelled get no answer.
        let id = id?;
        let params = map.get("params").cloned().unwrap_or(Json::Null);
        let result = match method {
            "initialize" => Ok(self.initialize(&params)),
            "notifications/initialized" | "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": self.tool_list()})),
            "tools/call" => self.call_tool(&params),
            other => Err((METHOD_NOT_FOUND, format!("method not found: {other}"))),
        };
        Some(match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => error_reply(id, code, message),
        })
    }

    fn initialize(&self, params: &Json) -> Json {
        let requested = params.get("protocolVersion").and_then(Json::as_str);
        let version = requested.filter(|v| PROTOCOL_VERSIONS.contains(v)).unwrap_or(PROTOCOL_VERSIONS[0]);
        let me = self.principal.name();
        json!({
            "protocolVersion": version,
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "meersh", "version": env!("CARGO_PKG_VERSION")},
            "instructions": format!(
                "meersh is an object shell. Pipelines pass typed records and results come back as JSON. \
                 You act as {me}. Without the user you may change or delete only what you created. \
                 Every pipeline is planned first, and a plan that needs the user is returned instead of applied. \
                 Lease files while you edit them, so other agents are asked before they touch them."
            ),
        })
    }

    fn call_tool(&mut self, params: &Json) -> Result<Json, (i64, String)> {
        let name = params
            .get("name")
            .and_then(Json::as_str)
            .ok_or((INVALID_PARAMS, "tools/call needs the name of a tool".to_string()))?;
        let empty = Json::Object(Map::new());
        let args = match params.get("arguments") {
            None | Some(Json::Null) => &empty,
            Some(args @ Json::Object(_)) => args,
            Some(_) => return Err((INVALID_PARAMS, "the arguments of a tool call must be an object".into())),
        };
        match catch_unwind(AssertUnwindSafe(|| self.call(name, args))) {
            Ok(Some(outcome)) => Ok(outcome.into_result()),
            Ok(None) => Err((INVALID_PARAMS, format!("unknown tool '{name}'"))),
            Err(panic) => {
                let why = panic
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_default();
                Err((INTERNAL_ERROR, format!("the tool {name} crashed ({why})")))
            }
        }
    }
}

fn error_reply(id: Json, code: i64, message: String) -> Json {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}
