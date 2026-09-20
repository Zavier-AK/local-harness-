//! Where a worker is allowed to work.
//!
//! Each role declares an [`Isolation`] mode and this module makes it true on disk:
//!
//! * `Worktree` — its own git worktree on its own branch. Parallel builders cannot
//!   collide, and the diff is reviewable before anything lands.
//! * `Readonly` — *also* a throwaway worktree, on top of the tool denial in the role
//!   layer. The denial states intent; the worktree means a model that writes anyway
//!   touches nothing real.
//! * `Shared` — the project root itself, serialized by an advisory lock. Without the lock
//!   two shared workers editing the same file silently destroy each other's work, so the
//!   lock is the feature, not an optimization.
//! * `None` — no workspace at all.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::event::DiffStat;
use crate::roles::Isolation;

/// Directory under the project root holding worker worktrees.
pub const WORKTREE_DIR: &str = ".harness/worktrees";

/// Prefix for branches the harness creates.
pub const BRANCH_PREFIX: &str = "harness";

async fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .await
        .with_context(|| format!("running `git {}`", args.join(" ")))?;

    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// A branch's changes, ready to render.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Patch {
    pub text: String,
    /// True when `text` holds only the first `max_lines` of a larger diff.
    pub truncated: bool,
    pub total_lines: usize,
}

/// A prepared workspace, held for as long as the worker runs.
///
/// Dropping the handle does not clean up — teardown is explicit via [`Workspace::release`]
/// so a finished worker's diff survives long enough to be reviewed.
pub struct Workspace {
    pub cwd: PathBuf,
    pub isolation: Isolation,
    /// Present for worktree-backed modes.
    pub branch: Option<String>,
    project_root: PathBuf,
    /// Held for the lifetime of a `Shared` worker; this is what serializes them.
    _shared_guard: Option<OwnedMutexGuard<()>>,
}

impl Workspace {
    /// The branch this workspace's changes could be merged from, if any.
    pub fn mergeable_branch(&self) -> Option<&str> {
        if self.isolation.is_mergeable() {
            self.branch.as_deref()
        } else {
            None
        }
    }

    /// What the worker changed, relative to the branch point.
    pub async fn diff(&self) -> Result<DiffStat> {
        if self.branch.is_none() {
            return Ok(DiffStat::default());
        }

        // Stage everything first so new files show up in the diff too.
        git(&self.cwd, &["add", "-A"]).await?;
        let numstat = git(&self.cwd, &["diff", "--cached", "--numstat"]).await?;

        let mut stat = DiffStat::default();
        for line in numstat.lines().filter(|l| !l.trim().is_empty()) {
            let mut fields = line.split('\t');
            let added = fields.next().unwrap_or("0");
            let removed = fields.next().unwrap_or("0");
            let path = fields.next().unwrap_or_default();

            // Binary files report `-`; count the file, not the lines.
            stat.insertions += added.parse::<usize>().unwrap_or(0);
            stat.deletions += removed.parse::<usize>().unwrap_or(0);
            if !path.is_empty() {
                stat.files.push(path.to_string());
            }
        }
        stat.files_changed = stat.files.len();

        Ok(stat)
    }

    /// Commit the worker's changes onto its branch so they survive teardown.
    pub async fn commit(&self, message: &str) -> Result<bool> {
        if self.branch.is_none() {
            return Ok(false);
        }

        git(&self.cwd, &["add", "-A"]).await?;
        if git(&self.cwd, &["diff", "--cached", "--name-only"]).await?.is_empty() {
            return Ok(false);
        }

        git(&self.cwd, &["commit", "--no-verify", "-m", message]).await?;
        Ok(true)
    }

    /// Remove the worktree. The branch is left behind so a reviewed diff can still be
    /// merged after the worker is gone.
    pub async fn release(self) -> Result<()> {
        if self.isolation == Isolation::Shared || self.branch.is_none() {
            return Ok(());
        }

        git(
            &self.project_root,
            &["worktree", "remove", "--force", &self.cwd.to_string_lossy()],
        )
        .await
        .with_context(|| format!("removing worktree {}", self.cwd.display()))?;

        Ok(())
    }
}

/// Prepares workspaces and owns the advisory lock that serializes shared-mode workers.
#[derive(Clone)]
pub struct Workspaces {
    project_root: PathBuf,
    shared_lock: Arc<Mutex<()>>,
}

