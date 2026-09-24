//! The voice agent: a small Claude that carries out spoken requests of several steps.
//!
//! The phrase matcher knows exact commands; this knows *what was meant*. "Open Notes and
//! jot down milk, eggs and bread, then remind me at six to go shopping" is three tool
//! calls to it, not a phrasing to anticipate.
//!
//! It is deliberately narrow:
//! * **Its tools are the same safe recipes voice already had** — open an app, search,
//!   note, reminder, music, volume, and the harness's own actions — served over a local
//!   MCP server. It has **no** built-in tools: no files, no shell, no web fetching.
//! * **Every tool call goes through [`super::finalize`]**, the same checks as a spoken
//!   command: an action that can't apply says why, and anything that lands, discards or
//!   stops work, or raises autonomy, only *asks* the person — it happens on their yes.
//! * **Coding is not its job.** Requests about the work go to the coding agent's chat
//!   box, for the person to send.
//! * **Web tasks are handed on** to the browser agent ([`VoiceAgent::start_browser`]),
//!   a stronger model whose only tools are the voice browser ([`super::browser`]).
//!
//! One long-lived `claude -p` process (Haiku by default) keeps its context between
//! requests, so a follow-up like "and add bread" makes sense and costs little.

use anyhow::{bail, Context, Result};
use futures::future::BoxFuture;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use serde::Deserialize;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedReceiver;

use super::browser::{BrowserClient, BrowserTools, BROWSE_BRIEF};
use super::everyday::{Control, Player, Site, SystemControl, When};
use super::matcher::{find_app, normalize};
use super::{finalize, spoken_status, Outcome, Pane, Snapshot, StatusTopic, VoiceAction};
use crate::agents::claude::ClaudeSession;
use crate::autonomy::Autonomy;
use crate::event::HarnessEvent;
use crate::mcp::{serve_tools, McpServer};
use crate::roles::{Isolation, Provider, Role};

pub const SERVER_NAME: &str = "voice";
pub const DEFAULT_MODEL: &str = "haiku";

/// What the agent's tools act through. The app implements it for real; the CLI has a dry
/// run that only reports.
pub trait Hands: Send + Sync + 'static {
    /// The harness as it is now.
    fn snapshot(&self) -> BoxFuture<'static, Snapshot>;
    /// Carry out a checked action. With `confirm`, only ask the person — it runs on their
    /// yes. Returns what to tell the agent.
    fn perform(
        &self,
        action: VoiceAction,
        confirm: bool,
        describe: String,
    ) -> BoxFuture<'static, Result<String, String>>;
    /// Put text in the coding agent's chat box, for the person to edit and send.
    fn chat_box(&self, text: String) -> BoxFuture<'static, ()>;
    /// One step is being taken, for the voice bar.
    fn step(&self, line: String);
    /// Hand a task that needs web pages to the browser agent. In the app it runs in the
    /// background and reports when done; returns what to tell the voice agent now.
    fn browse(&self, task: String) -> BoxFuture<'static, Result<String, String>>;
    /// Email addresses for a name, from the Mac's Contacts: (name, address).
    fn lookup_email(
        &self,
        name: String,
    ) -> BoxFuture<'static, Result<Vec<(String, String)>, String>>;
}

pub const BRIEF: &str = "You are the voice assistant built into Harness, a coding app on the person's Mac. The person just spoke to you. Their words come from speech recognition and may contain small errors, so read them generously.

Do what they asked with your tools, one step at a time, in the order they said it. Several requests in one sentence are several tool calls. Fill in sensible details yourself, such as the text of a note or the wording of a reminder, instead of asking.

Requests about code, the project, bugs, features, tests or programming are for the coding agent, not you. For those, call put_in_chat_box with their request, cleaned up but in their words, and do nothing else for that part. Never try to do coding work yourself.

Some actions (merging, discarding or undoing a change, approving or declining a delegation, stopping work, running or dropping a plan, raising autonomy) wait for the person's yes. When a tool says it asked, stop there and tell them it is waiting for their yes.

Opening a website or searching is open_website or search_web. Anything more on the web (reading a page, comparing, finding something on a site, clicking through, filling something in, anything in Gmail or another signed-in site) is for the browser assistant: call browse once with the whole task, every detail they gave, in their words. It works in its own Chrome window in the background and reports back itself, so just say you've started on it.

To write an email, call draft_email with the finished email, written as the person would write it (see what they told you about themselves, if anything). If you only have a name, call find_email_address first; if it finds nothing, leave the address empty. It opens as a draft in Gmail; they send it.

If something cannot be done with your tools, say so plainly. End with one short sentence saying what you did; it is read aloud, so use no markdown and no lists.";

/// The person's own words about themselves and how they write, added to a brief.
pub fn with_about(brief: &str, about: &str) -> String {
    let about = about.trim();
    if about.is_empty() {
        brief.to_string()
    } else {
        format!(
            "{brief}\n\nWhat the person has told you about themselves and how they write:\n{about}"
        )
    }
}

