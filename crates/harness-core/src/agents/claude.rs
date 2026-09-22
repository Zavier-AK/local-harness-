//! The `claude` CLI backend.
//!
//! Two shapes are built here:
//!
//! * [`run`] — a one-shot worker. Spawn, stream, collect, exit.
//! * [`ClaudeSession`] — the long-lived orchestrator. One process, many turns, fed JSONL
//!   on stdin via `--input-format stream-json`. This is what keeps the ~34k-token context
//!   floor a once-per-session cost instead of a once-per-message one, and it is the reason
//!   the head chat is cheap enough to leave running.

use anyhow::{Context, Result};
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc::UnboundedReceiver;

use super::claude_stream::{parse_line, ResultSummary};
use super::{compose_prompt, scrub_api_keys, EventSink, RunOutcome, WorkerSpec};
use crate::event::HarnessEvent;
use crate::roles::Role;

/// Shared argument construction for workers and the orchestrator alike.
fn base_args(role: &Role, streaming_input: bool) -> Vec<String> {
    let mut args = vec!["-p".to_string()];

    args.push("--output-format".into());
    args.push("stream-json".into());
    // stream-json output requires --verbose.
    args.push("--verbose".into());

    if streaming_input {
        args.push("--input-format".into());
        args.push("stream-json".into());
        args.push("--include-partial-messages".into());
    }

    if let Some(model) = &role.model {
        args.push("--model".into());
        args.push(model.clone());
    }

    let tools = role.effective_tools();
    if !tools.is_empty() {
        args.push("--allowedTools".into());
        args.push(tools.join(","));
    }

    // Stated explicitly even when the allow-list already omits them: a backend that
    // enables a tool by default must still be refused.
    let denied = role.denied_tools();
    if !denied.is_empty() {
        args.push("--disallowedTools".into());
        args.push(denied.join(","));
    }

    if let Some(mode) = &role.permission_mode {
        args.push("--permission-mode".into());
        args.push(mode.clone());
    }

    if let Some(max_turns) = role.max_turns {
        args.push("--max-turns".into());
        args.push(max_turns.to_string());
    }

    // Nobody is at a terminal to answer a prompt; anything unresolved should be denied
    // and reported rather than hanging the run.
    args.push("--permission-prompts".into());
    args.push("none".into());

    args
}

fn session_args(
    role: &Role,
    mcp_config: Option<&str>,
    append_system_prompt: Option<&str>,
    resume_session_id: Option<&str>,
) -> Vec<String> {
    let mut args = base_args(role, true);

    if let Some(session_id) = resume_session_id {
        args.push("--resume".into());
        args.push(session_id.into());
    }
    if let Some(config) = mcp_config {
        args.push("--mcp-config".into());
        args.push(config.into());
    }
    if let Some(prompt) = append_system_prompt {
        args.push("--append-system-prompt".into());
        args.push(prompt.into());
    }
    args
}

fn spawn(cwd: &Path, args: &[String]) -> Result<Child> {
    let mut cmd = Command::new("claude");
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    scrub_api_keys(&mut cmd);

    cmd.spawn()
        .context("spawning `claude` — is the CLI installed and on PATH?")
}

/// Drain a child's stderr into the tracing log so failures are diagnosable.
fn log_stderr(child: &mut Child, run_id: String) {
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::warn!(run_id = %run_id, "claude stderr: {line}");
            }
        });
    }
}

/// Read stdout to EOF, emitting events and returning the terminal `result` summary.
async fn pump_stdout(
    child: &mut Child,
    run_id: &str,
    sink: &EventSink,
) -> Result<Option<ResultSummary>> {
    let stdout = child.stdout.take().context("claude child has no stdout")?;
    let mut lines = BufReader::new(stdout).lines();
    let mut summary = None;

    while let Some(line) = lines.next_line().await? {
        match parse_line(run_id, &line) {
            Ok(parsed) => {
                for event in parsed.events {
                    let _ = sink.send(event);
                }
                if let Some(result) = parsed.result {
                    summary = Some(result);
                }
            }
            // Schema drift should degrade the transcript, not kill the run.
            Err(err) => tracing::warn!(run_id = %run_id, "{err}"),
        }
    }

    Ok(summary)
}

