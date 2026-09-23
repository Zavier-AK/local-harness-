//! The delegation tools, exposed to the orchestrator over MCP.
//!
//! This is what makes the head agent a supervisor rather than a chat window: Claude Code
//! natively supports `"type": "http"` MCP servers, so `--mcp-config` pointed at this
//! server turns `delegate(...)` into a first-class tool call. Nothing anywhere parses
//! intent out of prose.
//!
//! The server binds to loopback on an ephemeral port behind a bearer token generated per
//! session. It is a local privilege boundary, not a public API: anything that can reach
//! it can spend the subscription.

use anyhow::{Context, Result};
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{tool, tool_router};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;

use crate::engine::Harness;

/// Prefix Claude Code gives this server's tools once connected.
pub const SERVER_NAME: &str = "harness";

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DelegateParams {
    /// Which role to hand the work to. Call `list_roles` first if unsure.
    pub role: String,
    /// The task, written as a self-contained brief: the worker has its own context
    /// window and cannot see this conversation.
    pub task: String,
    /// Repository-relative paths the worker should read first.
    #[serde(default)]
    pub context_files: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkerIdParams {
    pub worker_id: String,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct CheckParams {
    /// Restrict to these workers. Omit for all of them.
    #[serde(default)]
    pub worker_ids: Vec<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RoleSummary {
    pub name: String,
    pub provider: String,
    pub model: Option<String>,
    pub isolation: String,
    pub can_edit_files: bool,
    pub brief: Option<String>,
    /// False when the backend is missing or unreachable. Delegating anyway will fail.
    pub available: bool,
    pub unavailable_reason: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct WorkerSummary {
    pub worker_id: String,
    pub role: String,
    pub status: String,
    pub summary: String,
    pub is_error: bool,
    pub files_changed: usize,
    /// Set once the worker's changes are on a branch that could be landed.
    pub branch: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Checks on a proposed merge: "verifying", or the risk level and why. A failure here
    /// is worth delegating a fix for before the person reviews it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification: Option<String>,
}

impl From<crate::engine::WorkerRecord> for WorkerSummary {
    fn from(record: crate::engine::WorkerRecord) -> Self {
        Self {
            worker_id: record.id,
            role: record.role,
            // The wire name (`awaiting_approval`), not the Debug one (`awaitingapproval`).
            status: serde_json::to_value(record.status)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default(),
            summary: record.summary,
            is_error: record.is_error,
            files_changed: record.diff.as_ref().map(|d| d.files_changed).unwrap_or(0),
            branch: record.branch,
            input_tokens: record.usage.total_input(),
            output_tokens: record.usage.output_tokens,
            verification: None,
        }
    }
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct MergeRequestResult {
    pub worker_id: String,
    pub files_changed: usize,
    pub insertions: usize,
    pub deletions: usize,
    pub files: Vec<String>,
    /// Always false: the request is queued for a human, never applied here.
    pub merged: bool,
    pub note: String,
}

/// A delegation that failed before it ran — surfaced as a tool result rather than a
/// protocol error, so the orchestrator can read the reason and pick another role instead
/// of the turn dying.
fn failed_summary(role: String, err: anyhow::Error) -> WorkerSummary {
    WorkerSummary {
        worker_id: String::new(),
        role,
        status: "failed".into(),
        summary: format!("{err:#}"),
        is_error: true,
        files_changed: 0,
        branch: None,
        input_tokens: 0,
        output_tokens: 0,
        verification: None,
    }
}

fn awaiting_approval(worker_id: String, role: String) -> WorkerSummary {
    WorkerSummary {
        worker_id,
        role,
        status: "awaiting_approval".into(),
        summary: "The person approves each delegation in this project. This one starts when \
                  they approve it, and you will be told how it ends. Do not wait or poll for \
                  it; carry on, or end your turn."
            .into(),
        is_error: false,
        files_changed: 0,
        branch: None,
        input_tokens: 0,
        output_tokens: 0,
        verification: None,
    }
}

#[derive(Clone)]
pub struct HarnessTools {
    harness: Arc<Harness>,
}

#[tool_router(server_handler)]
impl HarnessTools {
    pub fn new(harness: Arc<Harness>) -> Self {
        Self { harness }
    }

    #[tool(
        name = "list_roles",
        description = "List the worker roles available for delegation, with the model \
                       behind each, whether it can edit files, and what it is for. Roles \
                       marked available=false have a missing or unreachable backend and \
                       will fail if you delegate to them. Call this before delegating if \
                       you are unsure which role fits."
    )]
    async fn list_roles(&self) -> Json<Vec<RoleSummary>> {
        Json(
            self.harness
                .list_roles_probed()
                .await
                .into_iter()
                .map(|r| RoleSummary {
                    name: r.name,
                    provider: r.provider,
                    model: r.model,
                    isolation: r.isolation,
                    can_edit_files: r.can_edit_files,
                    brief: r.brief,
                    available: r.available.unwrap_or(true),
                    unavailable_reason: r.unavailable_reason,
                })
                .collect(),
        )
    }

    #[tool(
        name = "delegate",
        description = "Hand a task to a worker and wait for it to finish. The worker runs \
                       in its own context window and returns only its result, so write the \
                       task as a self-contained brief. Prefer this over doing the work \
                       yourself: workers are cheaper and run in isolated workspaces."
    )]
    async fn delegate(&self, Parameters(params): Parameters<DelegateParams>) -> Json<WorkerSummary> {
        // Under the `Ask` autonomy level the person approves each delegation. Waiting for
        // them inside this call would time out, so it returns at once instead.
        if self.harness.autonomy().await.delegation_needs_approval() {
            return Json(match self
                .harness
                .queue_for_approval(&params.role, &params.task, params.context_files)
                .await
            {
                Ok(worker_id) => awaiting_approval(worker_id, params.role),
                Err(err) => failed_summary(params.role, err),
            });
        }
        match self
            .harness
            .delegate(&params.role, &params.task, params.context_files)
            .await
        {
            Ok(record) => Json(record.into()),
            Err(err) => Json(failed_summary(params.role, err)),
        }
    }

    #[tool(
        name = "delegate_async",
        description = "Start a worker and return its id immediately, without waiting. Use \
                       this to fan several workers out in parallel, then poll with \
                       check_workers and read results with collect."
    )]
    async fn delegate_async(
        &self,
        Parameters(params): Parameters<DelegateParams>,
    ) -> Json<serde_json::Value> {
        if self.harness.autonomy().await.delegation_needs_approval() {
            return Json(match self
                .harness
                .queue_for_approval(&params.role, &params.task, params.context_files)
                .await
            {
                Ok(worker_id) => serde_json::to_value(awaiting_approval(worker_id, params.role))
                    .unwrap_or_default(),
                Err(err) => serde_json::json!({ "error": format!("{err:#}") }),
            });
        }
        match self
            .harness
            .delegate_async(&params.role, &params.task, params.context_files)
            .await
        {
            Ok(worker_id) => Json(serde_json::json!({ "worker_id": worker_id, "status": "queued" })),
            Err(err) => Json(serde_json::json!({ "error": format!("{err:#}") })),
        }
    }

    #[tool(
        name = "check_workers",
        description = "Report the status of workers started with delegate_async. Statuses \
                       are queued, blocked, running, done, failed, or cancelled."
    )]
    async fn check_workers(
        &self,
        Parameters(params): Parameters<CheckParams>,
    ) -> Json<Vec<WorkerSummary>> {
        let all = self.harness.workers().await;
        let filtered = if params.worker_ids.is_empty() {
            all
        } else {
            all.into_iter()
                .filter(|w| params.worker_ids.contains(&w.id))
                .collect()
        };
        let mut summaries = Vec::with_capacity(filtered.len());
        for record in filtered {
            let mut summary = WorkerSummary::from(record);
            summary.verification = self.harness.verification_line(&summary.worker_id).await;
            summaries.push(summary);
        }
        Json(summaries)
    }

    #[tool(
        name = "collect",
        description = "Read a finished worker's result, including what it changed. Returns \
                       the worker's current state if it is still running."
    )]
    async fn collect(&self, Parameters(params): Parameters<WorkerIdParams>) -> Json<serde_json::Value> {
        match self.harness.worker(&params.worker_id).await {
            Some(record) => {
                let mut summary = WorkerSummary::from(record);
                summary.verification = self.harness.verification_line(&summary.worker_id).await;
                Json(serde_json::to_value(summary).unwrap_or_default())
            }
            None => Json(serde_json::json!({
                "error": format!("no worker `{}`", params.worker_id)
            })),
        }
    }

    #[tool(
        name = "request_merge",
        description = "Propose landing a worker's changes. This does NOT merge: it queues \
                       the diff for the human to review and approve in the app. Say so \
                       when you report back — the work is not landed until they click."
    )]
    async fn request_merge(
        &self,
        Parameters(params): Parameters<WorkerIdParams>,
    ) -> Json<serde_json::Value> {
        match self.harness.request_merge(&params.worker_id).await {
            Ok(diff) => Json(
                serde_json::to_value(MergeRequestResult {
                    worker_id: params.worker_id,
                    files_changed: diff.files_changed,
                    insertions: diff.insertions,
                    deletions: diff.deletions,
                    files: diff.files,
                    merged: false,
                    note: "Queued for human review. Nothing is landed until it is approved \
                           in the app. It is being checked (tests, an independent review) in the \
                           meantime; `check_workers` shows the result, and a failure is worth \
                           delegating a fix for."
                        .into(),
                })
                .unwrap_or_default(),
            ),
            Err(err) => Json(serde_json::json!({ "error": format!("{err:#}") })),
        }
    }
}

