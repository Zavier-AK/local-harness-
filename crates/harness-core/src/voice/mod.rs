//! Voice: turning what the person said into something the harness does.
//!
//! Whisper (in the app) writes the words out; this module decides what they mean. Three
//! steps, cheapest first:
//!
//! 1. [`matcher`] — exact commands, instantly, no model. It also pulls out the free text an
//!    action needs (an app name, a URL, a message for the head agent), which no classifier
//!    can.
//! 2. [`laya`] — Laya, an open 421M *decision* model, for phrasing the matcher misses. It
//!    only ever chooses between options the harness offers, with a calibrated probability,
//!    so it cannot invent a worker, and a low probability means "not sure" rather than a
//!    confident wrong answer. Below the threshold, nothing is done on a guess.
//! 3. Otherwise the words go to the head agent as an ordinary message: nothing said is
//!    lost, and requests about the work belong there anyway.
//!
//! Whatever decides, [`finalize`] checks the action against the harness as it is (is there
//! a plan to run? a merge to approve?) and marks what must be confirmed before it runs.

pub mod computer;
pub mod eval;
pub mod everyday;
pub mod laya;
pub mod matcher;

use serde::{Deserialize, Serialize};

use crate::autonomy::Autonomy;

/// Something voice can make the harness (or, within a safe list, the computer) do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum VoiceAction {
    Navigate {
        pane: Pane,
    },
    /// By project root.
    SwitchProject {
        project: String,
    },
    OpenWorker {
        worker: String,
    },
    Status {
        topic: StatusTopic,
    },
    /// Say something to the head agent, as if typed in the chat.
    AskHead {
        text: String,
    },
    /// Interrupt the head agent's current turn.
    StopTurn,
    StopWorker {
        worker: String,
    },
    ApproveMerge {
        worker: String,
    },
    RejectMerge {
        worker: String,
        reason: Option<String>,
    },
    UndoMerge {
        worker: String,
    },
    ApproveDelegation {
        worker: String,
    },
    DeclineDelegation {
        worker: String,
        reason: Option<String>,
    },
    SetAutonomy {
        level: Autonomy,
    },
    RunPlan,
    DiscardPlan,
    PlanFeedback {
        note: String,
    },
    StopNight,
    ProposeNight,
    /// Open the night-shift form, with the goal filled in if one was said. A score command
    /// is not something to dictate.
    NightSetup {
        goal: Option<String>,
    },
    OpenApp {
        name: String,
    },
    OpenUrl {
        url: String,
    },
    OpenFolder {
        path: String,
    },
    RevealProject,
    OpenProjectInEditor,
    /// A web search, on a site ("youtube") or the web, in a browser if one was named.
    Search {
        query: String,
        site: everyday::Site,
        browser: Option<String>,
    },
    /// Play, pause, skip… in Spotify or Music; `None` means whichever is playing.
    Media {
        app: Option<everyday::Player>,
        control: everyday::Control,
    },
    /// A playlist by name. Music plays it; Spotify opens its search for it.
    PlayPlaylist {
        app: Option<everyday::Player>,
        name: String,
    },
    /// Anything else to play ("some jazz", "Drake").
    PlayQuery {
        app: Option<everyday::Player>,
        query: String,
    },
    NewNote {
        text: String,
    },
    Remind {
        text: String,
        when: Option<everyday::When>,
    },
    /// The Mac's own volume and display.
    System {
        control: everyday::SystemControl,
    },
    /// Answers to a pending confirmation.
    Confirm,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pane {
    Chat,
    Plan,
    Night,
    Preview,
    Tools,
    Settings,
}

