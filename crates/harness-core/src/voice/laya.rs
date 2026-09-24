//! Laya: a small open decision model for what the phrase matcher misses.
//!
//! Laya (Convai Innovations, Apache 2.0, 421M parameters) does not write text. It reads a
//! state and answers typed questions — pick one of these options, or yes/no — with a
//! calibrated probability for every option, all in one forward pass. That fits voice:
//! the options are the harness's own actions and the workers that exist, so it cannot
//! invent one, and a low probability is an honest "not sure" that sends the words to the
//! head agent instead of acting on a guess.
//!
//! Its own card says it is a base to fine-tune rather than a zero-shot engine, so the
//! threshold matters, and `harness-cli voice --eval` measures it on our phrases.
//!
//! Each question's options must fit in 192 tokens, which rules out one question listing
//! every action with a description. Instead a short `domain` question picks the area and
//! small follow-up questions pick within it; all are answered in the same call.
//!
//! The model runs in a Node helper (`app/voice-sidecar`) using the official
//! `@receptron/laya` package, spoken to one JSON object per line.

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use super::matcher::{self, Slots};
use super::{Outcome, Pane, Snapshot, StatusTopic, VoiceAction, WorkerRef};
use crate::autonomy::Autonomy;

/// At most this many workers are offered by name; the most relevant first.
const MAX_WORKERS: usize = 8;
const MAX_PROJECTS: usize = 10;

fn short(text: &str, words: usize) -> String {
    let line = text.lines().next().unwrap_or_default();
    let taken: Vec<&str> = line.split_whitespace().take(words).collect();
    let more = line.split_whitespace().count() > words;
    format!("{}{}", taken.join(" "), if more { "…" } else { "" })
}

/// Workers worth offering, most relevant first: waiting for the person, then running,
/// then the most recent.
fn offered_workers(snapshot: &Snapshot) -> Vec<&WorkerRef> {
    let mut workers: Vec<&WorkerRef> = snapshot.workers.iter().collect();
    workers.sort_by_key(|w| {
        let rank = if w.merge_pending || w.awaiting_approval() {
            0
        } else if w.is_running() {
            1
        } else {
            2
        };
        (rank, std::cmp::Reverse(w.number))
    });
    workers.truncate(MAX_WORKERS);
    workers
}

fn worker_line(w: &WorkerRef) -> String {
    let mut line = format!(
        "{} {}: {}",
        w.role.replace('_', " "),
        w.status.replace('_', " "),
        short(&w.task, 5)
    );
    if w.merge_pending {
        line.push_str(&format!(
            ", change waiting ({})",
            w.risk.as_deref().unwrap_or("unchecked")
        ));
    }
    line
}

/// What Laya reads: the words, and just enough of the harness to read them in context
/// ("approve it" means something when a merge is waiting).
pub fn state(transcript: &str, snapshot: &Snapshot) -> Value {
    let mut state = Map::new();
    state.insert("person_said".into(), json!(transcript));
    let waiting: Vec<String> = snapshot
        .workers
        .iter()
        .filter(|w| w.merge_pending || w.awaiting_approval())
        .take(4)
        .map(|w| format!("#{} {}", w.number, worker_line(w)))
        .collect();
    if !waiting.is_empty() {
        state.insert("waiting_for_person".into(), json!(waiting));
    }
    let running: Vec<String> = snapshot
        .workers
        .iter()
        .filter(|w| w.is_running())
        .take(4)
        .map(|w| format!("#{} {}", w.number, w.role.replace('_', " ")))
        .collect();
    if !running.is_empty() {
        state.insert("running".into(), json!(running));
    }
    if let Some(plan) = &snapshot.plan {
        state.insert(
            "plan".into(),
            json!(format!("{}: {}", plan.status, plan.title)),
        );
    }
    if let Some(night) = &snapshot.night {
        state.insert("night_shift".into(), json!(night.status));
    }
    Value::Object(state)
}

fn choice(instructions: &str, criteria: &[(&str, &str)]) -> Value {
    let criteria: Map<String, Value> = criteria
        .iter()
        .map(|(key, text)| (key.to_string(), json!(text)))
        .collect();
    json!({ "type": "choice", "instructions": instructions, "criteria": criteria })
}

const DOMAINS: [(&str, &str); 9] = [
    (
        "status",
        "asks what is running, waiting, or how things are going",
    ),
    ("navigate", "wants to see a tab, a page, or another project"),
    ("work", "asks for work on the code, or says anything else"),
    ("stop_assistant", "tells the assistant to stop or be quiet"),
    (
        "workers",
        "approve, reject, undo, stop or open a worker or its change",
    ),
    ("autonomy", "changes how much runs without asking"),
    ("plan", "run or drop the plan"),
    ("night_shift", "about the overnight improvement run"),
    ("computer", "open an app, a website or a folder"),
];