// ---------------------------------------------------------------- tool parameters

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AppParams {
    /// The app's name, e.g. "Notes", "Spotify", "Google Chrome".
    pub name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UrlParams {
    /// A web address, e.g. "github.com" or "https://news.ycombinator.com".
    pub url: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchParams {
    /// What to search for.
    pub query: String,
    /// Where: "web" (default), "youtube", "amazon", "github", "maps", "wikipedia" or "reddit".
    #[serde(default)]
    pub site: Option<String>,
    /// A browser to use, e.g. "Google Chrome" or "Safari". Leave out for the default browser.
    #[serde(default)]
    pub browser: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NoteParams {
    /// The whole note. The first line becomes its title.
    pub text: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReminderParams {
    /// What to be reminded of, e.g. "Go shopping".
    pub text: String,
    /// A clock time in 24-hour "HH:MM", e.g. "18:00". Leave out if none was said.
    #[serde(default)]
    pub at: Option<String>,
    /// Or a delay in minutes from now.
    #[serde(default)]
    pub in_minutes: Option<u32>,
    /// True if the time is tomorrow rather than today.
    #[serde(default)]
    pub tomorrow: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MusicParams {
    /// "play", "pause", "next" or "previous".
    pub control: String,
    /// "spotify" or "music" (Apple Music). Leave out for whichever is playing.
    #[serde(default)]
    pub app: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PlayParams {
    /// A playlist name, song, artist or genre.
    pub what: String,
    /// True when it is one of the person's playlists.
    #[serde(default)]
    pub playlist: Option<bool>,
    /// "spotify" or "music". Leave out for whichever is playing.
    #[serde(default)]
    pub app: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct VolumeParams {
    /// "up", "down", "mute", "unmute" or "set".
    pub change: String,
    /// For "set": 0 to 100.
    #[serde(default)]
    pub percent: Option<u8>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FolderParams {
    /// "downloads", "documents", "desktop", "home", or "project" for the open project.
    pub folder: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TopicParams {
    /// "overview", "workers", "waiting" (what waits for the person), "plan" or "night".
    pub topic: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ViewParams {
    /// "chat", "plan", "night", "preview", "tools" or "settings".
    pub view: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ProjectParams {
    /// The project's name.
    pub name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkerParams {
    /// The worker's number, as in "worker 3".
    pub number: u32,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkerActionParams {
    /// "approve_merge", "reject_merge", "undo_merge", "approve_delegation",
    /// "decline_delegation" or "stop".
    pub action: String,
    /// The worker's number. Leave out when only one worker fits.
    #[serde(default)]
    pub number: Option<u32>,
    /// Why, for rejecting or declining.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PlanParams {
    /// "run", "discard", or "feedback".
    pub action: String,
    /// For "feedback": what to tell the planner.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NightParams {
    /// "stop", "propose" (put its work up for review) or "setup" (open the form).
    pub action: String,
    /// For "setup": what to improve.
    #[serde(default)]
    pub goal: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AutonomyParams {
    /// "ask", "review", "land_safe" or "land_most".
    pub level: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ChatParams {
    /// The request for the coding agent, in the person's words, tidied up.
    pub text: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BrowseParams {
    /// The whole task, with every detail the person gave, in their words.
    pub task: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EmailParams {
    /// Email addresses, comma-separated. Empty if not known.
    #[serde(default)]
    pub to: String,
    pub subject: String,
    /// The finished email, greeting to sign-off, as the person would write it.
    pub body: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NameParams {
    /// A person's name, e.g. "Sam" or "Sam Patel".
    pub name: String,
}

// ---------------------------------------------------------------- the tools

#[derive(Clone)]
pub struct VoiceTools {
    hands: Arc<dyn Hands>,
}

fn player(name: Option<&str>) -> Option<Player> {
    match name.map(|n| n.to_lowercase()) {
        Some(n) if n.contains("spotify") => Some(Player::Spotify),
        Some(n) if n.contains("music") || n.contains("itunes") => Some(Player::Music),
        _ => None,
    }
}

fn parse_clock(at: &str) -> Option<(u8, u8)> {
    let at = at.trim().to_lowercase();
    let (time, pm) = match at.strip_suffix("pm") {
        Some(t) => (t.trim().to_string(), Some(true)),
        None => match at.strip_suffix("am") {
            Some(t) => (t.trim().to_string(), Some(false)),
            None => (at.clone(), None),
        },
    };
    let (h, m) = time.split_once(':').unwrap_or((time.as_str(), "0"));
    let mut hour: u8 = h.trim().parse().ok()?;
    let minute: u8 = m.trim().parse().ok()?;
    match pm {
        Some(true) if hour < 12 => hour += 12,
        Some(false) if hour == 12 => hour = 0,
        _ => {}
    }
    (hour < 24 && minute < 60).then_some((hour, minute))
}

impl VoiceTools {
    pub fn new(hands: Arc<dyn Hands>) -> Self {
        Self { hands }
    }

    /// Check an action against the harness, then do it or ask for the person's yes.
    async fn run(&self, action: VoiceAction) -> String {
        let snapshot = self.hands.snapshot().await;
        match finalize(action, &snapshot) {
            Outcome::Reply { text } => text,
            Outcome::Act {
                action,
                confirm,
                describe,
            } => {
                if !confirm {
                    self.hands.step(describe.clone());
                }
                match self.hands.perform(action, confirm, describe.clone()).await {
                    Ok(_) if confirm => format!(
                        "Asked the person to confirm: {describe}. It happens only if they say yes. \
                         Do not ask again or wait for it."
                    ),
                    Ok(message) if message.is_empty() => format!("Done: {describe}."),
                    Ok(message) => format!("Done: {message}"),
                    Err(error) => format!("Could not do it: {error}"),
                }
            }
            _ => "Nothing to do.".into(),
        }
    }

    async fn worker_id(
        &self,
        number: Option<u32>,
        fits: impl Fn(&super::WorkerRef) -> bool,
    ) -> Result<String, String> {
        let snapshot = self.hands.snapshot().await;
        let candidates: Vec<&super::WorkerRef> =
            snapshot.workers.iter().filter(|w| fits(w)).collect();
        match number {
            Some(n) => snapshot
                .workers
                .iter()
                .find(|w| w.number == n)
                .map(|w| w.id.clone())
                .ok_or_else(|| format!("There is no worker {n}.")),
            None if candidates.len() == 1 => Ok(candidates[0].id.clone()),
            None if candidates.is_empty() => Err("No worker fits that.".into()),
            None => Err(format!(
                "Which worker? {}",
                candidates
                    .iter()
                    .map(|w| format!("#{} the {}", w.number, w.role))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }
}

#[tool_router(server_handler)]
impl VoiceTools {
    #[tool(
        name = "open_app",
        description = "Open an application on the Mac by name."
    )]
    async fn open_app(&self, Parameters(p): Parameters<AppParams>) -> String {
        let snapshot = self.hands.snapshot().await;
        let name = if snapshot.apps.is_empty() {
            p.name.clone()
        } else {
            match find_app(&normalize(&p.name), &snapshot.apps) {
                Some(app) => app,
                None => return format!("No installed app matches \"{}\".", p.name),
            }
        };
        self.run(VoiceAction::OpenApp { name }).await
    }

    #[tool(
        name = "open_website",
        description = "Open a web page in the default browser."
    )]
    async fn open_website(&self, Parameters(p): Parameters<UrlParams>) -> String {
        let url = if p.url.starts_with("http://") || p.url.starts_with("https://") {
            p.url
        } else {
            format!("https://{}", p.url.trim())
        };
        self.run(VoiceAction::OpenUrl { url }).await
    }

    #[tool(
        name = "search_web",
        description = "Search the web or a site (YouTube, Amazon, GitHub, Maps, Wikipedia, Reddit) and open the results in a browser."
    )]
    async fn search_web(&self, Parameters(p): Parameters<SearchParams>) -> String {
        let site = p.site.as_deref().map(|s| s.to_lowercase()).and_then(|s| {
            Some(match s.as_str() {
                "youtube" => Site::Youtube,
                "amazon" => Site::Amazon,
                "github" => Site::Github,
                "maps" | "google maps" => Site::Maps,
                "wikipedia" => Site::Wikipedia,
                "reddit" => Site::Reddit,
                _ => return None,
            })
        });
        let snapshot = self.hands.snapshot().await;
        let browser = p.browser.map(|b| {
            if snapshot.apps.is_empty() {
                b
            } else {
                find_app(&normalize(&b), &snapshot.apps).unwrap_or(b)
            }
        });
        self.run(VoiceAction::Search {
            query: p.query,
            site: site.unwrap_or(Site::Web),
            browser,
        })
        .await
    }

    #[tool(
        name = "new_note",
        description = "Create a new note in Apple Notes with the given text."
    )]
    async fn new_note(&self, Parameters(p): Parameters<NoteParams>) -> String {
        self.run(VoiceAction::NewNote { text: p.text }).await
    }

    #[tool(
        name = "add_reminder",
        description = "Add a reminder in Apple Reminders, optionally at a time."
    )]
    async fn add_reminder(&self, Parameters(p): Parameters<ReminderParams>) -> String {
        let days_ahead = u8::from(p.tomorrow.unwrap_or(false));
        let when = match (p.in_minutes, p.at.as_deref().and_then(parse_clock)) {
            (Some(minutes), _) => Some(When::In {
                seconds: minutes.saturating_mul(60),
            }),
            (None, Some((hour, minute))) => Some(When::At {
                hour,
                minute,
                days_ahead,
            }),
            (None, None) if days_ahead == 1 => Some(When::At {
                hour: 9,
                minute: 0,
                days_ahead,
            }),
            _ => None,
        };
        self.run(VoiceAction::Remind { text: p.text, when }).await
    }

    #[tool(
        name = "music",
        description = "Play, pause, skip to the next song or go back a song in Spotify or Apple Music."
    )]
    async fn music(&self, Parameters(p): Parameters<MusicParams>) -> String {
        let control = match p.control.to_lowercase().as_str() {
            "play" | "resume" => Control::Play,
            "pause" | "stop" => Control::Pause,
            "next" | "skip" => Control::Next,
            "previous" | "back" => Control::Previous,
            other => return format!("Unknown music control \"{other}\"."),
        };
        self.run(VoiceAction::Media {
            app: player(p.app.as_deref()),
            control,
        })
        .await
    }

    #[tool(
        name = "play_music",
        description = "Play a playlist, song, artist or genre. Apple Music plays it directly; Spotify opens its search for it."
    )]
    async fn play_music(&self, Parameters(p): Parameters<PlayParams>) -> String {
        let app = player(p.app.as_deref());
        if p.playlist.unwrap_or(false) {
            self.run(VoiceAction::PlayPlaylist { app, name: p.what })
                .await
        } else {
            self.run(VoiceAction::PlayQuery { app, query: p.what })
                .await
        }
    }

    #[tool(
        name = "mac_volume",
        description = "Change the Mac's volume: up, down, mute, unmute, or set to a percentage."
    )]
    async fn mac_volume(&self, Parameters(p): Parameters<VolumeParams>) -> String {
        let control = match p.change.to_lowercase().as_str() {
            "up" => SystemControl::VolumeUp,
            "down" => SystemControl::VolumeDown,
            "mute" => SystemControl::Mute,
            "unmute" => SystemControl::Unmute,
            "set" => SystemControl::SetVolume {
                percent: p.percent.unwrap_or(50).min(100),
            },
            other => return format!("Unknown volume change \"{other}\"."),
        };
        self.run(VoiceAction::System { control }).await
    }

    #[tool(
        name = "lock_screen",
        description = "Lock the screen (sleeps the display)."
    )]
    async fn lock_screen(&self) -> String {
        self.run(VoiceAction::System {
            control: SystemControl::SleepDisplay,
        })
        .await
    }

    #[tool(
        name = "open_folder",
        description = "Open a folder in Finder: downloads, documents, desktop, home, or the project."
    )]
    async fn open_folder(&self, Parameters(p): Parameters<FolderParams>) -> String {
        let action = match p.folder.to_lowercase().as_str() {
            "project" => VoiceAction::RevealProject,
            "downloads" => VoiceAction::OpenFolder { path: "~/Downloads".into() },
            "documents" => VoiceAction::OpenFolder { path: "~/Documents".into() },
            "desktop" => VoiceAction::OpenFolder { path: "~/Desktop".into() },
            "home" => VoiceAction::OpenFolder { path: "~".into() },
            other => return format!("I can only open downloads, documents, desktop, home or the project, not \"{other}\"."),
        };
        self.run(action).await
    }

    #[tool(
        name = "open_project_in_editor",
        description = "Open the current project in the person's code editor."
    )]
    async fn open_project_in_editor(&self) -> String {
        self.run(VoiceAction::OpenProjectInEditor).await
    }

    #[tool(
        name = "harness_status",
        description = "What the coding app is doing: workers running, changes waiting for the person, the plan, the night shift."
    )]
    async fn harness_status(&self, Parameters(p): Parameters<TopicParams>) -> String {
        let topic = StatusTopic::parse(&p.topic.to_lowercase()).unwrap_or(StatusTopic::Overview);
        let snapshot = self.hands.snapshot().await;
        spoken_status(topic, &snapshot)
            .unwrap_or_else(|| "Usage limits are in the app's limits panel.".into())
    }

    #[tool(
        name = "show_in_app",
        description = "Show a part of the coding app: chat, plan, night, preview, tools or settings."
    )]
    async fn show_in_app(&self, Parameters(p): Parameters<ViewParams>) -> String {
        match Pane::parse(&p.view.to_lowercase()) {
            Some(pane) => self.run(VoiceAction::Navigate { pane }).await,
            None => format!("There is no \"{}\" view.", p.view),
        }
    }

    #[tool(
        name = "switch_project",
        description = "Switch the coding app to another open project."
    )]
    async fn switch_project(&self, Parameters(p): Parameters<ProjectParams>) -> String {
        let snapshot = self.hands.snapshot().await;
        let wanted = normalize(&p.name);
        let found = snapshot
            .projects
            .iter()
            .find(|pr| normalize(&pr.name) == wanted)
            .or_else(|| {
                let partial: Vec<_> = snapshot
                    .projects
                    .iter()
                    .filter(|pr| normalize(&pr.name).contains(&wanted))
                    .collect();
                (partial.len() == 1).then(|| partial[0])
            });
        match found {
            Some(project) => {
                self.run(VoiceAction::SwitchProject {
                    project: project.root.clone(),
                })
                .await
            }
            None => format!(
                "No open project is called \"{}\". Open projects: {}.",
                p.name,
                snapshot
                    .projects
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    #[tool(
        name = "open_worker",
        description = "Show one worker's details and changes, by its number."
    )]
    async fn open_worker(&self, Parameters(p): Parameters<WorkerParams>) -> String {
        match self.worker_id(Some(p.number), |_| true).await {
            Ok(worker) => self.run(VoiceAction::OpenWorker { worker }).await,
            Err(error) => error,
        }
    }

    #[tool(
        name = "worker_action",
        description = "Approve, reject or undo a worker's change; approve or decline a waiting delegation; or stop a running worker. Anything that changes code or stops work asks the person first."
    )]
    async fn worker_action(&self, Parameters(p): Parameters<WorkerActionParams>) -> String {
        let action = p.action.to_lowercase();
        let snapshot = self.hands.snapshot().await;
        let landed = snapshot.landed.clone();
        let fits = |w: &super::WorkerRef| match action.as_str() {
            "approve_merge" | "reject_merge" => w.merge_pending,
            "undo_merge" => landed.contains(&w.id),
            "approve_delegation" | "decline_delegation" => w.awaiting_approval(),
            "stop" => w.is_running(),
            _ => false,
        };
        let worker = match (action.as_str(), p.number) {
            ("undo_merge", None) => match snapshot.landed.last() {
                Some(id) => id.clone(),
                None => return "Nothing has landed that can be undone.".into(),
            },
            _ => match self.worker_id(p.number, fits).await {
                Ok(id) => id,
                Err(error) => return error,
            },
        };
        let voice_action = match action.as_str() {
            "approve_merge" => VoiceAction::ApproveMerge { worker },
            "reject_merge" => VoiceAction::RejectMerge {
                worker,
                reason: p.reason,
            },
            "undo_merge" => VoiceAction::UndoMerge { worker },
            "approve_delegation" => VoiceAction::ApproveDelegation { worker },
            "decline_delegation" => VoiceAction::DeclineDelegation {
                worker,
                reason: p.reason,
            },
            "stop" => VoiceAction::StopWorker { worker },
            other => return format!("Unknown worker action \"{other}\"."),
        };
        self.run(voice_action).await
    }

    #[tool(
        name = "plan",
        description = "Run or drop the proposed plan, or send the planner feedback on its draft."
    )]
    async fn plan(&self, Parameters(p): Parameters<PlanParams>) -> String {
        let action = match p.action.to_lowercase().as_str() {
            "run" => VoiceAction::RunPlan,
            "discard" | "drop" | "stop" => VoiceAction::DiscardPlan,
            "feedback" => VoiceAction::PlanFeedback {
                note: p.note.unwrap_or_default(),
            },
            other => return format!("Unknown plan action \"{other}\"."),
        };
        self.run(action).await
    }

    #[tool(
        name = "night_shift",
        description = "Stop the night shift, put its kept work up for review, or open the form to set one up."
    )]
    async fn night_shift(&self, Parameters(p): Parameters<NightParams>) -> String {
        let action = match p.action.to_lowercase().as_str() {
            "stop" => VoiceAction::StopNight,
            "propose" => VoiceAction::ProposeNight,
            "setup" | "start" => VoiceAction::NightSetup { goal: p.goal },
            other => return format!("Unknown night shift action \"{other}\"."),
        };
        self.run(action).await
    }

    #[tool(
        name = "set_autonomy",
        description = "Set how much runs without asking: ask, review, land_safe or land_most. Raising it asks the person first."
    )]
    async fn set_autonomy(&self, Parameters(p): Parameters<AutonomyParams>) -> String {
        match Autonomy::parse(&p.level) {
            Some(level) => self.run(VoiceAction::SetAutonomy { level }).await,
            None => format!("Unknown autonomy level \"{}\".", p.level),
        }
    }

    #[tool(
        name = "stop_coding_agent",
        description = "Interrupt the coding agent's current reply."
    )]
    async fn stop_coding_agent(&self) -> String {
        self.run(VoiceAction::StopTurn).await
    }

    #[tool(
        name = "browse",
        description = "Hand a task that needs web pages to the browser assistant, which works in its own Chrome window in the background: reading or comparing pages, finding something on a site, clicking through, filling in forms, anything in Gmail or another signed-in site. Give the whole task in the person's words."
    )]
    async fn browse(&self, Parameters(p): Parameters<BrowseParams>) -> String {
        self.hands.step(format!("Browsing: {}", p.task));
        match self.hands.browse(p.task).await {
            Ok(message) => message,
            Err(error) => format!("Could not start browsing: {error}"),
        }
    }

    #[tool(
        name = "stop_browsing",
        description = "Stop the browser assistant's current task."
    )]
    async fn stop_browsing(&self) -> String {
        self.run(VoiceAction::StopBrowsing).await
    }

    #[tool(
        name = "draft_email",
        description = "Open a finished email as a draft in Gmail, for the person to check and send. Never sends."
    )]
    async fn draft_email(&self, Parameters(p): Parameters<EmailParams>) -> String {
        self.run(VoiceAction::DraftEmail {
            to: p.to,
            subject: p.subject,
            body: p.body,
        })
        .await
    }

    #[tool(
        name = "find_email_address",
        description = "Look up a person's email address in the Mac's Contacts by name."
    )]
    async fn find_email_address(&self, Parameters(p): Parameters<NameParams>) -> String {
        match self.hands.lookup_email(p.name.clone()).await {
            Ok(found) if found.is_empty() => format!("No contact matches “{}”.", p.name),
            Ok(found) => found
                .iter()
                .map(|(name, address)| format!("{name}: {address}"))
                .collect::<Vec<_>>()
                .join("\n"),
            Err(error) => format!("Could not look it up: {error}"),
        }
    }

    #[tool(
        name = "put_in_chat_box",
        description = "For anything about code, the project or programming: type the request into the coding agent's chat box for the person to review and send. Never sent automatically."
    )]
    async fn put_in_chat_box(&self, Parameters(p): Parameters<ChatParams>) -> String {
        self.hands.step("Put in the chat box".into());
        self.hands.chat_box(p.text).await;
        "In the chat box, waiting for the person to send it.".into()
    }
}

