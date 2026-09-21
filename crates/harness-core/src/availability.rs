//! Whether a role can actually run right now.
//!
//! The default fleet names backends that may not be installed. Without this check the
//! orchestrator picks a role, spends a turn delegating to it, and gets back "is the CLI
//! installed and on PATH?" — which reads as the app being broken rather than the fleet
//! being unconfigured. Probing up front turns that into a fact the UI can show and the
//! head agent can plan around.

use std::ffi::OsStr;
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

/// Whether `name` resolves to an executable within the given `PATH` value.
///
/// Takes the search path as an argument rather than reading it. `PATH` is process-global
/// and shared by every thread, so a test that rewrites it to exercise this function
/// breaks any *other* test spawning a subprocess at that moment — which is how a third
/// of this suite could fail at random under `cargo test`. Keeping the lookup pure means
/// the tests never touch the environment at all.
///
/// Done by hand rather than by spawning `which`, so probing a fleet costs no processes.
pub fn binary_in_path(name: &str, path_value: &OsStr) -> bool {
    std::env::split_paths(path_value).any(|dir| {
        let candidate = dir.join(name);
        // A directory named `claude` on PATH should not count as the CLI.
        candidate.is_file() && is_executable(&candidate)
    })
}

/// [`binary_in_path`] against the current process's `PATH`.
pub fn binary_on_path(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| binary_in_path(name, &path))
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
    let path = std::env::var_os("PATH").unwrap_or_default();
    probe_in_path(role, &path).await
}

/// [`probe`] against an explicit `PATH`, so tests need not mutate the environment.
pub async fn probe_in_path(role: &Role, path_value: &OsStr) -> Availability {
    match role.provider {
        Provider::Mock => Availability::yes(),

        Provider::Claude => {
            if binary_in_path("claude", path_value) {
                Availability::yes()
            } else {
                Availability::no("the `claude` CLI is not on PATH — install Claude Code and run `claude /login`")
            }
        }

        Provider::Codex => {
            if !binary_in_path("codex", path_value) {
                return Availability::no(
                    "the `codex` CLI is not on PATH — install Codex and run `codex login`",
                );
            }

            // A Codex role pointed at a model server also needs that server running.
            //
            // `base_url` wins where it is set, because the server need not be on this
            // machine: a role can target another host on the network, and assuming
            // localhost would report it unavailable while it is serving perfectly well.
            // The built-in provider ids imply their own default ports; anything else is
            // a custom provider whose endpoint lives in the user's Codex config, which
            // this cannot see, so it is left alone rather than guessed at.
            let model_provider = role.provider_opts.get("model_provider").map(String::as_str);

            match (role.base_url.as_deref(), model_provider) {
                (Some(base_url), _) => {
                    let root = base_url.trim_end_matches('/').trim_end_matches("/v1");
                    probe_http(root, "the model server").await
                }
                (None, Some("ollama")) => probe_http("http://localhost:11434", "Ollama").await,
                (None, Some("lmstudio")) => probe_http("http://localhost:1234", "LM Studio").await,
                (None, _) => Availability::yes(),
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

    use std::ffi::OsString;

    /// A PATH containing one directory, built without touching the process environment.
    fn path_of(dir: &Path) -> OsString {
        std::env::join_paths([dir]).expect("join_paths")
    }

    fn write_executable(dir: &Path, name: &str) {
        let target = dir.join(name);
        std::fs::write(&target, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn finds_an_executable_and_rejects_a_made_up_one() {
        let dir = tempfile::tempdir().unwrap();
        write_executable(dir.path(), "claude");
        let path = path_of(dir.path());

        assert!(binary_in_path("claude", &path));
        assert!(!binary_in_path("definitely-not-a-real-binary-xyzzy", &path));
    }

    #[test]
    fn an_empty_path_resolves_nothing() {
        assert!(!binary_in_path("claude", OsStr::new("")));
    }

    #[test]
    fn a_directory_on_path_is_not_an_executable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("codex")).unwrap();

        assert!(
            !binary_in_path("codex", &path_of(dir.path())),
            "a directory must not be mistaken for the CLI"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_non_executable_file_does_not_count() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("codex"), "not executable").unwrap();

        assert!(!binary_in_path("codex", &path_of(dir.path())));
    }

    #[tokio::test]
    async fn mock_is_always_available() {
        assert_eq!(
            probe_in_path(&role(Provider::Mock), OsStr::new("")).await,
            Availability::yes()
        );
    }

    #[tokio::test]
    async fn a_missing_cli_explains_how_to_get_it() {
        // An empty search path means no CLI resolves, without disturbing the real one.
        let empty = OsStr::new("");

        let claude = probe_in_path(&role(Provider::Claude), empty).await;
        assert!(!claude.available);
        assert!(claude.reason.unwrap().contains("claude /login"));

        let codex = probe_in_path(&role(Provider::Codex), empty).await;
        assert!(!codex.available);
        assert!(codex.reason.unwrap().contains("codex login"));
    }

    #[tokio::test]
    async fn a_present_cli_with_no_model_server_is_available() {
        let dir = tempfile::tempdir().unwrap();
        write_executable(dir.path(), "claude");

        let result = probe_in_path(&role(Provider::Claude), &path_of(dir.path())).await;
        assert!(result.available, "{result:?}");
    }

    #[tokio::test]
    async fn an_unreachable_local_server_is_named_in_the_reason() {
        let mut local = role(Provider::OpenaiCompat);
        // Port 1 is reserved and nothing will be listening on it.
        local.base_url = Some("http://127.0.0.1:1/v1".into());

        let result = probe_in_path(&local, OsStr::new("")).await;
        assert!(!result.available);
        assert!(result.reason.unwrap().contains("127.0.0.1:1"));
    }

    #[tokio::test]
    async fn a_codex_role_probes_its_own_base_url_when_given_one() {
        // A stub `codex` on a synthetic PATH, so this no longer depends on the CLI
        // actually being installed — it used to skip itself on most machines.
        let dir = tempfile::tempdir().unwrap();
        write_executable(dir.path(), "codex");

        let mut remote = role(Provider::Codex);
        remote.provider_opts.insert("model_provider".into(), "bionic".into());
        remote.base_url = Some("http://127.0.0.1:1/v1".into());

        let result = probe_in_path(&remote, &path_of(dir.path())).await;
        assert!(!result.available);
        assert!(result.reason.unwrap().contains("127.0.0.1:1"));
    }

    #[tokio::test]
    async fn a_custom_codex_provider_without_a_base_url_is_not_guessed_at() {
        let dir = tempfile::tempdir().unwrap();
        write_executable(dir.path(), "codex");

        let mut custom = role(Provider::Codex);
        // Its endpoint lives in the user's Codex config, which we cannot read. Assuming
        // localhost here would mark a working remote model server as broken.
        custom.provider_opts.insert("model_provider".into(), "bionic".into());

        assert!(probe_in_path(&custom, &path_of(dir.path())).await.available);
    }

    #[tokio::test]
    async fn openai_compat_without_a_base_url_is_unavailable() {
        let result = probe_in_path(&role(Provider::OpenaiCompat), OsStr::new("")).await;
        assert!(!result.available);
    }
}