impl Pane {
    pub const ALL: [Pane; 6] = [
        Pane::Chat,
        Pane::Plan,
        Pane::Night,
        Pane::Preview,
        Pane::Tools,
        Pane::Settings,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Pane::Chat => "chat",
            Pane::Plan => "plan",
            Pane::Night => "night",
            Pane::Preview => "preview",
            Pane::Tools => "tools",
            Pane::Settings => "settings",
        }
    }

    pub fn parse(text: &str) -> Option<Pane> {
        Pane::ALL.into_iter().find(|pane| pane.as_str() == text)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusTopic {
    /// Everything at once, briefly.
    Overview,
    Workers,
    /// What is waiting for the person: merges and delegations.
    Waiting,
    Plan,
    Night,
    /// Subscription limits. Answered by the app, which holds the quota report.
    Limits,
}

impl StatusTopic {
    pub const ALL: [StatusTopic; 6] = [
        StatusTopic::Overview,
        StatusTopic::Workers,
        StatusTopic::Waiting,
        StatusTopic::Plan,
        StatusTopic::Night,
        StatusTopic::Limits,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            StatusTopic::Overview => "overview",
            StatusTopic::Workers => "workers",
            StatusTopic::Waiting => "waiting",
            StatusTopic::Plan => "plan",
            StatusTopic::Night => "night",
            StatusTopic::Limits => "limits",
        }
    }

    pub fn parse(text: &str) -> Option<StatusTopic> {
        StatusTopic::ALL
            .into_iter()
            .find(|topic| topic.as_str() == text)
    }
}

/// The harness as voice sees it: enough to resolve "the builder" or "it", and to answer
/// "what's waiting for me?" without asking anything else.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub projects: Vec<ProjectRef>,
    /// Root of the project in front, if any.
    pub active_project: Option<String>,
    /// In the order they started, so the last is the most recent.
    pub workers: Vec<WorkerRef>,
    pub plan: Option<PlanRef>,
    pub night: Option<NightRef>,
    pub autonomy: Autonomy,
    /// Workers whose merge landed and can still be undone, most recent last.
    pub landed: Vec<String>,
    /// Apps installed on this computer, by name ("Notes", "Visual Studio Code"). "Open X"
    /// is an app command only if X is one of these; empty means unknown, and the name is
    /// then taken as said.
    #[serde(default)]
    pub apps: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectRef {
    pub root: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerRef {
    pub id: String,
    /// Spoken number: "worker 3".
    pub number: u32,
    pub role: String,
    /// `WorkerStatus` in its serde form: `running`, `done`, `awaiting_approval`, …
    pub status: String,
    /// First line of the task, for telling workers apart.
    pub task: String,
    /// A merge is proposed and waiting for the person.
    pub merge_pending: bool,
    /// Risk of the pending merge, once checked: `low`, `medium`, `high`, or `unverified`.
    pub risk: Option<String>,
}

impl WorkerRef {
    pub fn is_running(&self) -> bool {
        matches!(
            self.status.as_str(),
            "queued" | "blocked" | "preparing" | "running"
        )
    }

    pub fn awaiting_approval(&self) -> bool {
        self.status == "awaiting_approval"
    }

    /// How to say it: "the builder (#3)".
    pub fn spoken(&self) -> String {
        format!("the {} (#{})", self.role.replace('_', " "), self.number)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanRef {
    pub title: String,
    /// `draft`, `running`, `finished`, `discarded`.
    pub status: String,
    pub landed: usize,
    pub total: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NightRef {
    /// `running`, `finished`, `stopped`.
    pub status: String,
    pub kept: usize,
    pub tried: usize,
    pub baseline: Option<f64>,
    pub best: Option<f64>,
    pub proposed: bool,
}

/// What the person's words come to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    /// Do this. `confirm` means ask first; `describe` is how to say what will happen.
    Act {
        action: VoiceAction,
        confirm: bool,
        describe: String,
    },
    /// Understood, but something is missing: ask this.
    Clarify { question: String },
    /// Understood, and there is nothing to do: say this (e.g. "there's no plan to run").
    Reply { text: String },
    /// Not a command for the app: send it to the head agent.
    ToHead { text: String },
    /// Nothing was said.
    Nothing,
}

/// Which step decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Matcher,
    Laya,
    /// Neither: the words go to the head agent as they are.
    Fallback,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Interpretation {
    pub transcript: String,
    pub outcome: Outcome,
    pub source: Source,
    /// Laya's probability for the intent it chose, when Laya decided or was asked.
    pub confidence: Option<f64>,
    /// Laya's own time for the call, when it ran.
    pub laya_ms: Option<u64>,
    /// Spoken answer for a status question, filled in here for everything but limits.
    pub reply: Option<String>,
}

/// Whether an action must be confirmed before it runs: anything that changes code on the
/// person's branch, throws work away, stops work, or raises autonomy.
pub fn needs_confirmation(action: &VoiceAction, snapshot: &Snapshot) -> bool {
    use VoiceAction::*;
    match action {
        ApproveMerge { .. }
        | RejectMerge { .. }
        | UndoMerge { .. }
        | ApproveDelegation { .. }
        | DeclineDelegation { .. }
        | StopWorker { .. }
        | RunPlan
        | DiscardPlan
        | StopNight => true,
        SetAutonomy { level } => autonomy_rank(*level) > autonomy_rank(snapshot.autonomy),
        _ => false,
    }
}

fn autonomy_rank(level: Autonomy) -> u8 {
    match level {
        Autonomy::Ask => 0,
        Autonomy::Review => 1,
        Autonomy::LandSafe => 2,
        Autonomy::LandMost => 3,
    }
}

fn worker<'a>(snapshot: &'a Snapshot, id: &str) -> Option<&'a WorkerRef> {
    snapshot.workers.iter().find(|w| w.id == id)
}

fn worker_label(snapshot: &Snapshot, id: &str) -> String {
    worker(snapshot, id)
        .map(WorkerRef::spoken)
        .unwrap_or_else(|| id.to_string())
}

fn autonomy_words(level: Autonomy) -> &'static str {
    match level {
        Autonomy::Ask => "Ask",
        Autonomy::Review => "Review",
        Autonomy::LandSafe => "Land safe",
        Autonomy::LandMost => "Land most",
    }
}

