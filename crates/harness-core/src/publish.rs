//! Taking a worker's change to GitHub: push its branch and open a pull request.
//!
//! With the GitHub CLI (`gh`) installed and signed in, the pull request is opened
//! directly. Without it, the branch is still pushed with the person's own git credentials,
//! and GitHub's "open a pull request" page is handed back to open in the browser, filled
//! in. Nothing here runs through a shell: every argument is passed as-is.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// A GitHub repository, from a remote's URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GithubRepo {
    pub owner: String,
    pub repo: String,
}

/// Where the branch will go.
#[derive(Debug, Clone, Serialize)]
pub struct Remote {
    pub name: String,
    pub url: String,
    pub github: Option<GithubRepo>,
}

/// Whether the GitHub CLI can open the pull request itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GhState {
    Missing,
    SignedOut,
    Ready,
}

/// The pull request as the person will send it, editable before it goes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrDraft {
    pub title: String,
    pub body: String,
    /// The branch name to push to on the remote.
    pub remote_branch: String,
    /// The branch the pull request asks to merge into.
    pub base: String,
    pub draft: bool,
}

/// Everything the dialog needs to offer a pull request.
#[derive(Debug, Clone, Serialize)]
pub struct PrPlan {
    pub suggested: PrDraft,
    pub remote: Option<Remote>,
    pub gh: GhState,
}

/// How a pull request came to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenedVia {
    /// Opened by `gh`.
    Gh,
    /// Branch pushed; the pull request page is waiting in the browser.
    Browser,
    /// Branch pushed to a remote that isn't GitHub.
    PushedOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    pub url: String,
    pub number: Option<u64>,
    pub remote_branch: String,
    pub via: OpenedVia,
}

/// Checks and state of an open pull request, as `gh` reports them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PrStatus {
    pub state: String,
    pub draft: bool,
    pub passed: usize,
    pub failed: usize,
    pub pending: usize,
    /// Names of the checks that failed.
    pub failing: Vec<String>,
}

async fn run(cwd: &Path, program: &str, args: &[&str], stdin: Option<&str>) -> Result<String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        // A push must never sit waiting for a password nobody can type.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(if stdin.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .with_context(|| format!("running {program} — is it installed?"))?;
    if let (Some(text), Some(mut input)) = (stdin, child.stdin.take()) {
        input.write_all(text.as_bytes()).await?;
    }
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        child.wait_with_output(),
    )
    .await
    .with_context(|| format!("{program} took longer than two minutes"))??;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let out = String::from_utf8_lossy(&output.stdout).trim().to_string();
        bail!("{}", if err.is_empty() { out } else { err });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// `owner/repo` from the ways GitHub remotes are written.
pub fn parse_github(url: &str) -> Option<GithubRepo> {
    let url = url.trim();
    let rest = url
        .strip_prefix("git@github.com:")
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))
        .or_else(|| url.strip_prefix("https://github.com/"))
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| {
            // https://user@github.com/owner/repo
            let after = url.strip_prefix("https://")?;
            let (_, path) = after.split_once("@github.com/")?;
            Some(path)
        })?;
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let (owner, repo) = rest.split_once('/')?;
    let ok = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    (ok(owner) && ok(repo) && !repo.contains('/')).then(|| GithubRepo {
        owner: owner.to_string(),
        repo: repo.to_string(),
    })
}

/// The project's remote: `origin` if there is one, else the only one.
pub async fn remote(project: &Path) -> Result<Option<Remote>> {
    let names = run(project, "git", &["remote"], None).await?;
    let names: Vec<&str> = names
        .lines()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .collect();
    let name = if names.contains(&"origin") {
        "origin"
    } else if names.len() == 1 {
        names[0]
    } else {
        return Ok(None);
    };
    let url = run(project, "git", &["remote", "get-url", name], None).await?;
    Ok(Some(Remote {
        name: name.to_string(),
        github: parse_github(&url),
        url,
    }))
}

/// The branch the checkout is on, which is what a worker branched from.
pub async fn current_branch(project: &Path) -> Result<Option<String>> {
    let name = run(project, "git", &["rev-parse", "--abbrev-ref", "HEAD"], None).await?;
    Ok((name != "HEAD" && !name.is_empty()).then_some(name))
}

