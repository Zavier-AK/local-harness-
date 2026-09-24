//! Checking a worker's branch before its merge is put in front of a person.
//!
//! Three kinds of check, cheapest first:
//!
//! * **Signals** — deterministic, free, always run: how big the change is, whether it
//!   touches paths where a mistake is expensive (migrations, auth, CI, lockfiles), whether
//!   it deletes files, whether code changed with no test touched.
//! * **Commands** — the project's own checks (`[verify] commands` in `roles.toml`), run in
//!   a fresh checkout of exactly what would be merged.
//! * **Review** — a different agent reads the change and returns a verdict. A first,
//!   ideally cheap, reviewer runs on everything; a second only when the first calls the
//!   change medium or high risk, so the expensive model is spent where it matters.
//!
//! The result is a risk level with its reasons. The one rule it never breaks: a change
//! nothing actually checked is reported as **unverified**, not as low risk.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

use crate::agents::{self, WorkerSpec};
use crate::event::{DiffStat, Usage};
use crate::roles::Role;

/// How much of the diff a reviewer is shown. Enough for any reviewable change; a worker
/// that rewrote a lockfile should not blow the reviewer's context.
pub const REVIEW_PATCH_LINES: usize = 1500;

/// How much command output is kept for the card.
const OUTPUT_TAIL_LINES: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    Low,
    Medium,
    High,
}

impl Risk {
    fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "low" => Some(Self::Low),
            "medium" | "med" | "moderate" => Some(Self::Medium),
            "high" | "critical" => Some(Self::High),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckKind {
    Signals,
    Command,
    Review,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Passed,
    Failed,
    /// Not run, and the summary says why — no reviewer configured, or it is unavailable.
    Skipped,
    /// Ran, but produced nothing usable: a checkout that failed, a verdict that would not
    /// parse. Distinct from `Failed`, which means the check ran and found a problem.
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub severity: String,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub line: Option<u64>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Check {
    pub kind: CheckKind,
    pub name: String,
    pub status: CheckStatus,
    pub summary: String,
    /// The tail of a command's output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// A signal's or reviewer's verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk: Option<Risk>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<Finding>,
    /// Who reviewed: `role (provider/model)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerificationReport {
    pub risk: Risk,
    /// False when no command ran and no reviewer returned a verdict. The card says
    /// "unverified" then, whatever the risk level.
    pub verified: bool,
    /// Why the risk is what it is, most important first.
    pub reasons: Vec<String>,
    pub checks: Vec<Check>,
}

// ------------------------------------------------------------------- signals

/// A deterministic observation about the diff.
#[derive(Debug, Clone, PartialEq)]
pub struct Signal {
    pub risk: Risk,
    pub reason: String,
}

/// Paths where a mistake is expensive enough to deserve a careful look whatever the
/// reviewer says.
fn high_risk_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    name.starts_with(".env")
        || lower.contains("migration")
        || lower.starts_with(".github/workflows/")
        || name == "roles.toml"
        || lower.starts_with(".claude/")
        || [
            "auth",
            "security",
            "secret",
            "credential",
            "password",
            "crypto",
            "permission",
        ]
        .iter()
        .any(|word| lower.contains(word))
}

/// Paths that change what the project depends on or how it is built.
fn build_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    [
        "cargo.lock",
        "cargo.toml",
        "package-lock.json",
        "package.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "poetry.lock",
        "uv.lock",
        "pyproject.toml",
        "go.mod",
        "go.sum",
        "gemfile",
        "gemfile.lock",
        "dockerfile",
        ".gitlab-ci.yml",
        "makefile",
    ]
    .contains(&name.as_str())
        || (name.starts_with("requirements") && name.ends_with(".txt"))
}

const CODE_EXTENSIONS: &[&str] = &[
    "rs", "ts", "tsx", "js", "jsx", "mjs", "py", "go", "java", "kt", "swift", "rb", "c", "cc",
    "cpp", "h", "hpp", "cs", "php", "scala", "ex", "exs",
];

