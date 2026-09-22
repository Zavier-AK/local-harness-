//! A deterministic stand-in, so the orchestration loop — delegation, isolation, the event
//! stream, the store — is testable without a network or a logged-in CLI.
//!
//! The task string doubles as a control channel: a task beginning with `FAIL:` produces a
//! failed run, and one beginning with `WRITE:<path>:<contents>` writes a file, which is how
//! the isolation tests prove a worker's edits land in its worktree and nowhere else.
//! `SLOW:<path>:<contents>` writes the file and then does not finish, which is how the
//! cancellation tests prove a stopped worker keeps what it had already written.

use anyhow::Result;

use super::{compose_prompt, EventSink, RunOutcome, WorkerSpec};
use crate::event::{HarnessEvent, Usage};

pub async fn run(spec: &WorkerSpec, sink: &EventSink) -> Result<RunOutcome> {
    let prompt = compose_prompt(spec);

    let _ = sink.send(HarnessEvent::SessionStarted {
        run_id: spec.run_id.clone(),
        backend_session_id: Some(format!("mock-{}", spec.run_id)),
        provider: Some("mock".into()),
        model: spec.role.model.clone(),
        tools: spec.role.effective_tools(),
        mcp_servers: Vec::new(),
    });

    let mut is_error = false;

    let text = if let Some(rest) = spec.task.strip_prefix("FAIL:") {
        is_error = true;
        rest.trim().to_string()
    } else if let Some(rest) = spec.task.strip_prefix("SLOW:") {
        let (path, contents) = rest.split_once(':').unwrap_or((rest, "partial"));
        let target = spec.cwd.join(path.trim());
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&target, contents).await?;
        // Long enough that only a stop ends it within a test.
        tokio::time::sleep(std::time::Duration::from_secs(600)).await;
        "finished slowly".to_string()
    } else if let Some(rest) = spec.task.strip_prefix("WRITE:") {
        let (path, contents) = rest.split_once(':').unwrap_or((rest, "mock contents"));
        let target = spec.cwd.join(path.trim());

        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&target, contents).await?;

        let _ = sink.send(HarnessEvent::ToolCall {
            run_id: spec.run_id.clone(),
            tool_use_id: "mock-write".into(),
            name: "Write".into(),
            input: serde_json::json!({ "path": path.trim() }),
        });

        format!("wrote {}", target.display())
    } else {
        format!("[{}] {}", spec.role_name, prompt)
    };

    let usage = Usage {
        input_tokens: prompt.len() as u64,
        output_tokens: text.len() as u64,
        ..Default::default()
    };

    let _ = sink.send(HarnessEvent::AssistantText {
        run_id: spec.run_id.clone(),
        text: text.clone(),
        partial: false,
    });

    let _ = sink.send(HarnessEvent::RunFinished {
        run_id: spec.run_id.clone(),
        text: text.clone(),
        usage,
        cost_usd: None,
        is_error,
    });

    Ok(RunOutcome {
        text,
        usage,
        cost_usd: None,
        backend_session_id: Some(format!("mock-{}", spec.run_id)),
        is_error,
        cancelled: false,
    })
}