pub async fn gh_state(project: &Path) -> GhState {
    match run(project, "gh", &["--version"], None).await {
        Err(_) => GhState::Missing,
        Ok(_) => match run(project, "gh", &["auth", "status"], None).await {
            Ok(_) => GhState::Ready,
            Err(_) => GhState::SignedOut,
        },
    }
}

/// A branch name for the remote: `harness/<role>-<first words of the task>`.
pub fn suggest_branch(role: &str, task: &str) -> String {
    let words: Vec<String> = task
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .take(6)
        .map(|w| w.to_ascii_lowercase())
        .collect();
    let role: String = role
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let mut name = format!("harness/{}", role.trim_matches('-'));
    if !words.is_empty() {
        name.push('-');
        name.push_str(&words.join("-"));
    }
    name.chars()
        .take(60)
        .collect::<String>()
        .trim_end_matches('-')
        .to_string()
}

/// A title from the task's first line.
pub fn suggest_title(task: &str) -> String {
    let line = task
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("Changes from Harness");
    let mut title: String = line.chars().take(72).collect();
    if title.len() < line.len() {
        title.push('…');
    }
    title
}

/// A description: what it was asked, what it says it did, what changed, how it checked out.
pub fn suggest_body(
    task: &str,
    summary: &str,
    files: &[String],
    checks: Option<&crate::verify::VerificationReport>,
) -> String {
    let mut body = String::new();
    body.push_str("## What was asked\n\n");
    body.push_str(task.trim());
    if !summary.trim().is_empty() {
        body.push_str("\n\n## What changed\n\n");
        body.push_str(summary.trim());
    }
    if !files.is_empty() {
        body.push_str("\n\n## Files\n\n");
        for file in files.iter().take(40) {
            body.push_str(&format!("- `{file}`\n"));
        }
        if files.len() > 40 {
            body.push_str(&format!("- …and {} more\n", files.len() - 40));
        }
    }
    if let Some(report) = checks {
        body.push_str(&format!(
            "\n\n## Checks\n\n{} risk{}\n",
            report.risk.as_str(),
            if report.verified { "" } else { " (unverified)" }
        ));
        for check in &report.checks {
            body.push_str(&format!("- {}: {}\n", check.name, check.summary));
        }
    }
    body.push_str("\n\n---\n_Made with Harness._\n");
    body
}

/// Refuse anything that isn't a plain branch name — no options, no ref tricks.
async fn check_branch_name(project: &Path, name: &str, what: &str) -> Result<()> {
    if name.is_empty() || name.starts_with('-') || name.contains(char::is_whitespace) {
        bail!("`{name}` isn't a usable {what} name");
    }
    run(
        project,
        "git",
        &["check-ref-format", "--branch", name],
        None,
    )
    .await
    .with_context(|| format!("`{name}` isn't a usable {what} name"))?;
    Ok(())
}

/// Push `local_branch` (a harness branch) and open a pull request for it.
pub async fn open(project: &Path, local_branch: &str, draft: &PrDraft) -> Result<PullRequest> {
    if !local_branch.starts_with("harness/") {
        bail!("refusing to push `{local_branch}`: not a harness branch");
    }
    check_branch_name(project, &draft.remote_branch, "branch").await?;
    check_branch_name(project, &draft.base, "base branch").await?;
    if draft.title.trim().is_empty() {
        bail!("the pull request needs a title");
    }
    let remote = remote(project).await?.context(
        "this project has no remote to push to — add one with `git remote add origin <url>`",
    )?;

    let refspec = format!("{local_branch}:refs/heads/{}", draft.remote_branch);
    run(
        project,
        "git",
        &["push", "--", &remote.name, &refspec],
        None,
    )
    .await
    .map_err(|err| {
        anyhow::anyhow!(
            "pushing to {} failed: {err:#}. If git needs signing in, `gh auth login` \
                 (then `gh auth setup-git`) sets it up.",
            remote.name
        )
    })?;

    let Some(github) = &remote.github else {
        return Ok(PullRequest {
            url: remote.url.clone(),
            number: None,
            remote_branch: draft.remote_branch.clone(),
            via: OpenedVia::PushedOnly,
        });
    };

    if gh_state(project).await == GhState::Ready {
        let repo = format!("--repo={}/{}", github.owner, github.repo);
        let head = format!("--head={}", draft.remote_branch);
        let base = format!("--base={}", draft.base);
        let title = format!("--title={}", draft.title.trim());
        let mut args = vec![
            "pr",
            "create",
            &repo,
            &head,
            &base,
            &title,
            "--body-file",
            "-",
        ];
        if draft.draft {
            args.push("--draft");
        }
        let out = run(project, "gh", &args, Some(&draft.body)).await?;
        let url = out
            .lines()
            .rev()
            .find(|l| l.starts_with("https://"))
            .unwrap_or(out.as_str())
            .trim()
            .to_string();
        return Ok(PullRequest {
            number: url.rsplit('/').next().and_then(|n| n.parse().ok()),
            url,
            remote_branch: draft.remote_branch.clone(),
            via: OpenedVia::Gh,
        });
    }

    Ok(PullRequest {
        url: compare_url(github, draft),
        number: None,
        remote_branch: draft.remote_branch.clone(),
        via: OpenedVia::Browser,
    })
}

