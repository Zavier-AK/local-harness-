//! Engine for a hierarchical multi-agent harness.
//!
//! The premise: vendor CLIs (`claude`, `codex`) authenticate with the subscription OAuth
//! token they already hold, so driving them as subprocesses spends subscription quota
//! rather than API credits. This crate is the orchestration layer over that — a head
//! agent that delegates to a fleet of workers, each with a role and an isolation policy.
//!
//! Deliberately free of any GUI dependency: the Tauri app is a thin shell over this, and
//! `harness-cli` drives the identical engine from a terminal, so the whole loop is
//! testable without a desktop.

pub mod agents;
pub mod autonomy;
pub mod availability;
pub mod detection;
pub mod engine;
pub mod event;
pub mod extensions;
pub mod hooks;
pub mod isolation;
pub mod mcp;
pub mod native;
pub mod orchestrator;
pub mod plan;
pub mod quota;
pub mod roles;
pub mod roles_patch;
pub mod store;
pub mod verify;

pub use detection::{
    ConfigurableRole, DetectedBackend, DetectedModel, FleetInspection, ModelOption, RoleModelPatch,
};
pub use engine::{Harness, RoleInfo, WorkerRecord};
pub use event::{DiffStat, HarnessEvent, Usage, WorkerStatus};
pub use isolation::{Workspace, Workspaces};
pub use orchestrator::Orchestrator;
pub use roles::{Isolation, Provider, Role, RoleRegistry};
pub use store::Store;