/// A running MCP server: the address and token the orchestrator needs to reach it.
pub struct McpServer {
    pub addr: SocketAddr,
    pub token: String,
    shutdown: tokio::sync::oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<()>,
}

impl McpServer {
    pub fn url(&self) -> String {
        format!("http://{}/mcp", self.addr)
    }

    /// The `--mcp-config` JSON that points the Claude CLI at this server.
    pub fn claude_mcp_config(&self) -> String {
        self.claude_mcp_config_with(&Default::default())
    }

    /// Ours plus the user's servers, in one config. Ours is written last so a user entry
    /// can never replace the delegation server (the name is also refused on the way in).
    pub fn claude_mcp_config_with(
        &self,
        others: &std::collections::BTreeMap<String, serde_json::Value>,
    ) -> String {
        let mut servers: serde_json::Map<String, serde_json::Value> =
            others.iter().map(|(name, config)| (name.clone(), config.clone())).collect();
        servers.insert(
            SERVER_NAME.to_string(),
            serde_json::json!({
                "type": "http",
                "url": self.url(),
                "headers": { "Authorization": format!("Bearer {}", self.token) }
            }),
        );
        serde_json::json!({ "mcpServers": servers }).to_string()
    }

    /// Tool names to pass to `--allowedTools` so delegation needs no approval prompt.
    pub fn allowed_tool_names() -> Vec<String> {
        ["list_roles", "delegate", "delegate_async", "check_workers", "collect", "request_merge"]
            .iter()
            .map(|t| format!("mcp__{SERVER_NAME}__{t}"))
            .collect()
    }