/// Run a single-shot Claude worker.
pub async fn run(spec: &WorkerSpec, sink: &EventSink) -> Result<RunOutcome> {
    let mut args = base_args(&spec.role, false);
    args.push(compose_prompt(spec));

    let mut child = spawn(&spec.cwd, &args)?;
    log_stderr(&mut child, spec.run_id.clone());

    // Nothing is written to a one-shot worker's stdin; close it so the CLI does not wait.
    drop(child.stdin.take());

    let summary = pump_stdout(&mut child, &spec.run_id, sink).await?;
    let status = child.wait().await?;

    match summary {
        Some(result) => Ok(RunOutcome {
            text: result.text,
            usage: result.usage,
            cost_usd: result.cost_usd,
            backend_session_id: result.backend_session_id,
            is_error: result.is_error,
            cancelled: false,
        }),
        // No result line at all: the CLI died before finishing a turn.
        None => Ok(RunOutcome {
            text: format!("claude exited with {status} before producing a result"),
            is_error: true,
            ..Default::default()
        }),
    }
}

/// A long-lived `claude -p` process in streaming-input mode.
///
/// The process stays up across turns, so the system prompt, tool definitions and CLAUDE.md
/// are paid for once. Follow-up turns read that context from cache instead of rebuilding it.
pub struct ClaudeSession {
    child: Child,
    stdin: ChildStdin,
    /// Correlates control requests with their responses.
    next_request: u64,
    pub run_id: String,
    pub backend_session_id: Option<String>,
}

