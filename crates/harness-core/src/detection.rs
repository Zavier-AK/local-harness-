//! Discovery of model backends that are usable on this machine.
//!
//! Availability answers "can this configured role run?". Discovery answers the separate
//! question "what is installed or serving models right now?". Keeping those apart avoids
//! reporting that no local model exists merely because a role still points at another
//! local server.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

use crate::availability::binary_on_path;
use crate::roles::{Provider, RoleRegistry};

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
}

pub async fn detect_backends() -> Vec<DetectedBackend> {
    let claude = cli_backend(
        "claude",
        "Claude CLI",
        "claude",
        &["sonnet", "opus", "haiku"],
        "Claude CLI found on PATH",
        "Claude CLI not found — install Claude Code and run `claude /login`",
    );
    let codex = cli_backend(
        "codex",
        "Codex CLI",
        "codex",
        &[],
        "Codex CLI found on PATH",
        "Codex CLI not found — Codex-backed roles remain unavailable",
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

async fn detect_openai_backend(id: &str, label: &str, base_url: &str) -> DetectedBackend {
    let endpoint = format!("{}/models", base_url.trim_end_matches('/'));
    let result = async {
        let client = reqwest::Client::builder()
            .timeout(DISCOVERY_TIMEOUT)
            .build()?;
        let response = client.get(&endpoint).send().await?.error_for_status()?;
        let value: Value = response.json().await?;
        Ok::<_, reqwest::Error>(models_from_openai_json(&value))
    }
    .await;

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

fn configurable_roles(
    registry: &RoleRegistry,
    backends: &[DetectedBackend],
) -> Vec<ConfigurableRole> {
    registry
        .roles
        .iter()
        .map(|(name, role)| {
            let (options, blocked_reason) = match role.provider {
                Provider::Claude => {
                    let options = backend_options(backends, "claude", Provider::Claude.as_str());
                    let reason = options.is_empty().then(|| "Claude CLI is not available".into());
                    (options, reason)
                }
                Provider::OpenaiCompat if role.isolation == crate::roles::Isolation::None => {
                    let mut options = backend_options(
                        backends,
                        "lmstudio",
                        Provider::OpenaiCompat.as_str(),
                    );
                    options.extend(backend_options(
                        backends,
                        "ollama",
                        Provider::OpenaiCompat.as_str(),
                    ));
                    let reason = options
                        .is_empty()
                        .then(|| "No compatible local chat models were detected".into());
                    (options, reason)
                }
                Provider::Codex => (
                    Vec::new(),
                    Some(
                        "Codex setup is deferred; local tool-enabled roles cannot be reassigned yet"
                            .into(),
                    ),
                ),
                Provider::OpenaiCompat => (
                    Vec::new(),
                    Some("Raw local models can only back text-only (`none`) roles".into()),
                ),
                Provider::Mock => (
                    Vec::new(),
                    Some("The deterministic mock backend has no selectable model".into()),
                ),
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
        })
        .collect()
}

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

        let matches_option = view.options.iter().any(|option| {
            option.provider == role.provider.as_str()
                && option.model == patch.model
                && option.base_url == patch.base_url
        });
        anyhow::ensure!(
            matches_option,
            "model `{}` is not a detected compatible option for role `{}`",
            patch.model,
            patch.role_name
        );
    }
    Ok(())
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
    fn local_models_are_only_offered_to_text_only_roles() {
        let backends = vec![
            DetectedBackend {
                id: "claude".into(),
                label: "Claude CLI".into(),
                available: true,
                message: String::new(),
                base_url: None,
                models: vec![DetectedModel {
                    id: "sonnet".into(),
                    capability: "chat".into(),
                }],
            },
            DetectedBackend {
                id: "lmstudio".into(),
                label: "LM Studio".into(),
                available: true,
                message: String::new(),
                base_url: Some(LM_STUDIO_URL.into()),
                models: vec![DetectedModel {
                    id: "qwen".into(),
                    capability: "chat".into(),
                }],
            },
        ];
        let roles = configurable_roles(&registry(), &backends);
        assert_eq!(
            roles
                .iter()
                .find(|r| r.name == "local")
                .unwrap()
                .options
                .len(),
            1
        );
        assert!(roles
            .iter()
            .find(|r| r.name == "local_builder")
            .unwrap()
            .options
            .is_empty());
        assert!(roles
            .iter()
            .find(|r| r.name == "local_builder")
            .unwrap()
            .blocked_reason
            .as_deref()
            .unwrap()
            .contains("Codex"));
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
