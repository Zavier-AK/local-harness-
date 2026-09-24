//! Measuring voice understanding on labelled phrases, instead of trusting a model card.
//!
//! `harness-cli voice --eval` runs every phrase in `phrases.toml` through the matcher and,
//! where it is installed, through Laya, and reports for each how often it was right, how
//! often it did the *wrong* thing, and how often it held back (sent the words on, or asked).
//! Wrong is what matters: holding back costs a moment, acting wrongly costs trust. The
//! suggested threshold is the lowest at which Laya is never wrong on this set.

use serde::Deserialize;
use std::sync::Arc;

use super::laya::{self, LayaClient};
use super::matcher;
use super::{finalize, NightRef, Outcome, PlanRef, ProjectRef, Snapshot, VoiceAction, WorkerRef};
use crate::autonomy::Autonomy;

#[derive(Debug, Clone, Deserialize)]
pub struct Phrase {
    pub said: String,
    pub expect: String,
}

#[derive(Deserialize)]
struct File {
    phrase: Vec<Phrase>,
}

pub fn phrases() -> Vec<Phrase> {
    toml::from_str::<File>(include_str!("phrases.toml"))
        .expect("phrases.toml is valid")
        .phrase
}

/// The harness the phrases are labelled against. See the top of `phrases.toml`.
pub fn fixture() -> Snapshot {
    let worker = |id: &str, number, role: &str, status: &str, task: &str| WorkerRef {
        id: id.into(),
        number,
        role: role.into(),
        status: status.into(),
        task: task.into(),
        merge_pending: false,
        risk: None,
    };
    let mut builder = worker("w-aaa", 1, "builder", "done", "Retry the price fetcher");
    builder.merge_pending = true;
    builder.risk = Some("low".into());
    Snapshot {
        projects: vec![
            ProjectRef {
                root: "/p/shop".into(),
                name: "shop".into(),
            },
            ProjectRef {
                root: "/p/blog".into(),
                name: "blog".into(),
            },
        ],
        active_project: Some("/p/shop".into()),
        workers: vec![
            builder,
            worker("w-bbb", 2, "tester", "running", "Run the integration tests"),
            worker(
                "w-ccc",
                3,
                "reviewer",
                "awaiting_approval",
                "Review the auth module",
            ),
        ],
        plan: Some(PlanRef {
            title: "Retries".into(),
            status: "draft".into(),
            landed: 0,
            total: 3,
        }),
        night: Some(NightRef {
            status: "running".into(),
            kept: 2,
            tried: 5,
            baseline: Some(58.7),
            best: Some(33.9),
            proposed: false,
        }),
        autonomy: Autonomy::Review,
        landed: vec!["w-old".into()],
        apps: [
            "Notes",
            "Safari",
            "Visual Studio Code",
            "Slack",
            "Google Chrome",
            "Cursor",
            "Terminal",
            "Finder",
            "Microsoft Word",
            "Microsoft Excel",
            "LM Studio",
            "Spotify",
            "System Settings",
        ]
        .into_iter()
        .map(String::from)
        .collect(),
    }
}

