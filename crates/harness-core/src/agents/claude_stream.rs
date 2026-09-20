//! Translating the Claude CLI's `--output-format stream-json` into harness events.
//!
//! Kept separate from process handling so it can be tested against recorded fixtures.
//! The CLI's schema is not a stable contract, so this parser is deliberately lenient:
//! anything it does not recognize yields no events rather than an error, and a
//! malformed line is reported without killing the run.

use serde_json::Value;

use crate::event::{HarnessEvent, Usage};

/// What a `result` line told us, over and above the events it produced.
#[derive(Debug, Default, Clone)]
pub struct ResultSummary {
    pub text: String,
    pub usage: Usage,
    pub cost_usd: Option<f64>,
    pub is_error: bool,
    pub backend_session_id: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct ParsedLine {
    pub events: Vec<HarnessEvent>,
    /// Set only on the terminal `result` line.
    pub result: Option<ResultSummary>,
    /// Set on `system/init`; the id to hand `--resume` if the process dies.
    pub backend_session_id: Option<String>,
}

fn usage_from(value: &Value) -> Usage {
    serde_json::from_value(value.clone()).unwrap_or_default()
}

/// The CLI reports tool results as either a bare string or a list of content blocks.
fn flatten_content(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| {
                item.get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| item.as_str().map(str::to_string))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Parse one line of the stream. `run_id` tags the events for UI routing.
///
/// Messages carrying a non-null `parent_tool_use_id` come from a subagent nested inside
/// this run; they are tagged with that id so the UI can nest them rather than
/// interleaving them into the parent transcript.
pub fn parse_line(run_id: &str, line: &str) -> Result<ParsedLine, String> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(ParsedLine::default());
    }

    let value: Value =
        serde_json::from_str(line).map_err(|e| format!("unparseable stream line: {e}"))?;

    let mut out = ParsedLine::default();
    let msg_type = value.get("type").and_then(Value::as_str).unwrap_or_default();

