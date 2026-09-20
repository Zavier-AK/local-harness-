//! Whether a role can actually run right now.
//!
//! The default fleet names backends that may not be installed. Without this check the
//! orchestrator picks a role, spends a turn delegating to it, and gets back "is the CLI
//! installed and on PATH?" — which reads as the app being broken rather than the fleet
//! being unconfigured. Probing up front turns that into a fact the UI can show and the
//! head agent can plan around.

use std::path::Path;
use std::time::Duration;

use crate::roles::{Provider, Role};

/// How long to wait on a local model server before calling it unreachable. Generous
/// enough for a loaded machine, short enough not to stall session startup.
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Availability {
    pub available: bool,
    /// Why not, phrased as something the reader can act on.
    pub reason: Option<String>,
}

impl Availability {
    fn yes() -> Self {
        Self { available: true, reason: None }
    }

    fn no(reason: impl Into<String>) -> Self {
        Self { available: false, reason: Some(reason.into()) }
    }
}

/// Whether an executable is resolvable on `PATH`.
///
/// Done by hand rather than by spawning `which`, so probing a fleet costs no processes.
pub fn binary_on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };

    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(name);
        // A directory named `claude` on PATH should not count as the CLI.
        candidate.is_file() && is_executable(&candidate)
    })
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    true
}

/// Probe one role's backend.
pub async fn probe(role: &Role) -> Availability {
    match role.provider {
        Provider::Mock => Availability::yes(),

        Provider::Claude => {
            if binary_on_path("claude") {
                Availability::yes()
            } else {
                Availability::no("the `claude` CLI is not on PATH — install Claude Code and run `claude /login`")
            }
        }

        Provider::Codex => {
            if !binary_on_path("codex") {
                return Availability::no(
                    "the `codex` CLI is not on PATH — install Codex and run `codex login`",
                );
            }

            // A Codex role pointed at a local provider also needs that server running.
            match role.provider_opts.get("model_provider").map(String::as_str) {
                Some("ollama") => probe_http("http://localhost:11434", "Ollama").await,
                Some("lmstudio") => probe_http("http://localhost:1234", "LM Studio").await,
                _ => Availability::yes(),
            }
        }

        Provider::OpenaiCompat => match &role.base_url {
            Some(base_url) => {
                let root = base_url.trim_end_matches('/').trim_end_matches("/v1");
                probe_http(root, "the local model server").await
            }
            None => Availability::no("no base_url configured"),
        },
    }
}

/// A liveness check, not a correctness check: any HTTP response means something is
/// listening, which is all this needs to distinguish "not running" from "misconfigured".
async fn probe_http(root: &str, label: &str) -> Availability {
    let client = match reqwest::Client::builder().timeout(PROBE_TIMEOUT).build() {
        Ok(client) => client,
        Err(err) => return Availability::no(format!("could not build an HTTP client: {err}")),
    };

    match client.get(root).send().await {
        Ok(_) => Availability::yes(),
        Err(_) => Availability::no(format!("{label} is not reachable at {root}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roles::Isolation;
    use std::collections::BTreeMap;

    fn role(provider: Provider) -> Role {
        Role {
            provider,
            model: None,
            isolation: Isolation::None,
            tools: vec![],
            brief: None,
            permission_mode: None,
            base_url: None,
            provider_opts: BTreeMap::new(),
            fallback_role: None,
            max_turns: None,
        }
    }

    #[test]
    fn finds_a_real_binary_and_rejects_a_made_up_one() {
        // `sh` exists on every platform this runs on.
        assert!(binary_on_path("sh"));
        assert!(!binary_on_path("definitely-not-a-real-binary-xyzzy"));
    }

    #[test]
    fn a_directory_on_path_is_not_an_executable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("codex")).unwrap();

        let original = std::env::var_os("PATH");
        std::env::set_var("PATH", dir.path());
        let found = binary_on_path("codex");
        match original {
            Some(path) => std::env::set_var("PATH", path),
            None => std::env::remove_var("PATH"),
        }

        assert!(!found, "a directory must not be mistaken for the CLI");
    }

    #[tokio::test]
    async fn mock_is_always_available() {
        assert_eq!(probe(&role(Provider::Mock)).await, Availability::yes());
    }

    #[tokio::test]
    async fn a_missing_cli_explains_how_to_get_it() {
        // Nothing is on PATH, so every CLI-backed role is unavailable.
        let original = std::env::var_os("PATH");
        std::env::set_var("PATH", "");

        let claude = probe(&role(Provider::Claude)).await;
        let codex = probe(&role(Provider::Codex)).await;

        match original {
            Some(path) => std::env::set_var("PATH", path),
            None => std::env::remove_var("PATH"),
        }

        assert!(!claude.available);
        assert!(claude.reason.unwrap().contains("claude /login"));
        assert!(!codex.available);
        assert!(codex.reason.unwrap().contains("codex login"));
    }

    #[tokio::test]
    async fn an_unreachable_local_server_is_named_in_the_reason() {
        let mut local = role(Provider::OpenaiCompat);
        // Port 1 is reserved and nothing will be listening on it.
        local.base_url = Some("http://127.0.0.1:1/v1".into());

        let result = probe(&local).await;
        assert!(!result.available);
        assert!(result.reason.unwrap().contains("127.0.0.1:1"));
    }

    #[tokio::test]
    async fn openai_compat_without_a_base_url_is_unavailable() {
        let result = probe(&role(Provider::OpenaiCompat)).await;
        assert!(!result.available);
    }
}
