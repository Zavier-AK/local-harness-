//! Agent backends.
//!
//! Dispatch is a plain enum rather than a trait object: the set of backends is closed and
//! small, and this keeps the whole layer free of `async_trait` indirection.

pub mod claude;
pub mod claude_stream;
pub mod codex;
pub mod mock;
pub mod openai_compat;

use anyhow::Result;
use std::path::PathBuf;
use tokio::sync::mpsc::UnboundedSender;

use crate::event::{HarnessEvent, Usage};
use crate::roles::{Provider, Role};

pub type EventSink = UnboundedSender<HarnessEvent>;

/// One unit of delegated work.
#[derive(Debug, Clone)]
pub struct WorkerSpec {
    pub run_id: String,
    pub role_name: String,
    pub role: Role,
    pub task: String,
    /// Already prepared by the isolation layer — a worktree, the project root, or nothing.
    pub cwd: PathBuf,
    /// Files the orchestrator wants the worker to look at first.
    pub context_files: Vec<String>,
}

#[derive(Debug, Default, Clone)]
pub struct RunOutcome {
    pub text: String,
    pub usage: Usage,
    pub cost_usd: Option<f64>,
    pub backend_session_id: Option<String>,
    pub is_error: bool,
}

/// Run a worker to completion, streaming events as they arrive.
pub async fn run_worker(spec: &WorkerSpec, sink: &EventSink) -> Result<RunOutcome> {
    match spec.role.provider {
        Provider::Claude => claude::run(spec, sink).await,
        Provider::Codex => codex::run(spec, sink).await,
        Provider::OpenaiCompat => openai_compat::run(spec, sink).await,
        Provider::Mock => mock::run(spec, sink).await,
    }
}

/// The prompt handed to a worker: its role brief, the files it was pointed at, the task.
pub fn compose_prompt(spec: &WorkerSpec) -> String {
    let mut parts = Vec::new();

    if let Some(brief) = &spec.role.brief {
        parts.push(brief.clone());
    }

    if !spec.context_files.is_empty() {
        parts.push(format!(
            "Relevant files:\n{}",
            spec.context_files
                .iter()
                .map(|f| format!("- {f}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }

    parts.push(spec.task.clone());
    parts.join("\n\n")
}

/// Remove API-key credentials from a child's environment.
///
/// This is load-bearing, not hygiene. With `ANTHROPIC_API_KEY` set, the CLI bills the API;
/// without it, the CLI falls back to the OAuth token from `claude /login` and the call is
/// covered by the subscription. Same reasoning for the Codex/OpenAI keys.
pub fn scrub_api_keys(cmd: &mut tokio::process::Command) {
    for key in [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "CLAUDE_API_KEY",
        "OPENAI_API_KEY",
        "CODEX_API_KEY",
    ] {
        cmd.env_remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roles::{Isolation, Provider};

    fn spec_with(brief: Option<&str>, files: &[&str]) -> WorkerSpec {
        WorkerSpec {
            run_id: "r1".into(),
            role_name: "builder".into(),
            role: Role {
                provider: Provider::Mock,
                model: None,
                isolation: Isolation::None,
                tools: vec![],
                brief: brief.map(str::to_string),
                permission_mode: None,
                base_url: None,
                provider_opts: Default::default(),
                fallback_role: None,
                max_turns: None,
            },
            task: "Do the thing.".into(),
            cwd: PathBuf::from("/tmp"),
            context_files: files.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn prompt_orders_brief_then_files_then_task() {
        let prompt = compose_prompt(&spec_with(Some("You are a builder."), &["src/a.rs", "src/b.rs"]));
        assert_eq!(
            prompt,
            "You are a builder.\n\nRelevant files:\n- src/a.rs\n- src/b.rs\n\nDo the thing."
        );
    }

    #[test]
    fn prompt_omits_empty_sections() {
        assert_eq!(compose_prompt(&spec_with(None, &[])), "Do the thing.");
    }

    #[test]
    fn scrubbing_removes_every_api_key_variable() {
        // Verified behaviourally in the claude backend's test; here we just assert the
        // list stays in sync with the doc comment's claim.
        let mut cmd = tokio::process::Command::new("true");
        scrub_api_keys(&mut cmd);
        let removed: Vec<_> = cmd
            .as_std()
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_string_lossy().to_string())
            .collect();
        assert!(removed.contains(&"ANTHROPIC_API_KEY".to_string()));
        assert!(removed.contains(&"OPENAI_API_KEY".to_string()));
        assert_eq!(removed.len(), 5);
    }
}