const WORKER_ACTIONS: [(&str, &str); 7] = [
    ("approve_merge", "approve or merge a change"),
    ("reject_merge", "reject or discard a change"),
    ("undo", "undo a change that landed"),
    ("approve_task", "let a waiting task start"),
    ("decline_task", "refuse a waiting task"),
    ("stop", "stop a running worker"),
    ("open", "look at a worker"),
];

/// The questions for one utterance. While a confirmation is pending, the only question
/// is whether the words agree to it.
pub fn questions(snapshot: &Snapshot, pending: Option<&str>) -> Value {
    let mut q = Map::new();
    if let Some(pending) = pending {
        q.insert(
            "yes".into(),
            json!({
                "type": "noul",
                "instructions": format!("Does the person agree to go ahead with: {pending}?"),
            }),
        );
        return Value::Object(q);
    }

    q.insert(
        "domain".into(),
        choice("What does the person want the coding app to do?", &DOMAINS),
    );
    q.insert(
        "status_topic".into(),
        choice(
            "What are they asking about?",
            &[
                ("overview", "everything, briefly"),
                ("workers", "which workers are running"),
                ("waiting", "what waits for their review or approval"),
                ("plan", "the plan"),
                ("night", "the night shift"),
                ("limits", "how much usage is left"),
            ],
        ),
    );
    let mut views = vec![
        ("chat", "the chat"),
        ("plan", "the plan board"),
        ("night", "the night shift page"),
        ("preview", "the app preview"),
        ("tools", "tools and skills"),
        ("settings", "settings"),
    ];
    if snapshot.projects.len() > 1 {
        views.push(("project", "another project"));
    }
    q.insert(
        "view".into(),
        choice("Which part of the app do they want to see?", &views),
    );

    if !snapshot.workers.is_empty() {
        q.insert(
            "worker_action".into(),
            choice("What should happen to the worker?", &WORKER_ACTIONS),
        );
        let offered = offered_workers(snapshot);
        let mut options: Vec<(String, String)> = offered
            .iter()
            .map(|w| (format!("#{}", w.number), worker_line(w)))
            .collect();
        options.push(("none".into(), "no particular worker".into()));
        let options: Vec<(&str, &str)> = options
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        q.insert(
            "worker".into(),
            choice("Which worker do they mean?", &options),
        );
    }
    q.insert(
        "autonomy".into(),
        choice(
            "Which autonomy level do they want?",
            &[
                ("ask", "ask before every task"),
                ("review", "review every change"),
                ("land_safe", "low-risk changes land by themselves"),
                ("land_most", "most changes land by themselves"),
            ],
        ),
    );
    if snapshot.plan.is_some() {
        q.insert(
            "plan_action".into(),
            choice(
                "What should happen to the plan?",
                &[("run", "run it"), ("discard", "drop or stop it")],
            ),
        );
    }
    q.insert(
        "night_action".into(),
        choice(
            "What about the night shift?",
            &[
                ("stop", "stop it"),
                ("propose", "put its work up for review"),
                ("setup", "start a new one"),
            ],
        ),
    );
    q.insert(
        "computer_target".into(),
        choice(
            "What should be opened?",
            &[
                ("app", "an application"),
                ("website", "a website"),
                ("folder", "a folder like downloads"),
                ("project_finder", "the project in Finder"),
                ("project_editor", "the project in a code editor"),
            ],
        ),
    );
    q.insert(
        "folder".into(),
        choice(
            "Which folder?",
            &[
                ("downloads", "downloads"),
                ("documents", "documents"),
                ("desktop", "desktop"),
                ("home", "home"),
            ],
        ),
    );
    if snapshot.projects.len() > 1 {
        let options: Vec<(String, String)> = snapshot
            .projects
            .iter()
            .take(MAX_PROJECTS)
            .enumerate()
            .map(|(i, p)| (format!("p{}", i + 1), p.name.clone()))
            .collect();
        let options: Vec<(&str, &str)> = options
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        q.insert("project".into(), choice("Which project?", &options));
    }
    Value::Object(q)
}

/// A choice answer: the option and its probability.
fn picked(answers: &Map<String, Value>, question: &str) -> Option<(String, f64)> {
    let answer = answers.get(question)?;
    let option = answer.get("choice")?.as_str()?.to_string();
    let p = answer
        .pointer(&format!(
            "/probabilities/{}",
            option.replace('~', "~0").replace('/', "~1")
        ))?
        .as_f64()?;
    Some((option, p))
}

/// What Laya's answers come to. `action: None` means `otherwise` says what to do instead.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub action: Option<VoiceAction>,
    pub otherwise: Outcome,
    /// The lowest probability among the answers the decision rests on.
    pub confidence: Option<f64>,
}