fn is_code(path: &str) -> bool {
    path.rsplit_once('.')
        .is_some_and(|(_, ext)| CODE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

fn is_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    lower.contains("/tests/")
        || lower.starts_with("tests/")
        || lower.contains("/test/")
        || lower.starts_with("test/")
        || lower.contains("__tests__")
        || name.starts_with("test_")
        || name.contains("_test.")
        || name.contains(".test.")
        || name.contains(".spec.")
}

/// Whether the patch adds a test inline — Rust keeps unit tests beside the code, so a
/// path-only check would call every Rust change untested.
fn patch_adds_tests(patch: &str) -> bool {
    patch
        .lines()
        .filter(|line| line.starts_with('+'))
        .any(|line| {
            let line = line.trim_start_matches('+').trim();
            line.starts_with("#[test]")
                || line.starts_with("#[tokio::test")
                || line.starts_with("def test_")
                || line.starts_with("it(")
                || line.starts_with("test(")
                || line.starts_with("describe(")
                || line.starts_with("func Test")
        })
}

pub fn signals(diff: &DiffStat, patch: &str) -> Vec<Signal> {
    let mut found = Vec::new();

    let lines = diff.insertions + diff.deletions;
    if lines > 800 {
        found.push(Signal {
            risk: Risk::High,
            reason: format!(
                "large change: {lines} lines across {} files",
                diff.files_changed
            ),
        });
    } else if lines > 250 || diff.files_changed > 15 {
        found.push(Signal {
            risk: Risk::Medium,
            reason: format!(
                "sizeable change: {lines} lines across {} files",
                diff.files_changed
            ),
        });
    }

    let sensitive: Vec<&str> = diff
        .files
        .iter()
        .map(String::as_str)
        .filter(|p| high_risk_path(p))
        .collect();
    if !sensitive.is_empty() {
        found.push(Signal {
            risk: Risk::High,
            reason: format!("touches sensitive paths: {}", list(&sensitive)),
        });
    }

    let build: Vec<&str> = diff
        .files
        .iter()
        .map(String::as_str)
        .filter(|p| build_path(p))
        .collect();
    if !build.is_empty() {
        found.push(Signal {
            risk: Risk::Medium,
            reason: format!("changes dependencies or build config: {}", list(&build)),
        });
    }

    let deleted = patch
        .lines()
        .filter(|l| l.starts_with("deleted file mode"))
        .count();
    if deleted > 0 {
        found.push(Signal {
            risk: Risk::Medium,
            reason: format!(
                "deletes {deleted} file{}",
                if deleted == 1 { "" } else { "s" }
            ),
        });
    }

    if patch.lines().any(|l| l.starts_with("Binary files ")) {
        found.push(Signal {
            risk: Risk::Medium,
            reason: "adds or changes a binary file".into(),
        });
    }

    let code_changed = diff.files.iter().any(|p| is_code(p) && !is_test_path(p));
    let tests_touched = diff.files.iter().any(|p| is_test_path(p)) || patch_adds_tests(patch);
    if code_changed && !tests_touched {
        found.push(Signal {
            risk: Risk::Medium,
            reason: "changes code without touching any test".into(),
        });
    }

    found
}

fn list(paths: &[&str]) -> String {
    let shown: Vec<&str> = paths.iter().take(3).copied().collect();
    let more = paths.len().saturating_sub(shown.len());
    if more > 0 {
        format!("{} and {more} more", shown.join(", "))
    } else {
        shown.join(", ")
    }
}

pub fn signals_check(signals: &[Signal]) -> Check {
    let risk = signals.iter().map(|s| s.risk).max().unwrap_or(Risk::Low);
    Check {
        kind: CheckKind::Signals,
        name: "Change shape".into(),
        status: CheckStatus::Passed,
        summary: if signals.is_empty() {
            "small, touches nothing sensitive".into()
        } else {
            signals
                .iter()
                .map(|s| s.reason.clone())
                .collect::<Vec<_>>()
                .join("; ")
        },
        output: None,
        risk: Some(risk),
        findings: Vec::new(),
        reviewer: None,
    }
}

// ------------------------------------------------------------------ commands

/// Run one of the project's checks in the checkout.
pub async fn run_command(cwd: &Path, command: &str, timeout: Duration) -> Check {
    let child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        // A check that hits the timeout must not keep running behind our back.
        .kill_on_drop(true)
        .output();

    let (status, summary, output) = match tokio::time::timeout(timeout, child).await {
        Err(_) => (
            CheckStatus::Failed,
            format!("timed out after {}s", timeout.as_secs()),
            None,
        ),
        Ok(Err(err)) => (CheckStatus::Error, format!("could not run: {err}"), None),
        Ok(Ok(out)) => {
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            let tail = tail(&text, OUTPUT_TAIL_LINES);
            if out.status.success() {
                (CheckStatus::Passed, "passed".to_string(), Some(tail))
            } else {
                let code = out
                    .status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or("a signal".into());
                (
                    CheckStatus::Failed,
                    format!("failed (exit {code})"),
                    Some(tail),
                )
            }
        }
    };

    Check {
        kind: CheckKind::Command,
        name: command.to_string(),
        status,
        summary,
        output: output.filter(|o| !o.trim().is_empty()),
        risk: (status == CheckStatus::Failed).then_some(Risk::High),
        findings: Vec::new(),
        reviewer: None,
    }
}

fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

// -------------------------------------------------------------------- review

/// What the reviewer is asked to return. Written so it is not itself valid JSON, which
/// keeps a reviewer that merely echoes its prompt from being read as a verdict.
const CONTRACT: &str = r#"Reply with ONLY one JSON object and nothing else, in this shape:
{"risk_level": "low" | "medium" | "high", "summary": "<one sentence>", "findings": [{"severity": "low" | "medium" | "high", "file": "<path>", "line": <number or null>, "message": "<what is wrong and why it matters>"}]}

risk_level: high = likely broken, unsafe, or loses data; medium = plausible bugs, or missing tests worth a look; low = safe to land as it is. Report only what matters: no style nits, no praise."#;

pub struct ReviewRequest<'a> {
    pub task: &'a str,
    pub summary: &'a str,
    pub patch: &'a str,
    pub patch_truncated: bool,
    /// Whether the reviewer runs in the checkout and can read files beyond the diff.
    pub has_checkout: bool,
}