/// Check an action against the harness as it is, and say what will happen. An action
/// that cannot apply becomes a reply saying why, rather than an error later.
pub fn finalize(action: VoiceAction, snapshot: &Snapshot) -> Outcome {
    use VoiceAction::*;
    let reply = |text: &str| Outcome::Reply {
        text: text.to_string(),
    };

    // Preconditions.
    match &action {
        ApproveMerge { worker: id } | RejectMerge { worker: id, .. } => {
            if !worker(snapshot, id).is_some_and(|w| w.merge_pending) {
                return reply("That worker has no merge waiting.");
            }
        }
        UndoMerge { worker: id } if !snapshot.landed.contains(id) => {
            return reply("That worker has nothing landed to undo.");
        }
        ApproveDelegation { worker: id } | DeclineDelegation { worker: id, .. } => {
            if !worker(snapshot, id).is_some_and(WorkerRef::awaiting_approval) {
                return reply("That worker is not waiting for approval.");
            }
        }
        StopWorker { worker: id } if !worker(snapshot, id).is_some_and(WorkerRef::is_running) => {
            return reply("That worker is not running.");
        }
        OpenWorker { worker: id } if worker(snapshot, id).is_none() => {
            return reply("I don't know that worker.");
        }
        RunPlan => match &snapshot.plan {
            Some(plan) if plan.status == "draft" => {}
            Some(plan) if plan.status == "running" => return reply("The plan is already running."),
            _ => return reply("There is no plan waiting to run."),
        },
        DiscardPlan | PlanFeedback { .. } => {
            if !snapshot
                .plan
                .as_ref()
                .is_some_and(|p| p.status == "draft" || p.status == "running")
            {
                return reply("There is no plan to change.");
            }
            if matches!(action, PlanFeedback { .. })
                && snapshot.plan.as_ref().is_some_and(|p| p.status != "draft")
            {
                return reply("The plan is already running; feedback is for a draft.");
            }
        }
        StopNight
            if snapshot
                .night
                .as_ref()
                .is_none_or(|n| n.status != "running") =>
        {
            return reply("No night shift is running.");
        }
        ProposeNight => match &snapshot.night {
            Some(night) if night.status == "running" => {
                return reply("The night shift is still running.");
            }
            Some(night) if night.proposed => return reply("The night's work is already proposed."),
            Some(night) if night.kept > 0 => {}
            Some(_) => {
                return reply("The night shift kept nothing, so there is nothing to propose.")
            }
            None => return reply("There has been no night shift."),
        },
        SetAutonomy { level } if *level == snapshot.autonomy => {
            return Outcome::Reply {
                text: format!("Autonomy is already {}.", autonomy_words(*level)),
            };
        }
        RevealProject | OpenProjectInEditor if snapshot.active_project.is_none() => {
            return reply("No project is open.");
        }
        _ => {}
    }

    let describe = match &action {
        Navigate { pane } => format!("Show {}", pane.as_str()),
        SwitchProject { project } => {
            let name = snapshot
                .projects
                .iter()
                .find(|p| p.root == *project)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| project.clone());
            format!("Switch to {name}")
        }
        OpenWorker { worker: id } => format!("Open {}", worker_label(snapshot, id)),
        Status { topic } => format!("Status: {}", topic.as_str()),
        AskHead { text } => format!("Tell the head agent: {text}"),
        StopTurn => "Stop the head agent's turn".into(),
        StopWorker { worker: id } => format!("Stop {}", worker_label(snapshot, id)),
        ApproveMerge { worker: id } => {
            let risk = worker(snapshot, id)
                .and_then(|w| w.risk.clone())
                .map(|r| format!(" — {r} risk"))
                .unwrap_or_default();
            format!("Merge {}'s change{risk}", worker_label(snapshot, id))
        }
        RejectMerge { worker: id, .. } => {
            format!("Discard {}'s change", worker_label(snapshot, id))
        }
        UndoMerge { worker: id } => format!("Undo {}'s landing", worker_label(snapshot, id)),
        ApproveDelegation { worker: id } => format!("Let {} start", worker_label(snapshot, id)),
        DeclineDelegation { worker: id, .. } => format!("Decline {}", worker_label(snapshot, id)),
        SetAutonomy { level } => format!("Set autonomy to {}", autonomy_words(*level)),
        RunPlan => format!(
            "Run the plan \"{}\"",
            snapshot
                .plan
                .as_ref()
                .map(|p| p.title.as_str())
                .unwrap_or("")
        ),
        DiscardPlan => "Discard the plan".into(),
        PlanFeedback { note } => format!("Plan feedback: {note}"),
        StopNight => "Stop the night shift".into(),
        ProposeNight => "Propose the night's work for review".into(),
        NightSetup { goal: Some(goal) } => format!("Set up a night shift: {goal}"),
        NightSetup { goal: None } => "Set up a night shift".into(),
        OpenApp { name } => format!("Open {name}"),
        OpenUrl { url } => format!("Open {url}"),
        OpenFolder { path } => format!("Open {path}"),
        RevealProject => "Show the project in Finder".into(),
        OpenProjectInEditor => "Open the project in your editor".into(),
        Search {
            query,
            site,
            browser,
        } => {
            let place = site.label();
            match browser {
                Some(browser) => format!("Search {place} for “{query}” in {browser}"),
                None => format!("Search {place} for “{query}”"),
            }
        }
        Media { app, control } => {
            format!(
                "{} {}",
                control.label(),
                app.map(|a| a.label()).unwrap_or("the music")
            )
        }
        PlayPlaylist { app, name } => format!(
            "Play the playlist “{name}” in {}",
            app.map(|a| a.label()).unwrap_or("your music app")
        ),
        PlayQuery { app, query } => format!(
            "Play “{query}” in {}",
            app.map(|a| a.label()).unwrap_or("your music app")
        ),
        NewNote { text } => format!("New note: {text}"),
        Remind {
            text,
            when: Some(when),
        } => format!("Remind you to {text} {}", when.label()),
        Remind { text, when: None } => format!("Remind you to {text}"),
        System { control } => control.label().to_string(),
        Confirm => "Yes".into(),
        Cancel => "Cancel".into(),
    };
    Outcome::Act {
        confirm: needs_confirmation(&action, snapshot),
        action,
        describe,
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn score(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.2}").trim_end_matches('0').to_string()
    }
}