impl Workspaces {
    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        Self {
            project_root: project_root.into(),
            shared_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    /// Prepare a workspace for one worker.
    ///
    /// For `Shared`, this waits until any other shared worker has finished — the caller
    /// should report [`WorkerStatus::Blocked`](crate::event::WorkerStatus::Blocked) while
    /// it waits.
    pub async fn prepare(&self, worker_id: &str, isolation: Isolation) -> Result<Workspace> {
        match isolation {
            Isolation::None => Ok(Workspace {
                cwd: self.project_root.clone(),
                isolation,
                branch: None,
                project_root: self.project_root.clone(),
                _shared_guard: None,
            }),

            Isolation::Shared => {
                let guard = self.shared_lock.clone().lock_owned().await;
                Ok(Workspace {
                    cwd: self.project_root.clone(),
                    isolation,
                    branch: None,
                    project_root: self.project_root.clone(),
                    _shared_guard: Some(guard),
                })
            }

            Isolation::Worktree | Isolation::Readonly => {
                let branch = format!("{BRANCH_PREFIX}/{worker_id}");
                let path = self.project_root.join(WORKTREE_DIR).join(worker_id);

                if let Some(parent) = path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }

                git(
                    &self.project_root,
                    &["worktree", "add", "-b", &branch, &path.to_string_lossy(), "HEAD"],
                )
                .await
                .context("creating the worker's worktree")?;

                Ok(Workspace {
                    cwd: path,
                    isolation,
                    branch: Some(branch),
                    project_root: self.project_root.clone(),
                    _shared_guard: None,
                })
            }
        }
    }

    /// The unified diff a branch carries, for review before it is landed.
    ///
    /// Computed from the branch rather than the worktree, because the worktree is torn
    /// down as soon as the worker finishes while the branch is kept precisely so its work
    /// stays reviewable. The three-dot form diffs against the merge base, so a `HEAD`
    /// that moved on after the worker started does not pollute the patch.
    ///
    /// Truncated at `max_lines`: a worker that rewrote a lockfile should not be able to
    /// wedge the reviewer's UI.
    pub async fn patch(&self, branch: &str, max_lines: usize) -> Result<Patch> {
        if !branch.starts_with(&format!("{BRANCH_PREFIX}/")) {
            bail!("refusing to read `{branch}`: not a harness branch");
        }

        let text = git(
            &self.project_root,
            &["diff", "--no-color", &format!("HEAD...{branch}")],
        )
        .await?;

        let total = text.lines().count();
        if total > max_lines {
            let head: Vec<&str> = text.lines().take(max_lines).collect();
            return Ok(Patch {
                text: head.join("\n"),
                truncated: true,
                total_lines: total,
            });
        }

        Ok(Patch { text, truncated: false, total_lines: total })
    }

    /// Land a reviewed branch on the current checkout.
    ///
    /// Only ever called behind an explicit human approval: the orchestrator can propose a
    /// merge, never perform one.
    pub async fn merge(&self, branch: &str) -> Result<String> {
        if !branch.starts_with(&format!("{BRANCH_PREFIX}/")) {
            bail!("refusing to merge `{branch}`: not a harness branch");
        }

        git(
            &self.project_root,
            &["merge", "--no-ff", "-m", &format!("harness: merge {branch}"), branch],
        )
        .await
    }

    /// Drop a branch whose work was rejected.
    pub async fn discard(&self, branch: &str) -> Result<()> {
        if !branch.starts_with(&format!("{BRANCH_PREFIX}/")) {
            bail!("refusing to delete `{branch}`: not a harness branch");
        }
        git(&self.project_root, &["branch", "-D", branch]).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn scratch_repo() -> (tempfile::TempDir, Workspaces) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();

        git(&root, &["init", "-q", "-b", "main"]).await.unwrap();
        git(&root, &["config", "user.email", "harness@test"]).await.unwrap();
        git(&root, &["config", "user.name", "Harness Test"]).await.unwrap();
        tokio::fs::write(root.join("README.md"), "base\n").await.unwrap();
        git(&root, &["add", "-A"]).await.unwrap();
        git(&root, &["commit", "-q", "-m", "init"]).await.unwrap();

        let workspaces = Workspaces::new(root);
        (dir, workspaces)
    }

    #[tokio::test]
    async fn worktree_mode_isolates_writes_from_the_project_root() {
        let (dir, ws) = scratch_repo().await;
        let workspace = ws.prepare("w1", Isolation::Worktree).await.unwrap();

        assert_ne!(workspace.cwd, *ws.project_root());
        assert_eq!(workspace.branch.as_deref(), Some("harness/w1"));

        tokio::fs::write(workspace.cwd.join("new.txt"), "from worker").await.unwrap();

        // The project root is untouched while the worker runs.
        assert!(!dir.path().join("new.txt").exists());

        let diff = workspace.diff().await.unwrap();
        assert_eq!(diff.files_changed, 1);
        assert_eq!(diff.files, vec!["new.txt"]);
        assert_eq!(diff.insertions, 1);
    }

