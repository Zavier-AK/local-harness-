//! What is left of a subscription, and how much of that is actually knowable.
//!
//! The two vendors differ sharply here, and pretending otherwise would produce a
//! confident-looking number that is wrong:
//!
//! * **Codex** writes real, server-reported rate limits into its session rollout files:
//!   a used percentage and a reset time for both the short and long window. We read
//!   those. They are a snapshot from the last turn, not live, so they are reported with
//!   the time they were observed and go stale rather than silently ageing.
//!
//! * **Claude** reports its quota on its own stream. Every turn of a `claude -p` process
//!   emits a `rate_limit_event` with server-reported utilization and reset times for the
//!   five-hour and weekly windows. The harness keeps the latest one. Before the first turn
//!   of a session there is nothing yet, which is reported as missing, not as zero.
//!
//!   (An earlier version of this module said Claude exposed nothing readable. That was
//!   wrong: `/usage` is interactive-only and statusLine never runs under `-p`, but the
//!   stream itself carries the figures.)
//!
//! The three states are borrowed from Codex's own `/status`, which classifies its data as
//! available, stale or missing. A meter that says "unknown" is more useful than one that
//! says 0%.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// How old an observation may be before it is reported as stale rather than current.
const STALE_AFTER_SECS: i64 = 30 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaState {
    /// A real, recent, server-reported figure.
    Available,
    /// A real figure, but old enough that it may have moved.
    Stale,
    /// Nothing to report. Rendered as unknown, never as zero.
    Missing,
}

/// One limit window, e.g. the rolling five hours or the week.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaWindow {
    pub label: String,
    pub used_percent: f64,
    /// Unix seconds when this window resets, when the vendor says.
    pub resets_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderQuota {
    pub provider: String,
    pub state: QuotaState,
    /// When the figures were observed. Absent when there is nothing to report.
    pub observed_at: Option<i64>,
    pub windows: Vec<QuotaWindow>,
    /// Why there is no figure, when there is none. Shown to the user verbatim, so it
    /// explains rather than apologises.
    pub note: Option<String>,
}

impl ProviderQuota {
    fn missing(provider: &str, note: &str) -> Self {
        Self {
            provider: provider.to_string(),
            state: QuotaState::Missing,
            observed_at: None,
            windows: Vec::new(),
            note: Some(note.to_string()),
        }
    }
}

fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

/// Claude's quota, from the latest `rate_limit_event` this app has seen.
///
/// `latest` is `(observed_at, windows)`. The figures are real but only as fresh as the last
/// turn, so an old report is marked stale rather than shown as current.
pub fn claude_quota(latest: Option<(i64, Vec<QuotaWindow>)>) -> ProviderQuota {
    let Some((observed_at, windows)) = latest.filter(|(_, windows)| !windows.is_empty()) else {
        return ProviderQuota::missing(
            "claude",
            "Claude reports its quota with every turn. Nothing has run yet in this session, \
             so there is no figure to show.",
        );
    };
    ProviderQuota {
        provider: "claude".into(),
        state: if now() - observed_at > STALE_AFTER_SECS {
            QuotaState::Stale
        } else {
            QuotaState::Available
        },
        observed_at: Some(observed_at),
        windows,
        note: None,
    }
}

/// Codex's quota, read from its session rollout files.
pub fn codex_quota() -> ProviderQuota {
    match dirs_home() {
        Some(home) => codex_quota_in(&home.join(".codex").join("sessions")),
        None => ProviderQuota::missing("codex", "Could not locate a home directory"),
    }
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Same, against an explicit sessions directory.
///
/// Codex records a `token_count` event carrying a `rate_limits` object on each turn. The
/// most recent one across all sessions is the freshest thing available without asking
/// Codex to run, which would itself spend quota.
pub fn codex_quota_in(sessions_dir: &Path) -> ProviderQuota {
    if !sessions_dir.is_dir() {
        return ProviderQuota::missing(
            "codex",
            "No Codex sessions found yet — run Codex once and its usage will appear here",
        );
    }

    let mut newest: Option<(i64, Vec<QuotaWindow>)> = None;
    for file in rollout_files(sessions_dir) {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        // Last match wins: a rollout is append-only, so later lines are more recent.
        for line in text.lines() {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let Some((observed_at, windows)) = rate_limits_from(&value) else {
                continue;
            };
            if newest.as_ref().is_none_or(|(seen, _)| observed_at >= *seen) {
                newest = Some((observed_at, windows));
            }
        }
    }

    let Some((observed_at, windows)) = newest else {
        return ProviderQuota::missing(
            "codex",
            "Codex has not reported a rate limit yet — it records one after a turn",
        );
    };

    let state = if now() - observed_at > STALE_AFTER_SECS {
        QuotaState::Stale
    } else {
        QuotaState::Available
    };

    ProviderQuota {
        provider: "codex".into(),
        state,
        observed_at: Some(observed_at),
        windows,
        note: None,
    }
}

/// Rollout files, newest-looking first. Codex nests them by date, so this walks.
fn rollout_files(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
            {
                found.push(path);
            }
        }
    }
    found
}

