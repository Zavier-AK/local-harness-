//! The `codex` CLI backend — ChatGPT Plus quota, and the cheapest route to an *agentic*
//! local model.
//!
//! Codex reserves `ollama` and `lmstudio` as built-in provider ids, so a role that sets
//! `provider_opts = { model_provider = "ollama" }` gets a local model with a real tool
//! loop and sandboxing for free. That is strictly better than hand-rolling a tool loop
//! over raw chat completions, which is why `openai_compat` is reserved for work that
//! needs no tools at all.

use anyhow::{Context, Result};
use serde_json::Value;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use super::{compose_prompt, scrub_api_keys, EventSink, RunOutcome, WorkerSpec};
use crate::event::{HarnessEvent, Usage};
use crate::roles::{Isolation, Role};

/// Codex's sandbox level for a given isolation mode.
///
/// The worktree already contains the blast radius; this is the second layer, so a
/// readonly worker is refused writes by the CLI itself rather than only by convention.
fn sandbox_mode(isolation: Isolation) -> &'static str {
    match isolation {
        Isolation::Readonly | Isolation::None => "read-only",
        Isolation::Worktree | Isolation::Shared => "workspace-write",
    }
}

fn build_args(role: &Role, prompt: &str) -> Vec<String> {
    let mut args = vec!["exec".to_string(), "--json".to_string()];

    args.push("--sandbox".into());
    args.push(sandbox_mode(role.isolation).into());

    if let Some(model) = &role.model {
        args.push("--model".into());
        args.push(model.clone());
    }

    // `-c key=value` overrides, e.g. model_provider=ollama for a local agentic worker.
    for (key, value) in &role.provider_opts {
        args.push("-c".into());
        args.push(format!("{key}={value}"));
    }

    args.push(prompt.to_string());
    args
}

