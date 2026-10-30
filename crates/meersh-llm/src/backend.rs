use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};

use crate::config::{BackendConfig, BackendKind};

pub struct Request {
    pub system: String,
    pub prompt: String,
    /// JSON Schema the answer must follow. Its root is always an object, because that is
    /// what every backend supports.
    pub schema: Option<Json>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub text: String,
    /// The parsed answer when the request had a schema.
    pub json: Option<Json>,
    pub model: String,
}

pub fn complete(config: &BackendConfig, request: &Request) -> Result<Response, String> {
    match config.kind {
        BackendKind::Ollama => ollama(config, request),
        BackendKind::Openai => openai(config, request),
        BackendKind::ClaudeCode => claude_code(config, request),
    }
}

fn agent(config: &BackendConfig) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(config.timeout_secs.unwrap_or(600))))
        .http_status_as_error(false)
        .build()
        .into()
}

fn post(config: &BackendConfig, url: &str, body: &Json) -> Result<Json, String> {
    let mut request = agent(config).post(url);
    if let Some(var) = &config.api_key_env {
        let key = std::env::var(var).map_err(|_| format!("environment variable {var} is not set"))?;
        request = request.header("Authorization", &format!("Bearer {key}"));
    }
    let mut response = request.send_json(body).map_err(|e| format!("{url}: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        let detail = response.body_mut().read_to_string().unwrap_or_default();
        return Err(format!("{url} answered {status}: {detail}"));
    }
    response.body_mut().read_json().map_err(|e| format!("{url}: invalid response: {e}"))
}

fn messages(request: &Request) -> Json {
    chat(request, false)
}

/// `think: false` does not stop every model from reasoning. Qwen3 on Ollama then writes its
/// reasoning into the answer itself, which costs minutes on a CPU. Qwen3 has its own switch,
/// `/no_think` at the end of the user message.
fn chat(request: &Request, no_think: bool) -> Json {
    let prompt = if no_think { format!("{}\n/no_think", request.prompt) } else { request.prompt.clone() };
    json!([
        {"role": "system", "content": request.system},
        {"role": "user", "content": prompt},
    ])
}

fn wants_no_think(config: &BackendConfig, model: &str) -> bool {
    config.think == Some(false) && model.to_ascii_lowercase().contains("qwen3")
}

/// Removes reasoning from an answer. `<think>...</think>` blocks go, and when a model left
/// out the opening tag, everything up to the last `</think>` goes.
pub(crate) fn strip_thinking(text: &str) -> String {
    let mut out = text.to_string();
    while let (Some(start), Some(end)) = (out.find("<think>"), out.find("</think>")) {
        if end < start {
            break;
        }
        out.replace_range(start..end + "</think>".len(), "");
    }
    if let Some(end) = out.rfind("</think>") {
        out = out[end + "</think>".len()..].to_string();
    }
    out.trim().to_string()
}

/// Streams text to the terminal without the reasoning. The first part of an answer is held
/// back until it is clear whether a think block comes, then everything after it flows.
struct ThinkFilter {
    held: String,
    decided: bool,
    /// Set after a think block, until the first visible character of the answer.
    trimming: bool,
}

const HOLD: usize = 400;

impl ThinkFilter {
    fn push(&mut self, piece: &str, out: &mut dyn FnMut(&str)) {
        if self.decided {
            let piece = if self.trimming { piece.trim_start() } else { piece };
            if !piece.is_empty() {
                self.trimming = false;
                out(piece);
            }
            return;
        }
        self.held.push_str(piece);
        if let Some(end) = self.held.find("</think>") {
            let rest = self.held[end + "</think>".len()..].trim_start().to_string();
            self.decided = true;
            self.trimming = rest.is_empty();
            if !rest.is_empty() {
                out(&rest);
            }
        } else if !self.held.trim_start().starts_with("<think>") && self.held.len() > HOLD {
            self.decided = true;
            out(&self.held);
        }
    }

    fn finish(&mut self, out: &mut dyn FnMut(&str)) {
        if !self.decided {
            out(&strip_thinking(&self.held));
        }
    }
}

fn ollama(config: &BackendConfig, request: &Request) -> Result<Response, String> {
    let url = config.url.as_deref().unwrap_or("http://localhost:11434");
    let model = match &config.model {
        Some(model) => model.clone(),
        None => first_ollama_model(config, url)?,
    };
    let mut options = json!({"temperature": 0});
    if let Some(num_ctx) = config.num_ctx {
        options["num_ctx"] = json!(num_ctx);
    }
    let no_think = wants_no_think(config, &model);
    let mut body = json!({"model": model, "stream": false, "messages": chat(request, no_think), "options": options});
    let wrapped = request.schema.is_none() && config.think == Some(false);
    if let Some(schema) = &request.schema {
        body["format"] = schema.clone();
    } else if wrapped {
        body["format"] = answer_schema();
    }
    if let Some(think) = config.think {
        body["think"] = json!(think);
    }
    let answer = post(config, &format!("{url}/api/chat"), &body)?;
    let text = answer["message"]["content"].as_str().unwrap_or_default().to_string();
    if wrapped {
        let json: Json = serde_json::from_str(text.trim()).unwrap_or_default();
        let answer = json["answer"].as_str().map_or_else(|| strip_thinking(&text), str::to_string);
        return Ok(Response { text: answer, json: None, model });
    }
    Response::from_text(text, request.schema.is_some(), model)
}

/// A text answer as JSON, `{"answer": "..."}`. Constrained decoding makes the model start the
/// answer right away, which is the only reliable way to stop some reasoning models from
/// thinking aloud when `think: false` is ignored.
fn answer_schema() -> Json {
    json!({"type": "object", "properties": {"answer": {"type": "string"}}, "required": ["answer"]})
}

/// Pulls the value of `"answer"` out of a JSON object while it streams in, so the text can be
/// shown as it arrives.
#[derive(Default)]
struct AnswerStream {
    raw: String,
    /// Byte offset in `raw` where the string value starts, once found.
    start: Option<usize>,
    /// Bytes of the value already looked at.
    read: usize,
    done: bool,
}

impl AnswerStream {
    fn push(&mut self, piece: &str, out: &mut dyn FnMut(&str)) {
        if self.done {
            return;
        }
        self.raw.push_str(piece);
        if self.start.is_none() {
            let Some(key) = self.raw.find("\"answer\"") else { return };
            let after = &self.raw[key + 8..];
            let Some(colon) = after.find(':') else { return };
            let Some(quote) = after[colon..].find('"') else { return };
            self.start = Some(key + 8 + colon + quote + 1);
        }
        let start = self.start.expect("set above");
        let value = &self.raw[start..];
        let mut text = String::new();
        let mut chars = value[self.read..].char_indices().peekable();
        let mut consumed = self.read;
        while let Some((i, c)) = chars.next() {
            match c {
                '"' => {
                    self.done = true;
                    consumed = self.read + i + 1;
                    break;
                }
                '\\' => {
                    // An escape needs its next character, and \u needs four more.
                    let rest = &value[self.read + i..];
                    let needed = if rest.starts_with("\\u") { 6 } else { 2 };
                    if rest.len() < needed {
                        break;
                    }
                    let escape = &rest[..needed];
                    text.push_str(&serde_json::from_str::<String>(&format!("\"{escape}\"")).unwrap_or_default());
                    for _ in 1..needed {
                        chars.next();
                    }
                    consumed = self.read + i + needed;
                }
                other => {
                    text.push(other);
                    consumed = self.read + i + other.len_utf8();
                }
            }
        }
        self.read = consumed;
        if !text.is_empty() {
            out(&text);
        }
    }
}

/// Ollama with `stream: true`. Every line of the answer is a JSON object with the next piece.
pub(crate) fn ollama_stream(
    config: &BackendConfig,
    request: &Request,
    on_token: &mut dyn FnMut(&str),
) -> Result<Response, String> {
    use std::io::BufRead;
    let url = config.url.as_deref().unwrap_or("http://localhost:11434");
    let model = match &config.model {
        Some(model) => model.clone(),
        None => first_ollama_model(config, url)?,
    };
    let mut options = json!({"temperature": 0});
    if let Some(num_ctx) = config.num_ctx {
        options["num_ctx"] = json!(num_ctx);
    }
    let no_think = wants_no_think(config, &model);
    let mut body = json!({"model": model, "stream": true, "messages": chat(request, no_think), "options": options});
    let wrapped = config.think == Some(false);
    if wrapped {
        body["format"] = answer_schema();
    }
    if let Some(think) = config.think {
        body["think"] = json!(think);
    }
    let endpoint = format!("{url}/api/chat");
    let mut response = agent(config).post(&endpoint).send_json(&body).map_err(|e| format!("{endpoint}: {e}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let detail = response.body_mut().read_to_string().unwrap_or_default();
        return Err(format!("{endpoint} answered {status}: {detail}"));
    }
    let reader = std::io::BufReader::new(response.body_mut().as_reader());
    let mut text = String::new();
    let mut filter = ThinkFilter { held: String::new(), decided: false, trimming: false };
    let mut answer = AnswerStream::default();
    let mut shown = String::new();
    for line in reader.lines() {
        let line = line.map_err(|e| e.to_string())?;
        let Ok(chunk) = serde_json::from_str::<Json>(&line) else { continue };
        if let Some(piece) = chunk["message"]["content"].as_str() {
            if wrapped {
                answer.push(piece, &mut |s| {
                    shown.push_str(s);
                    on_token(s);
                });
            } else {
                filter.push(piece, on_token);
            }
            text.push_str(piece);
        }
        if chunk["done"].as_bool() == Some(true) {
            break;
        }
    }
    if wrapped {
        return Ok(Response { text: shown, json: None, model });
    }
    filter.finish(on_token);
    Ok(Response { text: strip_thinking(&text), json: None, model })
}

fn first_ollama_model(config: &BackendConfig, url: &str) -> Result<String, String> {
    let mut response = agent(config).get(&format!("{url}/api/tags")).call().map_err(|e| format!("{url}: {e}"))?;
    let tags: Json = response.body_mut().read_json().map_err(|e| e.to_string())?;
    tags["models"][0]["name"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("no model installed in Ollama at {url}, run `ollama pull qwen3:4b`"))
}

fn openai(config: &BackendConfig, request: &Request) -> Result<Response, String> {
    let url = config.url.as_deref().unwrap_or("http://localhost:8080/v1");
    let model = config.model.clone().unwrap_or_else(|| "default".into());
    let mut body = json!({"model": model, "messages": messages(request), "temperature": 0});
    if let Some(schema) = &request.schema {
        body["response_format"] = json!({
            "type": "json_schema",
            "json_schema": {"name": "meersh_stage", "schema": schema, "strict": true},
        });
    }
    let answer = post(config, &format!("{url}/chat/completions"), &body)?;
    let text = answer["choices"][0]["message"]["content"].as_str().unwrap_or_default().to_string();
    Response::from_text(text, request.schema.is_some(), model)
}

/// Runs `claude -p` as a plain model. `--system-prompt` replaces the coding agent prompt and
/// `--tools ""` removes all tools, so the call cannot touch the machine. `--safe-mode` keeps
/// the user's CLAUDE.md, hooks, plugins and MCP servers out, which would otherwise leak into
/// every answer. An agentic mode would keep the tools and let Claude Code work on the
/// current directory.
fn claude_code(config: &BackendConfig, request: &Request) -> Result<Response, String> {
    let mut command = Command::new("claude");
    command
        .args(["-p", "--output-format", "json", "--no-session-persistence", "--safe-mode", "--tools", ""])
        .args(["--system-prompt", &request.system])
        // The variable marks a nested session. A shell started from Claude Code inherits it.
        .env_remove("CLAUDECODE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(model) = &config.model {
        command.args(["--model", model]);
    }
    if let Some(schema) = &request.schema {
        command.args(["--json-schema", &schema.to_string()]);
    }
    let mut child = command.spawn().map_err(|e| format!("cannot start claude: {e}"))?;
    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(request.prompt.as_bytes())
        .map_err(|e| format!("cannot send the prompt to claude: {e}"))?;
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    let answer: Json = serde_json::from_slice(&output.stdout).map_err(|_| {
        format!("claude failed: {}{}", String::from_utf8_lossy(&output.stderr), String::from_utf8_lossy(&output.stdout))
    })?;
    let text = answer["result"].as_str().unwrap_or_default().to_string();
    if answer["is_error"].as_bool() == Some(true) {
        return Err(format!("claude: {text}"));
    }
    let model = config.model.clone().unwrap_or_else(|| "claude-code".into());
    if request.schema.is_some()
        && let Some(structured) = answer.get("structured_output").filter(|v| !v.is_null())
    {
        return Ok(Response { text, json: Some(structured.clone()), model });
    }
    Response::from_text(text, request.schema.is_some(), model)
}

impl Response {
    fn from_text(text: String, structured: bool, model: String) -> Result<Response, String> {
        let text = strip_thinking(&text);
        if !structured {
            return Ok(Response { text, json: None, model });
        }
        // Models without constrained decoding like to wrap JSON in markdown fences.
        let trimmed =
            text.trim().trim_start_matches("```json").trim_start_matches("```").trim_end_matches("```").trim();
        let json =
            serde_json::from_str(trimmed).map_err(|e| format!("the model did not return valid JSON ({e}): {text}"))?;
        Ok(Response { text, json: Some(json), model })
    }
}

#[cfg(test)]
mod tests {
    use super::{ThinkFilter, strip_thinking};

    #[test]
    fn thinking_is_removed() {
        assert_eq!(strip_thinking("<think>hmm</think>\n\nfind . -type d"), "find . -type d");
        assert_eq!(strip_thinking("Okay, let me see.\n</think>\n\nls"), "ls");
        assert_eq!(strip_thinking("plain answer"), "plain answer");
    }

    #[test]
    fn answers_stream_out_of_json() {
        let mut shown = String::new();
        let mut stream = super::AnswerStream::default();
        for piece in ["{\"ans", "wer\": \"Der Hund", " bellt \\\"", "laut\\\"\\n", "\\u00e4\"", "}"] {
            stream.push(piece, &mut |s| shown.push_str(s));
        }
        assert_eq!(shown, "Der Hund bellt \"laut\"\nä");
    }

    #[test]
    fn the_stream_filter_holds_back_reasoning() {
        let mut shown = String::new();
        let mut filter = ThinkFilter { held: String::new(), decided: false, trimming: false };
        for piece in ["Okay, ", "let me think.", "</think>", "\n\nls | where ", "type == dir"] {
            filter.push(piece, &mut |s| shown.push_str(s));
        }
        filter.finish(&mut |s| shown.push_str(s));
        assert_eq!(shown, "ls | where type == dir");
    }
}
