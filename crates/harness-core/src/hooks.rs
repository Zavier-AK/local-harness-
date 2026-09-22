//! Claude Code hooks the harness answers, so native subagents use our worktrees.
//!
//! When the head agent delegates with Claude Code's own `Agent` tool to a subagent
//! defined with `isolation: "worktree"`, Claude Code would normally create the worktree
//! itself. A `WorktreeCreate` hook *replaces* that — Claude Code runs the hook, reads the
//! path it prints, and uses that directory as the subagent's working copy. So the harness
//! answers it with the same code its own workers use: the same location under
//! `.harness/worktrees/`, the same `harness/<name>` branch naming the merge gate trusts,
//! the same `[worktree]` bootstrap (`.env`, `npm ci`), the same bundled skills.
//!
//! Verified against the real CLI (2.1.280): the hook receives `{name, cwd, ...}` on stdin,
//! its last stdout line becomes the subagent's working directory, and the subagent's
//! writes land there — Claude Code itself refuses a subagent's write to the main
//! checkout's path and redirects it to the worktree.
//!
//! These run as short-lived processes, one per worktree, possibly several at once. That
//! is why worktree bookkeeping takes a file lock rather than only an in-memory one.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::isolation::{Workspaces, WORKTREE_DIR};
use crate::roles::{Isolation, RoleRegistry, WorktreeSetup};

#[derive(Debug, Deserialize)]
struct CreateInput {
    name: String,
    cwd: PathBuf,
}

#[derive(Debug, Deserialize)]
struct RemoveInput {
    worktree_path: PathBuf,
}

/// A worktree name we are willing to turn into a path and a branch.
///
/// Claude Code generates these (`agent-af79…`), but the value still becomes a directory
/// and a ref name, so anything that could climb out of `.harness/worktrees/` or form an
/// odd ref is refused rather than trusted.
fn check_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 100
        && !name.starts_with('.')
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !name.contains("..");
    if !ok {
        bail!("refusing worktree name {name:?}");
    }
    Ok(())
}

/// The project's `[worktree]` bootstrap, if it has a `roles.toml` that declares one.
///
/// A project without one still gets a plain worktree; a malformed one fails the hook,
/// because silently skipping `npm ci` produces a builder that fails confusingly later.
fn worktree_setup(project: &Path) -> Result<WorktreeSetup> {
    let roles = project.join("roles.toml");
    if !roles.is_file() {
        return Ok(WorktreeSetup::default());
    }
    Ok(RoleRegistry::load(&roles)?.worktree)
}

/// Answer `WorktreeCreate`: build the worktree and return the path Claude Code should use.
pub async fn worktree_create(stdin: &str) -> Result<PathBuf> {
    let input: CreateInput =
        serde_json::from_str(stdin).context("reading WorktreeCreate input")?;
    check_name(&input.name)?;

    let project = input
        .cwd
        .canonicalize()
        .with_context(|| format!("resolving {}", input.cwd.display()))?;
    let workspaces = Workspaces::with_setup(project.clone(), worktree_setup(&project)?);
    workspaces.ensure_git_exclude().await.ok();

    let workspace = workspaces
        .prepare(&input.name, Isolation::Worktree)
        .await
        .with_context(|| format!("creating worktree {}", input.name))?;

    // Deliberately not released: dropping a Workspace leaves it on disk, and the engine
    // adopts it by name once the subagent reports back.
    Ok(workspace.cwd.clone())
}

