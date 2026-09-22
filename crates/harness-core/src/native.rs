//! Delegating through Claude Code's own subagents instead of around them.
//!
//! The head agent is a Claude model trained to delegate with Claude Code's built-in
//! `Agent` tool. Hiding that tool and offering a bespoke `delegate` instead meant every
//! Claude worker was a fresh `claude -p` process paying the full context floor, and the
//! model working against its training. So Claude roles that need a worktree are handed to
//! Claude Code as subagent definitions (`--agents`), and the head delegates to them
//! natively. Codex and local roles have no such path — Claude Code's subagents are Claude
//! sessions — so they stay behind the harness's MCP `delegate`, which is the split the
//! Claude Code docs themselves recommend.
//!
//! What stays the harness's: the worktree (via the `WorktreeCreate` hook — see
//! [`crate::hooks`]), the bootstrap and skills inside it, and the human merge gate.

use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

use crate::roles::{Isolation, Provider, Role, RoleRegistry};

/// What every native subagent is told about its situation, after its role's own brief.
///
/// The point that matters most: its work does not appear in the project checkout. In
/// testing, a head agent that could not see a subagent's file in the checkout copied it
/// there with `cp` — routing around the review entirely. The subagent is told the same
/// thing so it does not try to "fix" it either.
const SUBAGENT_PREAMBLE: &str = "You are working in your own git worktree, a private copy \\
of the project on its own branch. Work only inside your current directory. Your changes \\
are reviewed by a person before they reach the project, so they will not appear in the \\
main checkout, and must never be copied there. When you finish, say what you changed and \\
what you verified, concretely.";

/// Claude roles that go through Claude Code's `Agent` tool.
///
/// Only roles that want a worktree (`worktree`, or `readonly`, which gets a throwaway
/// one). `shared` roles stay on `delegate`, because the harness serializes them with a
/// lock a native subagent would not take; `none` has no filesystem to isolate.
pub fn native_roles(registry: &RoleRegistry) -> BTreeMap<String, Role> {
    registry
        .roles
        .iter()
        .filter(|(_, role)| {
            role.provider == Provider::Claude
                && matches!(role.isolation, Isolation::Worktree | Isolation::Readonly)
        })
        .map(|(name, role)| (name.clone(), role.clone()))
        .collect()
}

/// The `--agents` JSON for this fleet, or `None` if no role goes native.
pub fn agents_json(registry: &RoleRegistry) -> Option<String> {
    let roles = native_roles(registry);
    if roles.is_empty() {
        return None;
    }

    let mut agents = Map::new();
    for (name, role) in roles {
        let brief = role.brief.clone().unwrap_or_default();
        let mut agent = Map::new();
        agent.insert(
            "description".into(),
            Value::String(if brief.is_empty() {
                format!("The {name} role.")
            } else {
                brief.clone()
            }),
        );
        agent.insert(
            "prompt".into(),
            Value::String(if brief.is_empty() {
                SUBAGENT_PREAMBLE.to_string()
            } else {
                format!("{brief}\n\n{SUBAGENT_PREAMBLE}")
            }),
        );
        agent.insert("isolation".into(), json!("worktree"));

        let tools = role.effective_tools();
        if !tools.is_empty() {
            agent.insert("tools".into(), json!(tools));
        }
        // Per agent, not per session: a session-wide deny list binds every subagent too
        // ("disabled for this session, in subagents as well as here"), which would stop
        // a builder from writing because the head is read-only.
        let denied = role.denied_tools();
        if !denied.is_empty() {
            agent.insert("disallowedTools".into(), json!(denied));
        }
        if let Some(model) = &role.model {
            agent.insert("model".into(), json!(model));
        }
        if let Some(mode) = &role.permission_mode {
            agent.insert("permissionMode".into(), json!(mode));
        }
        if let Some(turns) = role.max_turns {
            agent.insert("maxTurns".into(), json!(turns));
        }
        agents.insert(name, Value::Object(agent));
    }
    Some(Value::Object(agents).to_string())
}