pub fn decide(
    answers: &Map<String, Value>,
    snapshot: &Snapshot,
    slots: &Slots,
    transcript: &str,
    threshold: f64,
    pending: bool,
) -> Decision {
    let to_head = |confidence| Decision {
        action: None,
        otherwise: Outcome::ToHead {
            text: transcript.to_string(),
        },
        confidence,
    };
    let ask = |question: &str, confidence| Decision {
        action: None,
        otherwise: Outcome::Clarify {
            question: question.to_string(),
        },
        confidence,
    };
    let act = |action, confidence| Decision {
        action: Some(action),
        otherwise: Outcome::Nothing,
        confidence,
    };

    if pending {
        let p = answers
            .get("yes")
            .and_then(|a| a.get("noul"))
            .and_then(Value::as_f64);
        return match p {
            Some(p) if p >= threshold => act(VoiceAction::Confirm, Some(p)),
            Some(p) if p <= 1.0 - threshold => act(VoiceAction::Cancel, Some(1.0 - p)),
            other => ask("Say yes to go ahead, or cancel.", other),
        };
    }

    let Some((domain, p_domain)) = picked(answers, "domain") else {
        return to_head(None);
    };
    if p_domain < threshold {
        return to_head(Some(p_domain));
    }
    // A follow-up answer counts only if it is as sure as the threshold; the confidence
    // reported is the weaker of the two.
    let sure = |question: &str| -> Option<(String, f64)> {
        picked(answers, question).filter(|(_, p)| *p >= threshold)
    };
    let both = |p: f64| Some(p_domain.min(p));

    match domain.as_str() {
        "work" => Decision {
            action: None,
            otherwise: Outcome::ToHead {
                text: slots
                    .message
                    .clone()
                    .unwrap_or_else(|| transcript.to_string()),
            },
            confidence: Some(p_domain),
        },
        "stop_assistant" => act(VoiceAction::StopTurn, Some(p_domain)),
        "status" => {
            let (topic, p) = sure("status_topic")
                .and_then(|(t, p)| StatusTopic::parse(&t).map(|t| (t, p)))
                .unwrap_or((StatusTopic::Overview, p_domain));
            act(VoiceAction::Status { topic }, both(p))
        }
        "navigate" => match sure("view") {
            Some((view, p)) if view == "project" => match sure("project") {
                Some((key, p2)) => {
                    let index: usize = key.trim_start_matches('p').parse().unwrap_or(0);
                    match snapshot.projects.get(index.wrapping_sub(1)) {
                        Some(project) => act(
                            VoiceAction::SwitchProject {
                                project: project.root.clone(),
                            },
                            both(p.min(p2)),
                        ),
                        None => ask("Which project?", both(p)),
                    }
                }
                None => ask("Which project?", both(p)),
            },
            Some((view, p)) => match Pane::parse(&view) {
                Some(pane) => act(VoiceAction::Navigate { pane }, both(p)),
                None => ask("Which part of the app?", Some(p_domain)),
            },
            None => ask("Which part of the app?", Some(p_domain)),
        },
        "workers" => {
            let Some((action, p)) = sure("worker_action") else {
                return ask("What should happen to the worker?", Some(p_domain));
            };
            let candidates: Vec<&WorkerRef> = snapshot
                .workers
                .iter()
                .filter(|w| match action.as_str() {
                    "approve_merge" | "reject_merge" => w.merge_pending,
                    "undo" => snapshot.landed.contains(&w.id),
                    "approve_task" | "decline_task" => w.awaiting_approval(),
                    "stop" => w.is_running(),
                    _ => true,
                })
                .collect();
            if action == "undo" && candidates.is_empty() {
                // The last landing may be a worker from before this snapshot's list.
                return match snapshot.landed.last() {
                    Some(id) => act(VoiceAction::UndoMerge { worker: id.clone() }, both(p)),
                    None => Decision {
                        action: None,
                        otherwise: Outcome::Reply {
                            text: "Nothing has landed that can be undone.".into(),
                        },
                        confidence: both(p),
                    },
                };
            }
            if candidates.is_empty() {
                let text = match action.as_str() {
                    "approve_merge" | "reject_merge" => "No change is waiting for you.",
                    "approve_task" | "decline_task" => "No task is waiting for approval.",
                    "stop" => "No worker is running.",
                    _ => "There are no workers yet.",
                };
                return Decision {
                    action: None,
                    otherwise: Outcome::Reply { text: text.into() },
                    confidence: both(p),
                };
            }
            // An explicit reference in the words wins; then Laya's pick among the
            // candidates; then the only candidate, if there is just one.
            let words = matcher::normalize(transcript);
            let chosen = matcher::resolve(&words, &candidates, &snapshot.workers)
                .map(|w| (w, p))
                .or_else(|| {
                    let (key, p2) = sure("worker")?;
                    let number: u32 = key.trim_start_matches('#').parse().ok()?;
                    candidates
                        .iter()
                        .copied()
                        .find(|w| w.number == number)
                        .map(|w| (w, p.min(p2)))
                });
            let Some((worker, p)) = chosen else {
                let named: Vec<String> = candidates
                    .iter()
                    .take(4)
                    .map(|w| format!("#{} the {}", w.number, w.role.replace('_', " ")))
                    .collect();
                return ask(&format!("Which one: {}?", named.join(", or ")), both(p));
            };
            let id = worker.id.clone();
            let action = match action.as_str() {
                "approve_merge" => VoiceAction::ApproveMerge { worker: id },
                "reject_merge" => VoiceAction::RejectMerge {
                    worker: id,
                    reason: slots.reason.clone(),
                },
                "undo" => VoiceAction::UndoMerge { worker: id },
                "approve_task" => VoiceAction::ApproveDelegation { worker: id },
                "decline_task" => VoiceAction::DeclineDelegation {
                    worker: id,
                    reason: slots.reason.clone(),
                },
                "stop" => VoiceAction::StopWorker { worker: id },
                _ => VoiceAction::OpenWorker { worker: id },
            };
            act(action, both(p))
        }
        "autonomy" => match sure("autonomy").and_then(|(l, p)| Autonomy::parse(&l).map(|l| (l, p)))
        {
            Some((level, p)) => act(VoiceAction::SetAutonomy { level }, both(p)),
            None => ask(
                "Which level: ask, review, land safe, or land most?",
                Some(p_domain),
            ),
        },
        "plan" => match sure("plan_action") {
            Some((a, p)) if a == "run" => act(VoiceAction::RunPlan, both(p)),
            Some((_, p)) => act(VoiceAction::DiscardPlan, both(p)),
            None if snapshot.plan.is_none() => Decision {
                action: None,
                otherwise: Outcome::Reply {
                    text: "There is no plan.".into(),
                },
                confidence: Some(p_domain),
            },
            None => ask("Run the plan, or drop it?", Some(p_domain)),
        },
        "night_shift" => match sure("night_action") {
            Some((a, p)) if a == "stop" => act(VoiceAction::StopNight, both(p)),
            Some((a, p)) if a == "propose" => act(VoiceAction::ProposeNight, both(p)),
            Some((_, p)) => act(
                VoiceAction::NightSetup {
                    goal: slots.goal.clone(),
                },
                both(p),
            ),
            None => ask(
                "Stop the night shift, propose its work, or start a new one?",
                Some(p_domain),
            ),
        },
        "computer" => match sure("computer_target") {
            // Laya says "an app"; only an app that is installed makes it one. Anything
            // else after "open" is most likely about the work.
            Some((t, p)) if t == "app" => match slots.app.as_deref() {
                Some(phrase) => match matcher::resolve_open(phrase, snapshot) {
                    Some(action) => act(action, both(p)),
                    None => to_head(both(p)),
                },
                None => ask("Which app?", both(p)),
            },
            Some((t, p)) if t == "website" => match &slots.url {
                Some(url) => act(VoiceAction::OpenUrl { url: url.clone() }, both(p)),
                None => ask("Which website?", both(p)),
            },
            Some((t, p)) if t == "project_finder" => act(VoiceAction::RevealProject, both(p)),
            Some((t, p)) if t == "project_editor" => act(VoiceAction::OpenProjectInEditor, both(p)),
            Some((_, p)) => match sure("folder") {
                Some((folder, p2)) => {
                    let path = match folder.as_str() {
                        "downloads" => "~/Downloads",
                        "documents" => "~/Documents",
                        "desktop" => "~/Desktop",
                        _ => "~",
                    };
                    act(
                        VoiceAction::OpenFolder { path: path.into() },
                        both(p.min(p2)),
                    )
                }
                None => ask("Which folder?", both(p)),
            },
            None => ask("What should I open?", Some(p_domain)),
        },
        _ => to_head(Some(p_domain)),
    }
}

