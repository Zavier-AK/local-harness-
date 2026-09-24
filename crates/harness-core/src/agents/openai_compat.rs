//! Raw OpenAI-compatible chat completions — Ollama (`:11434/v1`) and LM Studio
//! (`:1234/v1`) both speak this.
//!
//! No tool loop and no filesystem: this backend is for work that is pure text in, text
//! out (summarize, classify, draft, second-opinion). Anything that needs tools goes
//! through the Codex backend's local provider instead, which supplies an agent loop we
//! would otherwise have to write.

use anyhow::{bail, Context, Result};
use serde_json::Value;

use super::{compose_prompt, EventSink, RunOutcome, WorkerSpec};
use crate::event::{HarnessEvent, Usage};

pub async fn run(spec: &WorkerSpec, sink: &EventSink) -> Result<RunOutcome> {
    let base_url = spec
        .role
        .base_url
        .as_deref()
        .context("openai_compat role has no base_url")?;

    let model = spec
        .role
        .model
        .as_deref()
        .context("openai_compat role has no model")?;

    let endpoint = format!("{}/chat/completions", base_url.trim_end_matches('/'));

    let body = serde_json::json!({
        "model": model,
        "messages": [{ "role": "user", "content": compose_prompt(spec) }],
        "stream": false,
    });

    let response = reqwest::Client::new()
        .post(&endpoint)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {endpoint} — is the local model server running?"))?;

    let status = response.status();
    let payload: Value = response
        .json()
        .await
        .context("decoding the local model's response")?;

    if !status.is_success() {
        let message = payload
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("local model server returned an error")
            .to_string();
        let _ = sink.send(HarnessEvent::Error {
            run_id: spec.run_id.clone(),
            message: message.clone(),
        });
        bail!("{endpoint} returned {status}: {message}");
    }

    let text = payload
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let usage = payload
        .get("usage")
        .map(|u| Usage {
            input_tokens: u.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0),
            output_tokens: u
                .get("completion_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            ..Default::default()
        })
        .unwrap_or_default();

    let _ = sink.send(HarnessEvent::AssistantText {
        run_id: spec.run_id.clone(),
        text: text.clone(),
        partial: false,
    });

    let _ = sink.send(HarnessEvent::RunFinished {
        run_id: spec.run_id.clone(),
        text: text.clone(),
        usage,
        // Local inference is free; a dollar figure here would be noise in the meter.
        cost_usd: None,
        is_error: false,
    });

    Ok(RunOutcome {
        text,
        usage,
        cost_usd: None,
        backend_session_id: None,
        is_error: false,
        cancelled: false,
    })
}