/// Answer `WorktreeRemove`: keep any work on its branch, then remove the worktree.
///
/// Claude Code only fires this for a subagent that left nothing behind — one that made
/// changes keeps its worktree, and the engine commits and removes it after the subagent
/// reports. Committing here anyway costs nothing and means no path through this hook can
/// discard work.
pub async fn worktree_remove(stdin: &str) -> Result<()> {
    let input: RemoveInput =
        serde_json::from_str(stdin).context("reading WorktreeRemove input")?;
    let path = input
        .worktree_path
        .canonicalize()
        .with_context(|| format!("resolving {}", input.worktree_path.display()))?;

    // Only ever touch a worktree the harness made: `<project>/.harness/worktrees/<name>`.
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("worktree path has no name")?
        .to_string();
    check_name(&name)?;
    let project = path
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .context("worktree path is not inside a project")?;
    if project.join(WORKTREE_DIR).join(&name) != path {
        bail!("refusing to remove {}: not a harness worktree", path.display());
    }

    let workspaces = Workspaces::new(project.to_path_buf());
    let workspace = workspaces
        .adopt(&name, Isolation::Worktree)
        .context("worktree vanished before it could be removed")?;
    workspace.commit(&format!("harness: {name}")).await.ok();
    workspace.release().await
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn git(cwd: &Path, args: &[&str]) {
        let out = tokio::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .await
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    async fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q", "-b", "main"]).await;
        git(dir.path(), &["config", "user.email", "h@t"]).await;
        git(dir.path(), &["config", "user.name", "H"]).await;
        tokio::fs::write(dir.path().join("README.md"), "base\n").await.unwrap();
        git(dir.path(), &["add", "-A"]).await;
        git(dir.path(), &["commit", "-q", "-m", "init"]).await;
        dir
    }

    #[tokio::test]
    async fn create_builds_a_bootstrapped_harness_worktree() {
        let dir = repo().await;
        tokio::fs::write(
            dir.path().join("roles.toml"),
            "[worktree]\ncopy = [\".env\"]\nsetup = [\"echo ready > deps.txt\"]\n\n[roles.builder]\nprovider = \"claude\"\n",
        )
        .await
        .unwrap();
        tokio::fs::write(dir.path().join(".env"), "KEY=1\n").await.unwrap();

        let input = serde_json::json!({
            "session_id": "s", "cwd": dir.path(), "hook_event_name": "WorktreeCreate",
            "name": "agent-abc123",
        })
        .to_string();
        let path = worktree_create(&input).await.unwrap();

        let root = dir.path().canonicalize().unwrap();
        assert_eq!(path, root.join(".harness/worktrees/agent-abc123"));
        // Same bootstrap our own workers get: the .env and the setup command.
        assert_eq!(tokio::fs::read_to_string(path.join(".env")).await.unwrap(), "KEY=1\n");
        assert!(path.join("deps.txt").exists());
        // Same skills too.
        assert!(path.join(".claude/skills/harness-worktree/SKILL.md").exists());
        // On the branch the merge gate recognizes.
        let head = tokio::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(&path)
            .output()
            .await
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), "harness/agent-abc123");
    }

    #[tokio::test]
    async fn create_refuses_a_name_that_could_escape_the_worktree_dir() {
        let dir = repo().await;
        for name in ["../../evil", ".hidden", "-flag", "a/b", ""] {
            let input = serde_json::json!({ "cwd": dir.path(), "name": name }).to_string();
            assert!(worktree_create(&input).await.is_err(), "should refuse {name:?}");
        }
    }

    #[tokio::test]
    async fn remove_keeps_work_on_its_branch() {
        let dir = repo().await;
        let input = serde_json::json!({ "cwd": dir.path(), "name": "agent-rm1" }).to_string();
        let path = worktree_create(&input).await.unwrap();
        tokio::fs::write(path.join("late.txt"), "kept\n").await.unwrap();

        let input = serde_json::json!({ "worktree_path": path }).to_string();
        worktree_remove(&input).await.unwrap();

        assert!(!path.exists());
        let shown = tokio::process::Command::new("git")
            .args(["show", "harness/agent-rm1:late.txt"])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&shown.stdout), "kept\n");
    }

    #[tokio::test]
    async fn remove_refuses_anything_that_is_not_a_harness_worktree() {
        let dir = repo().await;
        // A real directory, but not one of ours.
        let other = dir.path().join("src");
        tokio::fs::create_dir_all(&other).await.unwrap();
        let input = serde_json::json!({ "worktree_path": other }).to_string();
        assert!(worktree_remove(&input).await.is_err());
        assert!(other.exists(), "must not have touched it");
    }
}