// ---------------------------------------------------------------------- the helper

#[derive(Debug, Clone)]
pub struct LayaConfig {
    /// The Node executable.
    pub node: PathBuf,
    /// `laya-server.mjs` in the voice sidecar folder.
    pub script: PathBuf,
    /// Unload after this long unused, to give back the ~2 GB the model holds.
    pub idle: Duration,
    /// Ceiling for one decision.
    pub timeout: Duration,
    /// Ceiling for loading, which may include the first ~1.7 GB download.
    pub load_timeout: Duration,
}

impl LayaConfig {
    pub fn new(script: PathBuf) -> Self {
        Self {
            node: PathBuf::from("node"),
            script,
            idle: Duration::from_secs(10 * 60),
            timeout: Duration::from_secs(10),
            load_timeout: Duration::from_secs(60 * 60),
        }
    }

    /// The sidecar this source tree ships, for development runs.
    pub fn bundled_script() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../app/voice-sidecar/laya-server.mjs")
    }
}

/// Where Laya stands, for Settings and the voice bar.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LayaState {
    /// The helper or its package is not there; `hint` says how to install it.
    NotInstalled {
        hint: String,
    },
    /// Not running. `downloaded` is whether the weights are already on disk, when known.
    Stopped {
        downloaded: Option<bool>,
    },
    Loading {
        file: Option<String>,
        received: u64,
        total: Option<u64>,
    },
    Ready,
    Failed {
        error: String,
    },
}

