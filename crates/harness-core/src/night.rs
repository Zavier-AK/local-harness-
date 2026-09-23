//! Night shift: an improvement loop that runs while nobody is watching.
//!
//! Karpathy's autoresearch pattern, pointed at a codebase: a goal in plain words, a command
//! that prints a score, and optionally a command that must keep passing. Each round, a
//! worker tries one focused change on top of the best result so far; the harness scores
//! it in a fresh checkout and keeps it only if the guard passes and the score improves.
//! Everything else is thrown away. By morning there is one branch holding every kept
//! improvement, and a report of what was tried.
//!
//! It works on its own branch, `harness/night-<id>`, and never touches the person's
//! checkout. Landing the night's work is an ordinary merge proposal — verified, gated,
//! and subject to the autonomy level like any other.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::roles::{Isolation, RoleRegistry};

pub const MAX_EXPERIMENTS: u32 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Higher,
    Lower,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NightConfig {
    /// What to improve, in plain words — Karpathy's `program.md`.
    pub goal: String,
    /// Prints the score; the last number in its output is taken.
    pub metric: String,
    pub direction: Direction,
    /// Must succeed for a change to be kept, e.g. the test suite.
    #[serde(default)]
    pub guard: Option<String>,
    /// The role that makes each change. It needs a worktree to edit in.
    pub role: String,
    #[serde(default = "default_experiments")]
    pub max_experiments: u32,
    #[serde(default = "default_hours")]
    pub max_hours: f64,
    /// Ceiling for the metric and the guard, each run.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_experiments() -> u32 {
    20
}
fn default_hours() -> f64 {
    8.0
}
fn default_timeout() -> u64 {
    900
}

impl NightConfig {
    pub fn validate(&self, registry: &RoleRegistry) -> Result<()> {
        if self.goal.trim().is_empty() {
            bail!("say what to improve");
        }
        if self.metric.trim().is_empty() {
            bail!("give a command that prints the score");
        }
        if !(1..=MAX_EXPERIMENTS).contains(&self.max_experiments) {
            bail!("experiments must be between 1 and {MAX_EXPERIMENTS}");
        }
        if !(self.max_hours > 0.0 && self.max_hours <= 24.0) {
            bail!("hours must be between 0 and 24");
        }
        if self.timeout_secs == 0 {
            bail!("the command timeout must be more than zero");
        }
        let role = registry.get(&self.role)?;
        if role.isolation != Isolation::Worktree {
            bail!(
                "role `{}` has isolation `{}`; a night shift needs a role that edits in its own \
                 worktree",
                self.role,
                role.isolation.as_str()
            );
        }
        Ok(())
    }