/// Tool names for `--allowedTools`.
pub fn tool_names() -> Vec<String> {
    [
        "open_app",
        "open_website",
        "search_web",
        "new_note",
        "add_reminder",
        "music",
        "play_music",
        "mac_volume",
        "lock_screen",
        "open_folder",
        "open_project_in_editor",
        "harness_status",
        "show_in_app",
        "switch_project",
        "open_worker",
        "worker_action",
        "plan",
        "night_shift",
        "set_autonomy",
        "stop_coding_agent",
        "browse",
        "stop_browsing",
        "draft_email",
        "find_email_address",
        "put_in_chat_box",
    ]
    .iter()
    .map(|t| format!("mcp__{SERVER_NAME}__{t}"))
    .collect()
}

/// A short picture of the harness to send with each request, so "approve it" and "worker
/// 2" mean something without a tool call.
pub fn context_line(snapshot: &Snapshot) -> String {
    let mut parts = Vec::new();
    if let Some(active) = &snapshot.active_project {
        let name = snapshot
            .projects
            .iter()
            .find(|p| &p.root == active)
            .map(|p| p.name.as_str())
            .unwrap_or(active);
        let others: Vec<&str> = snapshot
            .projects
            .iter()
            .filter(|p| &p.root != active)
            .map(|p| p.name.as_str())
            .collect();
        if others.is_empty() {
            parts.push(format!("project {name}"));
        } else {
            parts.push(format!("project {name} (also open: {})", others.join(", ")));
        }
    }
    let workers: Vec<String> = snapshot
        .workers
        .iter()
        .filter(|w| w.is_running() || w.merge_pending || w.awaiting_approval())
        .take(8)
        .map(|w| {
            let state = if w.merge_pending {
                "change waiting"
            } else if w.awaiting_approval() {
                "waiting to start"
            } else {
                "running"
            };
            format!("#{} {} ({state})", w.number, w.role)
        })
        .collect();
    if !workers.is_empty() {
        parts.push(format!("workers: {}", workers.join(", ")));
    }
    if let Some(plan) = &snapshot.plan {
        parts.push(format!("plan \"{}\" is {}", plan.title, plan.status));
    }
    if let Some(night) = &snapshot.night {
        parts.push(format!("night shift {}", night.status));
    }
    parts.join("; ")
}

