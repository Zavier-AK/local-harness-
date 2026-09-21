//! The fleet: which backend runs a role, and how much of the filesystem it may touch.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// Which CLI or endpoint backs a role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    /// The `claude` CLI, subscription OAuth.
    Claude,
    /// The `codex` CLI. Also the way local models get a real agent loop, via
    /// `model_provider = "ollama" | "lmstudio"`.
    Codex,
    /// Raw OpenAI-compatible chat completions. No agent loop, no tools, near-zero latency.
    OpenaiCompat,
    /// Deterministic stand-in so the orchestration loop is testable without a network.
    Mock,
}

impl Provider {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::OpenaiCompat => "openai_compat",
            Self::Mock => "mock",
        }
    }
}

/// How much of the working tree a role's workers can reach.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    /// Own git worktree on its own branch. Parallel-safe. The default for anything that writes.
    #[default]
    Worktree,
    /// Throwaway worktree *and* edit tools denied. The denial states intent; the worktree
    /// means a model that writes anyway touches nothing real.
    Readonly,
    /// The project root itself, the way Claude Code already works. Serialized by an
    /// advisory lock — at most one shared worker at a time, or they clobber each other.
    Shared,
    /// No filesystem at all. For summarize/classify/draft work.
    None,
}

impl Isolation {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Worktree => "worktree",
            Self::Readonly => "readonly",
            Self::Shared => "shared",
            Self::None => "none",
        }
    }

    /// Whether changes made under this mode can ever be landed.
    pub fn is_mergeable(&self) -> bool {
        matches!(self, Self::Worktree)
    }
}

/// Tools denied to readonly roles regardless of what the role config asks for.
pub const READONLY_DENIED_TOOLS: &[&str] = &["Edit", "Write", "NotebookEdit", "MultiEdit"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Role {
    pub provider: Provider,

    #[serde(default)]
    pub model: Option<String>,

    #[serde(default)]
    pub isolation: Isolation,

    /// Passed to the backing CLI as its allow-list. Ignored by `openai_compat`.
    #[serde(default)]
    pub tools: Vec<String>,

    /// Appended to the worker's system prompt — what this role is for.
    #[serde(default)]
    pub brief: Option<String>,

    /// Claude CLI permission mode, e.g. `acceptEdits`.
    #[serde(default)]
    pub permission_mode: Option<String>,

    /// Endpoint for `openai_compat` (Ollama: `http://localhost:11434/v1`).
    #[serde(default)]
    pub base_url: Option<String>,

    /// Extra `-c key=value` config overrides for the Codex CLI. This is how a local
    /// model is given an agent loop: `model_provider = "ollama"`.
    #[serde(default)]
    pub provider_opts: BTreeMap<String, String>,

    /// Where to send this role's work when the primary backend is rate-limited.
    #[serde(default)]
    pub fallback_role: Option<String>,

    /// Runaway-loop rail for agentic backends.
    #[serde(default)]
    pub max_turns: Option<u32>,
}

impl Role {
    /// The role's allow-list, minus anything its isolation mode forbids.
    pub fn effective_tools(&self) -> Vec<String> {
        if self.isolation == Isolation::Readonly {
            self.tools
                .iter()
                .filter(|t| !READONLY_DENIED_TOOLS.contains(&t.as_str()))
                .cloned()
                .collect()
        } else {
            self.tools.clone()
        }
    }

    /// Tools to pass to `--disallowedTools`. Belt to `effective_tools`' braces: a role
    /// that never listed `Edit` still gets it explicitly denied, so a backend that
    /// enables tools by default can't sneak one in.
    pub fn denied_tools(&self) -> Vec<String> {
        if self.isolation == Isolation::Readonly {
            READONLY_DENIED_TOOLS.iter().map(|s| s.to_string()).collect()
        } else {
            Vec::new()
        }
    }
}

/// How a fresh worktree is made usable before a worker starts in it.
///
/// `git worktree add` checks out tracked files and nothing else: no `.env`, no
/// `node_modules`, no build output. A builder told to "run the tests" in such a tree
/// fails on its first command, so a project that needs bootstrapping declares it here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeSetup {
    /// Gitignored files to copy in from the project root, e.g. `.env`. Missing entries
    /// are skipped rather than failing: not every project has every file.
    ///
    /// Copied, never symlinked — a symlinked `.env` means a worker editing it silently
    /// rewrites the real one.
    #[serde(default)]
    pub copy: Vec<String>,

    /// Shell commands run in the worktree, in order, after `copy`.
    ///
    /// Install dependencies here (`npm ci`, `uv sync`). Do not symlink `node_modules`
    /// from the project root: bundlers reject module paths outside the tree.
    #[serde(default)]
    pub setup: Vec<String>,

    /// Ceiling for each `setup` command. A cold `npm ci` is slow, but a command that
    /// waits on a prompt would otherwise hang the worker forever.
    #[serde(default = "default_setup_timeout")]
    pub timeout_secs: u64,
}

