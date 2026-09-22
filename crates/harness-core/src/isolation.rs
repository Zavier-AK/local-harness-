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
use crate::roles::{Isolation, WorktreeSetup};

/// Directory under the project root holding worker worktrees.
pub const WORKTREE_DIR: &str = ".harness/worktrees";

/// Prefix for branches the harness creates.
pub const BRANCH_PREFIX: &str = "harness";

/// The line written into `.git/info/exclude`.
const HARNESS_EXCLUDE: &str = ".harness/";

/// Where skills are materialized inside a worker's worktree, and the prefix that marks
/// them as ours.
///
/// Namespaced: a project may have its own `.claude/skills/`, which must keep working and
/// must keep being committable. Only entries under this prefix are the harness's.
const SKILLS_DIR: &str = ".claude/skills";
const SKILL_PREFIX: &str = "harness-";

/// Pathspec holding harness-managed skills back from a worker's commits.
///
/// Worktrees are staged with `git add -A`, which would otherwise sweep the skills we put
/// there into the worker's branch and land them in the user's repository on merge.
const EXCLUDE_SKILLS: &str = ":(exclude).claude/skills/harness-*";

/// The skills every worker gets, baked into the binary.
///
/// Compiled in rather than read from disk because workers run in worktrees of the
/// *user's* project, not of this repository — there is no path to a `skills/` directory
/// from there once the app is installed somewhere else.
const BUNDLED_SKILLS: &[(&str, &str)] = &[
    ("harness-worktree", include_str!("../../../skills/harness-worktree/SKILL.md")),
    ("harness-review", include_str!("../../../skills/harness-review/SKILL.md")),
    (
        "harness-risky-changes",
        include_str!("../../../skills/harness-risky-changes/SKILL.md"),
    ),
];

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

/// Serialize git's worktree bookkeeping across *processes*, not just tasks.
///
/// The in-process mutex covers workers this engine starts. Native subagents are different:
/// Claude Code creates their worktrees by running our `WorktreeCreate` hook as a separate
/// process per subagent, and two subagents started together run two hooks at once. Two
/// concurrent `git worktree add`s from separate processes corrupt `.git/worktrees/` exactly
/// as two tasks would, so the guard has to be something every process can see: an OS file
/// lock in the repository's common git directory, released when the file is dropped.
async fn lock_worktree_admin(repo: &Path) -> Result<std::fs::File> {
    let common = git(repo, &["rev-parse", "--git-common-dir"]).await?;
    let common = {
        let path = PathBuf::from(&common);
        if path.is_absolute() { path } else { repo.join(path) }
    };
    let path = common.join("harness-worktree.lock");
    tokio::task::spawn_blocking(move || -> Result<std::fs::File> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        file.lock().context("waiting for the worktree lock")?;
        Ok(file)
    })
    .await
    .context("worktree lock task")?
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
    /// Guards git's worktree bookkeeping during teardown. See [`Workspaces::git_lock`].
    git_lock: Arc<Mutex<()>>,
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

        // Stage everything first so new files show up in the diff too — except the
        // skills the harness put here, which are not the worker's work.
        git(&self.cwd, &["add", "-A", "--", ".", EXCLUDE_SKILLS]).await?;
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

        git(&self.cwd, &["add", "-A", "--", ".", EXCLUDE_SKILLS]).await?;
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

        // Teardown mutates the same bookkeeping that creation does.
        let _guard = self.git_lock.lock().await;
        let _file_lock = lock_worktree_admin(&self.project_root).await?;
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
    /// Bootstrap applied to each new worktree. Empty by default.
    setup: WorktreeSetup,
    shared_lock: Arc<Mutex<()>>,
    /// Serializes git's worktree bookkeeping.
    ///
    /// `git worktree add` and `git worktree remove` both mutate `.git/worktrees/`, and
    /// concurrent invocations against one repository corrupt each other's administrative
    /// files — surfacing as `failed to read .git/worktrees/<id>/commondir`. Fanning
    /// several builders out at once is the normal case here, so this is a real failure
    /// mode rather than a theoretical one.
    ///
    /// Held only across the metadata operation, which takes milliseconds. Workers
    /// themselves still run fully in parallel.
    git_lock: Arc<Mutex<()>>,
}