pub struct AgentReply {
    /// What it said at the end, to show and read aloud.
    pub text: String,
    /// How many tools it called.
    pub steps: usize,
    pub ms: u64,
    /// Tokens for the whole turn (every model call in it).
    pub usage: crate::event::Usage,
    /// The CLI's estimate at API prices. On a subscription nothing is billed; it is a
    /// measure of how much of the plan's allowance the turn used.
    pub cost_usd: Option<f64>,
}

/// The running voice agent: its tool server and its Claude session.
pub struct VoiceAgent {
    session: ClaudeSession,
    events: UnboundedReceiver<HarnessEvent>,
    mcp: McpServer,
}

impl VoiceAgent {
    /// Start the voice agent: the safe list as its tools. `cwd` should be an empty folder
    /// of its own, so no project's instructions are read into it. `about` is what the
    /// person wrote about themselves in Settings.
    pub async fn start(
        hands: Arc<dyn Hands>,
        model: &str,
        cwd: &Path,
        about: &str,
    ) -> Result<Self> {
        Self::launch(
            move || VoiceTools::new(Arc::clone(&hands)),
            tool_names(),
            &with_about(BRIEF, about),
            model,
            20,
            cwd,
        )
        .await
    }

    /// Start the browser agent: the voice browser as its only tools.
    pub async fn start_browser(
        hands: Arc<dyn Hands>,
        browser: Arc<BrowserClient>,
        model: &str,
        cwd: &Path,
        about: &str,
    ) -> Result<Self> {
        Self::launch(
            move || BrowserTools::new(Arc::clone(&browser), Arc::clone(&hands)),
            super::browser::tool_names(SERVER_NAME),
            &with_about(BROWSE_BRIEF, about),
            model,
            80,
            cwd,
        )
        .await
    }