fn default_setup_timeout() -> u64 {
    600
}

impl Default for WorktreeSetup {
    fn default() -> Self {
        Self { copy: Vec::new(), setup: Vec::new(), timeout_secs: default_setup_timeout() }
    }
}

impl WorktreeSetup {
    pub fn is_empty(&self) -> bool {
        self.copy.is_empty() && self.setup.is_empty()
    }

    fn validate(&self) -> Result<()> {
        for entry in &self.copy {
            let path = Path::new(entry);
            if path.is_absolute() {
                bail!("worktree.copy entry `{entry}` must be relative to the project root");
            }
            // A `..` entry would pull files from outside the project into every worktree.
            if path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
                bail!("worktree.copy entry `{entry}` may not escape the project root with `..`");
            }
        }
        if self.timeout_secs == 0 {
            bail!("worktree.timeout_secs must be greater than zero");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RoleRegistry {
    #[serde(default)]
    pub roles: BTreeMap<String, Role>,

    /// Which role the head chat delegates through by default when it names none.
    #[serde(default)]
    pub default_role: Option<String>,

    /// Bootstrap applied to every worktree this project creates.
    #[serde(default)]
    pub worktree: WorktreeSetup,
}

impl RoleRegistry {
    pub fn from_toml(src: &str) -> Result<Self> {
        let registry: RoleRegistry = toml::from_str(src).context("parsing role registry")?;
        registry.validate()?;
        Ok(registry)
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let src = std::fs::read_to_string(path)
            .with_context(|| format!("reading role registry at {}", path.display()))?;
        Self::from_toml(&src)
    }

    pub fn get(&self, name: &str) -> Result<&Role> {
        self.roles
            .get(name)
            .with_context(|| format!("no role named `{name}`; known roles: {}", self.role_names().join(", ")))
    }

    pub fn role_names(&self) -> Vec<String> {
        self.roles.keys().cloned().collect()
    }

    /// Run the registry's own rules over this registry.
    ///
    /// Role editing builds a one-role registry from a proposed change and calls this, so
    /// "is this role legal" has exactly one definition rather than two that can drift.
    pub fn check(&self) -> Result<()> {
        self.validate()
    }

    fn validate(&self) -> Result<()> {
        if self.roles.is_empty() {
            bail!("role registry defines no roles");
        }

        for (name, role) in &self.roles {
            if role.provider == Provider::OpenaiCompat && role.base_url.is_none() {
                bail!("role `{name}` uses openai_compat but sets no base_url");
            }
            if role.provider == Provider::OpenaiCompat && role.isolation != Isolation::None {
                bail!(
                    "role `{name}` uses openai_compat with isolation `{}`, but that backend has no \
                     filesystem access; use isolation = \"none\"",
                    role.isolation.as_str()
                );
            }
            if let Some(fallback) = &role.fallback_role {
                if !self.roles.contains_key(fallback) {
                    bail!("role `{name}` falls back to `{fallback}`, which is not defined");
                }
                if fallback == name {
                    bail!("role `{name}` falls back to itself");
                }
            }
        }

        if let Some(default) = &self.default_role {
            if !self.roles.contains_key(default) {
                bail!("default_role `{default}` is not defined");
            }
        }

        self.worktree.validate()?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
default_role = "builder"

[roles.builder]
provider = "claude"
model = "sonnet"
isolation = "worktree"
tools = ["Read", "Edit", "Bash"]
fallback_role = "local_builder"

[roles.local_builder]
provider = "codex"
model = "qwen3.6:35b-a3b"
isolation = "worktree"
provider_opts = { model_provider = "ollama" }

[roles.tester]
provider = "codex"
isolation = "readonly"
tools = ["Read", "Bash", "Edit"]

[roles.summarizer]
provider = "openai_compat"
base_url = "http://localhost:11434/v1"
model = "gemma4:12b"
isolation = "none"
"#;

    #[test]
    fn parses_a_full_registry() {
        let reg = RoleRegistry::from_toml(SAMPLE).unwrap();
        assert_eq!(reg.role_names().len(), 4);
        assert_eq!(reg.default_role.as_deref(), Some("builder"));

        let local = reg.get("local_builder").unwrap();
        assert_eq!(local.provider, Provider::Codex);
        // The trick that gives a local model a real agent loop.
        assert_eq!(local.provider_opts.get("model_provider").unwrap(), "ollama");
    }

    #[test]
    fn readonly_strips_edit_tools_and_denies_them() {
        let reg = RoleRegistry::from_toml(SAMPLE).unwrap();
        let tester = reg.get("tester").unwrap();

        // `Edit` was listed, but the isolation mode outranks the role's own list.
        assert_eq!(tester.effective_tools(), vec!["Read", "Bash"]);
        assert!(tester.denied_tools().contains(&"Edit".to_string()));
    }

    #[test]
    fn worktree_roles_keep_their_tools() {
        let reg = RoleRegistry::from_toml(SAMPLE).unwrap();
        let builder = reg.get("builder").unwrap();
        assert_eq!(builder.effective_tools(), vec!["Read", "Edit", "Bash"]);
        assert!(builder.denied_tools().is_empty());
    }

    #[test]
    fn rejects_openai_compat_without_base_url() {
        let err = RoleRegistry::from_toml(
            r#"
            [roles.x]
            provider = "openai_compat"
            isolation = "none"
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("no base_url"), "{err}");
    }

    #[test]
    fn rejects_openai_compat_claiming_filesystem_isolation() {
        let err = RoleRegistry::from_toml(
            r#"
            [roles.x]
            provider = "openai_compat"
            base_url = "http://localhost:11434/v1"
            isolation = "worktree"
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("no filesystem access"), "{err}");
    }

    #[test]
    fn rejects_dangling_and_self_referential_fallbacks() {
        let dangling = RoleRegistry::from_toml(
            r#"
            [roles.x]
            provider = "claude"
            fallback_role = "nope"
            "#,
        )
        .unwrap_err();
        assert!(dangling.to_string().contains("not defined"), "{dangling}");

        let looped = RoleRegistry::from_toml(
            r#"
            [roles.x]
            provider = "claude"
            fallback_role = "x"
            "#,
        )
        .unwrap_err();
        assert!(looped.to_string().contains("falls back to itself"), "{looped}");
    }

    #[test]
    fn parses_the_worktree_bootstrap_block() {
        let registry = RoleRegistry::from_toml(
            r#"
default_role = "builder"

[worktree]
copy = [".env", ".env.local"]
setup = ["npm ci"]

[roles.builder]
provider = "claude"
isolation = "worktree"
"#,
        )
        .unwrap();

        assert_eq!(registry.worktree.copy, [".env", ".env.local"]);
        assert_eq!(registry.worktree.setup, ["npm ci"]);
        assert_eq!(registry.worktree.timeout_secs, 600, "should fall back to the default");
    }

    #[test]
    fn a_fleet_without_a_worktree_block_bootstraps_nothing() {
        let registry = RoleRegistry::from_toml(SAMPLE).unwrap();
        assert!(registry.worktree.is_empty());
    }

    #[test]
    fn rejects_copy_entries_that_escape_the_project() {
        // `..` would pull files from outside the project into every worker's tree.
        for entry in ["../../.ssh/id_rsa", "/etc/passwd"] {
            let source = format!(
                r#"
[worktree]
copy = ["{entry}"]

[roles.builder]
provider = "claude"
isolation = "worktree"
"#
            );
            let error = RoleRegistry::from_toml(&source)
                .expect_err("should reject `{entry}`")
                .to_string();
            assert!(
                error.contains("escape the project root") || error.contains("must be relative"),
                "got: {error}"
            );
        }
    }

    #[test]
    fn only_worktree_isolation_can_be_merged() {
        assert!(Isolation::Worktree.is_mergeable());
        assert!(!Isolation::Readonly.is_mergeable());
        assert!(!Isolation::Shared.is_mergeable());
        assert!(!Isolation::None.is_mergeable());
    }
}