#[derive(Debug, Clone)]
pub struct LayaReply {
    pub answers: Map<String, Value>,
    pub ms: u64,
}

struct Helper {
    _child: Child,
    stdin: ChildStdin,
    stdout: tokio::io::Lines<BufReader<ChildStdout>>,
}

/// The Node helper, started on demand and stopped when idle. One request at a time.
pub struct LayaClient {
    config: LayaConfig,
    helper: tokio::sync::Mutex<Option<Helper>>,
    state: tokio::sync::watch::Sender<LayaState>,
    last_used: std::sync::Mutex<Instant>,
    next_id: AtomicU64,
}

impl LayaClient {
    pub fn new(config: LayaConfig) -> Arc<Self> {
        let initial = match Self::check_installed(&config) {
            Ok(()) => LayaState::Stopped { downloaded: None },
            Err(hint) => LayaState::NotInstalled { hint },
        };
        Arc::new(Self {
            config,
            helper: tokio::sync::Mutex::new(None),
            state: tokio::sync::watch::channel(initial).0,
            last_used: std::sync::Mutex::new(Instant::now()),
            next_id: AtomicU64::new(1),
        })
    }

    fn check_installed(config: &LayaConfig) -> std::result::Result<(), String> {
        let dir = config
            .script
            .parent()
            .map(PathBuf::from)
            .unwrap_or_default();
        if !config.script.is_file() {
            return Err(format!(
                "the voice helper is missing: {}",
                config.script.display()
            ));
        }
        if !dir.join("node_modules/@receptron/laya").is_dir() {
            return Err(format!("run `npm install` in {}", dir.display()));
        }
        Ok(())
    }

    pub fn state(&self) -> LayaState {
        self.state.borrow().clone()
    }

    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<LayaState> {
        self.state.subscribe()
    }

    fn set(&self, state: LayaState) {
        self.state.send_replace(state);
    }