/// A short spoken answer to a status question. `None` for limits, which the app answers
/// from its own quota report.
pub fn spoken_status(topic: StatusTopic, snapshot: &Snapshot) -> Option<String> {
    let running: Vec<&WorkerRef> = snapshot.workers.iter().filter(|w| w.is_running()).collect();
    let merges: Vec<&WorkerRef> = snapshot
        .workers
        .iter()
        .filter(|w| w.merge_pending)
        .collect();
    let approvals: Vec<&WorkerRef> = snapshot
        .workers
        .iter()
        .filter(|w| w.awaiting_approval())
        .collect();

    let workers_line = || -> String {
        if running.is_empty() {
            return "No workers are running.".into();
        }
        let named: Vec<String> = running
            .iter()
            .take(3)
            .map(|w| format!("{} on {}", w.spoken(), short(&w.task, 60)))
            .collect();
        let more = running.len().saturating_sub(3);
        format!(
            "{} running: {}{}.",
            plural(running.len(), "worker", "workers"),
            named.join("; "),
            if more > 0 {
                format!("; and {more} more")
            } else {
                String::new()
            }
        )
    };
    let waiting_line = || -> String {
        if merges.is_empty() && approvals.is_empty() {
            return "Nothing is waiting for you.".into();
        }
        let mut parts = Vec::new();
        if !merges.is_empty() {
            let named: Vec<String> = merges
                .iter()
                .take(3)
                .map(|w| match &w.risk {
                    Some(risk) => format!("{}'s, {risk} risk", w.spoken()),
                    None => format!("{}'s, still being checked", w.spoken()),
                })
                .collect();
            parts.push(format!(
                "{} to review: {}",
                plural(merges.len(), "change", "changes"),
                named.join("; ")
            ));
        }
        if !approvals.is_empty() {
            let named: Vec<String> = approvals.iter().take(3).map(|w| w.spoken()).collect();
            parts.push(format!(
                "{} waiting to start: {}",
                plural(approvals.len(), "delegation", "delegations"),
                named.join(", ")
            ));
        }
        format!("{}.", parts.join(". "))
    };
    let plan_line = || -> String {
        match &snapshot.plan {
            None => "There is no plan.".into(),
            Some(plan) => match plan.status.as_str() {
                "draft" => format!(
                    "The plan \"{}\" is waiting for you to review it.",
                    plan.title
                ),
                "running" => format!(
                    "The plan \"{}\" is running: {} of {} steps landed.",
                    plan.title, plan.landed, plan.total
                ),
                _ => format!(
                    "The plan \"{}\" finished with {} of {} steps landed.",
                    plan.title, plan.landed, plan.total
                ),
            },
        }
    };
    let night_line = || -> String {
        let Some(night) = &snapshot.night else {
            return "No night shift has run.".into();
        };
        let scores = match (night.baseline, night.best) {
            (Some(base), Some(best)) if best != base => {
                format!(", from {} to {}", score(base), score(best))
            }
            _ => String::new(),
        };
        let state = match night.status.as_str() {
            "running" => "is running",
            "stopped" => "stopped",
            _ => "finished",
        };
        format!(
            "The night shift {state}: {} kept of {} tried{scores}.",
            night.kept, night.tried
        )
    };

    Some(match topic {
        StatusTopic::Limits => return None,
        StatusTopic::Workers => workers_line(),
        StatusTopic::Waiting => waiting_line(),
        StatusTopic::Plan => plan_line(),
        StatusTopic::Night => night_line(),
        StatusTopic::Overview => {
            let mut lines = vec![workers_line(), waiting_line()];
            if snapshot
                .plan
                .as_ref()
                .is_some_and(|p| p.status == "draft" || p.status == "running")
            {
                lines.push(plan_line());
            }
            if snapshot
                .night
                .as_ref()
                .is_some_and(|n| n.status == "running")
            {
                lines.push(night_line());
            }
            lines.join(" ")
        }
    })
}