pub fn review_prompt(request: &ReviewRequest) -> String {
    let mut prompt = String::from(
        "You are reviewing a change another agent made, before a person decides whether to \
         merge it. You did not write it. Look for what would actually go wrong.\n\n",
    );
    prompt.push_str(&format!(
        "The task it was given:\n{}\n\n",
        request.task.trim()
    ));
    if !request.summary.trim().is_empty() {
        prompt.push_str(&format!(
            "What it said it did:\n{}\n\n",
            request.summary.trim()
        ));
    }
    prompt.push_str(&format!(
        "The diff{}:\n```diff\n{}\n```\n\n",
        if request.patch_truncated {
            " (truncated — the full change is larger)"
        } else {
            ""
        },
        request.patch.trim_end()
    ));
    if request.has_checkout {
        prompt.push_str(
            "The changed branch is checked out in your working directory. Read files there if \
             the diff is not enough. Do not modify anything.\n\n",
        );
    }
    prompt.push_str(CONTRACT);
    prompt
}

#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub risk: Risk,
    pub summary: String,
    pub findings: Vec<Finding>,
}

/// The first JSON object in `text` that reads as a verdict. Models wrap JSON in prose
/// and code fences however they are asked, so the whole text is searched rather than
/// parsed as-is.
pub fn parse_verdict(text: &str) -> Option<Verdict> {
    #[derive(Deserialize)]
    struct Raw {
        risk_level: String,
        #[serde(default)]
        summary: String,
        #[serde(default)]
        findings: Vec<RawFinding>,
    }
    #[derive(Deserialize)]
    struct RawFinding {
        #[serde(default)]
        severity: Option<String>,
        #[serde(default)]
        file: Option<String>,
        #[serde(default)]
        line: Option<serde_json::Value>,
        #[serde(default)]
        message: String,
    }

    for (start, _) in text.match_indices('{') {
        let Some(end) = matching_brace(&text[start..]) else {
            continue;
        };
        let Ok(raw) = serde_json::from_str::<Raw>(&text[start..start + end + 1]) else {
            continue;
        };
        let Some(risk) = Risk::parse(&raw.risk_level) else {
            continue;
        };
        return Some(Verdict {
            risk,
            summary: raw.summary.trim().to_string(),
            findings: raw
                .findings
                .into_iter()
                .filter(|f| !f.message.trim().is_empty())
                .map(|f| Finding {
                    severity: f
                        .severity
                        .as_deref()
                        .and_then(Risk::parse)
                        .unwrap_or(Risk::Medium)
                        .as_str()
                        .to_string(),
                    file: f.file.filter(|p| !p.trim().is_empty()),
                    line: f
                        .line
                        .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok())),
                    message: f.message.trim().to_string(),
                })
                .collect(),
        });
    }
    None
}

