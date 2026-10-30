//! meersh as the execution layer for coding agents, over the Model Context Protocol.
//!
//! The server speaks JSON-RPC 2.0 with one message per line. Every pipeline runs under the
//! principal `agent:<name>` and is planned first. A plan in which every action is allowed is
//! applied. When an action needs the user and the client can show a form (MCP elicitation),
//! the server asks the user through the client and applies the plan only after an approval.
//! Otherwise nothing changes and the agent gets the plan back, together with a hint on how
//! the user can approve it. AUTHORITY.md, section 8, describes the design.

mod tools;
mod typed;

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::os::fd::AsFd;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use meersh_engine::{Engine, Principal, Stack};
use serde_json::{Map, Value as Json, json};

/// The protocol versions this server understands, newest first. All of them agree on tools
/// and ping. Elicitation came with 2025-06-18, and only a client that declares it is asked.
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
pub fn serve_io(engine: Arc<Engine>, agent: &str, reader: impl BufRead, writer: impl Write) -> io::Result<()> {
    let mut server = Server::new(engine, agent);
    let mut client = Client::new(reader, writer);
    while let Some(line) = client.next_message()? {
        if let Some(reply) = server.handle(&line, &mut client) {
            client.send(&reply)?;
        }
    }
    Ok(())
}

/// The connection to the client. Besides the replies it carries the questions a tool call
/// asks the user while it runs.
pub(crate) struct Client<'a> {
    reader: Box<dyn BufRead + 'a>,
    writer: Box<dyn Write + 'a>,
    /// Messages that arrived while a question was open. They are handled after the call.
    pending: VecDeque<Vec<u8>>,
    /// The id of the tools/call being served, so a cancellation of it ends an open question.
    call: Option<Json>,
    asked: u64,
}

impl<'a> Client<'a> {
    fn new(reader: impl BufRead + 'a, writer: impl Write + 'a) -> Client<'a> {
        Client { reader: Box::new(reader), writer: Box::new(writer), pending: VecDeque::new(), call: None, asked: 0 }
    }

    /// The next message, the ones that waited first. `None` when the client closed its end.
    fn next_message(&mut self) -> io::Result<Option<Vec<u8>>> {
        match self.pending.pop_front() {
            Some(line) => Ok(Some(line)),
            None => self.read_message(),
        }
    }

    /// The next line from the client that is not blank.
    fn read_message(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut line = Vec::new();
        loop {
            line.clear();
            if self.reader.read_until(b'\n', &mut line)? == 0 {
                return Ok(None);
            }
            if !line.iter().all(u8::is_ascii_whitespace) {
                return Ok(Some(line));
            }
        }
    }

    fn send(&mut self, message: &Json) -> io::Result<()> {
        serde_json::to_writer(&mut self.writer, message)?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()
    }

    /// Asks the user with `elicitation/create` and waits for the answer. A ping in between is
    /// answered and other messages wait until the call is done. `None` when the client
    /// answered with an error, cancelled the call or closed the connection.
    pub(crate) fn elicit(&mut self, message: &str, schema: Json) -> Option<Json> {
        self.asked += 1;
        let id = json!(format!("ask-{}", self.asked));
        let params = json!({"message": message, "requestedSchema": schema});
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": "elicitation/create", "params": params})).ok()?;
        loop {
            let line = self.read_message().ok()??;
            let Ok(Json::Object(map)) = serde_json::from_slice::<Json>(&line) else {
                self.pending.push_back(line);
                continue;
            };
            let method = map.get("method").and_then(Json::as_str);
            match method {
                None if map.get("id") == Some(&id) => return map.get("result").cloned(),
                Some("ping") if map.contains_key("id") => {
                    self.send(&json!({"jsonrpc": "2.0", "id": map["id"], "result": {}})).ok()?;
                }
                Some("notifications/cancelled")
                    if self.call.is_some()
                        && map.get("params").and_then(|p| p.get("requestId")) == self.call.as_ref() =>
                {
                    return None;
                }
                _ => self.pending.push_back(line),
            }
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
    /// The client declared that it can show the user a form.
    can_ask: bool,
}

impl Server {
    fn new(engine: Arc<Engine>, agent: &str) -> Server {
        let principal = Principal::Agent(agent.to_string());
        let stack = Stack::default().with_principal(principal.clone());
        Server { engine, principal, stack, can_ask: false }
    }

    fn handle(&mut self, line: &[u8], client: &mut Client) -> Option<Json> {
        let message: Json = match serde_json::from_slice(line) {
            Ok(message) => message,
            Err(e) => return Some(error_reply(Json::Null, PARSE_ERROR, format!("parse error, {e}"))),
        };
        match message {
            Json::Array(batch) if batch.is_empty() => {
                Some(error_reply(Json::Null, INVALID_REQUEST, "an empty batch is not a request".into()))
            }
            Json::Array(batch) => {
                let replies: Vec<Json> = batch.into_iter().filter_map(|m| self.handle_message(m, client)).collect();
                (!replies.is_empty()).then_some(Json::Array(replies))
            }
            other => self.handle_message(other, client),
        }
    }

    fn handle_message(&mut self, message: Json, client: &mut Client) -> Option<Json> {
        let Json::Object(map) = message else {
            return Some(error_reply(Json::Null, INVALID_REQUEST, "a message must be a JSON object".into()));
        };
        let id = map.get("id").cloned();
        let Some(method) = map.get("method").and_then(Json::as_str) else {
            // An answer to a question that is no longer open. Open ones are read by Client::elicit.
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
            "tools/call" => {
                client.call = Some(id.clone());
                let result = self.call_tool(&params, client);
                client.call = None;
                result
            }
            other => Err((METHOD_NOT_FOUND, format!("method not found: {other}"))),
        };
        Some(match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => error_reply(id, code, message),
        })
    }

    fn initialize(&mut self, params: &Json) -> Json {
        let requested = params.get("protocolVersion").and_then(Json::as_str);
        let version = requested.filter(|v| PROTOCOL_VERSIONS.contains(v)).unwrap_or(PROTOCOL_VERSIONS[0]);
        // An empty object stands for forms, the only kind of elicitation before 2025-11-25.
        let elicitation = params.pointer("/capabilities/elicitation").and_then(Json::as_object);
        self.can_ask = elicitation.is_some_and(|e| e.is_empty() || e.contains_key("form"));
        let me = self.principal.name();
        json!({
            "protocolVersion": version,
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "meersh", "version": env!("CARGO_PKG_VERSION")},
            "instructions": format!(
                "meersh is an object shell. Pipelines pass typed records and results come back as JSON. \
                 You act as {me}. Without the user you may change or delete only what you created. \
                 Every pipeline is planned first. A plan that needs the user is shown to the user when your \
                 client can ask, and returned instead of applied when the user declines or cannot be asked. \
                 Lease files while you edit them, so other agents are asked before they touch them."
            ),
        })
    }

    fn call_tool(&mut self, params: &Json, client: &mut Client) -> Result<Json, (i64, String)> {
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
        match catch_unwind(AssertUnwindSafe(|| self.call(name, args, client))) {
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
