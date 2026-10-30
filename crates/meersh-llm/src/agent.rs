//! `llm agent`. Claude Code works on a task with meersh as its only tool.
//!
//! The built in tools of Claude Code are switched off. The agent gets the meersh MCP server
//! (`meersh mcp --agent claude`) instead, so every file it creates and every process it
//! starts goes through meersh's authority under the principal `agent:claude`. What it may
//! do without asking is what the policy allows that principal, which by default is creating
//! files in the project and changing or removing what it created itself. Everything else
//! comes back as a plan the agent has to report.

use std::io::Write;
use std::process::{Command as Process, Stdio};

use meersh_engine::value::{Record, Type, Value};
use meersh_engine::{Cap, Command, Ctx, Effect, PipelineData, Result, Signature};
use serde_json::{Value as Json, json};

const AGENT_PROMPT: &str = "You work through the meersh MCP tools only. meersh is an object shell. Use the `run` tool \
for pipelines such as `ls src | where size > 10kb`, and `help` to learn commands and types. Actions that need the \
user's approval come back as a plan that was not applied. Report them in your answer instead of trying other ways.";

pub struct LlmAgent;

impl Command for LlmAgent {
    fn signature(&self) -> Signature {
        Signature::build("llm agent")
            .desc(
                "Let Claude Code work on a task with meersh as its only tool. Its actions run under the principal \
                 agent:claude and are bounded by ownership and policy.",
            )
            .input(Type::Nothing)
            .output(Type::record("llm.agent"))
            .rest("task", Type::String, "what the agent should do")
            .named("model", Type::String, Some('m'), "Claude model, for example sonnet or haiku")
            .named("name", Type::String, None, "agent name for the principal, claude by default")
            .named("budget", Type::Float, None, "maximum dollars to spend")
            .effect(Effect::Llm)
            .example(
                "llm agent \"remove the build artefacts you created yesterday\"",
                "an agent cleans up after itself",
            )
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let task = ctx.rest_strings(0)?.join(" ");
        if task.trim().is_empty() {
            return Err(ctx.err("what should the agent do?"));
        }
        ctx.check_now(Cap::LlmSend { backend: "claude-code".into(), local: false })?;
        let exe =
            ctx.engine.exec_helper.clone().ok_or_else(|| ctx.err("needs the meersh binary for its MCP server"))?;
        let name = ctx.named_string("name")?.unwrap_or_else(|| "claude".into());
        let servers = json!({"mcpServers": {"meersh": {"command": exe, "args": ["mcp", "--agent", name]}}});
        let mut command = Process::new("claude");
        command
            .args(["-p", "--output-format", "json", "--no-session-persistence", "--tools", ""])
            .args(["--strict-mcp-config", "--mcp-config", &servers.to_string()])
            .args(["--allowedTools", "mcp__meersh__*"])
            .args(["--append-system-prompt", AGENT_PROMPT])
            .env_remove("CLAUDECODE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(model) = ctx.named_string("model")? {
            command.args(["--model", &model]);
        }
        if let Some(budget) = ctx.named("budget")? {
            command.args(["--max-budget-usd", &budget.to_text()]);
        }
        let mut child = command.spawn().map_err(|e| ctx.err(format!("cannot start claude ({e})")))?;
        child.stdin.take().expect("stdin was piped").write_all(task.as_bytes())?;
        let output = child.wait_with_output()?;
        let answer: Json = serde_json::from_slice(&output.stdout).map_err(|_| {
            ctx.err(format!(
                "claude failed: {}{}",
                String::from_utf8_lossy(&output.stderr),
                String::from_utf8_lossy(&output.stdout)
            ))
        })?;
        let record = Record::typed("llm.agent")
            .with("agent", format!("agent:{name}"))
            .with("result", answer["result"].as_str().unwrap_or_default())
            .with("turns", answer["num_turns"].as_i64())
            .with("cost_usd", answer["total_cost_usd"].as_f64())
            .with("error", answer["is_error"].as_bool().unwrap_or(false));
        Ok(PipelineData::Value(Value::Record(record)))
    }
}