    #[tokio::test]
    async fn parallel_worktree_workers_do_not_collide() {
        let (_dir, ws) = scratch_repo().await;

        let a = ws.prepare("wa", Isolation::Worktree).await.unwrap();
        let b = ws.prepare("wb", Isolation::Worktree).await.unwrap();

        // Both write the same path; in shared mode this is the corruption case.
        tokio::fs::write(a.cwd.join("same.txt"), "from a").await.unwrap();
        tokio::fs::write(b.cwd.join("same.txt"), "from b").await.unwrap();

        assert_eq!(tokio::fs::read_to_string(a.cwd.join("same.txt")).await.unwrap(), "from a");
        assert_eq!(tokio::fs::read_to_string(b.cwd.join("same.txt")).await.unwrap(), "from b");
        assert_ne!(a.branch, b.branch);
    }

    #[tokio::test]
    async fn shared_mode_serializes_workers() {
        let (_dir, ws) = scratch_repo().await;

        let first = ws.prepare("s1", Isolation::Shared).await.unwrap();
        assert_eq!(first.cwd, *ws.project_root());

        // A second shared worker must wait for the first to finish.
        let ws2 = ws.clone();
        let pending = tokio::spawn(async move { ws2.prepare("s2", Isolation::Shared).await });

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!pending.is_finished(), "second shared worker should be blocked");