    // Nested subagent output gets its own stream key.
    let key = value
        .get("parent_tool_use_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| run_id.to_string());

    match msg_type {
        "system" => {
            let subtype = value.get("subtype").and_then(Value::as_str).unwrap_or_default();
            match subtype {
                "init" => {
                    let session_id = value.get("session_id").and_then(Value::as_str).map(str::to_string);
                    out.backend_session_id = session_id.clone();
                    out.events.push(HarnessEvent::SessionStarted {
                        run_id: key,
                        backend_session_id: session_id,
                        // Where the CLI reports it, this distinguishes subscription OAuth
                        // (`firstParty`) from API-key billing — the whole premise, observable.
                        provider: value.get("provider").and_then(Value::as_str).map(str::to_string),
                        model: value.get("model").and_then(Value::as_str).map(str::to_string),
                        tools: value
                            .get("tools")
                            .and_then(Value::as_array)
                            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                            .unwrap_or_default(),
                        mcp_servers: value
                            .get("mcp_servers")
                            .and_then(Value::as_array)
                            .map(|a| {
                                a.iter()
                                    .filter_map(|s| s.get("name").and_then(Value::as_str))
                                    .map(str::to_string)
                                    .collect()
                            })
                            .unwrap_or_default(),
                    });
                }
                "api_retry" => {
                    out.events.push(HarnessEvent::ApiRetry {
                        run_id: key,
                        attempt: value.get("attempt").and_then(Value::as_u64).unwrap_or(0) as u32,
                        max_retries: value.get("max_retries").and_then(Value::as_u64).unwrap_or(0) as u32,
                        retry_delay_ms: value.get("retry_delay_ms").and_then(Value::as_u64).unwrap_or(0),
                        error: value
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                            .to_string(),
                    });
                }
                // Other system subtypes (plugin_install, hook_*, permission_denied) carry no
                // state this engine acts on yet.
                _ => {}
            }
        }

        "assistant" => {
            let blocks = value
                .pointer("/message/content")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();

            for block in blocks {
                match block.get("type").and_then(Value::as_str).unwrap_or_default() {
                    "text" => {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            out.events.push(HarnessEvent::AssistantText {
                                run_id: key.clone(),
                                text: text.to_string(),
                                partial: false,
                            });
                        }
                    }
                    "thinking" => {
                        if let Some(text) = block.get("thinking").and_then(Value::as_str) {
                            out.events.push(HarnessEvent::Thinking {
                                run_id: key.clone(),
                                text: text.to_string(),
                            });
                        }
                    }
                    "tool_use" => {
                        out.events.push(HarnessEvent::ToolCall {
                            run_id: key.clone(),
                            tool_use_id: block
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            name: block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            input: block.get("input").cloned().unwrap_or(Value::Null),
                        });
                    }
                    _ => {}
                }
            }

            if let Some(usage) = value.pointer("/message/usage") {
                let usage = usage_from(usage);
                if usage.total_input() > 0 || usage.output_tokens > 0 {
                    // Carried on the result line too; recorded there to avoid double counting.
                    tracing::trace!(?usage, "assistant usage");
                }
            }
        }

        "user" => {
            let blocks = value
                .pointer("/message/content")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();

            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                    out.events.push(HarnessEvent::ToolResult {
                        run_id: key.clone(),
                        tool_use_id: block
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        content: flatten_content(block.get("content").unwrap_or(&Value::Null)),
                        is_error: block.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                    });
                }
            }
        }

        "stream_event" => {
            // Token-level deltas, only present with --include-partial-messages.
            if value.pointer("/event/delta/type").and_then(Value::as_str) == Some("text_delta") {
                if let Some(text) = value.pointer("/event/delta/text").and_then(Value::as_str) {
                    out.events.push(HarnessEvent::AssistantText {
                        run_id: key,
                        text: text.to_string(),
                        partial: true,
                    });
                }
            }
        }

        "result" => {
            let is_error = value
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or_else(|| {
                    value.get("subtype").and_then(Value::as_str).unwrap_or("success") != "success"
                });

            let summary = ResultSummary {
                text: value
                    .get("result")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                usage: value.get("usage").map(usage_from).unwrap_or_default(),
                cost_usd: value.get("total_cost_usd").and_then(Value::as_f64),
                is_error,
                backend_session_id: value
                    .get("session_id")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            };

            out.events.push(HarnessEvent::RunFinished {
                run_id: key,
                text: summary.text.clone(),
                usage: summary.usage,
                cost_usd: summary.cost_usd,
                is_error: summary.is_error,
            });
            out.result = Some(summary);
        }

        _ => {}
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_line_exposes_the_auth_provider() {
        let line = r#"{"type":"system","subtype":"init","session_id":"sess-1","model":"claude-sonnet-5",
            "provider":"firstParty","tools":["Read","Grep"],
            "mcp_servers":[{"name":"harness","status":"connected"}]}"#;

        let parsed = parse_line("run-1", line).unwrap();
        assert_eq!(parsed.backend_session_id.as_deref(), Some("sess-1"));

        match &parsed.events[0] {
            HarnessEvent::SessionStarted { provider, model, tools, mcp_servers, .. } => {
                // The observable proof that the subscription, not an API key, is paying.
                assert_eq!(provider.as_deref(), Some("firstParty"));
                assert_eq!(model.as_deref(), Some("claude-sonnet-5"));
                assert_eq!(tools, &["Read", "Grep"]);
                assert_eq!(mcp_servers, &["harness"]);
            }
            other => panic!("expected SessionStarted, got {other:?}"),
        }
    }

    #[test]
    fn assistant_line_yields_text_and_tool_calls_in_order() {
        let line = r#"{"type":"assistant","parent_tool_use_id":null,"message":{"content":[
            {"type":"text","text":"Delegating."},
            {"type":"tool_use","id":"tu_1","name":"mcp__harness__delegate","input":{"role":"builder"}}
        ]}}"#;

        let parsed = parse_line("run-1", line).unwrap();
        assert_eq!(parsed.events.len(), 2);

        assert!(matches!(&parsed.events[0],
            HarnessEvent::AssistantText { text, partial: false, .. } if text == "Delegating."));

        match &parsed.events[1] {
            HarnessEvent::ToolCall { tool_use_id, name, input, run_id } => {
                assert_eq!(run_id, "run-1");
                assert_eq!(tool_use_id, "tu_1");
                assert_eq!(name, "mcp__harness__delegate");
                assert_eq!(input["role"], "builder");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn subagent_messages_are_keyed_to_their_parent_tool_call() {
        let line = r#"{"type":"assistant","parent_tool_use_id":"tu_9","message":{"content":[
            {"type":"text","text":"nested"}]}}"#;

        let parsed = parse_line("run-1", line).unwrap();
        // Not the parent run: the UI needs to nest this, not interleave it.
        assert_eq!(parsed.events[0].stream_key(), "tu_9");
    }

    #[test]
    fn tool_results_flatten_both_string_and_block_content() {
        let as_string = r#"{"type":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":"tu_1","content":"done"}]}}"#;
        let parsed = parse_line("run-1", as_string).unwrap();
        assert!(matches!(&parsed.events[0],
            HarnessEvent::ToolResult { content, is_error: false, .. } if content == "done"));

        let as_blocks = r#"{"type":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":"tu_2","is_error":true,
             "content":[{"type":"text","text":"line one"},{"type":"text","text":"line two"}]}]}}"#;
        let parsed = parse_line("run-1", as_blocks).unwrap();
        assert!(matches!(&parsed.events[0],
            HarnessEvent::ToolResult { content, is_error: true, .. } if content == "line one\nline two"));
    }

    #[test]
    fn partial_text_deltas_are_marked_partial() {
        let line = r#"{"type":"stream_event","event":{"type":"content_block_delta",
            "delta":{"type":"text_delta","text":"tok"}}}"#;
        let parsed = parse_line("run-1", line).unwrap();
        assert!(matches!(&parsed.events[0],
            HarnessEvent::AssistantText { text, partial: true, .. } if text == "tok"));
    }

    #[test]
    fn rate_limit_retries_surface_for_backpressure() {
        let line = r#"{"type":"system","subtype":"api_retry","attempt":2,"max_retries":5,
            "retry_delay_ms":4000,"error":"rate_limit","session_id":"s"}"#;

        let parsed = parse_line("run-1", line).unwrap();
        match &parsed.events[0] {
            HarnessEvent::ApiRetry { attempt, max_retries, retry_delay_ms, error, .. } => {
                assert_eq!((*attempt, *max_retries, *retry_delay_ms), (2, 5, 4000));
                assert_eq!(error, "rate_limit");
            }
            other => panic!("expected ApiRetry, got {other:?}"),
        }
    }

    #[test]
    fn result_line_captures_the_cache_floor() {
        // The "say hi" numbers from the handoff notes: a ~34k floor for 14 output tokens.
        let line = r#"{"type":"result","subtype":"success","is_error":false,"result":"hi",
            "session_id":"sess-1","total_cost_usd":0.0643,
            "usage":{"input_tokens":4,"output_tokens":14,
                     "cache_creation_input_tokens":15105,"cache_read_input_tokens":18766}}"#;

        let parsed = parse_line("run-1", line).unwrap();
        let result = parsed.result.expect("result summary");
        assert_eq!(result.text, "hi");
        assert!(!result.is_error);
        assert_eq!(result.cost_usd, Some(0.0643));
        assert_eq!(result.usage.cache_creation_input_tokens, 15_105);
        assert_eq!(result.usage.total_input(), 4 + 15_105 + 18_766);
    }

    #[test]
    fn error_results_are_flagged_from_subtype_alone() {
        let line = r#"{"type":"result","subtype":"error_max_turns","result":"","session_id":"s"}"#;
        let parsed = parse_line("run-1", line).unwrap();
        assert!(parsed.result.unwrap().is_error);
    }

    #[test]
    fn unknown_and_blank_lines_are_ignored_not_fatal() {
        assert!(parse_line("run-1", "").unwrap().events.is_empty());
        assert!(parse_line("run-1", r#"{"type":"something_new_in_2027"}"#).unwrap().events.is_empty());
        // Schema drift must not take the run down.
        assert!(parse_line("run-1", r#"{"type":"system","subtype":"init"}"#).is_ok());
    }

    #[test]
    fn malformed_json_reports_rather_than_panics() {
        assert!(parse_line("run-1", "{not json").is_err());
    }
}