impl ClaudeSession {
    /// Start the orchestrator.
    ///
    /// `mcp_config` is the JSON handed to `--mcp-config`; it is how the delegation tools
    /// reach the head agent. `append_system_prompt` carries the orchestrator brief.
    pub async fn start(
        run_id: impl Into<String>,
        cwd: &Path,
        role: &Role,
        mcp_config: Option<&str>,
        append_system_prompt: Option<&str>,
        resume_session_id: Option<&str>,
        extra_args: &[String],
    ) -> Result<(Self, UnboundedReceiver<HarnessEvent>)> {
        let run_id = run_id.into();
        let mut args = session_args(role, mcp_config, append_system_prompt, resume_session_id);
        args.extend(extra_args.iter().cloned());

        let mut child = spawn(cwd, &args)?;
        log_stderr(&mut child, run_id.clone());

        let stdin = child.stdin.take().context("claude child has no stdin")?;
        let stdout = child.stdout.take().context("claude child has no stdout")?;

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let pump_run_id = run_id.clone();

        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                match parse_line(&pump_run_id, &line) {
                    Ok(parsed) => {
                        for event in parsed.events {
                            if tx.send(event).is_err() {
                                return; // receiver dropped; session is being torn down
                            }
                        }
                    }
                    Err(err) => tracing::warn!(run_id = %pump_run_id, "{err}"),
                }
            }
        });

        Ok((
            Self {
                child,
                stdin,
                next_request: 0,
                run_id,
                backend_session_id: None,
            },
            rx,
        ))
    }

    /// Queue a user turn. Returns as soon as the line is written; the reply arrives on the
    /// event stream.
    /// Stop the turn in progress, keeping the process and its conversation.
    ///
    /// Uses the CLI's stream-json control protocol. Verified against the real CLI: the
    /// turn ends within a fraction of a second with a `result` whose subtype is
    /// `error_during_execution`, and the same process answers the next turn normally —
    /// so a stop costs neither the conversation nor its cached context.
    pub async fn interrupt(&mut self) -> Result<()> {
        self.next_request += 1;
        let message = serde_json::json!({
            "type": "control_request",
            "request_id": format!("interrupt-{}", self.next_request),
            "request": { "subtype": "interrupt" },
        });
        let mut line = serde_json::to_string(&message)?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .context("writing an interrupt to the orchestrator — has the process exited?")?;
        self.stdin.flush().await?;
        Ok(())
    }

    pub async fn send(&mut self, text: &str) -> Result<()> {
        let message = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": text },
            "parent_tool_use_id": null,
        });

        let mut line = serde_json::to_string(&message)?;
        line.push('\n');

        self.stdin
            .write_all(line.as_bytes())
            .await
            .context("writing to the orchestrator's stdin — has the process exited?")?;
        self.stdin.flush().await?;
        Ok(())
    }

    /// Close stdin and wait for the process to drain.
    pub async fn shutdown(mut self) -> Result<()> {
        drop(self.stdin);
        // SIGTERM would leave the in-flight turn unfinished; closing stdin lets it settle.
        let _ = self.child.wait().await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roles::{Isolation, Provider};

    fn role(isolation: Isolation, tools: &[&str]) -> Role {
        Role {
            provider: Provider::Claude,
            model: Some("sonnet".into()),
            isolation,
            tools: tools.iter().map(|s| s.to_string()).collect(),
            brief: None,
            permission_mode: Some("acceptEdits".into()),
            base_url: None,
            provider_opts: Default::default(),
            fallback_role: None,
            max_turns: Some(30),
        }
    }

    fn arg_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        let idx = args.iter().position(|a| a == flag)?;
        args.get(idx + 1).map(String::as_str)
    }

    #[test]
    fn worker_args_carry_model_tools_and_rails() {
        let args = base_args(&role(Isolation::Worktree, &["Read", "Edit"]), false);
        assert_eq!(arg_value(&args, "--model"), Some("sonnet"));
        assert_eq!(arg_value(&args, "--allowedTools"), Some("Read,Edit"));
        assert_eq!(arg_value(&args, "--permission-mode"), Some("acceptEdits"));
        assert_eq!(arg_value(&args, "--max-turns"), Some("30"));
        assert_eq!(arg_value(&args, "--output-format"), Some("stream-json"));
        // Unattended: never block on a prompt nobody can answer.
        assert_eq!(arg_value(&args, "--permission-prompts"), Some("none"));
        // One-shot workers are not fed on stdin.
        assert!(!args.iter().any(|a| a == "--input-format"));
    }

    #[test]
    fn readonly_roles_both_lose_and_are_denied_edit_tools() {
        let args = base_args(&role(Isolation::Readonly, &["Read", "Edit", "Bash"]), false);
        assert_eq!(arg_value(&args, "--allowedTools"), Some("Read,Bash"));

        let denied = arg_value(&args, "--disallowedTools").unwrap();
        assert!(denied.contains("Edit"));
        assert!(denied.contains("Write"));
    }

    #[test]
    fn orchestrator_args_enable_streaming_input() {
        let args = base_args(&role(Isolation::Readonly, &["Read"]), true);
        assert_eq!(arg_value(&args, "--input-format"), Some("stream-json"));
        assert!(args.iter().any(|a| a == "--include-partial-messages"));
    }

    #[test]
    fn orchestrator_resumes_only_the_requested_session() {
        let role = role(Isolation::Readonly, &["Read"]);
        let args = session_args(
            &role,
            Some(r#"{"mcpServers":{}}"#),
            Some("delegate work"),
            Some("harness-session-123"),
        );
        assert_eq!(arg_value(&args, "--resume"), Some("harness-session-123"));
        assert!(!args.iter().any(|arg| arg == "--continue" || arg == "-c"));
        assert_eq!(
            arg_value(&args, "--mcp-config"),
            Some(r#"{"mcpServers":{}}"#)
        );
    }

    #[test]
    fn never_passes_bare_which_would_force_an_api_key() {
        // `--bare` skips OAuth and the keychain entirely, which defeats the whole premise.
        for streaming in [true, false] {
            let args = base_args(&role(Isolation::Worktree, &["Read"]), streaming);
            assert!(!args.iter().any(|a| a == "--bare"));
        }
    }

    #[tokio::test]
    async fn spawned_children_have_api_keys_removed() {
        // Prove the scrubbing reaches the child process, not just the Command.
        //
        // The key is set on the command rather than on this process: mutating the
        // environment here would be visible to every other test running concurrently.
        // Setting it explicitly is also the stronger assertion — scrubbing has to beat
        // an explicit value, not merely fail to pass one along.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "printenv ANTHROPIC_API_KEY || echo ABSENT"])
            .env("ANTHROPIC_API_KEY", "sk-should-not-survive")
            .stdout(Stdio::piped());
        scrub_api_keys(&mut cmd);

        let out = cmd.output().await.unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ABSENT");
    }
}