    async fn spawn(&self) -> Result<Helper> {
        if let Err(hint) = Self::check_installed(&self.config) {
            self.set(LayaState::NotInstalled { hint: hint.clone() });
            bail!(hint);
        }
        let mut child = Command::new(&self.config.node)
            .arg(&self.config.script)
            .current_dir(
                self.config
                    .script
                    .parent()
                    .unwrap_or(std::path::Path::new(".")),
            )
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| {
                format!(
                    "starting {} — is Node 20+ installed?",
                    self.config.node.display()
                )
            })?;
        let stdin = child.stdin.take().context("helper has no stdin")?;
        let stdout = child.stdout.take().context("helper has no stdout")?;
        Ok(Helper {
            _child: child,
            stdin,
            stdout: BufReader::new(stdout).lines(),
        })
    }

    /// Send one request and wait for its answer, passing progress along as it comes.
    async fn call(
        &self,
        helper: &mut Helper,
        mut request: Value,
        timeout: Duration,
    ) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        request["id"] = json!(id);
        let mut line = serde_json::to_string(&request)?;
        line.push('\n');
        helper
            .stdin
            .write_all(line.as_bytes())
            .await
            .context("the voice helper stopped")?;
        helper.stdin.flush().await?;
        let read = async {
            loop {
                let Some(line) = helper.stdout.next_line().await? else {
                    bail!("the voice helper exited");
                };
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if message.get("event").and_then(Value::as_str) == Some("progress") {
                    self.set(LayaState::Loading {
                        file: message
                            .get("file")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        received: message.get("received").and_then(Value::as_u64).unwrap_or(0),
                        total: message.get("total").and_then(Value::as_u64),
                    });
                    continue;
                }
                if message.get("id").and_then(Value::as_u64) != Some(id) {
                    continue;
                }
                if message.get("ok").and_then(Value::as_bool) == Some(true) {
                    return Ok(message);
                }
                let error = message
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                return Err(anyhow!("{error}"));
            }
        };
        match tokio::time::timeout(timeout, read).await {
            Ok(result) => result,
            Err(_) => bail!(
                "the voice helper did not answer within {}s",
                timeout.as_secs()
            ),
        }
    }

    /// Ask the helper whether the weights are on disk, without loading them.
    pub async fn probe(&self) -> Result<bool> {
        let mut guard = self.helper.lock().await;
        if guard.is_none() {
            *guard = Some(self.spawn().await?);
        }
        let helper = guard.as_mut().expect("just set");
        let reply = self
            .call(helper, json!({ "op": "status" }), self.config.timeout)
            .await;
        match reply {
            Ok(reply) => {
                let downloaded = reply
                    .get("downloaded")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let loaded = reply
                    .get("loaded")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                self.set(if loaded {
                    LayaState::Ready
                } else {
                    LayaState::Stopped {
                        downloaded: Some(downloaded),
                    }
                });
                Ok(downloaded)
            }
            Err(err) => {
                *guard = None;
                Err(err)
            }
        }
    }

    /// Download if needed and load the model. Slow the first time; seconds after that.
    pub async fn load(&self) -> Result<()> {
        let mut guard = self.helper.lock().await;
        if matches!(self.state(), LayaState::Ready) && guard.is_some() {
            return Ok(());
        }
        if guard.is_none() {
            *guard = Some(self.spawn().await?);
        }
        self.set(LayaState::Loading {
            file: None,
            received: 0,
            total: None,
        });
        let helper = guard.as_mut().expect("just set");
        match self
            .call(helper, json!({ "op": "load" }), self.config.load_timeout)
            .await
        {
            Ok(_) => {
                self.touch();
                self.set(LayaState::Ready);
                Ok(())
            }
            Err(err) => {
                *guard = None;
                self.set(LayaState::Failed {
                    error: format!("{err:#}"),
                });
                Err(err)
            }
        }
    }

    /// One decision. Never loads: a first ~1.7 GB download in the middle of a voice
    /// command would stall it. If Laya is not ready, loading starts in the background and
    /// this utterance is decided without it.
    pub async fn decide(self: &Arc<Self>, state: &Value, questions: &Value) -> Result<LayaReply> {
        if !matches!(self.state(), LayaState::Ready) {
            if matches!(
                self.state(),
                LayaState::Stopped {
                    downloaded: Some(true)
                }
            ) {
                let client = Arc::clone(self);
                tokio::spawn(async move {
                    let _ = client.load().await;
                });
            }
            bail!("Laya is not loaded");
        }
        let mut guard = self.helper.lock().await;
        let Some(helper) = guard.as_mut() else {
            self.set(LayaState::Stopped {
                downloaded: Some(true),
            });
            bail!("Laya is not running");
        };
        let request = json!({ "op": "decide", "state": state, "questions": questions });
        match self.call(helper, request, self.config.timeout).await {
            Ok(reply) => {
                self.touch();
                let answers = reply
                    .get("answers")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                let ms = reply.get("ms").and_then(Value::as_u64).unwrap_or(0);
                Ok(LayaReply { answers, ms })
            }
            Err(err) => {
                // A helper that crashed or hung is dropped; the next load starts a fresh one.
                *guard = None;
                self.set(LayaState::Failed {
                    error: format!("{err:#}"),
                });
                Err(err)
            }
        }
    }

    fn touch(&self) {
        *self.last_used.lock().expect("not poisoned") = Instant::now();
    }

    /// Stop the helper and give back its memory. The weights stay on disk.
    pub async fn unload(&self) {
        let had = self.helper.lock().await.take().is_some();
        if had || matches!(self.state(), LayaState::Ready | LayaState::Loading { .. }) {
            self.set(LayaState::Stopped {
                downloaded: Some(true),
            });
        }
    }

    /// Unload after `idle` unused. Runs until the client is dropped.
    pub fn spawn_idle_reaper(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let Some(client) = weak.upgrade() else { return };
                let idle = client.last_used.lock().expect("not poisoned").elapsed();
                if matches!(client.state(), LayaState::Ready) && idle >= client.config.idle {
                    tracing::info!("unloading Laya after {}s idle", idle.as_secs());
                    client.unload().await;
                }
            }
        });
    }

    /// Test hook: unload now if idle for at least `idle`.
    pub async fn unload_if_idle(&self) -> bool {
        let idle = self.last_used.lock().expect("not poisoned").elapsed();
        if matches!(self.state(), LayaState::Ready) && idle >= self.config.idle {
            self.unload().await;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::snapshot;
    use super::*;

    fn answer(choice: &str, p: f64) -> Value {
        json!({ "type": "choice", "choice": choice, "probabilities": { choice: p } })
    }

    fn answers(pairs: &[(&str, &str, f64)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(q, c, p)| (q.to_string(), answer(c, *p)))
            .collect()
    }

    fn decided(pairs: &[(&str, &str, f64)], said: &str) -> Decision {
        decide(
            &answers(pairs),
            &snapshot(),
            &matcher::slots(said),
            said,
            0.75,
            false,
        )
    }

    #[test]
    fn questions_offer_only_what_exists_and_stay_small() {
        let s = snapshot();
        let q = questions(&s, None);
        let worker = q["worker"]["criteria"].as_object().unwrap();
        assert!(
            worker.contains_key("#1") && worker.contains_key("#3") && worker.contains_key("none")
        );
        assert_eq!(
            worker["#1"], "builder done: Retry the price fetcher, change waiting (low)",
            "a worker is described in a few words"
        );
        assert!(q.get("plan_action").is_some() && q.get("project").is_some());
        for (name, question) in q.as_object().unwrap() {
            let options = question["criteria"]
                .as_object()
                .map(|c| c.len())
                .unwrap_or(0);
            assert!(options < 20, "{name} has {options} options");
        }
        // No workers, one project, no plan: those questions are not asked.
        let bare = questions(&Snapshot::default(), None);
        assert!(
            bare.get("worker").is_none()
                && bare.get("project").is_none()
                && bare.get("plan_action").is_none()
        );
        // A pending confirmation asks only that.
        let pending = questions(&s, Some("Merge the builder's change"));
        assert_eq!(
            pending.as_object().unwrap().keys().collect::<Vec<_>>(),
            vec!["yes"]
        );
    }

    #[test]
    fn the_state_carries_what_is_waiting() {
        let state = state("approve it", &snapshot());
        assert_eq!(state["person_said"], "approve it");
        assert_eq!(
            state["waiting_for_person"][0],
            "#1 builder done: Retry the price fetcher, change waiting (low)"
        );
        assert_eq!(state["running"][0], "#2 tester");
    }

    #[test]
    fn nothing_is_done_on_a_guess() {
        let unsure = decided(
            &[("domain", "workers", 0.6)],
            "do the thing with the builder",
        );
        assert_eq!(unsure.action, None);
        assert_eq!(
            unsure.otherwise,
            Outcome::ToHead {
                text: "do the thing with the builder".into()
            }
        );

        // The domain is sure but the follow-up is not: ask, don't guess.
        let half = decided(
            &[("domain", "autonomy", 0.9), ("autonomy", "land_most", 0.5)],
            "loosen it up",
        );
        assert!(matches!(half.otherwise, Outcome::Clarify { .. }));
    }

    #[test]
    fn a_sure_answer_becomes_an_action() {
        let merge = decided(
            &[
                ("domain", "workers", 0.93),
                ("worker_action", "approve_merge", 0.88),
                ("worker", "#1", 0.8),
            ],
            "yeah ship the fetcher fix",
        );
        assert_eq!(
            merge.action,
            Some(VoiceAction::ApproveMerge {
                worker: "w-aaa".into()
            })
        );
        assert_eq!(
            merge.confidence,
            Some(0.88),
            "the weakest link it rested on"
        );

        let level = decided(
            &[("domain", "autonomy", 0.9), ("autonomy", "land_safe", 0.85)],
            "let safe stuff through",
        );
        assert_eq!(
            level.action,
            Some(VoiceAction::SetAutonomy {
                level: Autonomy::LandSafe
            })
        );

        let project = decided(
            &[
                ("domain", "navigate", 0.9),
                ("view", "project", 0.9),
                ("project", "p2", 0.9),
            ],
            "over to the blog",
        );
        assert_eq!(
            project.action,
            Some(VoiceAction::SwitchProject {
                project: "/p/blog".into()
            })
        );
    }

    #[test]
    fn laya_never_picks_a_worker_that_does_not_fit() {
        // Laya says #2, but #2 has no merge waiting; the only candidate is #1.
        let d = decided(
            &[
                ("domain", "workers", 0.9),
                ("worker_action", "approve_merge", 0.9),
                ("worker", "#2", 0.9),
            ],
            "approve that one",
        );
        assert_eq!(
            d.action,
            Some(VoiceAction::ApproveMerge {
                worker: "w-aaa".into()
            })
        );
    }

    #[test]
    fn free_text_comes_from_the_words_not_the_model() {
        let app = decided(
            &[("domain", "computer", 0.9), ("computer_target", "app", 0.9)],
            "could you launch slack",
        );
        assert_eq!(
            app.action,
            Some(VoiceAction::OpenApp {
                name: "Slack".into()
            })
        );
        let missing = decided(
            &[
                ("domain", "computer", 0.9),
                ("computer_target", "website", 0.9),
            ],
            "open that site",
        );
        assert_eq!(
            missing.otherwise,
            Outcome::Clarify {
                question: "Which website?".into()
            }
        );
        let work = decided(&[("domain", "work", 0.9)], "the fetcher keeps timing out");
        assert_eq!(
            work.otherwise,
            Outcome::ToHead {
                text: "the fetcher keeps timing out".into()
            }
        );
    }

    #[test]
    fn a_pending_question_is_read_as_yes_or_no() {
        let s = snapshot();
        let yes: Map<String, Value> =
            [("yes".to_string(), json!({ "type": "noul", "noul": 0.93 }))]
                .into_iter()
                .collect();
        let no: Map<String, Value> = [("yes".to_string(), json!({ "type": "noul", "noul": 0.05 }))]
            .into_iter()
            .collect();
        let unsure: Map<String, Value> =
            [("yes".to_string(), json!({ "type": "noul", "noul": 0.5 }))]
                .into_iter()
                .collect();
        let run = |a: &Map<String, Value>| decide(a, &s, &Slots::default(), "hmm", 0.75, true);
        assert_eq!(run(&yes).action, Some(VoiceAction::Confirm));
        assert_eq!(run(&no).action, Some(VoiceAction::Cancel));
        assert!(matches!(run(&unsure).otherwise, Outcome::Clarify { .. }));
    }

    // ------------------------------------------------------------ the helper

    /// A stand-in for the Node helper with the same protocol, driven by `MODE`.
    fn fake_helper(dir: &std::path::Path, body: &str) -> LayaConfig {
        std::fs::create_dir_all(dir.join("node_modules/@receptron/laya")).unwrap();
        let script = dir.join("laya-server.mjs");
        std::fs::write(&script, body).unwrap();
        let mut config = LayaConfig::new(script);
        config.timeout = Duration::from_secs(3);
        config.load_timeout = Duration::from_secs(5);
        config.idle = Duration::from_millis(0);
        config
    }

    const ANSWERING: &str = r#"
import readline from "node:readline";
const rl = readline.createInterface({ input: process.stdin });
let loaded = false;
const send = (o) => process.stdout.write(JSON.stringify(o) + "\n");
rl.on("line", (line) => {
  const req = JSON.parse(line);
  if (req.op === "status") return send({ id: req.id, ok: true, downloaded: true, loaded });
  if (req.op === "load") {
    send({ event: "progress", file: "laya.onnx.data", received: 5, total: 10 });
    loaded = true;
    return send({ id: req.id, ok: true });
  }
  if (req.op === "decide") {
    if (req.state.person_said === "crash") process.exit(3);
    if (req.state.person_said === "hang") return;
    return send({ id: req.id, ok: true, ms: 42, answers: { domain: { type: "choice", choice: "status", probabilities: { status: 0.97 } } } });
  }
});
"#;

    fn node_available() -> bool {
        std::process::Command::new("node")
            .arg("--version")
            .output()
            .is_ok()
    }

    #[tokio::test]
    async fn the_helper_answers_and_reports_progress() {
        if !node_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let client = LayaClient::new(fake_helper(dir.path(), ANSWERING));
        assert_eq!(client.state(), LayaState::Stopped { downloaded: None });
        assert!(client.probe().await.unwrap());
        assert_eq!(
            client.state(),
            LayaState::Stopped {
                downloaded: Some(true)
            }
        );

        let watch = client.subscribe();
        client.load().await.unwrap();
        assert_eq!(client.state(), LayaState::Ready);
        let _ = watch.has_changed();

        let reply = client
            .decide(&json!({ "person_said": "status" }), &json!({}))
            .await
            .unwrap();
        assert_eq!(reply.ms, 42);
        assert_eq!(
            picked(&reply.answers, "domain"),
            Some(("status".into(), 0.97))
        );
    }

    #[tokio::test]
    async fn a_helper_that_crashes_or_hangs_is_dropped_and_can_be_loaded_again() {
        if !node_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let client = LayaClient::new(fake_helper(dir.path(), ANSWERING));
        client.load().await.unwrap();
        assert!(client
            .decide(&json!({ "person_said": "crash" }), &json!({}))
            .await
            .is_err());
        assert!(matches!(client.state(), LayaState::Failed { .. }));
        // Not ready: decided without Laya, no waiting.
        assert!(client
            .decide(&json!({ "person_said": "status" }), &json!({}))
            .await
            .is_err());

        client.load().await.unwrap();
        let started = Instant::now();
        assert!(client
            .decide(&json!({ "person_said": "hang" }), &json!({}))
            .await
            .is_err());
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the timeout held"
        );
        client.load().await.unwrap();
        assert!(client
            .decide(&json!({ "person_said": "status" }), &json!({}))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn an_idle_helper_is_unloaded() {
        if !node_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let client = LayaClient::new(fake_helper(dir.path(), ANSWERING));
        client.load().await.unwrap();
        assert!(client.unload_if_idle().await);
        assert_eq!(
            client.state(),
            LayaState::Stopped {
                downloaded: Some(true)
            }
        );
    }

    #[test]
    fn a_missing_helper_says_how_to_install_it() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("laya-server.mjs");
        std::fs::write(&script, "").unwrap();
        let client = LayaClient::new(LayaConfig::new(script));
        match client.state() {
            LayaState::NotInstalled { hint } => assert!(hint.contains("npm install"), "{hint}"),
            other => panic!("{other:?}"),
        }
    }
}