    pub async fn shutdown(self) {
        let _ = self.shutdown.send(());
        let _ = self.handle.await;
    }
}

fn random_token() -> String {
    use rand::Rng;
    let bytes: [u8; 24] = rand::rng().random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Start the MCP server on an ephemeral loopback port.
pub async fn serve(harness: Arc<Harness>) -> Result<McpServer> {
    serve_on(harness, "127.0.0.1:0".parse().unwrap()).await
}

pub async fn serve_on(harness: Arc<Harness>, bind: SocketAddr) -> Result<McpServer> {
    let token = random_token();

    let service = StreamableHttpService::new(
        move || Ok(HarnessTools::new(Arc::clone(&harness))),
        Arc::new(LocalSessionManager::default()),
        {
            // Request/response tools; no long-lived SSE stream needed.
            let mut config = StreamableHttpServerConfig::default();
            config.json_response = true;
            config
        },
    );

    let expected = format!("Bearer {token}");
    let app = axum::Router::new()
        .route_service("/mcp", service)
        .layer(axum::middleware::from_fn(move |req: axum::extract::Request, next: axum::middleware::Next| {
            let expected = expected.clone();
            async move {
                let presented = req
                    .headers()
                    .get(axum::http::header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default();

                // Anything that reaches this port can spend the subscription, so an
                // unauthenticated request is refused rather than merely logged.
                if presented != expected {
                    return axum::http::Response::builder()
                        .status(axum::http::StatusCode::UNAUTHORIZED)
                        .body(axum::body::Body::from("unauthorized"))
                        .unwrap();
                }
                next.run(req).await
            }
        }));

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding the MCP server to {bind}"))?;
    let addr = listener.local_addr()?;

    let (shutdown, rx) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(async move {
        let server = axum::serve(listener, app).with_graceful_shutdown(async {
            let _ = rx.await;
        });
        if let Err(err) = server.await {
            tracing::error!("MCP server stopped: {err}");
        }
    });

    tracing::info!("MCP server listening on http://{addr}/mcp");
    Ok(McpServer { addr, token, shutdown, handle })
}
