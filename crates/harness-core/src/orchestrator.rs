//! The head agent: one long-lived `claude -p` session with the delegation tools attached.
//!
//! Everything here is in service of one number. A fresh `claude -p` invocation rebuilds
//! its system prompt, tool definitions and `CLAUDE.md` from scratch — on the order of 34k
//! tokens before the first word of the actual prompt. Streaming-input mode keeps one
//! process alive across every turn of a conversation, so that cost is paid once per
//! session and subsequent turns read from cache instead.
//!
//! The orchestrator is also deliberately kept away from editing. It gets `Read`, `Grep`,
//! `Glob` and the delegation tools, and nothing else: a smaller tool set is a smaller
//! system prompt is a lower floor, and the workers are the ones with worktrees.

use anyhow::Result;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::agents::claude::ClaudeSession;
use crate::engine::Harness;
use crate::event::HarnessEvent;
use crate::mcp::{self, McpServer};
use crate::roles::{Isolation, Provider, Role};

/// Tools the head agent gets besides delegation. Enough to orient itself in the repo,
/// not enough to start doing the work itself.
pub const ORCHESTRATOR_TOOLS: &[&str] = &["Read", "Grep", "Glob"];

/// The brief appended to the head agent's system prompt.
pub fn orchestrator_brief(roles: &[crate::engine::RoleInfo]) -> String {
    let fleet = roles
        .iter()
        .map(|r| {
            format!(
                "- {} ({}{}, isolation: {}{}){}",
                r.name,
                r.provider,
                r.model.as_ref().map(|m| format!("/{m}")).unwrap_or_default(),
                r.isolation,
                if r.can_edit_files { ", can edit files" } else { ", read-only" },
                r.brief.as_ref().map(|b| format!(" — {b}")).unwrap_or_default(),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "You are the orchestrator of a local multi-agent harness. You plan and delegate; \
         you do not write code yourself.\n\n\
         Your fleet:\n{fleet}\n\n\
         How to work:\n\
         - Delegate with the `delegate` tool. Prefer delegating over answering from your \
         own context: workers run in their own context windows and cost far less of your \
         budget than doing the work here.\n\
         - Each worker sees only the task text you send it. Write self-contained briefs; \
         it cannot see this conversation.\n\
         - Use `delegate_async` plus `check_workers` when several pieces of work are \
         independent, so they run in parallel.\n\
         - Workers that edit files run in their own git worktree. Their changes are NOT on \
         the user's branch. To land them, call `request_merge` — this only queues the diff \
         for the user to approve. Never tell the user work has landed; tell them it is \
         waiting for their review.\n\
         - Report back concisely: what you delegated, what came back, what needs a decision."
    )
}

/// The head agent's own role definition. Readonly by construction.
pub fn orchestrator_role(model: Option<String>, max_turns: Option<u32>) -> Role {
    let mut tools: Vec<String> = ORCHESTRATOR_TOOLS.iter().map(|s| s.to_string()).collect();
    tools.extend(McpServer::allowed_tool_names());

    Role {
        provider: Provider::Claude,
        model,
        // The head agent proposes; workers dispose. It never needs write access.
        isolation: Isolation::Readonly,
        tools,
        brief: None,
        permission_mode: None,
        base_url: None,
        provider_opts: Default::default(),
        fallback_role: None,
        max_turns,
    }
}

/// A running head chat: the MCP server, the Claude process, and the event stream.
pub struct Orchestrator {
    session: ClaudeSession,
    mcp: McpServer,
    harness: Arc<Harness>,
}

impl Orchestrator {
    /// Start the MCP server and the head agent wired to it.
    pub async fn start(
        harness: Arc<Harness>,
        project_root: &Path,
        model: Option<String>,
        max_turns: Option<u32>,
    ) -> Result<(Self, UnboundedReceiver<HarnessEvent>)> {
        let mcp = mcp::serve(Arc::clone(&harness)).await?;
        let role = orchestrator_role(model, max_turns);
        let brief = orchestrator_brief(&harness.list_roles());

        let run_id = format!("orchestrator-{}", harness.session_id());
        harness.register_orchestrator(&run_id, role.model.as_deref()).await;

        let (session, events) = ClaudeSession::start(
            run_id,
            project_root,
            &role,
            Some(&mcp.claude_mcp_config()),
            Some(&brief),
        )
        .await?;

        Ok((Self { session, mcp, harness }, events))
    }

    pub fn harness(&self) -> &Arc<Harness> {
        &self.harness
    }

    pub fn mcp_url(&self) -> String {
        self.mcp.url()
    }

    /// Queue a user turn. The reply arrives on the event stream.
    pub async fn send(&mut self, text: &str) -> Result<()> {
        self.session.send(text).await
    }

    pub async fn shutdown(self) -> Result<()> {
        self.session.shutdown().await?;
        self.mcp.shutdown().await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::RoleInfo;

    fn fleet() -> Vec<RoleInfo> {
        vec![
            RoleInfo {
                name: "builder".into(),
                provider: "claude".into(),
                model: Some("sonnet".into()),
                isolation: "worktree".into(),
                can_edit_files: true,
                brief: Some("Implements changes.".into()),
            },
            RoleInfo {
                name: "reviewer".into(),
                provider: "codex".into(),
                model: None,
                isolation: "readonly".into(),
                can_edit_files: false,
                brief: None,
            },
        ]
    }

    #[test]
    fn orchestrator_is_readonly_and_cannot_edit() {
        let role = orchestrator_role(Some("opus".into()), Some(50));
        assert_eq!(role.isolation, Isolation::Readonly);

        let tools = role.effective_tools();
        assert!(!tools.iter().any(|t| t == "Edit" || t == "Write"));
        assert!(role.denied_tools().contains(&"Edit".to_string()));
    }

    #[test]
    fn orchestrator_gets_the_delegation_tools() {
        let tools = orchestrator_role(None, None).effective_tools();
        for expected in ["mcp__harness__delegate", "mcp__harness__list_roles", "mcp__harness__request_merge"] {
            assert!(tools.iter().any(|t| t == expected), "missing {expected} in {tools:?}");
        }
    }

    #[test]
    fn tool_surface_stays_small_to_keep_the_context_floor_down() {
        // Three read tools plus six delegation tools. If this grows, the per-session
        // floor grows with it, so the assertion is a tripwire rather than trivia.
        assert_eq!(orchestrator_role(None, None).effective_tools().len(), 9);
    }

    #[test]
    fn brief_lists_the_fleet_with_capabilities() {
        let brief = orchestrator_brief(&fleet());
        assert!(brief.contains("builder (claude/sonnet, isolation: worktree, can edit files)"));
        assert!(brief.contains("reviewer (codex, isolation: readonly, read-only)"));
        assert!(brief.contains("Implements changes."));
    }

    #[test]
    fn brief_tells_the_agent_that_merges_need_a_human() {
        let brief = orchestrator_brief(&fleet());
        assert!(brief.contains("only queues the diff"));
        assert!(brief.contains("Never tell the user work has landed"));
    }
}