/// Pull the rate-limit snapshot out of one rollout line, if it carries one.
///
/// Written leniently on purpose: Codex's event vocabulary has shifted between releases,
/// and the fields are nested differently depending on version. A missing field means "not
/// this line" rather than a parse error that would lose the whole file.
fn rate_limits_from(value: &serde_json::Value) -> Option<(i64, Vec<QuotaWindow>)> {
    let limits = value
        .pointer("/payload/rate_limits")
        .or_else(|| value.pointer("/rate_limits"))
        .or_else(|| value.pointer("/payload/info/rate_limits"))?;

    let observed_at = value
        .get("timestamp")
        .and_then(parse_timestamp)
        .or_else(|| {
            value
                .pointer("/payload/timestamp")
                .and_then(parse_timestamp)
        })
        .unwrap_or_else(now);

    let mut windows = Vec::new();
    for (key, label) in [("primary", "5h"), ("secondary", "weekly")] {
        let Some(window) = limits.get(key) else {
            continue;
        };
        let Some(used) = window
            .get("used_percent")
            .and_then(serde_json::Value::as_f64)
        else {
            continue;
        };

        // Codex reports either an absolute reset time or seconds until reset, depending
        // on where in its stack the snapshot came from. Normalize to absolute.
        let resets_at = window
            .get("resets_at")
            .and_then(serde_json::Value::as_i64)
            .or_else(|| {
                window
                    .get("resets_in_seconds")
                    .and_then(serde_json::Value::as_i64)
                    .map(|secs| observed_at + secs)
            });

        windows.push(QuotaWindow {
            label: label.to_string(),
            used_percent: used,
            resets_at,
        });
    }

    (!windows.is_empty()).then_some((observed_at, windows))
}

fn parse_timestamp(value: &serde_json::Value) -> Option<i64> {
    if let Some(seconds) = value.as_i64() {
        return Some(seconds);
    }
    let text = value.as_str()?;
    time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
        .ok()
        .map(|parsed| parsed.unix_timestamp())
}