        first.release().await.unwrap(); // drops the guard
        let second = tokio::time::timeout(Duration::from_secs(5), pending)
            .await
            .expect("second worker should unblock once the first releases")
            .unwrap()
            .unwrap();
        assert_eq!(second.cwd, *ws.project_root());
    }

    #[tokio::test]
    async fn worktree_workers_run_concurrently_with_a_shared_worker() {
        let (_dir, ws) = scratch_repo().await;
        let _shared = ws.prepare("s1", Isolation::Shared).await.unwrap();

        // The lock is only for shared mode; worktree workers are unaffected.
        let worktree = tokio::time::timeout(
            Duration::from_secs(5),
            ws.prepare("w1", Isolation::Worktree),
        )
        .await
        .expect("worktree prepare must not block on the shared lock")
        .unwrap();

        assert_ne!(worktree.cwd, *ws.project_root());
    }

    #[tokio::test]
    async fn readonly_mode_still_gets_a_throwaway_worktree() {
        let (dir, ws) = scratch_repo().await;
        let workspace = ws.prepare("r1", Isolation::Readonly).await.unwrap();

        // A model that writes despite the tool denial hits the sandbox, not the repo.
        tokio::fs::write(workspace.cwd.join("sneaky.txt"), "x").await.unwrap();
        assert!(!dir.path().join("sneaky.txt").exists());

        // ...and its branch is never offered for merge.
        assert_eq!(workspace.mergeable_branch(), None);
    }

    #[tokio::test]
    async fn only_worktree_work_is_offered_for_merge() {
        let (_dir, ws) = scratch_repo().await;

        let worktree = ws.prepare("w1", Isolation::Worktree).await.unwrap();
        assert_eq!(worktree.mergeable_branch(), Some("harness/w1"));

        let shared = ws.prepare("s1", Isolation::Shared).await.unwrap();
        assert_eq!(shared.mergeable_branch(), None);
    }

    #[tokio::test]
    async fn committed_work_survives_worktree_teardown_and_merges() {
        let (dir, ws) = scratch_repo().await;
        let workspace = ws.prepare("w1", Isolation::Worktree).await.unwrap();

        tokio::fs::write(workspace.cwd.join("feature.txt"), "shipped\n").await.unwrap();
        assert!(workspace.commit("harness: worker w1").await.unwrap());

        let branch = workspace.mergeable_branch().unwrap().to_string();
        workspace.release().await.unwrap();

        // Worktree gone, branch retained.
        assert!(!dir.path().join(WORKTREE_DIR).join("w1").exists());

        ws.merge(&branch).await.unwrap();
        assert_eq!(
            tokio::fs::read_to_string(dir.path().join("feature.txt")).await.unwrap(),
            "shipped\n"
        );
    }

    #[tokio::test]
    async fn committing_nothing_reports_nothing() {
        let (_dir, ws) = scratch_repo().await;
        let workspace = ws.prepare("w1", Isolation::Worktree).await.unwrap();
        assert!(!workspace.commit("empty").await.unwrap());
    }

    #[tokio::test]
    async fn refuses_to_touch_branches_it_did_not_create() {
        let (_dir, ws) = scratch_repo().await;

        let err = ws.merge("main").await.unwrap_err();
        assert!(err.to_string().contains("not a harness branch"), "{err}");

        let err = ws.discard("main").await.unwrap_err();
        assert!(err.to_string().contains("not a harness branch"), "{err}");
    }

    #[tokio::test]
    async fn discard_drops_a_rejected_branch() {
        let (_dir, ws) = scratch_repo().await;
        let workspace = ws.prepare("w1", Isolation::Worktree).await.unwrap();
        tokio::fs::write(workspace.cwd.join("bad.txt"), "no").await.unwrap();
        workspace.commit("harness: worker w1").await.unwrap();

        let branch = workspace.branch.clone().unwrap();
        workspace.release().await.unwrap();
        ws.discard(&branch).await.unwrap();

        let branches = git(ws.project_root(), &["branch", "--list"]).await.unwrap();
        assert!(!branches.contains("harness/w1"));
    }

    #[tokio::test]
    async fn patch_shows_what_a_branch_changed() {
        let (_dir, ws) = scratch_repo().await;
        let workspace = ws.prepare("w1", Isolation::Worktree).await.unwrap();

        tokio::fs::write(workspace.cwd.join("feature.txt"), "line one\nline two\n").await.unwrap();
        workspace.commit("harness: worker w1").await.unwrap();
        let branch = workspace.branch.clone().unwrap();
        workspace.release().await.unwrap();

        // Readable after teardown: the worktree is gone, the branch is the record.
        let patch = ws.patch(&branch, 500).await.unwrap();
        assert!(patch.text.contains("+++ b/feature.txt"), "{}", patch.text);
        assert!(patch.text.contains("+line one"));
        assert!(patch.text.contains("+line two"));
        assert!(!patch.truncated);
    }

    #[tokio::test]
    async fn patch_ignores_commits_made_after_the_worker_started() {
        let (dir, ws) = scratch_repo().await;
        let workspace = ws.prepare("w1", Isolation::Worktree).await.unwrap();
        tokio::fs::write(workspace.cwd.join("worker.txt"), "from worker\n").await.unwrap();
        workspace.commit("harness: worker w1").await.unwrap();
        let branch = workspace.branch.clone().unwrap();
        workspace.release().await.unwrap();

        // Meanwhile the user commits something unrelated on their own branch.
        tokio::fs::write(dir.path().join("unrelated.txt"), "from user\n").await.unwrap();
        git(ws.project_root(), &["add", "-A"]).await.unwrap();
        git(ws.project_root(), &["commit", "-q", "-m", "user work"]).await.unwrap();

        // The review must show only the worker's change, not the user's.
        let patch = ws.patch(&branch, 500).await.unwrap();
        assert!(patch.text.contains("worker.txt"), "{}", patch.text);
        assert!(!patch.text.contains("unrelated.txt"), "{}", patch.text);
    }

    #[tokio::test]
    async fn oversized_patches_are_truncated_not_dumped() {
        let (_dir, ws) = scratch_repo().await;
        let workspace = ws.prepare("w1", Isolation::Worktree).await.unwrap();

        let big: String = (0..500).map(|n| format!("line {n}\n")).collect();
        tokio::fs::write(workspace.cwd.join("big.txt"), big).await.unwrap();
        workspace.commit("harness: worker w1").await.unwrap();
        let branch = workspace.branch.clone().unwrap();
        workspace.release().await.unwrap();

        let patch = ws.patch(&branch, 20).await.unwrap();
        assert!(patch.truncated);
        assert_eq!(patch.text.lines().count(), 20);
        // The full size is still reported, so the reviewer knows what they are not seeing.
        assert!(patch.total_lines > 400, "{}", patch.total_lines);
    }

    #[tokio::test]
    async fn patch_refuses_branches_it_did_not_create() {
        let (_dir, ws) = scratch_repo().await;
        let err = ws.patch("main", 100).await.unwrap_err();
        assert!(err.to_string().contains("not a harness branch"), "{err}");
    }

    #[tokio::test]
    async fn none_mode_allocates_no_branch() {
        let (_dir, ws) = scratch_repo().await;
        let workspace = ws.prepare("n1", Isolation::None).await.unwrap();
        assert!(workspace.branch.is_none());
        assert_eq!(workspace.diff().await.unwrap(), DiffStat::default());
    }
}