/// `--settings` JSON registering the harness as Claude Code's worktree creator.
///
/// `hook` is the command that runs a hook, e.g. the app binary plus `__harness-hook`;
/// `worktree-create` / `worktree-remove` are appended.
pub fn hook_settings(hook: &[String]) -> String {
    let command = |event: &str| {
        let mut parts: Vec<String> = hook.iter().map(|part| shell_quote(part)).collect();
        parts.push(event.to_string());
        parts.join(" ")
    };
    json!({
        "hooks": {
            "WorktreeCreate": [{ "hooks": [{ "type": "command", "command": command("worktree-create") }] }],
            "WorktreeRemove": [{ "hooks": [{ "type": "command", "command": command("worktree-remove") }] }],
        }
    })
    .to_string()
}

/// Quote one argument for `sh`. Hook commands run through a shell, and an app installed
/// under a path with a space in it must still be found.
fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', r"'\''"))
}

/// The worker id the harness uses for a native subagent: the same name its worktree and
/// branch got from the hook, so everything lines up without a lookup table.
pub fn worker_id(task_id: &str) -> String {
    format!("agent-{task_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fleet() -> RoleRegistry {
        RoleRegistry::from_toml(
            r#"
[roles.builder]
provider = "claude"
model = "sonnet"
isolation = "worktree"
tools = ["Read", "Edit", "Write", "Bash"]
permission_mode = "acceptEdits"
brief = "Implement the change."
max_turns = 60

[roles.architect]
provider = "claude"
isolation = "readonly"
tools = ["Read", "Grep"]

[roles.pair]
provider = "claude"
isolation = "shared"

[roles.reviewer]
provider = "codex"
isolation = "readonly"

[roles.local]
provider = "openai_compat"
base_url = "http://localhost:1234/v1"
isolation = "none"
"#,
        )
        .unwrap()
    }

    #[test]
    fn only_claude_roles_that_want_a_worktree_go_native() {
        let names: Vec<String> = native_roles(&fleet()).into_keys().collect();
        // shared stays on delegate for its lock; codex and local have no native path.
        assert_eq!(names, ["architect", "builder"]);
    }

    #[test]
    fn a_role_becomes_a_subagent_definition() {
        let agents: Value = serde_json::from_str(&agents_json(&fleet()).unwrap()).unwrap();
        let builder = &agents["builder"];
        assert_eq!(builder["isolation"], "worktree");
        assert_eq!(builder["model"], "sonnet");
        assert_eq!(builder["permissionMode"], "acceptEdits");
        assert_eq!(builder["maxTurns"], 60);
        assert_eq!(builder["tools"], json!(["Read", "Edit", "Write", "Bash"]));
        assert_eq!(builder["description"], "Implement the change.");
        let prompt = builder["prompt"].as_str().unwrap();
        assert!(prompt.starts_with("Implement the change."));
        assert!(prompt.contains("must never be copied there"));
        assert!(builder.get("disallowedTools").is_none());
    }

    #[test]
    fn a_readonly_role_denies_edits_on_itself_not_the_session() {
        let agents: Value = serde_json::from_str(&agents_json(&fleet()).unwrap()).unwrap();
        let denied = agents["architect"]["disallowedTools"].as_array().unwrap();
        assert!(denied.iter().any(|t| t == "Write"));
        assert!(denied.iter().any(|t| t == "Edit"));
    }

    #[test]
    fn a_fleet_with_no_native_roles_passes_no_agents() {
        let registry = RoleRegistry::from_toml(
            "[roles.reviewer]\nprovider = \"codex\"\nisolation = \"readonly\"\n",
        )
        .unwrap();
        assert!(agents_json(&registry).is_none());
    }

    #[test]
    fn hook_commands_survive_a_path_with_spaces_and_quotes() {
        let settings: Value = serde_json::from_str(&hook_settings(&[
            "/Applications/My Harness.app/Contents/MacOS/it's".into(),
            "__harness-hook".into(),
        ]))
        .unwrap();
        let create = settings["hooks"]["WorktreeCreate"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert_eq!(
            create,
            r"'/Applications/My Harness.app/Contents/MacOS/it'\''s' '__harness-hook' worktree-create"
        );
        let remove = settings["hooks"]["WorktreeRemove"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(remove.ends_with("worktree-remove"));
    }
}