/// Every provider's quota, for the panel.
pub fn all_quotas(claude: Option<(i64, Vec<QuotaWindow>)>) -> Vec<ProviderQuota> {
    vec![claude_quota(claude), codex_quota()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn claude_before_its_first_turn_is_missing_not_zero() {
        let quota = claude_quota(None);
        assert_eq!(quota.state, QuotaState::Missing);
        assert!(
            quota.windows.is_empty(),
            "an unknown share of a limit is not an empty one"
        );
        assert!(quota.note.unwrap().contains("every turn"));
    }

    #[test]
    fn a_recent_claude_report_is_shown_as_it_came() {
        let windows = vec![
            QuotaWindow {
                label: "5h".into(),
                used_percent: 48.0,
                resets_at: Some(1790122800),
            },
            QuotaWindow {
                label: "weekly".into(),
                used_percent: 6.0,
                resets_at: Some(1790668800),
            },
        ];
        let quota = claude_quota(Some((now() - 30, windows.clone())));
        assert_eq!(quota.state, QuotaState::Available);
        assert_eq!(quota.windows, windows);
        assert!(quota.note.is_none());
    }

    #[test]
    fn an_old_claude_report_is_stale_not_current() {
        let windows = vec![QuotaWindow {
            label: "5h".into(),
            used_percent: 90.0,
            resets_at: None,
        }];
        let quota = claude_quota(Some((now() - STALE_AFTER_SECS - 60, windows)));
        // Real figures, but a turn since then may have moved them.
        assert_eq!(quota.state, QuotaState::Stale);
        assert_eq!(quota.windows[0].used_percent, 90.0);
    }

    #[test]
    fn codex_limits_are_read_from_the_newest_rollout_line() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions/2026/09/21");
        let recent = now() - 60;
        write(
            &sessions,
            "rollout-a.jsonl",
            &format!(
                r#"{{"type":"thread.started"}}
{{"type":"token_count","timestamp":{older},"payload":{{"rate_limits":{{"primary":{{"used_percent":10.0,"resets_in_seconds":600}}}}}}}}
{{"type":"token_count","timestamp":{recent},"payload":{{"rate_limits":{{"primary":{{"used_percent":73.5,"resets_in_seconds":3600}},"secondary":{{"used_percent":41.0,"resets_at":1790000000}}}}}}}}
"#,
                older = recent - 900,
                recent = recent
            ),
        );

        let quota = codex_quota_in(&dir.path().join("sessions"));
        assert_eq!(quota.state, QuotaState::Available);
        assert_eq!(quota.windows.len(), 2);

        // Last line wins: a rollout is append-only, so 73.5 supersedes 10.0.
        let five_hour = &quota.windows[0];
        assert_eq!(five_hour.label, "5h");
        assert_eq!(five_hour.used_percent, 73.5);
        // Relative resets are normalized to absolute, so the UI has one thing to render.
        assert_eq!(five_hour.resets_at, Some(recent + 3600));
        assert_eq!(quota.windows[1].resets_at, Some(1790000000));
    }

    #[test]
    fn an_old_observation_is_reported_stale_rather_than_current() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        write(
            &sessions,
            "rollout-old.jsonl",
            &format!(
                r#"{{"type":"token_count","timestamp":{},"payload":{{"rate_limits":{{"primary":{{"used_percent":20.0}}}}}}}}"#,
                now() - STALE_AFTER_SECS - 60
            ),
        );

        let quota = codex_quota_in(&sessions);
        // The figure is real, just old. Reporting it as current would be the lie.
        assert_eq!(quota.state, QuotaState::Stale);
        assert_eq!(quota.windows[0].used_percent, 20.0);
    }

    #[test]
    fn nothing_to_report_is_missing_not_zero() {
        let dir = tempfile::tempdir().unwrap();
        let absent = codex_quota_in(&dir.path().join("nope"));
        assert_eq!(absent.state, QuotaState::Missing);
        assert!(absent.windows.is_empty());
        assert!(absent.note.is_some());

        // A session file with no rate-limit line is the same situation.
        let sessions = dir.path().join("sessions");
        write(
            &sessions,
            "rollout-x.jsonl",
            "{\"type\":\"turn.completed\"}\n",
        );
        assert_eq!(codex_quota_in(&sessions).state, QuotaState::Missing);
    }

    #[test]
    fn malformed_lines_do_not_discard_the_rest_of_the_file() {
        // Codex's event vocabulary has shifted between releases, so one unreadable line
        // must not cost us the snapshot sitting next to it.
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        write(
            &sessions,
            "rollout-mixed.jsonl",
            &format!(
                r#"not json at all
{{"type":"something_new","payload":{{"unexpected":true}}}}
{{"type":"token_count","timestamp":{},"payload":{{"rate_limits":{{"primary":{{"used_percent":55.0}}}}}}}}
"#,
                now() - 30
            ),
        );

        let quota = codex_quota_in(&sessions);
        assert_eq!(quota.state, QuotaState::Available);
        assert_eq!(quota.windows[0].used_percent, 55.0);
    }

    #[test]
    fn rollouts_are_found_in_codexs_nested_date_directories() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        write(
            &sessions.join("2026").join("09").join("21"),
            "rollout-deep.jsonl",
            &format!(
                r#"{{"type":"token_count","timestamp":{},"payload":{{"rate_limits":{{"primary":{{"used_percent":12.0}}}}}}}}"#,
                now() - 10
            ),
        );
        assert_eq!(codex_quota_in(&sessions).windows[0].used_percent, 12.0);
    }

    #[test]
    fn an_rfc3339_timestamp_is_accepted_too() {
        assert_eq!(
            parse_timestamp(&serde_json::json!("2026-09-21T12:00:00Z")),
            Some(1789992000)
        );
        // A bare number is already unix seconds and passes through unchanged.
        assert_eq!(
            parse_timestamp(&serde_json::json!(1789992000)),
            Some(1789992000)
        );
    }
}