/// Where the object starting at `text[0]` ends, skipping braces inside strings.
fn matching_brace(text: &str) -> Option<usize> {
    let (mut depth, mut in_string, mut escaped) = (0usize, false, false);
    for (i, c) in text.char_indices() {
        if in_string {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

pub fn reviewer_label(role_name: &str, role: &Role) -> String {
    match &role.model {
        Some(model) => format!("{role_name} ({}/{model})", role.provider.as_str()),
        None => format!("{role_name} ({})", role.provider.as_str()),
    }
}

/// Ask one reviewer for a verdict. A reply that will not parse gets one retry with a
/// reminder, then becomes an `Error` check rather than a guess.
///
/// Returns the check and the usage it cost, so the caller can put it in the meter.
pub async fn review(
    role_name: &str,
    role: &Role,
    run_id: &str,
    cwd: &Path,
    prompt: &str,
) -> (Check, Usage) {
    let label = reviewer_label(role_name, role);
    let availability = crate::availability::probe(role).await;
    if !availability.available {
        return (
            Check {
                kind: CheckKind::Review,
                name: format!("Review by {role_name}"),
                status: CheckStatus::Skipped,
                summary: format!(
                    "skipped: {}",
                    availability
                        .reason
                        .unwrap_or_else(|| "reviewer unavailable".into())
                ),
                output: None,
                risk: None,
                findings: Vec::new(),
                reviewer: Some(label),
            },
            Usage::default(),
        );
    }

    // The reviewer's own events are not the rail's business: it is part of a merge
    // card, not a worker. Its usage is still recorded by the caller.
    let (sink, _quiet) = tokio::sync::mpsc::unbounded_channel();
    let mut usage = Usage::default();
    let mut last_error = String::new();

    for attempt in 0..2 {
        let task = if attempt == 0 {
            prompt.to_string()
        } else {
            format!(
                "{prompt}\n\nYour previous reply could not be read as that JSON object. Reply \
                 with the JSON object only."
            )
        };
        let spec = WorkerSpec {
            run_id: format!("{run_id}-{attempt}"),
            role_name: role_name.to_string(),
            role: role.clone(),
            task,
            cwd: cwd.to_path_buf(),
            context_files: Vec::new(),
            extras: Default::default(),
        };
        match agents::run_worker(&spec, &sink).await {
            Ok(outcome) => {
                usage.input_tokens += outcome.usage.input_tokens;
                usage.output_tokens += outcome.usage.output_tokens;
                usage.cache_creation_input_tokens += outcome.usage.cache_creation_input_tokens;
                usage.cache_read_input_tokens += outcome.usage.cache_read_input_tokens;
                if outcome.is_error {
                    last_error = first_line(&outcome.text);
                    continue;
                }
                if let Some(verdict) = parse_verdict(&outcome.text) {
                    let status = if verdict.risk == Risk::High {
                        CheckStatus::Failed
                    } else {
                        CheckStatus::Passed
                    };
                    return (
                        Check {
                            kind: CheckKind::Review,
                            name: format!("Review by {role_name}"),
                            status,
                            summary: if verdict.summary.is_empty() {
                                format!("{} risk", verdict.risk.as_str())
                            } else {
                                verdict.summary.clone()
                            },
                            output: None,
                            risk: Some(verdict.risk),
                            findings: verdict.findings,
                            reviewer: Some(label),
                        },
                        usage,
                    );
                }
                last_error = "the reply was not the requested JSON".into();
            }
            Err(err) => last_error = format!("{err:#}"),
        }
    }

    (
        Check {
            kind: CheckKind::Review,
            name: format!("Review by {role_name}"),
            status: CheckStatus::Error,
            summary: format!("no usable verdict: {last_error}"),
            output: None,
            risk: None,
            findings: Vec::new(),
            reviewer: Some(label),
        },
        usage,
    )
}

fn first_line(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(200)
        .collect()
}

// ---------------------------------------------------------------------- risk

/// The overall verdict. The highest risk anything found wins; a failed command is high
/// on its own. A later reviewer's verdict replaces an earlier one's, since the second
/// pass only runs to overrule the first.
pub fn assess(checks: Vec<Check>) -> VerificationReport {
    let mut reasons = Vec::new();
    let mut risk = Risk::Low;

    for check in checks.iter().filter(|c| c.kind == CheckKind::Command) {
        if matches!(check.status, CheckStatus::Failed | CheckStatus::Error) {
            risk = Risk::High;
            reasons.push(format!("`{}` {}", check.name, check.summary));
        }
    }

    let verdict = checks
        .iter()
        .rfind(|c| c.kind == CheckKind::Review && c.risk.is_some());
    if let Some(review) = verdict {
        let review_risk = review.risk.unwrap_or(Risk::Low);
        risk = risk.max(review_risk);
        if review_risk > Risk::Low {
            reasons.push(format!(
                "{}: {}",
                review.reviewer.as_deref().unwrap_or("reviewer"),
                review.summary
            ));
        }
    }

    for check in checks.iter().filter(|c| c.kind == CheckKind::Signals) {
        let signal_risk = check.risk.unwrap_or(Risk::Low);
        risk = risk.max(signal_risk);
        if signal_risk > Risk::Low {
            reasons.push(check.summary.clone());
        }
    }

    let ran_command = checks.iter().any(|c| {
        c.kind == CheckKind::Command
            && matches!(c.status, CheckStatus::Passed | CheckStatus::Failed)
    });
    let verified = ran_command || verdict.is_some();
    if !verified {
        reasons.push(
            "nothing checked this change: add commands or a reviewer under [verify] in roles.toml"
                .into(),
        );
    }

    VerificationReport {
        risk,
        verified,
        reasons,
        checks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff(files: &[&str], lines: usize) -> DiffStat {
        DiffStat {
            files_changed: files.len(),
            insertions: lines,
            deletions: 0,
            files: files.iter().map(|f| f.to_string()).collect(),
        }
    }

    fn reasons(signals: &[Signal]) -> String {
        signals
            .iter()
            .map(|s| s.reason.as_str())
            .collect::<Vec<_>>()
            .join(" | ")
    }

    #[test]
    fn a_small_docs_change_raises_nothing() {
        assert!(signals(&diff(&["README.md"], 4), "+more words").is_empty());
    }

    #[test]
    fn sensitive_paths_are_high_risk() {
        for path in [
            "db/migrations/0003_users.sql",
            "src/auth/session.rs",
            ".env.production",
            ".github/workflows/release.yml",
            "roles.toml",
        ] {
            let found = signals(&diff(&[path], 3), "");
            assert!(
                found.iter().any(|s| s.risk == Risk::High),
                "{path}: {}",
                reasons(&found)
            );
        }
    }

    #[test]
    fn dependency_changes_are_medium() {
        let found = signals(&diff(&["Cargo.lock", "docs/x.md"], 10), "");
        assert!(found
            .iter()
            .any(|s| s.risk == Risk::Medium && s.reason.contains("Cargo.lock")));
    }

    #[test]
    fn size_scales_the_risk() {
        assert!(signals(&diff(&["a.md"], 300), "")
            .iter()
            .any(|s| s.risk == Risk::Medium));
        assert!(signals(&diff(&["a.md"], 900), "")
            .iter()
            .any(|s| s.risk == Risk::High));
    }

    #[test]
    fn code_without_tests_is_flagged_unless_a_test_came_with_it() {
        let untested = signals(&diff(&["src/lib.rs"], 10), "+fn add() {}");
        assert!(reasons(&untested).contains("without touching any test"));

        // A test file alongside, or a Rust test added inline, both count.
        assert!(signals(&diff(&["src/app.ts", "src/app.test.ts"], 10), "").is_empty());
        assert!(signals(
            &diff(&["src/lib.rs"], 10),
            "+fn add() {}\n+    #[test]\n+    fn adds() {}"
        )
        .is_empty());
    }

    #[test]
    fn deletions_and_binaries_are_noted() {
        let patch = "diff --git a/old.txt b/old.txt\ndeleted file mode 100644\nBinary files a/x.png and b/x.png differ";
        let found = reasons(&signals(&diff(&["old.txt", "x.png"], 1), patch));
        assert!(
            found.contains("deletes 1 file") && found.contains("binary"),
            "{found}"
        );
    }

    #[test]
    fn a_verdict_is_found_inside_prose_and_fences() {
        let text = "Here is my review:\n```json\n{\"risk_level\": \"Medium\", \"summary\": \"Missing a { brace } check\", \"findings\": [{\"severity\": \"high\", \"file\": \"src/a.rs\", \"line\": \"12\", \"message\": \"unwrap on user input\"}, {\"message\": \"\"}]}\n```\nThanks!";
        let verdict = parse_verdict(text).unwrap();
        assert_eq!(verdict.risk, Risk::Medium);
        assert_eq!(
            verdict.summary, "Missing a { brace } check",
            "braces in strings are not structure"
        );
        assert_eq!(verdict.findings.len(), 1, "an empty finding is dropped");
        assert_eq!(
            verdict.findings[0].line,
            Some(12),
            "a line given as a string still counts"
        );
        assert_eq!(verdict.findings[0].severity, "high");
    }

    #[test]
    fn no_verdict_is_invented() {
        assert!(parse_verdict("looks good to me").is_none());
        assert!(parse_verdict("{\"summary\": \"no risk level\"}").is_none());
        assert!(parse_verdict("{\"risk_level\": \"spicy\"}").is_none());
        // The contract itself, echoed back, must not read as a verdict.
        assert!(parse_verdict(CONTRACT).is_none());
    }

    fn check(kind: CheckKind, status: CheckStatus, risk: Option<Risk>) -> Check {
        Check {
            kind,
            name: "x".into(),
            status,
            summary: "s".into(),
            output: None,
            risk,
            findings: Vec::new(),
            reviewer: Some("r".into()),
        }
    }

    #[test]
    fn a_failed_command_makes_the_change_high_risk() {
        let report = assess(vec![
            check(CheckKind::Signals, CheckStatus::Passed, Some(Risk::Low)),
            check(CheckKind::Command, CheckStatus::Failed, Some(Risk::High)),
            check(CheckKind::Review, CheckStatus::Passed, Some(Risk::Low)),
        ]);
        assert_eq!(report.risk, Risk::High);
        assert!(report.verified);
    }

    #[test]
    fn the_second_reviewer_overrules_the_first() {
        let report = assess(vec![
            check(CheckKind::Review, CheckStatus::Passed, Some(Risk::Medium)),
            check(CheckKind::Review, CheckStatus::Passed, Some(Risk::Low)),
        ]);
        assert_eq!(report.risk, Risk::Low);
    }

    #[test]
    fn nothing_checked_is_unverified_not_low_risk() {
        let report = assess(vec![
            check(CheckKind::Signals, CheckStatus::Passed, Some(Risk::Low)),
            check(CheckKind::Review, CheckStatus::Skipped, None),
            check(CheckKind::Review, CheckStatus::Error, None),
        ]);
        assert!(!report.verified);
        assert!(report.reasons.iter().any(|r| r.contains("[verify]")));
    }

    #[test]
    fn signals_can_raise_a_clean_review() {
        let report = assess(vec![
            check(CheckKind::Signals, CheckStatus::Passed, Some(Risk::High)),
            check(CheckKind::Review, CheckStatus::Passed, Some(Risk::Low)),
        ]);
        assert_eq!(
            report.risk,
            Risk::High,
            "a migration is worth a look whatever the reviewer says"
        );
    }

    #[tokio::test]
    async fn commands_report_pass_fail_and_timeout_with_their_output() {
        let dir = tempfile::tempdir().unwrap();
        let pass = run_command(dir.path(), "echo ok", Duration::from_secs(10)).await;
        assert_eq!(pass.status, CheckStatus::Passed);
        assert_eq!(pass.output.as_deref(), Some("ok"));

        let fail = run_command(dir.path(), "echo boom >&2; exit 3", Duration::from_secs(10)).await;
        assert_eq!(fail.status, CheckStatus::Failed);
        assert!(fail.summary.contains("exit 3"));
        assert_eq!(fail.output.as_deref(), Some("boom"));

        let slow = run_command(dir.path(), "sleep 5", Duration::from_millis(200)).await;
        assert_eq!(slow.status, CheckStatus::Failed);
        assert!(slow.summary.contains("timed out"));
    }
}
