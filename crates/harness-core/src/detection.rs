//! Discovery of model backends that are usable on this machine.
//!
//! Availability answers "can this configured role run?". Discovery answers the separate
//! question "what is installed or serving models right now?". Keeping those apart avoids
//! reporting that no local model exists merely because a role still points at another
//! local server.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

use crate::availability::binary_on_path;
use crate::roles::{Isolation, Provider, Role, RoleRegistry};
use std::collections::BTreeMap;

const DISCOVERY_TIMEOUT: Duration = Duration::from_millis(1_500);
const LM_STUDIO_URL: &str = "http://localhost:1234/v1";
const OLLAMA_URL: &str = "http://localhost:11434/v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectedModel {
    pub id: String,
    pub capability: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectedBackend {
    pub id: String,
    pub label: String,
    pub available: bool,
    pub message: String,
    pub base_url: Option<String>,
    pub models: Vec<DetectedModel>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelOption {
    pub id: String,
    pub label: String,
    pub provider: String,
    pub model: String,
    pub base_url: Option<String>,
    /// `-c key=value` overrides this option implies, e.g. `model_provider = "lmstudio"`.
    /// This is what gives a local model a real agent loop under Codex.
    #[serde(default)]
    pub provider_opts: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigurableRole {
    pub name: String,
    pub provider: String,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub isolation: String,
    pub options: Vec<ModelOption>,
    pub blocked_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetInspection {
    pub backends: Vec<DetectedBackend>,
    pub roles: Vec<ConfigurableRole>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleModelPatch {
    pub role_name: String,
    pub model: String,
    pub base_url: Option<String>,
    /// The backend to move this role onto. `None` keeps the role's current provider,
    /// so payloads written before cross-backend swapping still mean what they said.
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub provider_opts: BTreeMap<String, String>,
}

pub async fn detect_backends() -> Vec<DetectedBackend> {
    let claude = cli_backend(
        "claude",
        "Claude CLI",
        "claude",
        &["sonnet", "opus", "haiku"],
        "Claude CLI found on PATH",
        "not detected — install Claude Code, then `claude /login`",
    );
    let codex = cli_backend(
        "codex",
        "Codex CLI",
        "codex",
        &[],
        "Codex CLI found on PATH",
        "not detected — `npm i -g @openai/codex`, then `codex login`",
    );

    let (lm_studio, ollama) = tokio::join!(
        detect_openai_backend("lmstudio", "LM Studio", LM_STUDIO_URL),
        detect_openai_backend("ollama", "Ollama", OLLAMA_URL),
    );

    vec![claude, codex, lm_studio, ollama]
}

pub async fn inspect_fleet(registry: &RoleRegistry) -> FleetInspection {
    let backends = detect_backends().await;
    let roles = configurable_roles(registry, &backends);
    FleetInspection { backends, roles }
}

fn cli_backend(
    id: &str,
    label: &str,
    executable: &str,
    models: &[&str],
    found: &str,
    missing: &str,
) -> DetectedBackend {
    let available = binary_on_path(executable);
    DetectedBackend {
        id: id.into(),
        label: label.into(),
        available,
        message: if available {
            found.into()
        } else {
            missing.into()
        },
        base_url: None,
        models: if available {
            models
                .iter()
                .map(|id| DetectedModel {
                    id: (*id).into(),
                    capability: "chat".into(),
                })
                .collect()
        } else {
            Vec::new()
        },
    }
}

/// The models an OpenAI-compatible server lists at `{base_url}/models`. LM Studio and
/// Ollama both serve this, so it is the one listing both detection and the availability
/// probe use.
pub(crate) async fn list_models(base_url: &str) -> Result<Vec<DetectedModel>, reqwest::Error> {
    let endpoint = format!("{}/models", base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(DISCOVERY_TIMEOUT)
        .build()?;
    let response = client.get(&endpoint).send().await?.error_for_status()?;
    let value: Value = response.json().await?;
    Ok(models_from_openai_json(&value))
}

async fn detect_openai_backend(id: &str, label: &str, base_url: &str) -> DetectedBackend {
    let result = list_models(base_url).await;

    match result {
        Ok(models) => {
            let chat_count = models
                .iter()
                .filter(|model| model.capability == "chat")
                .count();
            DetectedBackend {
                id: id.into(),
                label: label.into(),
                available: true,
                message: format!(
                    "{label} detected at {base_url} — {chat_count} chat model{}",
                    if chat_count == 1 { "" } else { "s" }
                ),
                base_url: Some(base_url.into()),
                models,
            }
        }
        Err(_) => DetectedBackend {
            id: id.into(),
            label: label.into(),
            available: false,
            message: format!("{label} is not reachable at {base_url}"),
            base_url: Some(base_url.into()),
            models: Vec::new(),
        },
    }
}

fn models_from_openai_json(value: &Value) -> Vec<DetectedModel> {
    let mut models: Vec<_> = value
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("id").and_then(Value::as_str))
        .map(|id| {
            let lower = id.to_ascii_lowercase();
            let capability = if lower.contains("embed") {
                "embedding"
            } else {
                "chat"
            }
            .to_string();
            DetectedModel {
                id: id.to_string(),
                capability,
            }
        })
        .collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    models.dedup_by(|a, b| a.id == b.id);
    models
}

/// What each role could be reassigned to, given what is installed right now.
///
/// Options are gated on what the role's isolation *needs*, not on the backend it
/// happens to use today — that is what makes swapping a builder from Claude onto a
/// local model a config change rather than an edit.
fn configurable_roles(
    registry: &RoleRegistry,
    backends: &[DetectedBackend],
) -> Vec<ConfigurableRole> {
    registry
        .roles
        .iter()
        .map(|(name, role)| {
            let (options, blocked_reason) = match role.provider {
                // The mock backend exists to be deterministic; reassigning it would
                // defeat the point.
                Provider::Mock => (
                    Vec::new(),
                    Some("The deterministic mock backend has no selectable model".into()),
                ),

                // A role with no filesystem is a raw chat completion, so only the
                // OpenAI-compatible endpoints can serve it.
                _ if role.isolation == Isolation::None => {
                    let mut options = local_chat_options(backends);
                    options.retain(|option| option.provider == Provider::OpenaiCompat.as_str());
                    let reason = options
                        .is_empty()
                        .then(|| "No compatible local chat models were detected".into());
                    (options, reason)
                }

                // Everything else needs tools and a working directory, which means one
                // of the two agent CLIs. Codex is also how a local model gets a real
                // agent loop, so local models appear here too — via Codex, not raw HTTP.
                _ => {
                    let mut options = backend_options(backends, "claude", Provider::Claude.as_str());
                    options.extend(codex_options(backends));
                    let reason = options.is_empty().then(|| {
                        "Neither the Claude nor the Codex CLI is available, so this role \
                         cannot be reassigned"
                            .into()
                    });
                    (options, reason)
                }
            };

            ConfigurableRole {
                name: name.clone(),
                provider: role.provider.as_str().into(),
                model: role.model.clone(),
                base_url: role.base_url.clone(),
                isolation: role.isolation.as_str().into(),
                options,
                blocked_reason,
            }
        })
        .collect()
}

/// Local models reachable over plain chat completions.
fn local_chat_options(backends: &[DetectedBackend]) -> Vec<ModelOption> {
    let mut options = backend_options(backends, "lmstudio", Provider::OpenaiCompat.as_str());
    options.extend(backend_options(backends, "ollama", Provider::OpenaiCompat.as_str()));
    options
}

/// Codex-backed options: the CLI's own default, plus every detected local model wired
/// through Codex's reserved provider ids, which is what gives them tools and sandboxing.
fn codex_options(backends: &[DetectedBackend]) -> Vec<ModelOption> {
    if !backends.iter().any(|b| b.id == "codex" && b.available) {
        return Vec::new();
    }

    let mut options = Vec::new();
    for local in backends
        .iter()
        .filter(|b| b.available && matches!(b.id.as_str(), "lmstudio" | "ollama"))
    {
        for model in local.models.iter().filter(|m| m.capability == "chat") {
            options.push(ModelOption {
                id: format!("codex:{}:{}", local.id, model.id),
                label: format!("Codex · {} · {}", local.label, model.id),
                provider: Provider::Codex.as_str().into(),
                model: model.id.clone(),
                // Health-check target only; Codex is told which provider to use below.
                base_url: local.base_url.clone(),
                provider_opts: BTreeMap::from([
                    ("model_provider".to_string(), local.id.clone()),
                ]),
            });
        }
    }
    options
}

fn backend_options(
    backends: &[DetectedBackend],
    backend_id: &str,
    provider: &str,
) -> Vec<ModelOption> {
    let Some(backend) = backends
        .iter()
        .find(|backend| backend.id == backend_id && backend.available)
    else {
        return Vec::new();
    };

    backend
        .models
        .iter()
        .filter(|model| model.capability == "chat")
        .map(|model| ModelOption {
            id: format!("{}:{}", backend.id, model.id),
            label: format!("{} · {}", backend.label, model.id),
            provider: provider.into(),
            model: model.id.clone(),
            base_url: backend.base_url.clone(),
            provider_opts: BTreeMap::new(),
        })
        .collect()
}

/// Reject an assignment the machine cannot actually run, before it reaches roles.toml.
///
/// Two checks, because a provider swap can break a role in a way a model swap never
/// could: the option has to be one discovery actually offered for that role, and the
/// role that results has to still satisfy the registry's own rules.
pub fn validate_patches(
    registry: &RoleRegistry,
    inspection: &FleetInspection,
    patches: &[RoleModelPatch],
) -> anyhow::Result<()> {
    for patch in patches {
        let role = registry.get(&patch.role_name)?;
        let view = inspection
            .roles
            .iter()
            .find(|candidate| candidate.name == patch.role_name)
            .ok_or_else(|| anyhow::anyhow!("role `{}` was not inspected", patch.role_name))?;

        let provider = patch.provider.as_deref().unwrap_or(role.provider.as_str());

        let matches_option = view.options.iter().any(|option| {
            option.provider == provider
                && option.model == patch.model
                && option.base_url == patch.base_url
                && option.provider_opts == patch.provider_opts
        });
        anyhow::ensure!(
            matches_option,
            "`{}` on `{provider}` is not a detected compatible option for role `{}`",
            patch.model,
            patch.role_name
        );

        // The registry forbids combinations the backends cannot honour — an
        // openai_compat role that claims filesystem isolation, for instance. Apply the
        // patch to a copy and make it prove itself before anything is written.
        apply_to_role(role, patch)?;
    }
    Ok(())
}

/// The role a patch would produce, validated the same way a parsed registry is.
pub fn apply_to_role(role: &Role, patch: &RoleModelPatch) -> anyhow::Result<Role> {
    let mut patched = role.clone();
    if let Some(provider) = &patch.provider {
        patched.provider = parse_provider(provider)?;
    }
    patched.model = Some(patch.model.clone());
    patched.base_url = patch.base_url.clone();
    patched.provider_opts = patch.provider_opts.clone();

    // One role, checked in isolation: a single-entry registry reuses the real rules
    // rather than restating them here and letting the two drift.
    let mut probe = RoleRegistry::default();
    probe.roles.insert(patch.role_name.clone(), patched.clone());
    probe
        .check()
        .with_context(|| format!("role `{}` would become invalid", patch.role_name))?;

    Ok(patched)
}

fn parse_provider(name: &str) -> anyhow::Result<Provider> {
    match name {
        "claude" => Ok(Provider::Claude),
        "codex" => Ok(Provider::Codex),
        "openai_compat" => Ok(Provider::OpenaiCompat),
        "mock" => Ok(Provider::Mock),
        other => anyhow::bail!("unknown provider `{other}`"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> RoleRegistry {
        RoleRegistry::from_toml(
            r#"
            [roles.builder]
            provider = "claude"
            model = "sonnet"
            isolation = "worktree"

            [roles.local]
            provider = "openai_compat"
            model = "old"
            base_url = "http://localhost:11434/v1"
            isolation = "none"

            [roles.local_builder]
            provider = "codex"
            model = "qwen"
            isolation = "worktree"
            "#,
        )
        .unwrap()
    }

    #[test]
    fn parses_and_classifies_openai_model_lists() {
        let models = models_from_openai_json(&serde_json::json!({
            "data": [
                {"id": "qwen/coder"},
                {"id": "nomic-embed-text"},
                {"id": "qwen/coder"}
            ]
        }));
        assert_eq!(
            models,
            vec![
                DetectedModel {
                    id: "nomic-embed-text".into(),
                    capability: "embedding".into()
                },
                DetectedModel {
                    id: "qwen/coder".into(),
                    capability: "chat".into()
                },
            ]
        );
    }

    #[test]
    fn options_are_gated_on_what_a_role_needs_not_on_its_current_backend() {
        let backends = vec![
            DetectedBackend {
                id: "claude".into(),
                label: "Claude CLI".into(),
                available: true,
                message: String::new(),
                base_url: None,
                models: vec![DetectedModel { id: "sonnet".into(), capability: "chat".into() }],
            },
            DetectedBackend {
                id: "codex".into(),
                label: "Codex CLI".into(),
                available: true,
                message: String::new(),
                base_url: None,
                models: Vec::new(),
            },
            DetectedBackend {
                id: "lmstudio".into(),
                label: "LM Studio".into(),
                available: true,
                message: String::new(),
                base_url: Some(LM_STUDIO_URL.into()),
                models: vec![DetectedModel { id: "qwen".into(), capability: "chat".into() }],
            },
        ];
        let roles = configurable_roles(&registry(), &backends);
        let options_for = |name: &str| {
            roles.iter().find(|r| r.name == name).unwrap().options.clone()
        };

        // A `none` role is a raw chat completion, so only the HTTP endpoints can serve
        // it — offering it an agent CLI would produce a role the registry rejects.
        let local = options_for("local");
        assert_eq!(local.len(), 1);
        assert_eq!(local[0].provider, "openai_compat");

        // A worktree role needs tools, so it gets both agent CLIs — and, crucially, can
        // move between them. This is the swap the fleet editor exists for.
        let builder = options_for("builder");
        assert!(
            builder.iter().any(|o| o.provider == "claude" && o.model == "sonnet"),
            "a Claude builder should still be offered Claude models: {builder:?}"
        );
        let via_codex = builder
            .iter()
            .find(|o| o.provider == "codex")
            .expect("a Claude builder should be offered Codex too");
        assert_eq!(via_codex.model, "qwen");
        assert_eq!(
            via_codex.provider_opts.get("model_provider").map(String::as_str),
            Some("lmstudio"),
            "a local model under Codex needs the provider id that gives it a tool loop"
        );

        // Symmetrically, a Codex-backed role can move onto Claude.
        assert!(
            options_for("local_builder").iter().any(|o| o.provider == "claude"),
            "a Codex builder should be able to move back onto Claude"
        );
    }

    #[test]
    fn a_worktree_role_is_blocked_when_neither_agent_cli_is_installed() {
        // Local models alone cannot back a role that needs a filesystem: without an
        // agent CLI there is no tool loop to give them.
        let backends = vec![DetectedBackend {
            id: "lmstudio".into(),
            label: "LM Studio".into(),
            available: true,
            message: String::new(),
            base_url: Some(LM_STUDIO_URL.into()),
            models: vec![DetectedModel { id: "qwen".into(), capability: "chat".into() }],
        }];

        let roles = configurable_roles(&registry(), &backends);
        let builder = roles.iter().find(|r| r.name == "builder").unwrap();
        assert!(builder.options.is_empty());
        assert!(builder.blocked_reason.as_deref().unwrap().contains("cannot be reassigned"));
    }

    #[test]
    fn a_provider_swap_that_would_break_the_role_is_refused() {
        // openai_compat has no filesystem, so moving a worktree role onto it would
        // produce a registry the loader would reject. Catch it before it is written.
        let registry = registry();
        let role = registry.get("builder").unwrap();
        let error = apply_to_role(
            role,
            &RoleModelPatch {
                role_name: "builder".into(),
                model: "qwen".into(),
                base_url: Some(LM_STUDIO_URL.into()),
                provider: Some("openai_compat".into()),
                provider_opts: Default::default(),
            },
        )
        .expect_err("a worktree role on openai_compat must be refused")
        .to_string();
        assert!(error.contains("would become invalid"), "got: {error}");
    }

    #[test]
    fn a_provider_swap_rewrites_the_backend_and_its_options() {
        let registry = registry();
        let role = registry.get("builder").unwrap();
        let patched = apply_to_role(
            role,
            &RoleModelPatch {
                role_name: "builder".into(),
                model: "qwen".into(),
                base_url: None,
                provider: Some("codex".into()),
                provider_opts: BTreeMap::from([
                    ("model_provider".to_string(), "lmstudio".to_string()),
                ]),
            },
        )
        .unwrap();

        assert_eq!(patched.provider, Provider::Codex);
        assert_eq!(patched.model.as_deref(), Some("qwen"));
        assert_eq!(patched.provider_opts.get("model_provider").map(String::as_str), Some("lmstudio"));
        // Policy is not the model picker's business: isolation and tools survive a swap.
        assert_eq!(patched.isolation, role.isolation);
        assert_eq!(patched.tools, role.tools);
    }

    #[test]
    fn rejects_undetected_or_incompatible_assignments() {
        let registry = registry();
        let inspection = FleetInspection {
            backends: Vec::new(),
            roles: configurable_roles(&registry, &[]),
        };
        let err = validate_patches(
            &registry,
            &inspection,
            &[RoleModelPatch {
                role_name: "local".into(),
                model: "made-up".into(),
                base_url: Some(LM_STUDIO_URL.into()),
                provider: None,
                provider_opts: Default::default(),
            }],
        )
        .unwrap_err();
        assert!(err.to_string().contains("not a detected compatible option"));
    }

    #[tokio::test]
    async fn enumerates_models_from_a_live_openai_compatible_server() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).await.unwrap();
            let body = r#"{"data":[{"id":"qwen-local"},{"id":"embed-local"}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        let backend =
            detect_openai_backend("test", "Test server", &format!("http://{address}/v1")).await;

        assert!(backend.available);
        assert_eq!(backend.models.len(), 2);
        assert!(backend.message.contains("1 chat model"));
    }
}