impl Workspaces {
    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        Self {
            project_root: project_root.into(),
            setup: WorktreeSetup::default(),
            shared_lock: Arc::new(Mutex::new(())),
            git_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Same, with the project's worktree bootstrap attached.
    pub fn with_setup(project_root: impl Into<PathBuf>, setup: WorktreeSetup) -> Self {
        Self { setup, ..Self::new(project_root) }
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    /// A worktree this engine did not create — a native subagent's, made by our
    /// `WorktreeCreate` hook in another process — as a workspace it can diff, commit and
    /// release like any other.
    ///
    /// Found by name, because the hook puts every worktree at the same place ours go, on a
    /// branch named the same way. `None` if there is no such worktree.
    pub fn adopt(&self, worker_id: &str, isolation: Isolation) -> Option<Workspace> {
        let path = self.project_root.join(WORKTREE_DIR).join(worker_id);
        path.is_dir().then(|| Workspace {
            cwd: path,
            isolation,
            branch: Some(format!("{BRANCH_PREFIX}/{worker_id}")),
            project_root: self.project_root.clone(),
            _shared_guard: None,
            git_lock: Arc::clone(&self.git_lock),
        })
    }

    /// Keep worker worktrees out of the project's `git status`.
    ///
    /// Worktrees are created under `<project>/.harness/`, which is inside the repository.
    /// Unless the project happens to ignore that path, every worker's checkout shows up as
    /// untracked files in the parent repo — and a `shared`-isolation worker running
    /// `git add -A` would commit another worker's entire tree.
    ///
    /// Written to `.git/info/exclude` rather than `.gitignore`: the exclusion is this
    /// machine's business, not a change to the user's tracked files.
    pub async fn ensure_git_exclude(&self) -> Result<()> {
        let git_dir = git(&self.project_root, &["rev-parse", "--git-common-dir"]).await?;
        let git_dir = {
            let path = PathBuf::from(&git_dir);
            if path.is_absolute() { path } else { self.project_root.join(path) }
        };

        let exclude = git_dir.join("info").join("exclude");
        if let Some(parent) = exclude.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let current = tokio::fs::read_to_string(&exclude).await.unwrap_or_default();
        if current.lines().any(|line| line.trim() == HARNESS_EXCLUDE) {
            return Ok(());
        }

        let mut updated = current;
        if !updated.is_empty() && !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push_str(&format!("# added by local-harness\n{HARNESS_EXCLUDE}\n"));
        tokio::fs::write(&exclude, updated)
            .await
            .with_context(|| format!("writing {}", exclude.display()))?;
        Ok(())
    }

    /// Put the bundled skills where the worker's CLI will find them.
    ///
    /// Claude Code reads skills from `.claude/skills` relative to the working directory,
    /// so they go in the worktree rather than in this repository — a worker is working on
    /// the user's project, and skills vendored here would never reach it.
    ///
    /// They are excluded from what the worker commits (see [`EXCLUDE_SKILLS`]), so they
    /// are visible to the agent and invisible to the diff.
    async fn write_skills_inner(root: &Path) -> Result<()> {
        for (name, body) in BUNDLED_SKILLS {
            debug_assert!(name.starts_with(SKILL_PREFIX), "skills must carry the prefix");
            let dir = root.join(SKILLS_DIR).join(name);
            tokio::fs::create_dir_all(&dir)
                .await
                .with_context(|| format!("creating {}", dir.display()))?;
            tokio::fs::write(dir.join("SKILL.md"), body)
                .await
                .with_context(|| format!("writing the {name} skill"))?;
        }
        Ok(())
    }

    /// Copy declared files in, then run the project's setup commands.
    ///
    /// Failure here is fatal to the worker on purpose: a builder that starts in a tree
    /// with no dependencies fails later anyway, with a far more confusing message.
    async fn bootstrap(&self, cwd: &Path) -> Result<()> {
        for entry in &self.setup.copy {
            let from = self.project_root.join(entry);
            if !from.exists() {
                continue;
            }
            let to = cwd.join(entry);
            if let Some(parent) = to.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::copy(&from, &to)
                .await
                .with_context(|| format!("copying {entry} into the worktree"))?;
        }

        for command in &self.setup.setup {
            let run = Command::new("sh")
                .arg("-c")
                .arg(command)
                .current_dir(cwd)
                .output();

            let output = tokio::time::timeout(
                std::time::Duration::from_secs(self.setup.timeout_secs),
                run,
            )
            .await
            .with_context(|| {
                format!(
                    "worktree setup `{command}` exceeded {}s",
                    self.setup.timeout_secs
                )
            })?
            .with_context(|| format!("running worktree setup `{command}`"))?;

            if !output.status.success() {
                bail!(
                    "worktree setup `{command}` failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
        }

        Ok(())
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
                git_lock: Arc::clone(&self.git_lock),
            }),

            Isolation::Shared => {
                let guard = self.shared_lock.clone().lock_owned().await;
                Ok(Workspace {
                    cwd: self.project_root.clone(),
                    isolation,
                    branch: None,
                    project_root: self.project_root.clone(),
                    _shared_guard: Some(guard),
                    git_lock: Arc::clone(&self.git_lock),
                })
            }

            Isolation::Worktree | Isolation::Readonly => {
                let branch = format!("{BRANCH_PREFIX}/{worker_id}");
                let path = self.project_root.join(WORKTREE_DIR).join(worker_id);

                if let Some(parent) = path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }

                {
                    // Serialized: two concurrent `worktree add` calls leave git's
                    // administrative directory inconsistent.
                    let _guard = self.git_lock.lock().await;
                    let _file_lock = lock_worktree_admin(&self.project_root).await?;
                    git(
                        &self.project_root,
                        &["worktree", "add", "-b", &branch, &path.to_string_lossy(), "HEAD"],
                    )
                    .await
                    .context("creating the worker's worktree")?;
                }

                // A half-built worktree is worse than none: tear it down rather than
                // handing a worker a tree that is missing its dependencies.
                if let Err(error) = Self::write_skills_inner(&path)
                    .await
                    .and(self.bootstrap(&path).await)
                {
                    let _guard = self.git_lock.lock().await;
                    let _file_lock = lock_worktree_admin(&self.project_root).await;
                    let _ = git(
                        &self.project_root,
                        &["worktree", "remove", "--force", &path.to_string_lossy()],
                    )
                    .await;
                    return Err(error);
                }

                Ok(Workspace {
                    cwd: path,
                    isolation,
                    branch: Some(branch),
                    project_root: self.project_root.clone(),
                    _shared_guard: None,
                    git_lock: Arc::clone(&self.git_lock),
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

    /// Same scratch repo, but with a bootstrap attached.
    async fn scratch_repo_with(setup: WorktreeSetup) -> (tempfile::TempDir, Workspaces) {
        let (dir, workspaces) = scratch_repo().await;
        let workspaces = Workspaces::with_setup(workspaces.project_root().to_path_buf(), setup);
        (dir, workspaces)
    }

    #[tokio::test]
    async fn bootstrap_copies_untracked_files_and_runs_setup_commands() {
        let setup = WorktreeSetup {
            copy: vec![".env".into(), "missing.txt".into()],
            setup: vec!["echo installed > deps.txt".into()],
            timeout_secs: 60,
        };
        let (dir, workspaces) = scratch_repo_with(setup).await;

        // Gitignored, so `git worktree add` will not carry it across on its own.
        tokio::fs::write(dir.path().join(".env"), "SECRET=1\n").await.unwrap();

        let workspace = workspaces.prepare("w-boot", Isolation::Worktree).await.unwrap();

        let env = tokio::fs::read_to_string(workspace.cwd.join(".env")).await.unwrap();
        assert_eq!(env, "SECRET=1\n", "declared file should be copied into the worktree");
        assert!(
            workspace.cwd.join("deps.txt").exists(),
            "setup commands should run inside the worktree"
        );
        // A missing entry is skipped, not fatal.
        assert!(!workspace.cwd.join("missing.txt").exists());

        // Copied, not symlinked: editing it in the worktree must not touch the original.
        tokio::fs::write(workspace.cwd.join(".env"), "SECRET=2\n").await.unwrap();
        let original = tokio::fs::read_to_string(dir.path().join(".env")).await.unwrap();
        assert_eq!(original, "SECRET=1\n", "the project's own .env must be untouched");
    }

    #[tokio::test]
    async fn failed_setup_removes_the_half_built_worktree() {
        let setup = WorktreeSetup {
            copy: Vec::new(),
            setup: vec!["exit 3".into()],
            timeout_secs: 60,
        };
        let (dir, workspaces) = scratch_repo_with(setup).await;

        let error = match workspaces.prepare("w-fail", Isolation::Worktree).await {
            Err(error) => error,
            Ok(_) => panic!("a failing setup command must fail the workspace"),
        };
        assert!(error.to_string().contains("exit 3"), "got: {error}");

        // Handing a worker a tree with no dependencies is worse than handing it nothing.
        assert!(
            !dir.path().join(WORKTREE_DIR).join("w-fail").exists(),
            "the worktree should have been torn down"
        );
        let listed = git(dir.path(), &["worktree", "list"]).await.unwrap();
        assert!(!listed.contains("w-fail"), "git should not still track it: {listed}");
    }

    #[tokio::test]
    async fn bootstrap_is_skipped_when_nothing_is_declared() {
        let (_dir, workspaces) = scratch_repo().await;
        assert!(WorktreeSetup::default().is_empty());

        // The default path must stay exactly as it was before bootstrapping existed.
        let workspace = workspaces.prepare("w-plain", Isolation::Worktree).await.unwrap();
        assert!(workspace.cwd.join("README.md").exists());
    }

    #[tokio::test]
    async fn git_exclude_hides_worker_worktrees_and_is_idempotent() {
        let (dir, workspaces) = scratch_repo().await;

        workspaces.ensure_git_exclude().await.unwrap();
        workspaces.ensure_git_exclude().await.unwrap();

        let exclude = tokio::fs::read_to_string(dir.path().join(".git/info/exclude"))
            .await
            .unwrap();
        assert_eq!(
            exclude.lines().filter(|l| l.trim() == ".harness/").count(),
            1,
            "the rule should be written once, not appended on every session: {exclude}"
        );

        // The point of the rule: a worker's worktree must not show up as untracked.
        workspaces.prepare("w-hidden", Isolation::Worktree).await.unwrap();
        let status = git(dir.path(), &["status", "--porcelain"]).await.unwrap();
        assert!(status.is_empty(), "project should still look clean, got: {status}");
    }

    #[tokio::test]
    async fn every_worktree_gets_the_bundled_skills() {
        let (_dir, workspaces) = scratch_repo().await;
        let workspace = workspaces.prepare("w-skills", Isolation::Worktree).await.unwrap();

        for (name, _) in BUNDLED_SKILLS {
            let skill = workspace.cwd.join(SKILLS_DIR).join(name).join("SKILL.md");
            assert!(skill.is_file(), "{name} should be readable by the worker's CLI");
            let body = tokio::fs::read_to_string(&skill).await.unwrap();
            assert!(body.starts_with("---"), "{name} needs frontmatter to be loaded");
            assert!(body.contains(&format!("name: {name}")), "{name} frontmatter is wrong");
        }
    }

    #[tokio::test]
    async fn harness_skills_never_reach_the_workers_diff_or_branch() {
        let (_dir, workspaces) = scratch_repo().await;
        let workspace = workspaces.prepare("w-clean", Isolation::Worktree).await.unwrap();

        tokio::fs::write(workspace.cwd.join("feature.txt"), "real work\n").await.unwrap();

        // The skills are sitting right there; `git add -A` would otherwise sweep them
        // into the branch and land them in the user's repository on merge.
        let stat = workspace.diff().await.unwrap();
        assert_eq!(stat.files, ["feature.txt"], "only the worker's own work should show");

        assert!(workspace.commit("work").await.unwrap());
        let committed = git(&workspace.cwd, &["show", "--name-only", "--format=", "HEAD"])
            .await
            .unwrap();
        assert!(
            !committed.contains(".claude"),
            "harness skills must not be committed: {committed}"
        );
    }

    #[tokio::test]
    async fn a_projects_own_claude_skills_are_still_the_workers_to_commit() {
        let (dir, workspaces) = scratch_repo().await;

        // A project may keep its own skills. Ours are namespaced so theirs keep working
        // — and, crucially, keep being committable.
        let theirs = dir.path().join(SKILLS_DIR).join("project-convention");
        tokio::fs::create_dir_all(&theirs).await.unwrap();
        tokio::fs::write(theirs.join("SKILL.md"), "---\nname: project-convention\n---\n")
            .await
            .unwrap();
        git(dir.path(), &["add", "-A"]).await.unwrap();
        git(dir.path(), &["commit", "-q", "-m", "project skills"]).await.unwrap();

        let workspace = workspaces.prepare("w-both", Isolation::Worktree).await.unwrap();

        // Theirs came across with the checkout; ours sits alongside it.
        assert!(workspace.cwd.join(SKILLS_DIR).join("project-convention/SKILL.md").is_file());
        assert!(workspace.cwd.join(SKILLS_DIR).join("harness-review/SKILL.md").is_file());

        // Editing the project's own skill is ordinary work and must still be landable.
        tokio::fs::write(
            workspace.cwd.join(SKILLS_DIR).join("project-convention/SKILL.md"),
            "---\nname: project-convention\n---\nrevised\n",
        )
        .await
        .unwrap();

        let stat = workspace.diff().await.unwrap();
        assert_eq!(
            stat.files,
            [format!("{SKILLS_DIR}/project-convention/SKILL.md")],
            "the project's own skill is the worker's to change; ours is not"
        );
    }

    /// Two engines — or an engine and a hook process — share nothing in memory, so only
    /// the file lock stands between their worktree bookkeeping. Two independent instances
    /// have independent in-process locks, which is exactly that situation.
    ///
    /// Creation racing *teardown* is what corrupts: reproduced with plain git as
    /// `failed to read .git/worktrees/<id>/commondir` when adds, removes and lists run in
    /// separate processes. So one instance creates while the other tears down.
    ///
    /// Honest limit: this test did not fail without the lock in 10 runs — the race is
    /// timing-dependent and plain git hit it in roughly one round in four under heavier
    /// load. It guards that concurrent create and remove across instances stays correct;
    /// it is not proof the lock is needed. The plain-git reproduction is that proof.
    #[tokio::test]
    async fn separate_instances_can_create_and_remove_worktrees_at_the_same_time() {
        let (dir, first) = scratch_repo().await;
        let second = Workspaces::new(dir.path().to_path_buf());

        let mut old = Vec::new();
        for i in 0..12 {
            old.push(second.prepare(&format!("w-old{i}"), Isolation::Worktree).await.unwrap());
        }

        let mut tasks = Vec::new();
        for (i, workspace) in old.into_iter().enumerate() {
            let first = first.clone();
            tasks.push(tokio::spawn(async move {
                first.prepare(&format!("w-new{i}"), Isolation::Worktree).await.map(|_| ())
            }));
            tasks.push(tokio::spawn(async move { workspace.release().await }));
        }
        for task in tasks {
            task.await.unwrap().expect("no worktree operation should corrupt another");
        }

        let listed = git(dir.path(), &["worktree", "list"]).await.unwrap();
        assert_eq!(listed.lines().count(), 13, "12 new worktrees plus the main checkout");
    }

    #[tokio::test]
    async fn a_worktree_made_elsewhere_can_be_adopted_and_committed() {
        let (dir, workspaces) = scratch_repo().await;
        // As the hook would: same place, same branch naming.
        git(
            dir.path(),
            &["worktree", "add", "-q", "-b", "harness/agent-x1", ".harness/worktrees/agent-x1", "HEAD"],
        )
        .await
        .unwrap();
        tokio::fs::write(dir.path().join(".harness/worktrees/agent-x1/made.txt"), "native\n")
            .await
            .unwrap();

        let adopted = workspaces
            .adopt("agent-x1", Isolation::Worktree)
            .expect("the hook's worktree should be found by name");
        assert_eq!(adopted.mergeable_branch(), Some("harness/agent-x1"));
        assert_eq!(adopted.diff().await.unwrap().files, ["made.txt"]);
        assert!(adopted.commit("native work").await.unwrap());
        adopted.release().await.unwrap();

        // Worktree gone, branch and its commit kept for review.
        assert!(!dir.path().join(".harness/worktrees/agent-x1").exists());
        let shown = git(dir.path(), &["show", "harness/agent-x1:made.txt"]).await.unwrap();
        assert_eq!(shown, "native");

        assert!(workspaces.adopt("agent-missing", Isolation::Worktree).is_none());
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
    async fn many_worktrees_can_be_prepared_concurrently() {
        // Fanning builders out is the normal case for `delegate_async`, and concurrent
        // `git worktree add` against one repository used to corrupt git's bookkeeping:
        // `fatal: failed to read .git/worktrees/<id>/commondir`. Creation is serialized
        // now; this is the regression guard.
        let (_dir, ws) = scratch_repo().await;

        let mut handles = Vec::new();
        for n in 0..8 {
            let ws = ws.clone();
            handles.push(tokio::spawn(async move {
                let workspace = ws.prepare(&format!("w{n}"), Isolation::Worktree).await?;
                tokio::fs::write(workspace.cwd.join("f.txt"), format!("{n}")).await?;
                workspace.commit(&format!("worker {n}")).await?;
                let branch = workspace.branch.clone().unwrap();
                workspace.release().await?;
                anyhow::Ok(branch)
            }));
        }

        let mut branches = Vec::new();
        for handle in handles {
            branches.push(handle.await.unwrap().expect("worktree lifecycle must not race"));
        }

        branches.sort();
        branches.dedup();
        assert_eq!(branches.len(), 8, "every worker should get its own branch");
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