/// Map one line of `codex exec --json` output.
///
/// Codex's event vocabulary differs from Claude's and has shifted between releases, so
/// this matches on several spellings and ignores what it does not recognize.
pub fn parse_line(run_id: &str, line: &str) -> Result<Vec<HarnessEvent>, String> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(Vec::new());
    }

    let value: Value =
        serde_json::from_str(line).map_err(|e| format!("unparseable codex line: {e}"))?;

    // Codex nests the payload under `msg` in some versions and flattens it in others.
    let body = value.get("msg").unwrap_or(&value);
    let kind = body
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let mut events = Vec::new();

    match kind {
        "session.created" | "session_configured" => {
            events.push(HarnessEvent::SessionStarted {
                run_id: run_id.to_string(),
                backend_session_id: body
                    .get("session_id")
                    .or_else(|| body.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                provider: Some("codex".into()),
                model: body.get("model").and_then(Value::as_str).map(str::to_string),
                tools: Vec::new(),
                mcp_servers: Vec::new(),
                mcp_failed: Vec::new(),
            });
        }

        "agent_message" | "agent.message" => {
            if let Some(text) = body
                .get("message")
                .or_else(|| body.get("text"))
                .and_then(Value::as_str)
            {
                events.push(HarnessEvent::AssistantText {
                    run_id: run_id.to_string(),
                    text: text.to_string(),
                    partial: false,
                });
            }
        }

        "agent_message_delta" | "agent.message.delta" => {
            if let Some(text) = body.get("delta").and_then(Value::as_str) {
                events.push(HarnessEvent::AssistantText {
                    run_id: run_id.to_string(),
                    text: text.to_string(),
                    partial: true,
                });
            }
        }

        "agent_reasoning" | "agent.reasoning" => {
            if let Some(text) = body
                .get("text")
                .or_else(|| body.get("reasoning"))
                .and_then(Value::as_str)
            {
                events.push(HarnessEvent::Thinking {
                    run_id: run_id.to_string(),
                    text: text.to_string(),
                });
            }
        }

        "exec_command_begin" | "command.begin" => {
            let command = body
                .get("command")
                .map(|c| match c {
                    Value::Array(parts) => parts
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" "),
                    other => other.as_str().unwrap_or_default().to_string(),
                })
                .unwrap_or_default();

            events.push(HarnessEvent::ToolCall {
                run_id: run_id.to_string(),
                tool_use_id: body
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                name: "Bash".into(),
                input: serde_json::json!({ "command": command }),
            });
        }

        "exec_command_end" | "command.end" => {
            let exit_code = body.get("exit_code").and_then(Value::as_i64).unwrap_or(0);
            events.push(HarnessEvent::ToolResult {
                run_id: run_id.to_string(),
                tool_use_id: body
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                content: body
                    .get("stdout")
                    .or_else(|| body.get("output"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                is_error: exit_code != 0,
            });
        }

        "token_count" | "token.count" => {
            events.push(HarnessEvent::RunFinished {
                run_id: run_id.to_string(),
                text: String::new(),
                usage: Usage {
                    input_tokens: body.get("input_tokens").and_then(Value::as_u64).unwrap_or(0),
                    output_tokens: body.get("output_tokens").and_then(Value::as_u64).unwrap_or(0),
                    cache_read_input_tokens: body
                        .get("cached_input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    ..Default::default()
                },
                // Subscription-covered; no dollar figure is reported or meaningful.
                cost_usd: None,
                is_error: false,
            });
        }

        "error" => {
            events.push(HarnessEvent::Error {
                run_id: run_id.to_string(),
                message: body
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("codex reported an error")
                    .to_string(),
            });
        }

        _ => {}
    }

    Ok(events)
}

pub async fn run(spec: &WorkerSpec, sink: &EventSink) -> Result<RunOutcome> {
    let prompt = compose_prompt(spec);
    let args = build_args(&spec.role, &prompt);

    let mut cmd = Command::new("codex");
    cmd.args(&args)
        .current_dir(&spec.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    scrub_api_keys(&mut cmd);

    let mut child = cmd
        .spawn()
        .context("spawning `codex` — is the CLI installed and on PATH?")?;

    if let Some(stderr) = child.stderr.take() {
        let run_id = spec.run_id.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::warn!(run_id = %run_id, "codex stderr: {line}");
            }
        });
    }

    let stdout = child.stdout.take().context("codex child has no stdout")?;
    let mut lines = BufReader::new(stdout).lines();

    let mut outcome = RunOutcome::default();
    // Codex has no single terminal result line; the last agent message is the answer.
    let mut last_message = String::new();

    while let Some(line) = lines.next_line().await? {
        match parse_line(&spec.run_id, &line) {
            Ok(events) => {
                for event in events {
                    match &event {
                        HarnessEvent::AssistantText { text, partial: false, .. } => {
                            last_message = text.clone();
                        }
                        HarnessEvent::SessionStarted { backend_session_id, .. } => {
                            outcome.backend_session_id = backend_session_id.clone();
                        }
                        HarnessEvent::RunFinished { usage, .. } => outcome.usage.add(usage),
                        HarnessEvent::Error { message, .. } => {
                            outcome.is_error = true;
                            last_message = message.clone();
                        }
                        _ => {}
                    }
                    let _ = sink.send(event);
                }
            }
            Err(err) => tracing::warn!(run_id = %spec.run_id, "{err}"),
        }
    }

    let status = child.wait().await?;
    if !status.success() {
        outcome.is_error = true;
        if last_message.is_empty() {
            last_message = format!("codex exited with {status}");
        }
    }

    outcome.text = last_message;
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roles::Provider;
    use std::collections::BTreeMap;

    fn local_role() -> Role {
        let mut opts = BTreeMap::new();
        // The free agent loop over a local model.
        opts.insert("model_provider".to_string(), "ollama".to_string());
        Role {
            provider: Provider::Codex,
            model: Some("qwen3.6:35b-a3b".into()),
            isolation: Isolation::Worktree,
            tools: vec![],
            brief: None,
            permission_mode: None,
            base_url: None,
            provider_opts: opts,
            fallback_role: None,
            max_turns: None,
        }
    }

    #[test]
    fn local_models_are_routed_through_codex_provider_overrides() {
        let args = build_args(&local_role(), "do it");
        assert_eq!(args[0], "exec");
        assert!(args.iter().any(|a| a == "--json"));

        let idx = args.iter().position(|a| a == "-c").unwrap();
        assert_eq!(args[idx + 1], "model_provider=ollama");
        assert_eq!(args.last().unwrap(), "do it");
    }

    #[test]
    fn sandbox_follows_isolation() {
        assert_eq!(sandbox_mode(Isolation::Readonly), "read-only");
        assert_eq!(sandbox_mode(Isolation::None), "read-only");
        assert_eq!(sandbox_mode(Isolation::Worktree), "workspace-write");

        let mut role = local_role();
        role.isolation = Isolation::Readonly;
        let args = build_args(&role, "x");
        let idx = args.iter().position(|a| a == "--sandbox").unwrap();
        assert_eq!(args[idx + 1], "read-only");
    }

    #[test]
    fn parses_both_nested_and_flat_event_shapes() {
        // Nested under `msg`, as newer Codex releases emit.
        let nested = r#"{"id":"1","msg":{"type":"agent_message","message":"done"}}"#;
        assert!(matches!(&parse_line("r1", nested).unwrap()[0],
            HarnessEvent::AssistantText { text, .. } if text == "done"));

        // Flattened, as older ones do.
        let flat = r#"{"type":"agent_message","message":"done"}"#;
        assert!(matches!(&parse_line("r1", flat).unwrap()[0],
            HarnessEvent::AssistantText { text, .. } if text == "done"));
    }

    #[test]
    fn command_events_become_tool_calls_and_results() {
        let begin = r#"{"msg":{"type":"exec_command_begin","call_id":"c1","command":["cargo","test"]}}"#;
        match &parse_line("r1", begin).unwrap()[0] {
            HarnessEvent::ToolCall { name, input, tool_use_id, .. } => {
                assert_eq!(name, "Bash");
                assert_eq!(tool_use_id, "c1");
                assert_eq!(input["command"], "cargo test");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }

        let end = r#"{"msg":{"type":"exec_command_end","call_id":"c1","exit_code":1,"stdout":"boom"}}"#;
        assert!(matches!(&parse_line("r1", end).unwrap()[0],
            HarnessEvent::ToolResult { content, is_error: true, .. } if content == "boom"));
    }

    #[test]
    fn token_counts_carry_no_dollar_figure() {
        let line = r#"{"msg":{"type":"token_count","input_tokens":100,"output_tokens":50,"cached_input_tokens":20}}"#;
        match &parse_line("r1", line).unwrap()[0] {
            HarnessEvent::RunFinished { usage, cost_usd, .. } => {
                assert_eq!(usage.input_tokens, 100);
                assert_eq!(usage.cache_read_input_tokens, 20);
                // Subscription-covered: a dollar figure here would be fiction.
                assert_eq!(*cost_usd, None);
            }
            other => panic!("expected RunFinished, got {other:?}"),
        }
    }

    #[test]
    fn unknown_lines_are_ignored() {
        assert!(parse_line("r1", r#"{"msg":{"type":"future_event"}}"#).unwrap().is_empty());
        assert!(parse_line("r1", "").unwrap().is_empty());
        assert!(parse_line("r1", "{oops").is_err());
    }
}