/// An outcome in the form `phrases.toml` labels it: the action and its key argument.
pub fn label(outcome: &Outcome, snapshot: &Snapshot) -> String {
    use VoiceAction::*;
    let worker = |id: &str| {
        snapshot
            .workers
            .iter()
            .find(|w| w.id == id)
            .map(|w| format!("#{}", w.number))
            .unwrap_or_else(|| id.to_string())
    };
    let action = match outcome {
        Outcome::Act { action, .. } => action,
        Outcome::ToHead { .. } => return "to_head".into(),
        Outcome::Clarify { .. } => return "clarify".into(),
        Outcome::Reply { .. } => return "reply".into(),
        Outcome::Nothing => return "nothing".into(),
    };
    let tag = serde_json::to_value(action)
        .ok()
        .and_then(|v| v["action"].as_str().map(str::to_string))
        .unwrap_or_default();
    let argument = match action {
        Status { topic } => Some(topic.as_str().to_string()),
        Navigate { pane } => Some(pane.as_str().to_string()),
        SwitchProject { project } => snapshot
            .projects
            .iter()
            .find(|p| p.root == *project)
            .map(|p| p.name.clone()),
        OpenWorker { worker: id }
        | StopWorker { worker: id }
        | ApproveMerge { worker: id }
        | RejectMerge { worker: id, .. }
        | UndoMerge { worker: id }
        | ApproveDelegation { worker: id }
        | DeclineDelegation { worker: id, .. } => Some(worker(id)),
        SetAutonomy { level } => Some(level.as_str().to_string()),
        OpenApp { name } => Some(name.to_lowercase()),
        OpenUrl { url } => Some(
            url.trim_start_matches("https://")
                .trim_start_matches("http://")
                .trim_end_matches('/')
                .to_string(),
        ),
        OpenFolder { path } => Some(path.clone()),
        Search { query, site, .. } => Some(format!("{} {query}", site.label().to_lowercase())),
        Media { control, .. } => Some(
            control
                .label()
                .split(' ')
                .next()
                .unwrap_or("")
                .to_lowercase(),
        ),
        PlayPlaylist { name, .. } => Some(name.clone()),
        PlayQuery { query, .. } => Some(query.clone()),
        NewNote { text } => Some(text.clone()),
        Remind { text, .. } => Some(text.to_lowercase()),
        System { control } => Some(control.label().to_lowercase()),
        _ => None,
    };
    match argument {
        Some(argument) => format!("{tag} {argument}"),
        None => tag,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Right,
    /// Did something other than what was meant.
    Wrong,
    /// Held back: sent the words on, asked, or said it could not.
    Unsure,
}

pub fn verdict(expect: &str, outcome: &Outcome, snapshot: &Snapshot) -> Verdict {
    let got = label(outcome, snapshot);
    if got == expect {
        return Verdict::Right;
    }
    match outcome {
        Outcome::Act { .. } => Verdict::Wrong,
        _ => Verdict::Unsure,
    }
}

#[derive(Debug, Clone, Default)]
pub struct Tally {
    pub right: usize,
    pub wrong: usize,
    pub unsure: usize,
}

impl Tally {
    fn add(&mut self, verdict: Verdict) {
        match verdict {
            Verdict::Right => self.right += 1,
            Verdict::Wrong => self.wrong += 1,
            Verdict::Unsure => self.unsure += 1,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Row {
    pub said: String,
    pub expect: String,
    /// What the matcher made of it, if anything.
    pub matcher: Option<String>,
    pub matcher_verdict: Verdict,
    /// What Laya made of it at the chosen threshold, and its confidence.
    pub laya: Option<(String, Option<f64>)>,
    /// The whole pipeline at the chosen threshold.
    pub combined: String,
    pub verdict: Verdict,
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub phrases: usize,
    pub matcher: Tally,
    /// Laya alone, by threshold.
    pub laya: Vec<(f64, Tally)>,
    /// Matcher, then Laya at the chosen threshold, then the head agent.
    pub combined: Tally,
    pub laya_ms: Vec<u64>,
    pub rows: Vec<Row>,
    /// The lowest threshold at which Laya was never wrong here, if any.
    pub suggested_threshold: Option<f64>,
    pub laya_error: Option<String>,
}

pub const THRESHOLDS: [f64; 8] = [0.5, 0.6, 0.7, 0.75, 0.8, 0.85, 0.9, 0.95];

fn percentile(sorted: &[u64], p: f64) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let index = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted.get(index).copied()
}

impl Report {
    pub fn latency(&self) -> Option<(u64, u64)> {
        let mut sorted = self.laya_ms.clone();
        sorted.sort_unstable();
        Some((percentile(&sorted, 0.5)?, percentile(&sorted, 0.95)?))
    }
}

/// Run every phrase. `laya` is used if given and loaded; `threshold` is the one the
/// combined pipeline is judged at.
pub async fn run(laya: Option<&Arc<LayaClient>>, threshold: f64) -> Report {
    let snapshot = fixture();
    let phrases = phrases();
    let mut report = Report {
        phrases: phrases.len(),
        laya: THRESHOLDS.iter().map(|t| (*t, Tally::default())).collect(),
        ..Report::default()
    };
    for phrase in phrases {
        let matched =
            matcher::match_command(&phrase.said, &snapshot, false).map(|a| finalize(a, &snapshot));
        let matcher_verdict = match &matched {
            Some(outcome) => verdict(&phrase.expect, outcome, &snapshot),
            None => Verdict::Unsure,
        };
        report.matcher.add(matcher_verdict);

        let mut laya_at_threshold = None;
        if let Some(client) = laya {
            let state = laya::state(&phrase.said, &snapshot);
            let questions = laya::questions(&snapshot, None);
            match client.decide(&state, &questions).await {
                Ok(reply) => {
                    report.laya_ms.push(reply.ms);
                    let slots = matcher::slots(&phrase.said);
                    let at = |t: f64| {
                        let decision =
                            laya::decide(&reply.answers, &snapshot, &slots, &phrase.said, t, false);
                        let outcome = match decision.action {
                            Some(action) => finalize(action, &snapshot),
                            None => decision.otherwise,
                        };
                        (outcome, decision.confidence)
                    };
                    for (t, tally) in report.laya.iter_mut() {
                        tally.add(verdict(&phrase.expect, &at(*t).0, &snapshot));
                    }
                    laya_at_threshold = Some(at(threshold));
                }
                Err(err) => {
                    report.laya_error.get_or_insert_with(|| format!("{err:#}"));
                }
            }
        }

        let combined = match (&matched, &laya_at_threshold) {
            (Some(outcome), _) => outcome.clone(),
            (None, Some((outcome, _))) => outcome.clone(),
            (None, None) => Outcome::ToHead {
                text: phrase.said.clone(),
            },
        };
        let combined_verdict = verdict(&phrase.expect, &combined, &snapshot);
        report.combined.add(combined_verdict);
        report.rows.push(Row {
            matcher: matched.as_ref().map(|o| label(o, &snapshot)),
            matcher_verdict,
            laya: laya_at_threshold
                .as_ref()
                .map(|(o, c)| (label(o, &snapshot), *c)),
            combined: label(&combined, &snapshot),
            verdict: combined_verdict,
            said: phrase.said,
            expect: phrase.expect,
        });
    }
    if laya.is_some() && report.laya_error.is_none() {
        report.suggested_threshold = report
            .laya
            .iter()
            .find(|(_, tally)| tally.wrong == 0)
            .map(|(t, _)| *t);
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_phrase_has_a_label_the_code_can_produce() {
        let known = [
            "status",
            "navigate",
            "switch_project",
            "open_worker",
            "approve_merge",
            "reject_merge",
            "approve_delegation",
            "decline_delegation",
            "stop_worker",
            "undo_merge",
            "stop_turn",
            "ask_head",
            "to_head",
            "set_autonomy",
            "run_plan",
            "discard_plan",
            "stop_night",
            "night_setup",
            "open_app",
            "open_url",
            "open_folder",
            "reveal_project",
            "open_project_in_editor",
            "search",
            "media",
            "play_playlist",
            "play_query",
            "new_note",
            "remind",
            "system",
        ];
        let all = phrases();
        assert!(all.len() >= 55, "{} phrases", all.len());
        for phrase in all {
            let tag = phrase.expect.split(' ').next().unwrap();
            assert!(
                known.contains(&tag),
                "{:?} has an unknown label {tag}",
                phrase.said
            );
        }
    }

    /// The matcher may pass on a phrase — Laya or the head agent gets it — but it must
    /// never do the wrong thing.
    #[tokio::test]
    async fn the_matcher_is_never_wrong() {
        let report = run(None, 0.75).await;
        let wrong: Vec<String> = report
            .rows
            .iter()
            .filter(|row| row.matcher_verdict == Verdict::Wrong)
            .map(|row| {
                format!(
                    "{:?}: expected {}, got {}",
                    row.said,
                    row.expect,
                    row.matcher.as_ref().unwrap()
                )
            })
            .collect();
        assert!(
            wrong.is_empty(),
            "the matcher acted wrongly:\n{}",
            wrong.join("\n")
        );
        // It should also carry most of the plain commands by itself.
        assert!(
            report.matcher.right * 2 >= report.phrases,
            "matcher right on {}/{}",
            report.matcher.right,
            report.phrases
        );
        // Without Laya, nothing in the combined pipeline is wrong either.
        assert_eq!(report.combined.wrong, 0);
    }
}