/// GitHub's page for opening a pull request, filled in.
pub fn compare_url(github: &GithubRepo, draft: &PrDraft) -> String {
    let base = format!(
        "https://github.com/{}/{}/compare/{}...{}",
        github.owner, github.repo, draft.base, draft.remote_branch
    );
    // A very long body makes an unusable URL; the page can take the rest by hand.
    let body: String = draft.body.chars().take(6000).collect();
    let mut params = vec![
        ("expand", "1"),
        ("title", draft.title.trim()),
        ("body", body.as_str()),
    ];
    if draft.draft {
        params.push(("draft", "1"));
    }
    reqwest::Url::parse_with_params(&base, &params)
        .map(|u| u.to_string())
        .unwrap_or(base)
}

/// An open pull request's state and checks.
pub async fn status(project: &Path, url: &str) -> Result<PrStatus> {
    if !url.starts_with("https://github.com/") {
        bail!("not a GitHub pull request");
    }
    let out = run(
        project,
        "gh",
        &[
            "pr",
            "view",
            url,
            "--json",
            "state,isDraft,statusCheckRollup",
        ],
        None,
    )
    .await?;
    Ok(parse_status(&serde_json::from_str(&out)?))
}

fn parse_status(view: &serde_json::Value) -> PrStatus {
    let mut status = PrStatus {
        state: view["state"].as_str().unwrap_or("UNKNOWN").to_string(),
        draft: view["isDraft"].as_bool().unwrap_or(false),
        ..Default::default()
    };
    for check in view["statusCheckRollup"].as_array().into_iter().flatten() {
        // Check runs carry status/conclusion; commit statuses carry state.
        let conclusion = check["conclusion"]
            .as_str()
            .or_else(|| check["state"].as_str())
            .unwrap_or("")
            .to_ascii_uppercase();
        let running = matches!(
            check["status"].as_str().unwrap_or("COMPLETED"),
            "QUEUED" | "IN_PROGRESS" | "PENDING" | "WAITING" | "REQUESTED"
        ) || conclusion == "PENDING"
            || conclusion == "EXPECTED";
        let name = check["name"]
            .as_str()
            .or_else(|| check["context"].as_str())
            .unwrap_or("check")
            .to_string();
        if running {
            status.pending += 1;
        } else if matches!(conclusion.as_str(), "SUCCESS" | "NEUTRAL" | "SKIPPED") {
            status.passed += 1;
        } else {
            status.failed += 1;
            status.failing.push(name);
        }
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_remotes_in_every_spelling() {
        let want = Some(GithubRepo {
            owner: "Zavier-AK".into(),
            repo: "local-harness-".into(),
        });
        for url in [
            "git@github.com:Zavier-AK/local-harness-.git",
            "https://github.com/Zavier-AK/local-harness-.git",
            "https://github.com/Zavier-AK/local-harness-",
            "https://github.com/Zavier-AK/local-harness-/",
            "ssh://git@github.com/Zavier-AK/local-harness-.git",
            "https://token@github.com/Zavier-AK/local-harness-.git",
        ] {
            assert_eq!(parse_github(url), want, "{url}");
        }
        assert_eq!(parse_github("https://gitlab.com/a/b.git"), None);
        assert_eq!(parse_github("https://github.com/a/b/c"), None);
        assert_eq!(parse_github("https://github.com/a/b;rm"), None);
    }

    #[test]
    fn suggestions_are_tidy() {
        assert_eq!(
            suggest_branch("builder", "Add retries to the fetcher, with backoff!"),
            "harness/builder-add-retries-to-the-fetcher-with"
        );
        assert_eq!(suggest_branch("Test Writer", ""), "harness/test-writer");
        assert!(suggest_branch("b", &"word ".repeat(40)).len() <= 60);
        assert_eq!(
            suggest_title("\n  Fix the login bug\nmore detail"),
            "Fix the login bug"
        );
        assert!(suggest_title(&"x".repeat(100)).ends_with('…'));
        let body = suggest_body("Do it", "Done it", &["a.rs".into()], None);
        assert!(body.contains("## What was asked\n\nDo it"));
        assert!(body.contains("- `a.rs`"));
    }

    #[test]
    fn the_browser_page_is_filled_in() {
        let url = compare_url(
            &GithubRepo {
                owner: "o".into(),
                repo: "r".into(),
            },
            &PrDraft {
                title: "Fix & test".into(),
                body: "line one\nline two".into(),
                remote_branch: "harness/builder-fix".into(),
                base: "main".into(),
                draft: true,
            },
        );
        assert!(url.starts_with("https://github.com/o/r/compare/main...harness/builder-fix?"));
        assert!(url.contains("title=Fix+%26+test"));
        assert!(url.contains("body=line+one%0Aline+two"));
        assert!(url.contains("draft=1"));
    }

    #[test]
    fn checks_are_counted() {
        let view = serde_json::json!({
            "state": "OPEN",
            "isDraft": false,
            "statusCheckRollup": [
                {"name": "build", "status": "COMPLETED", "conclusion": "SUCCESS"},
                {"name": "lint", "status": "COMPLETED", "conclusion": "FAILURE"},
                {"name": "test", "status": "IN_PROGRESS", "conclusion": ""},
                {"context": "ci/legacy", "state": "PENDING"},
                {"context": "deploy", "state": "SUCCESS"}
            ]
        });
        let status = parse_status(&view);
        assert_eq!((status.passed, status.failed, status.pending), (2, 1, 2));
        assert_eq!(status.failing, vec!["lint".to_string()]);
        assert_eq!(status.state, "OPEN");
    }

    async fn repo_with_remote() -> (tempfile::TempDir, tempfile::TempDir) {
        let remote = tempfile::tempdir().unwrap();
        run(remote.path(), "git", &["init", "-q", "--bare"], None)
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "t@t"],
            vec!["config", "user.name", "T"],
        ] {
            run(root, "git", &args, None).await.unwrap();
        }
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        run(root, "git", &["add", "-A"], None).await.unwrap();
        run(root, "git", &["commit", "-q", "-m", "init"], None)
            .await
            .unwrap();
        run(root, "git", &["branch", "harness/w-1"], None)
            .await
            .unwrap();
        let url = remote.path().display().to_string();
        run(root, "git", &["remote", "add", "origin", &url], None)
            .await
            .unwrap();
        (dir, remote)
    }

    #[tokio::test]
    async fn a_branch_is_pushed_under_its_new_name() {
        let (dir, remote) = repo_with_remote().await;
        assert_eq!(
            current_branch(dir.path()).await.unwrap().as_deref(),
            Some("main")
        );
        let draft = PrDraft {
            title: "T".into(),
            body: "B".into(),
            remote_branch: "harness/builder-thing".into(),
            base: "main".into(),
            draft: false,
        };
        let pr = open(dir.path(), "harness/w-1", &draft).await.unwrap();
        assert_eq!(pr.via, OpenedVia::PushedOnly);
        let refs = run(remote.path(), "git", &["branch", "--list"], None)
            .await
            .unwrap();
        assert!(refs.contains("harness/builder-thing"), "{refs}");

        // Never anything but a harness branch, and never a name that reads as an option.
        assert!(open(dir.path(), "main", &draft).await.is_err());
        let sneaky = PrDraft {
            remote_branch: "--force".into(),
            ..draft.clone()
        };
        assert!(open(dir.path(), "harness/w-1", &sneaky).await.is_err());
        let spaced = PrDraft {
            remote_branch: "a b".into(),
            ..draft
        };
        assert!(open(dir.path(), "harness/w-1", &spaced).await.is_err());
    }
}
