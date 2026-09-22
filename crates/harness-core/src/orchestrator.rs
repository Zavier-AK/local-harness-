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
    orchestrator_brief_with(roles, &[])
}

/// The brief, for a fleet in which `native` roles are Claude Code subagents the head
/// delegates to with its own `Agent` tool rather than through `delegate`.
pub fn orchestrator_brief_with(roles: &[crate::engine::RoleInfo], native: &[String]) -> String {
    // A role whose backend is missing cannot do work, and naming it only invites the
    // head agent to spend a turn discovering that. Probed-unavailable roles are held
    // back; unprobed roles (available: None) are listed, since we do not know otherwise.
    let (usable, unusable): (Vec<_>, Vec<_>) =
        roles.iter().partition(|r| r.available.unwrap_or(true));

    let fleet = usable
        .iter()
        .map(|r| {
            format!(
                "- {} ({}{}, isolation: {}{}){}",
                r.name,
                r.provider,
                r.model
                    .as_ref()
                    .map(|m| format!("/{m}"))
                    .unwrap_or_default(),
                r.isolation,
                if r.can_edit_files {
                    ", can edit files"
                } else {
                    ", read-only"
                },
                r.brief
                    .as_ref()
                    .map(|b| format!(" — {b}"))
                    .unwrap_or_default(),
            ) + if native.contains(&r.name) {
                " [your subagent: use the Agent tool]"
            } else {
                ""
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    let unavailable = if unusable.is_empty() {
        String::new()
    } else {
        format!(
            "\n\nUnavailable right now — do NOT delegate to these, they will fail:\n{}",
            unusable
                .iter()
                .map(|r| format!(
                    "- {} ({})",
                    r.name,
                    r.unavailable_reason
                        .as_deref()
                        .unwrap_or("backend not reachable")
                ))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };

    // Where the head agent's own `Agent` tool is in play, say so first: it is the tool the
    // model already knows best, and the one piece of its behaviour that must be steered is
    // what it does when the work is not where it expects. Tested live: a head agent that
    // could not see a subagent's file in the checkout copied it there with `cp`, routing
    // around the review. It must be told that absence is correct.
    let native_note = if native.is_empty() {
        String::new()
    } else {
        "\n\
         - Roles marked [your subagent] are yours to delegate to with the Agent tool, \
         using the role name as subagent_type. Each works in its own git worktree on its \
         own branch. Its changes will NOT appear in the project checkout — that is \
         correct, not a failure. They are put in front of the user for review \
         automatically. Never copy them into the checkout, never redo them yourself, and \
         do not call `request_merge` for them. Tell the user the work is waiting for their \
         review.\n\
         - For every other role, use the `delegate` tool."
            .to_string()
    };

    format!(
        "You are the orchestrator of a local multi-agent harness. You plan and delegate; \
         you do not write code yourself.\n\n\
         Your fleet:\n{fleet}{unavailable}\n\n\
         How to work:{native_note}\n\
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

/// The head agent's role when some roles are native subagents.
///
/// Differs from [`orchestrator_role`] in two ways, both forced by how Claude Code applies
/// permissions, and both verified against the real CLI:
///
/// * It is allowed the `Agent` tool.
/// * It carries **no session-wide deny list.** `--disallowedTools` binds every subagent
///   in the session too — a builder subagent was refused `Write` with "disabled for this
///   session, in subagents as well as here". The head stays read-only another way: it is
///   approved only these tools, and with nobody to answer a permission prompt, any write
///   it attempts is refused (also verified). Each subagent carries its own permissions.
pub fn orchestrator_role_native(model: Option<String>, max_turns: Option<u32>) -> Role {
    let mut role = orchestrator_role(model, max_turns);
    role.tools.push("Agent".into());
    role.isolation = Isolation::None;
    role
}

/// A running head chat: the MCP server, the Claude process, and the event stream.
pub struct Orchestrator {
    session: ClaudeSession,
    mcp: McpServer,
    harness: Arc<Harness>,
    resumed_backend_session: bool,
}

impl Orchestrator {
    /// Start the MCP server and the head agent wired to it.
    pub async fn start(
        harness: Arc<Harness>,
        project_root: &Path,
        model: Option<String>,
        max_turns: Option<u32>,
        resume_session_id: Option<String>,
        native_hook: Option<Vec<String>>,
    ) -> Result<(Self, UnboundedReceiver<HarnessEvent>)> {
        let mcp = mcp::serve(Arc::clone(&harness)).await?;

        // Native delegation needs a way to run our worktree hook; without one, every role
        // stays on `delegate` exactly as before.
        let registry = harness.registry_snapshot().await;
        let agents = native_hook
            .as_ref()
            .and_then(|_| crate::native::agents_json(&registry));
        let native: Vec<String> = if agents.is_some() {
            crate::native::native_roles(&registry).into_keys().collect()
        } else {
            Vec::new()
        };

        let role = if native.is_empty() {
            orchestrator_role(model, max_turns)
        } else {
            orchestrator_role_native(model, max_turns)
        };
        // Skills reach the head agent and, through it, native subagents. The user's MCP
        // servers are connected but not approved for the head: it stays a planner, and a
        // role that should use one lists its tools.
        let extras = harness.extras_snapshot().await;
        let mut extra_args = Vec::new();
        if let Some(dir) = &extras.plugin_dir {
            extra_args.push("--plugin-dir".into());
            extra_args.push(dir.to_string_lossy().into_owned());
        }
        if let (Some(agents), Some(hook)) = (agents, &native_hook) {
            extra_args.push("--agents".into());
            extra_args.push(agents);
            extra_args.push("--settings".into());
            extra_args.push(crate::native::hook_settings(hook));
        }

        // Probe before briefing, so the head agent never plans around a dead backend.
        let brief = orchestrator_brief_with(&harness.list_roles_probed().await, &native);

        let run_id = format!("orchestrator-{}", harness.session_id());
        harness
            .register_orchestrator(&run_id, role.model.as_deref())
            .await;

        let (session, events) = ClaudeSession::start(
            run_id,
            project_root,
            &role,
            Some(&mcp.claude_mcp_config_with(&extras.mcp_servers)),
            Some(&brief),
            resume_session_id.as_deref(),
            &extra_args,
        )
        .await?;

        Ok((
            Self {
                session,
                mcp,
                harness,
                resumed_backend_session: resume_session_id.is_some(),
            },
            events,
        ))
    }

    pub fn harness(&self) -> &Arc<Harness> {
        &self.harness
    }

    pub fn mcp_url(&self) -> String {
        self.mcp.url()
    }

    pub fn resumed_backend_session(&self) -> bool {
        self.resumed_backend_session
    }

    /// Queue a user turn. The reply arrives on the event stream.
    pub async fn send(&mut self, text: &str) -> Result<()> {
        self.harness
            .record_head_event(HarnessEvent::UserMessage {
                run_id: self.session.run_id.clone(),
                text: text.to_string(),
            })
            .await;
        self.session.send(text).await
    }

    /// Stop the head agent's current turn. The conversation and the process survive,
    /// so the next message carries on from where it was stopped.
    pub async fn interrupt(&mut self) -> Result<()> {
        self.session.interrupt().await?;
        self.harness
            .record_head_event(HarnessEvent::TurnInterrupted {
                run_id: self.session.run_id.clone(),
            })
            .await;
        Ok(())
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
                available: Some(true),
                unavailable_reason: None,
            },
            RoleInfo {
                name: "reviewer".into(),
                provider: "codex".into(),
                model: None,
                isolation: "readonly".into(),
                can_edit_files: false,
                brief: None,
                available: Some(true),
                unavailable_reason: None,
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
        for expected in [
            "mcp__harness__delegate",
            "mcp__harness__list_roles",
            "mcp__harness__request_merge",
        ] {
            assert!(
                tools.iter().any(|t| t == expected),
                "missing {expected} in {tools:?}"
            );
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
    fn unavailable_roles_are_withheld_and_called_out() {
        let mut roles = fleet();
        roles[1].available = Some(false);
        roles[1].unavailable_reason = Some("the `codex` CLI is not on PATH".into());

        let brief = orchestrator_brief(&roles);

        // The usable role is still offered...
        assert!(brief.contains("builder (claude/sonnet"));
        // ...the unusable one is not listed as part of the fleet...
        assert!(!brief.contains("reviewer (codex"));
        // ...but is named explicitly, so the agent does not try to reach for it.
        assert!(brief.contains("do NOT delegate"));
        assert!(brief.contains("reviewer (the `codex` CLI is not on PATH)"));
    }

    #[test]
    fn unprobed_roles_are_treated_as_usable() {
        let mut roles = fleet();
        roles[0].available = None;
        roles[1].available = None;

        let brief = orchestrator_brief(&roles);
        assert!(brief.contains("builder"));
        assert!(brief.contains("reviewer"));
        assert!(!brief.contains("do NOT delegate"));
    }

    #[test]
    fn brief_tells_the_agent_that_merges_need_a_human() {
        let brief = orchestrator_brief(&fleet());
        assert!(brief.contains("only queues the diff"));
        assert!(brief.contains("Never tell the user work has landed"));
    }

    /// With native roles the head may use Claude Code's own `Agent` tool — and must still be
    /// unable to change anything itself, without a session-wide deny list (which would also
    /// bind every subagent it starts).
    #[test]
    fn the_native_head_can_delegate_but_cannot_write() {
        let role = orchestrator_role_native(None, None);
        let allowed = role.effective_tools();
        assert!(allowed.iter().any(|t| t == "Agent"));
        for tool in ["Edit", "Write", "MultiEdit", "NotebookEdit", "Bash"] {
            assert!(
                !allowed.iter().any(|t| t == tool),
                "the head must not be approved {tool}: a live head used Bash `cp` to copy a \
                 subagent's work past the review"
            );
        }
        assert!(
            role.denied_tools().is_empty(),
            "a session-wide deny list would stop builder subagents from writing"
        );
    }

    #[test]
    fn the_brief_steers_native_roles_to_the_agent_tool_and_away_from_the_checkout() {
        let brief = orchestrator_brief_with(&fleet(), &["builder".to_string()]);
        assert!(brief.contains("[your subagent: use the Agent tool]"));
        assert!(brief.contains("will NOT appear in the project checkout"));
        assert!(brief.contains("Never copy them into the checkout"));
        // The legacy brief is unchanged when nothing is native.
        assert!(!orchestrator_brief(&fleet()).contains("Agent tool"));
    }
}