    async fn launch<S, F>(
        make: F,
        tools: Vec<String>,
        brief: &str,
        model: &str,
        max_turns: u32,
        cwd: &Path,
    ) -> Result<Self>
    where
        S: rmcp::ServerHandler + Send + 'static,
        F: Fn() -> S + Send + Sync + 'static,
    {
        let mcp = serve_tools(SERVER_NAME, make, "127.0.0.1:0".parse()?).await?;
        let role = Role {
            provider: Provider::Claude,
            model: Some(model.to_string()),
            isolation: Isolation::None,
            tools,
            brief: None,
            permission_mode: None,
            base_url: None,
            provider_opts: Default::default(),
            fallback_role: None,
            max_turns: Some(max_turns),
        };
        std::fs::create_dir_all(cwd).with_context(|| format!("creating {}", cwd.display()))?;
        let extra = [
            // No built-in tools at all — no files, no shell — only its own.
            "--tools".to_string(),
            String::new(),
            "--strict-mcp-config".into(),
            "--system-prompt".into(),
            brief.into(),
            "--no-session-persistence".into(),
        ];
        let (session, events) = ClaudeSession::start(
            "voice-agent",
            cwd,
            &role,
            Some(&mcp.claude_mcp_config()),
            None,
            None,
            &extra,
        )
        .await?;
        Ok(Self {
            session,
            events,
            mcp,
        })
    }