fn short(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    if line.chars().count() > max {
        format!("{}…", line.chars().take(max).collect::<String>())
    } else {
        line.to_string()
    }
}

/// The whole pipeline: matcher, then Laya if it is there, then the head agent.
///
/// `pending` is the description of an action waiting for a yes or no, if any; then the
/// words are read as that answer first.
pub async fn interpret(
    transcript: &str,
    snapshot: &Snapshot,
    laya: Option<&std::sync::Arc<laya::LayaClient>>,
    threshold: f64,
    pending: Option<&str>,
) -> Interpretation {
    let transcript = transcript.trim().to_string();
    let done = |outcome: Outcome, source: Source, confidence: Option<f64>, laya_ms: Option<u64>| {
        let reply = match &outcome {
            Outcome::Act {
                action: VoiceAction::Status { topic },
                ..
            } => spoken_status(*topic, snapshot),
            _ => None,
        };
        Interpretation {
            transcript: transcript.clone(),
            outcome,
            source,
            confidence,
            laya_ms,
            reply,
        }
    };
    if matcher::normalize(&transcript).is_empty() {
        return done(Outcome::Nothing, Source::Matcher, None, None);
    }

    if let Some(action) = matcher::match_command(&transcript, snapshot, pending.is_some()) {
        return done(finalize(action, snapshot), Source::Matcher, None, None);
    }

    if let Some(client) = laya {
        let slots = matcher::slots(&transcript);
        let questions = laya::questions(snapshot, pending);
        let state = laya::state(&transcript, snapshot);
        match client.decide(&state, &questions).await {
            Ok(reply) => {
                let decision = laya::decide(
                    &reply.answers,
                    snapshot,
                    &slots,
                    &transcript,
                    threshold,
                    pending.is_some(),
                );
                let outcome = match decision.action {
                    Some(action) => finalize(action, snapshot),
                    None => decision.otherwise,
                };
                let source = if matches!(outcome, Outcome::ToHead { .. }) {
                    Source::Fallback
                } else {
                    Source::Laya
                };
                return done(outcome, source, decision.confidence, Some(reply.ms));
            }
            Err(err) => tracing::warn!("Laya could not decide, sending the words on: {err:#}"),
        }
    }

    // A pending question gets an answer, not a message to the head agent.
    if pending.is_some() {
        return done(
            Outcome::Clarify {
                question: "Say yes to go ahead, or cancel.".into(),
            },
            Source::Fallback,
            None,
            None,
        );
    }
    done(
        Outcome::ToHead {
            text: transcript.clone(),
        },
        Source::Fallback,
        None,
        None,
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn worker(id: &str, number: u32, role: &str, status: &str, task: &str) -> WorkerRef {
        WorkerRef {
            id: id.into(),
            number,
            role: role.into(),
            status: status.into(),
            task: task.into(),
            merge_pending: false,
            risk: None,
        }
    }

    /// A harness mid-afternoon: a builder with a merge waiting, a tester running, a
    /// delegation waiting under Ask, a draft plan, a running night shift.
    pub fn snapshot() -> Snapshot {
        super::eval::fixture()
    }

    #[test]
    fn only_actions_that_change_or_stop_things_are_confirmed() {
        let s = snapshot();
        assert!(needs_confirmation(
            &VoiceAction::ApproveMerge {
                worker: "w-aaa".into()
            },
            &s
        ));
        assert!(needs_confirmation(&VoiceAction::RunPlan, &s));
        assert!(needs_confirmation(&VoiceAction::StopNight, &s));
        assert!(!needs_confirmation(
            &VoiceAction::Navigate { pane: Pane::Plan },
            &s
        ));
        assert!(!needs_confirmation(
            &VoiceAction::OpenApp {
                name: "Safari".into()
            },
            &s
        ));
        // Raising autonomy is confirmed; lowering it never needs to be.
        assert!(needs_confirmation(
            &VoiceAction::SetAutonomy {
                level: Autonomy::LandSafe
            },
            &s
        ));
        assert!(!needs_confirmation(
            &VoiceAction::SetAutonomy {
                level: Autonomy::Ask
            },
            &s
        ));
    }

    #[test]
    fn an_action_that_cannot_apply_says_why() {
        let s = snapshot();
        let reply = |action| match finalize(action, &s) {
            Outcome::Reply { text } => text,
            other => panic!("expected a reply, got {other:?}"),
        };
        assert!(reply(VoiceAction::ApproveMerge {
            worker: "w-bbb".into()
        })
        .contains("no merge"));
        assert!(reply(VoiceAction::StopWorker {
            worker: "w-aaa".into()
        })
        .contains("not running"));
        assert!(reply(VoiceAction::ProposeNight).contains("still running"));
        assert!(reply(VoiceAction::SetAutonomy {
            level: Autonomy::Review
        })
        .contains("already"));
        assert!(reply(VoiceAction::UndoMerge {
            worker: "w-aaa".into()
        })
        .contains("nothing landed"));

        match finalize(
            VoiceAction::ApproveMerge {
                worker: "w-aaa".into(),
            },
            &s,
        ) {
            Outcome::Act {
                confirm, describe, ..
            } => {
                assert!(confirm);
                assert_eq!(describe, "Merge the builder (#1)'s change — low risk");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn status_is_answered_in_a_sentence() {
        let s = snapshot();
        let waiting = spoken_status(StatusTopic::Waiting, &s).unwrap();
        assert!(
            waiting.contains("1 change to review: the builder (#1)'s, low risk"),
            "{waiting}"
        );
        assert!(
            waiting.contains("1 delegation waiting to start: the reviewer (#3)"),
            "{waiting}"
        );
        let night = spoken_status(StatusTopic::Night, &s).unwrap();
        assert_eq!(
            night,
            "The night shift is running: 2 kept of 5 tried, from 58.7 to 33.9."
        );
        let overview = spoken_status(StatusTopic::Overview, &s).unwrap();
        assert!(
            overview.starts_with("1 worker running: the tester (#2) on Run the integration tests.")
        );
        assert!(
            spoken_status(StatusTopic::Limits, &s).is_none(),
            "the app answers limits"
        );
        assert_eq!(
            spoken_status(StatusTopic::Waiting, &Snapshot::default()).unwrap(),
            "Nothing is waiting for you."
        );
    }

    #[tokio::test]
    async fn without_laya_what_the_matcher_misses_goes_to_the_head_agent() {
        let s = snapshot();
        let heard = interpret(
            "the fetcher keeps timing out, can we fix it",
            &s,
            None,
            0.75,
            None,
        )
        .await;
        assert_eq!(heard.source, Source::Fallback);
        assert_eq!(
            heard.outcome,
            Outcome::ToHead {
                text: "the fetcher keeps timing out, can we fix it".into()
            }
        );
        let status = interpret("what's waiting for me?", &s, None, 0.75, None).await;
        assert_eq!(status.source, Source::Matcher);
        assert!(status.reply.unwrap().contains("to review"));
        assert_eq!(
            interpret("  um  ", &s, None, 0.75, None).await.outcome,
            Outcome::Nothing
        );
    }
}