    /// Whether `score` beats `best` in the direction that counts.
    pub fn improves(&self, score: f64, best: f64) -> bool {
        match self.direction {
            Direction::Higher => score > best,
            Direction::Lower => score < best,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NightStatus {
    Running,
    Finished,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Experiment {
    pub n: u32,
    pub worker_id: String,
    /// What the worker said it changed.
    pub summary: String,
    pub score: Option<f64>,
    pub kept: bool,
    /// Why it was kept or thrown away.
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NightReport {
    pub id: String,
    pub config: NightConfig,
    pub status: NightStatus,
    /// Where kept changes accumulate.
    pub branch: String,
    pub baseline: Option<f64>,
    pub best: Option<f64>,
    pub experiments: Vec<Experiment>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    /// Why it ended: budget spent, stopped by the person, a limit reached, a broken metric.
    pub ended_because: Option<String>,
    /// Set once the night's branch was proposed for review.
    #[serde(default)]
    pub proposed_as: Option<String>,
}

impl NightReport {
    pub fn kept(&self) -> usize {
        self.experiments.iter().filter(|e| e.kept).count()
    }

    /// One line for the morning.
    pub fn headline(&self) -> String {
        match (self.baseline, self.best) {
            (Some(base), Some(best)) if best != base => {
                let change = if base != 0.0 {
                    (best - base) / base.abs() * 100.0
                } else {
                    0.0
                };
                format!(
                    "{base} → {best} ({change:+.1}%) from {} kept of {} tried",
                    self.kept(),
                    self.experiments.len()
                )
            }
            (Some(base), _) => format!(
                "no improvement on {base} after {} tried",
                self.experiments.len()
            ),
            _ => "no baseline score".into(),
        }
    }
}

/// The score in a metric command's output: the last number on it, so a command may say
/// what it measured before it says the value.
pub fn parse_score(output: &str) -> Option<f64> {
    output
        .split(|c: char| c.is_whitespace() || matches!(c, ',' | ':' | '=' | ';' | '(' | ')' | '%'))
        .filter_map(leading_number)
        .next_back()
}

/// A number at the start of a token, with any unit after it dropped: `12.5ms` is 12.5.
/// A token that starts with a letter (`v2`, `x86`) is not a number.
fn leading_number(token: &str) -> Option<f64> {
    let end = token
        .char_indices()
        .find(|&(i, c)| !(c.is_ascii_digit() || c == '.' || ((c == '-' || c == '+') && i == 0)))
        .map(|(i, _)| i)
        .unwrap_or(token.len());
    token[..end]
        .trim_end_matches('.')
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
}

/// Run the score command and read the score from what it prints on stdout. Standard error
/// is not read: compilers and test runners print numbers there too.
pub async fn run_metric(
    cwd: &std::path::Path,
    command: &str,
    timeout: std::time::Duration,
) -> std::result::Result<f64, String> {
    let child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = match tokio::time::timeout(timeout, child).await {
        Err(_) => {
            return Err(format!(
                "`{command}` timed out after {}s",
                timeout.as_secs()
            ))
        }
        Ok(Err(err)) => return Err(format!("`{command}` could not run: {err}")),
        Ok(Ok(output)) => output,
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let last = stderr
            .lines()
            .chain(stdout.lines())
            .last()
            .unwrap_or_default();
        return Err(format!("`{command}` failed: {}", last.trim()));
    }
    parse_score(&stdout).ok_or_else(|| format!("`{command}` printed no number"))
}

/// How many earlier experiments the worker is told about — enough to avoid repeating a
/// failed idea, not so many the brief crowds out the goal.
const HISTORY: usize = 12;

/// The brief for one round: the goal, where the score stands, and what has been tried.
pub fn experiment_task(config: &NightConfig, best: Option<f64>, history: &[Experiment]) -> String {
    let direction = match config.direction {
        Direction::Higher => "higher is better",
        Direction::Lower => "lower is better",
    };
    let mut task = format!(
        "You are one round of an unattended improvement loop.\n\nGoal: {}\n\nThe score comes from \
         running `{}` ({direction}). The best so far is {}.\n",
        config.goal.trim(),
        config.metric,
        best.map(|b| b.to_string())
            .unwrap_or_else(|| "unknown".into()),
    );
    if let Some(guard) = &config.guard {
        task.push_str(&format!(
            "`{guard}` must keep passing, or the change is thrown away.\n"
        ));
    }
    let recent: Vec<&Experiment> = history.iter().rev().take(HISTORY).collect();
    if !recent.is_empty() {
        task.push_str("\nAlready tried (most recent first) — do not repeat these:\n");
        for experiment in recent {
            task.push_str(&format!(
                "- {} → {}: {}\n",
                experiment.summary.lines().next().unwrap_or("(no summary)"),
                if experiment.kept {
                    "kept"
                } else {
                    "thrown away"
                },
                experiment.reason
            ));
        }
    }
    task.push_str(
        "\nMake ONE focused change you expect to improve the score. Keep it small: it is judged \
         on its own. You may run the score command to check. End with one sentence saying what \
         you changed.",
    );
    task
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> NightConfig {
        NightConfig {
            goal: "make the benchmark faster".into(),
            metric: "./bench.sh".into(),
            direction: Direction::Lower,
            guard: Some("cargo test".into()),
            role: "builder".into(),
            max_experiments: 20,
            max_hours: 8.0,
            timeout_secs: 900,
        }
    }

    #[test]
    fn the_score_is_the_last_number_printed() {
        assert_eq!(parse_score("42"), Some(42.0));
        assert_eq!(
            parse_score("ran 3 times\nmean: 12.5ms"),
            Some(12.5),
            "a unit is dropped"
        );
        assert_eq!(
            parse_score("built for x86 in v2"),
            None,
            "a word with digits is not a number"
        );
        assert_eq!(parse_score("ran 3 times\nmean = 12.5 ms"), Some(12.5));
        assert_eq!(
            parse_score("tests: 118 passed, 2 failed (98.3%)"),
            Some(98.3)
        );
        assert_eq!(parse_score("score: 0.91."), Some(0.91));
        assert_eq!(parse_score("nothing to see"), None);
        assert_eq!(parse_score("NaN inf"), None);
    }

    #[test]
    fn improvement_follows_the_direction() {
        let mut c = config();
        assert!(c.improves(9.0, 10.0));
        assert!(!c.improves(10.0, 10.0), "equal is not better");
        c.direction = Direction::Higher;
        assert!(c.improves(11.0, 10.0));
    }

    #[test]
    fn a_night_shift_needs_a_role_that_can_edit() {
        let registry = RoleRegistry::from_toml(
            "[roles.builder]\nprovider = \"mock\"\nisolation = \"worktree\"\n\
             [roles.reviewer]\nprovider = \"mock\"\nisolation = \"readonly\"\n",
        )
        .unwrap();
        config().validate(&registry).unwrap();
        let mut readonly = config();
        readonly.role = "reviewer".into();
        assert!(readonly
            .validate(&registry)
            .unwrap_err()
            .to_string()
            .contains("own worktree"));
        for broken in [
            NightConfig {
                goal: " ".into(),
                ..config()
            },
            NightConfig {
                metric: "".into(),
                ..config()
            },
            NightConfig {
                max_experiments: 0,
                ..config()
            },
            NightConfig {
                max_hours: 30.0,
                ..config()
            },
        ] {
            assert!(broken.validate(&registry).is_err());
        }
    }

    #[test]
    fn each_round_is_told_what_was_already_tried() {
        let history = vec![Experiment {
            n: 1,
            worker_id: "w".into(),
            summary: "Inlined the hot loop.".into(),
            score: Some(11.0),
            kept: false,
            reason: "11 is not better than 10".into(),
        }];
        let task = experiment_task(&config(), Some(10.0), &history);
        assert!(task.contains("best so far is 10"));
        assert!(task.contains("lower is better"));
        assert!(task.contains("Inlined the hot loop. → thrown away"));
        assert!(task.contains("`cargo test` must keep passing"));
    }

    #[test]
    fn the_headline_says_how_far_it_got() {
        let mut report = NightReport {
            id: "n".into(),
            config: config(),
            status: NightStatus::Finished,
            branch: "harness/night-n".into(),
            baseline: Some(10.0),
            best: Some(8.0),
            experiments: vec![],
            started_at: 0,
            finished_at: None,
            ended_because: None,
            proposed_as: None,
        };
        assert!(report.headline().starts_with("10 → 8 (-20.0%)"));
        report.best = Some(10.0);
        assert!(report.headline().starts_with("no improvement"));
    }
}