    /// Hand the browser agent a task, and wait for it to finish.
    pub async fn run_task(&mut self, task: &str, timeout: Duration) -> Result<AgentReply> {
        self.exchange(&format!("The person asked: \"{task}\""), timeout)
            .await
    }

    /// Hand it what was said, and wait for it to finish.
    pub async fn ask(
        &mut self,
        said: &str,
        snapshot: &Snapshot,
        timeout: Duration,
    ) -> Result<AgentReply> {
        let context = context_line(snapshot);
        let message = if context.is_empty() {
            format!("The person said: \"{said}\"")
        } else {
            format!("The person said: \"{said}\"\n\n(Right now: {context}.)")
        };
        self.exchange(&message, timeout).await
    }

    async fn exchange(&mut self, message: &str, timeout: Duration) -> Result<AgentReply> {
        let started = Instant::now();
        // Anything left over from an earlier turn is not this answer.
        while self.events.try_recv().is_ok() {}
        self.session.send(message).await?;
        let mut steps = 0;
        let mut last_text = String::new();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let event = tokio::time::timeout_at(deadline, self.events.recv())
                .await
                .map_err(|_| {
                    anyhow::anyhow!("the voice agent took longer than {}s", timeout.as_secs())
                })?
                .context("the voice agent stopped")?;
            match event {
                HarnessEvent::ToolCall { .. } => steps += 1,
                HarnessEvent::AssistantText {
                    text,
                    partial: false,
                    ..
                } if !text.trim().is_empty() => {
                    last_text = text;
                }
                HarnessEvent::RunFinished {
                    text,
                    is_error,
                    usage,
                    cost_usd,
                    ..
                } => {
                    if is_error && text.trim().is_empty() {
                        bail!("the voice agent failed");
                    }
                    let text = if text.trim().is_empty() {
                        last_text
                    } else {
                        text
                    };
                    return Ok(AgentReply {
                        text: text.trim().to_string(),
                        steps,
                        ms: started.elapsed().as_millis() as u64,
                        usage,
                        cost_usd,
                    });
                }
                HarnessEvent::Error { message, .. } => bail!("the voice agent: {message}"),
                _ => {}
            }
        }
    }

    pub async fn shutdown(self) {
        let _ = self.session.shutdown().await;
        self.mcp.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use super::super::eval::fixture;
    use super::*;
    use std::sync::Mutex;

    /// Records what the tools asked for, and does nothing.
    #[derive(Default)]
    struct Recorder {
        done: Mutex<Vec<(VoiceAction, bool)>>,
        chat: Mutex<Vec<String>>,
    }

    impl Hands for Recorder {
        fn snapshot(&self) -> BoxFuture<'static, Snapshot> {
            Box::pin(async { fixture() })
        }
        fn perform(
            &self,
            action: VoiceAction,
            confirm: bool,
            _: String,
        ) -> BoxFuture<'static, Result<String, String>> {
            self.done.lock().unwrap().push((action, confirm));
            Box::pin(async { Ok(String::new()) })
        }
        fn chat_box(&self, text: String) -> BoxFuture<'static, ()> {
            self.chat.lock().unwrap().push(text);
            Box::pin(async {})
        }
        fn step(&self, _: String) {}
        fn browse(&self, task: String) -> BoxFuture<'static, Result<String, String>> {
            self.chat.lock().unwrap().push(format!("browse: {task}"));
            Box::pin(async { Ok("Started.".into()) })
        }
        fn lookup_email(
            &self,
            name: String,
        ) -> BoxFuture<'static, Result<Vec<(String, String)>, String>> {
            Box::pin(async move {
                Ok(if name.to_lowercase().contains("sam") {
                    vec![("Sam Patel".into(), "sam@example.com".into())]
                } else {
                    vec![]
                })
            })
        }
    }

    fn tools() -> (Arc<Recorder>, VoiceTools) {
        let recorder = Arc::new(Recorder::default());
        (recorder.clone(), VoiceTools::new(recorder))
    }

    #[tokio::test]
    async fn tools_become_the_same_checked_actions() {
        let (rec, tools) = tools();
        let reply = tools
            .new_note(Parameters(NoteParams {
                text: "Groceries\nmilk, eggs".into(),
            }))
            .await;
        assert!(reply.starts_with("Done"), "{reply}");
        tools
            .add_reminder(Parameters(ReminderParams {
                text: "Go shopping".into(),
                at: Some("18:00".into()),
                in_minutes: None,
                tomorrow: None,
            }))
            .await;
        tools
            .open_app(Parameters(AppParams {
                name: "notes".into(),
            }))
            .await;
        let done = rec.done.lock().unwrap().clone();
        assert_eq!(
            done[0],
            (
                VoiceAction::NewNote {
                    text: "Groceries\nmilk, eggs".into()
                },
                false
            )
        );
        assert_eq!(
            done[1],
            (
                VoiceAction::Remind {
                    text: "Go shopping".into(),
                    when: Some(When::At {
                        hour: 18,
                        minute: 0,
                        days_ahead: 0
                    })
                },
                false
            )
        );
        assert_eq!(
            done[2],
            (
                VoiceAction::OpenApp {
                    name: "Notes".into()
                },
                false
            ),
            "matched to the installed app"
        );
    }

    #[tokio::test]
    async fn anything_risky_only_asks() {
        let (rec, tools) = tools();
        let reply = tools
            .worker_action(Parameters(WorkerActionParams {
                action: "approve_merge".into(),
                number: None,
                reason: None,
            }))
            .await;
        assert!(reply.contains("Asked the person to confirm"), "{reply}");
        assert_eq!(
            rec.done.lock().unwrap()[0],
            (
                VoiceAction::ApproveMerge {
                    worker: "w-aaa".into()
                },
                true
            )
        );
        // And what can't apply says why, without reaching the hands.
        let reply = tools
            .worker_action(Parameters(WorkerActionParams {
                action: "stop".into(),
                number: Some(1),
                reason: None,
            }))
            .await;
        assert_eq!(reply, "That worker is not running.");
        assert_eq!(rec.done.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn code_goes_to_the_chat_box_and_apps_must_exist() {
        let (rec, tools) = tools();
        tools
            .put_in_chat_box(Parameters(ChatParams {
                text: "Fix the fetcher timeout".into(),
            }))
            .await;
        assert_eq!(
            rec.chat.lock().unwrap().as_slice(),
            ["Fix the fetcher timeout"]
        );
        let reply = tools
            .open_app(Parameters(AppParams {
                name: "Photoshop".into(),
            }))
            .await;
        assert!(reply.contains("No installed app"), "{reply}");
    }

    #[tokio::test]
    async fn emails_are_drafts_and_the_web_is_handed_on() {
        let (rec, tools) = tools();
        let found = tools
            .find_email_address(Parameters(NameParams { name: "Sam".into() }))
            .await;
        assert_eq!(found, "Sam Patel: sam@example.com");
        let reply = tools
            .draft_email(Parameters(EmailParams {
                to: "sam@example.com".into(),
                subject: "Running late".into(),
                body: "Hi Sam,\n\nTen minutes late.\n\nZ".into(),
            }))
            .await;
        assert!(reply.starts_with("Done"), "{reply}");
        assert!(matches!(
            &rec.done.lock().unwrap()[0],
            (VoiceAction::DraftEmail { to, .. }, false) if to == "sam@example.com"
        ));
        let reply = tools
            .browse(Parameters(BrowseParams {
                task: "find the cheapest trail runners on amazon.de".into(),
            }))
            .await;
        assert_eq!(reply, "Started.");
        assert_eq!(
            rec.chat.lock().unwrap().as_slice(),
            ["browse: find the cheapest trail runners on amazon.de"]
        );
        assert!(with_about(BRIEF, "I write short, casual emails.")
            .ends_with("I write short, casual emails."));
        assert_eq!(with_about(BRIEF, "  "), BRIEF);
    }

    #[test]
    fn clock_times_are_read_either_way() {
        assert_eq!(parse_clock("18:00"), Some((18, 0)));
        assert_eq!(parse_clock("6pm"), Some((18, 0)));
        assert_eq!(parse_clock("9:30 am"), Some((9, 30)));
        assert_eq!(parse_clock("12am"), Some((0, 0)));
        assert_eq!(parse_clock("25:00"), None);
    }

    #[test]
    fn the_context_names_what_can_be_referred_to() {
        let line = context_line(&fixture());
        assert!(line.contains("project shop (also open: blog)"), "{line}");
        assert!(line.contains("#1 builder (change waiting)"), "{line}");
        assert!(line.contains("#3 reviewer (waiting to start)"), "{line}");
        assert_eq!(tool_names().len(), 25);
    }
}
