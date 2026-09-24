//! Exact commands, recognized without a model.
//!
//! Word-level matching on a normalized transcript: lowercase, punctuation gone, number
//! words turned to digits, filler trimmed from the ends. The order of the checks matters —
//! "stop the night shift" must be read before "stop <worker>", and "open the plan" before
//! "open <app>" — and is the order of [`match_command`].
//!
//! Free text an action needs (a message for the head agent, a reason, an app name) is cut
//! from the *original* transcript, so its casing survives.

use super::{Pane, Snapshot, StatusTopic, VoiceAction, WorkerRef};
use crate::autonomy::Autonomy;

const NUMBERS: [(&str, &str); 30] = [
    ("zero", "0"),
    ("one", "1"),
    ("two", "2"),
    ("three", "3"),
    ("four", "4"),
    ("five", "5"),
    ("six", "6"),
    ("seven", "7"),
    ("eight", "8"),
    ("nine", "9"),
    ("ten", "10"),
    ("eleven", "11"),
    ("twelve", "12"),
    ("thirteen", "13"),
    ("fourteen", "14"),
    ("fifteen", "15"),
    ("sixteen", "16"),
    ("seventeen", "17"),
    ("eighteen", "18"),
    ("nineteen", "19"),
    ("twenty", "20"),
    ("first", "1"),
    ("second", "2"),
    ("third", "3"),
    ("fourth", "4"),
    ("fifth", "5"),
    ("sixth", "6"),
    ("seventh", "7"),
    ("eighth", "8"),
    ("ninth", "9"),
];

const LEADING_FILLER: [&str; 22] = [
    "um",
    "uh",
    "er",
    "erm",
    "hmm",
    "hey",
    "hi",
    "so",
    "and",
    "please",
    "harness",
    "ok so",
    "okay so",
    "can you",
    "could you",
    "would you",
    "will you",
    "i want to",
    "i want you to",
    "id like to",
    "lets",
    "go ahead and",
];
const TRAILING_FILLER: [&str; 8] = [
    "please",
    "thanks",
    "thank you",
    "right now",
    "at the moment",
    "now",
    "um",
    "uh",
];

/// Lowercase words, no punctuation, number words as digits, filler trimmed from the ends.
pub fn normalize(text: &str) -> String {
    let lowered = text.to_lowercase().replace(" dot ", ".");
    let cleaned: String = lowered
        .chars()
        .filter(|c| *c != '\'' && *c != '’')
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    let mut words: Vec<String> = cleaned
        .split_whitespace()
        .map(|word| {
            NUMBERS
                .iter()
                .find(|(spoken, _)| *spoken == word)
                .map(|(_, digit)| digit.to_string())
                .unwrap_or_else(|| word.to_string())
        })
        .collect();
    // "w3" and "number3" are how Whisper sometimes writes "worker three".
    words = words
        .into_iter()
        .flat_map(|word| match word.strip_prefix('w') {
            Some(rest) if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) => {
                vec!["worker".to_string(), rest.to_string()]
            }
            _ => vec![word],
        })
        .collect();
    let mut text = words.join(" ");
    loop {
        let before = text.clone();
        for filler in LEADING_FILLER {
            if let Some(rest) = strip_words(&text, filler) {
                text = rest.to_string();
            }
        }
        for filler in TRAILING_FILLER {
            if let Some(rest) = text.strip_suffix(filler) {
                if rest.is_empty() || rest.ends_with(' ') {
                    text = rest.trim_end().to_string();
                }
            }
        }
        if text == before {
            return text;
        }
    }
}

/// `text` without the leading words `prefix`, if it starts with them as whole words.
fn strip_words<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let rest = text.strip_prefix(prefix)?;
    if rest.is_empty() {
        Some(rest)
    } else {
        rest.strip_prefix(' ')
    }
}

fn has_word(text: &str, word: &str) -> bool {
    text.split(' ').any(|w| w == word)
}

fn has_phrase(text: &str, phrase: &str) -> bool {
    format!(" {text} ").contains(&format!(" {phrase} "))
}

fn is_any(text: &str, phrases: &[&str]) -> bool {
    phrases.contains(&text)
}

fn starts_any<'a>(text: &'a str, prefixes: &[&str]) -> Option<&'a str> {
    prefixes.iter().find_map(|p| strip_words(text, p))
}

/// The original words after the first whole-word, case-insensitive occurrence of
/// `marker`, with leading punctuation and "to"/"that" trimmed.
fn original_after(original: &str, markers: &[&str]) -> Option<String> {
    let lower = original.to_lowercase();
    let mut best: Option<usize> = None;
    for marker in markers {
        let mut from = 0;
        while let Some(found) = lower[from..].find(marker) {
            let start = from + found;
            let end = start + marker.len();
            let before_ok = start == 0 || !lower[..start].chars().last().unwrap().is_alphanumeric();
            let after_ok =
                end == lower.len() || !lower[end..].chars().next().unwrap().is_alphanumeric();
            if before_ok && after_ok {
                best = Some(best.map_or(end, |b: usize| b.min(end)));
                break;
            }
            from = end;
        }
    }
    let best = best?;
    if !original.is_char_boundary(best) {
        return None;
    }
    let rest =
        original[best..].trim_start_matches(|c: char| c.is_whitespace() || ",:;-—".contains(c));
    let mut rest = rest.to_string();
    for lead in ["to ", "that ", "To ", "That "] {
        if let Some(stripped) = rest.strip_prefix(lead) {
            rest = stripped.to_string();
        }
    }
    let rest = rest.trim().trim_end_matches(['.', '!']).trim().to_string();
    (!rest.is_empty()).then_some(rest)
}

/// Free text Laya cannot supply, cut from what was said.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Slots {
    pub app: Option<String>,
    pub url: Option<String>,
    pub message: Option<String>,
    pub reason: Option<String>,
    pub goal: Option<String>,
}

pub fn slots(original: &str) -> Slots {
    Slots {
        app: app_name(original),
        url: url_in(original),
        message: original_after(
            original,
            &[
                "tell claude",
                "ask claude",
                "tell the head agent",
                "ask the head agent",
                "tell the lead",
                "claude",
            ],
        ),
        reason: original_after(original, &["because", "reason being", "since"]),
        goal: original_after(
            original,
            &[
                "night shift to",
                "overnight to",
                "night shift that",
                "night shift:",
            ],
        ),
    }
}

const OPEN_VERBS: [&str; 5] = ["open", "launch", "start", "bring up", "fire up"];
/// Words that make "open …" about the code, not an app.
const WORK_NOUNS: [&str; 16] = [
    "file", "files", "function", "class", "test", "tests", "module", "issue", "branch", "diff",
    "readme", "pr", "pull", "worker", "plan", "project",
];

fn app_name(original: &str) -> Option<String> {
    let normalized = normalize(original);
    let rest = starts_any(&normalized, &OPEN_VERBS)?;
    if rest.is_empty()
        || rest.split(' ').any(|w| WORK_NOUNS.contains(&w))
        || url_in(original).is_some()
    {
        return None;
    }
    let name = original_after(original, &OPEN_VERBS)?;
    let name = name
        .trim_start_matches("the ")
        .trim_start_matches("The ")
        .trim_end_matches(" app")
        .trim_end_matches(" application")
        .trim_end_matches(['.', '!', '?'])
        .trim()
        .to_string();
    let words = name.split_whitespace().count();
    ((1..=4).contains(&words)).then(|| app_alias(&name))
}

/// The names people say for apps whose real names differ.
fn app_alias(name: &str) -> String {
    let lower = name.to_lowercase();
    let known = [
        ("vs code", "Visual Studio Code"),
        ("vscode", "Visual Studio Code"),
        ("code", "Visual Studio Code"),
        ("chrome", "Google Chrome"),
        ("iterm", "iTerm"),
        ("finder", "Finder"),
        ("terminal", "Terminal"),
        ("safari", "Safari"),
        ("slack", "Slack"),
        ("lm studio", "LM Studio"),
    ];
    if let Some((_, real)) = known.iter().find(|(spoken, _)| *spoken == lower) {
        return real.to_string();
    }
    name.split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A web address in what was said: `https://…`, or a bare domain like `github.com/x`.
/// Whisper writes "github dot com" as often as "github.com".
pub fn url_in(original: &str) -> Option<String> {
    let text = original.replace(" dot ", ".").replace(" slash ", "/");
    text.split_whitespace()
        .map(|token| {
            token
                .trim_matches(|c: char| ",;!?()\"'".contains(c))
                .trim_end_matches('.')
        })
        .find_map(|token| {
            let lower = token.to_lowercase();
            if lower.starts_with("http://") || lower.starts_with("https://") {
                return Some(token.to_string());
            }
            let host = lower.split('/').next().unwrap_or_default();
            let tld = host.rsplit('.').next().unwrap_or_default();
            let looks_like_host = host.contains('.')
                && !host.starts_with('.')
                && (2..=6).contains(&tld.len())
                && tld.chars().all(|c| c.is_ascii_alphabetic())
                && host
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
            looks_like_host.then(|| format!("https://{lower}"))
        })
}

fn pane(words: &str) -> Option<Pane> {
    let words = words
        .trim_start_matches("the ")
        .trim_start_matches("my ")
        .trim_end_matches(" tab")
        .trim_end_matches(" page")
        .trim_end_matches(" view")
        .trim_end_matches(" board");
    Some(match words {
        "chat" | "conversation" => Pane::Chat,
        "plan" => Pane::Plan,
        "night" | "night shift" | "overnight" => Pane::Night,
        "preview" => Pane::Preview,
        "tools" | "tools and skills" | "skills" => Pane::Tools,
        "settings" | "preferences" => Pane::Settings,
        _ => return None,
    })
}

fn level(words: &str) -> Option<Autonomy> {
    let words = words
        .replace("landsafe", "land safe")
        .replace("landmost", "land most");
    Autonomy::parse(&words.replace(' ', "_"))
}

fn project<'a>(words: &str, snapshot: &'a Snapshot) -> Option<&'a str> {
    let words = words
        .trim_start_matches("the ")
        .trim_end_matches(" project")
        .trim();
    if words.is_empty() {
        return None;
    }
    let named = |p: &&super::ProjectRef| normalize(&p.name) == words;
    let mut exact = snapshot.projects.iter().filter(named);
    if let (Some(one), None) = (exact.next(), exact.next()) {
        return Some(&one.root);
    }
    let mut partial = snapshot
        .projects
        .iter()
        .filter(|p| normalize(&p.name).contains(words));
    match (partial.next(), partial.next()) {
        (Some(one), None) => Some(&one.root),
        _ => None,
    }
}

/// Which worker the words mean, among `candidates`. `None` unless exactly one fits.
///
/// "worker 3" and "number 3" name a worker by its spoken number; a role ("the builder")
/// names it if only one candidate has that role; "the last one" is the most recent; and
/// with no reference at all ("approve it") the only candidate is meant — if there is only
/// one. Ambiguity is refused rather than guessed.
///
/// `all` is every worker, so that naming one who is not a candidate ("stop the builder"
/// when the builder is not running) is refused instead of falling back to the only
/// candidate.
pub fn resolve<'a>(
    words: &str,
    candidates: &[&'a WorkerRef],
    all: &[WorkerRef],
) -> Option<&'a WorkerRef> {
    let tokens: Vec<&str> = words.split(' ').collect();
    for (i, token) in tokens.iter().enumerate() {
        if matches!(*token, "worker" | "number" | "#") {
            if let Some(n) = tokens.get(i + 1).and_then(|t| t.parse::<u32>().ok()) {
                return candidates.iter().copied().find(|w| w.number == n);
            }
        }
    }
    let by_role: Vec<&WorkerRef> = candidates
        .iter()
        .copied()
        .filter(|w| {
            let role = w.role.replace('_', " ");
            has_phrase(words, &role) || has_phrase(words, &format!("{role}s"))
        })
        .collect();
    if !by_role.is_empty() {
        return (by_role.len() == 1).then(|| by_role[0]);
    }
    let names_someone_else = all.iter().any(|w| {
        let role = w.role.replace('_', " ");
        has_phrase(words, &role) || has_phrase(words, &format!("{role}s"))
    });
    if names_someone_else {
        return None;
    }
    if ["last", "latest", "most recent", "newest"]
        .iter()
        .any(|w| has_phrase(words, w))
    {
        return candidates.last().copied();
    }
    (candidates.len() == 1).then(|| candidates[0])
}

const YES: [&str; 14] = [
    "yes",
    "yeah",
    "yep",
    "yup",
    "sure",
    "confirm",
    "confirmed",
    "do it",
    "go ahead",
    "go for it",
    "ok",
    "okay",
    "yes please",
    "approve",
];
const NO: [&str; 10] = [
    "no",
    "nope",
    "cancel",
    "stop",
    "never mind",
    "nevermind",
    "dont",
    "abort",
    "no thanks",
    "forget it",
];

const STATUS: [(&str, StatusTopic); 40] = [
    ("status", StatusTopic::Overview),
    ("whats the status", StatusTopic::Overview),
    ("status update", StatusTopic::Overview),
    ("give me a status update", StatusTopic::Overview),
    ("whats happening", StatusTopic::Overview),
    ("whats going on", StatusTopic::Overview),
    ("whats up", StatusTopic::Overview),
    ("how are we doing", StatusTopic::Overview),
    ("where are we", StatusTopic::Overview),
    ("catch me up", StatusTopic::Overview),
    ("brief me", StatusTopic::Overview),
    ("whats running", StatusTopic::Workers),
    ("what is running", StatusTopic::Workers),
    ("whos working", StatusTopic::Workers),
    ("who is working", StatusTopic::Workers),
    ("what are the workers doing", StatusTopic::Workers),
    ("which workers are running", StatusTopic::Workers),
    ("any workers running", StatusTopic::Workers),
    ("whats waiting", StatusTopic::Waiting),
    ("whats waiting for me", StatusTopic::Waiting),
    ("what is waiting for me", StatusTopic::Waiting),
    ("anything waiting", StatusTopic::Waiting),
    ("anything waiting for me", StatusTopic::Waiting),
    ("anything for me", StatusTopic::Waiting),
    ("anything to review", StatusTopic::Waiting),
    ("what needs my attention", StatusTopic::Waiting),
    ("what do i need to review", StatusTopic::Waiting),
    ("hows the plan", StatusTopic::Plan),
    ("hows the plan going", StatusTopic::Plan),
    ("how is the plan going", StatusTopic::Plan),
    ("plan status", StatusTopic::Plan),
    ("hows the night shift", StatusTopic::Night),
    ("hows the night shift going", StatusTopic::Night),
    ("how is the night shift going", StatusTopic::Night),
    ("how did the night shift go", StatusTopic::Night),
    ("night shift status", StatusTopic::Night),
    ("morning report", StatusTopic::Night),
    ("how much quota is left", StatusTopic::Limits),
    ("how close am i to the limit", StatusTopic::Limits),
    ("whats my usage", StatusTopic::Limits),
];

/// An exact command, or `None` to let Laya (or the head agent) have it.
pub fn match_command(original: &str, snapshot: &Snapshot, pending: bool) -> Option<VoiceAction> {
    let n = normalize(original);
    let n = n.as_str();
    if n.is_empty() {
        return None;
    }

    // An answer to "are you sure?" comes first, and only then.
    if pending {
        if is_any(n, &YES) {
            return Some(VoiceAction::Confirm);
        }
        if is_any(n, &NO) {
            return Some(VoiceAction::Cancel);
        }
        return None;
    }

    if let Some((_, topic)) = STATUS.iter().find(|(phrase, _)| *phrase == n) {
        return Some(VoiceAction::Status { topic: *topic });
    }
    if is_any(n, &["limits", "quota", "usage", "how much is left"]) {
        return Some(VoiceAction::Status {
            topic: StatusTopic::Limits,
        });
    }

    // The head agent.
    if let Some(rest) = starts_any(
        n,
        &[
            "tell claude",
            "ask claude",
            "tell the head agent",
            "ask the head agent",
            "tell the lead",
        ],
    ) {
        if !rest.is_empty() {
            let text = original_after(
                original,
                &[
                    "tell claude",
                    "ask claude",
                    "tell the head agent",
                    "ask the head agent",
                    "tell the lead",
                ],
            )?;
            return Some(VoiceAction::AskHead { text });
        }
    }
    if is_any(
        n,
        &[
            "stop",
            "stop talking",
            "stop claude",
            "stop the head agent",
            "stop the turn",
            "interrupt",
            "be quiet",
            "hold on",
        ],
    ) {
        return Some(VoiceAction::StopTurn);
    }

    // The plan.
    if is_any(
        n,
        &[
            "run the plan",
            "run plan",
            "start the plan",
            "go with the plan",
            "execute the plan",
            "run the plan as it is",
        ],
    ) {
        return Some(VoiceAction::RunPlan);
    }
    if is_any(
        n,
        &[
            "discard the plan",
            "scrap the plan",
            "cancel the plan",
            "stop the plan",
            "drop the plan",
            "throw away the plan",
        ],
    ) {
        return Some(VoiceAction::DiscardPlan);
    }
    if starts_any(
        n,
        &[
            "plan feedback",
            "feedback on the plan",
            "tell the planner",
            "comment on the plan",
        ],
    )
    .is_some()
    {
        let note = original_after(
            original,
            &[
                "plan feedback",
                "feedback on the plan",
                "tell the planner",
                "comment on the plan",
            ],
        )?;
        return Some(VoiceAction::PlanFeedback { note });
    }

    // The night shift.
    if is_any(
        n,
        &[
            "stop the night shift",
            "stop night shift",
            "stop the night",
            "end the night shift",
            "cancel the night shift",
        ],
    ) {
        return Some(VoiceAction::StopNight);
    }
    if has_word(n, "propose") && n.contains("night")
        || is_any(
            n,
            &[
                "put the nights work up for review",
                "review the nights work",
            ],
        )
    {
        return Some(VoiceAction::ProposeNight);
    }
    if starts_any(
        n,
        &[
            "start a night shift",
            "start the night shift",
            "set up a night shift",
            "run a night shift",
            "start an overnight run",
            "start a night run",
        ],
    )
    .is_some()
    {
        return Some(VoiceAction::NightSetup {
            goal: slots(original).goal,
        });
    }

    // Autonomy.
    if let Some(level) = autonomy_command(n) {
        return Some(VoiceAction::SetAutonomy { level });
    }

    // Navigation, projects, and looking at a worker.
    if let Some(rest) = starts_any(
        n,
        &[
            "show me",
            "show",
            "open",
            "go to",
            "switch to",
            "take me to",
            "bring up",
        ],
    ) {
        if let Some(pane) = pane(rest) {
            return Some(VoiceAction::Navigate { pane });
        }
    }
    if let Some(rest) = starts_any(
        n,
        &[
            "switch to project",
            "switch project to",
            "open project",
            "go to project",
            "open the project",
        ],
    ) {
        if let Some(root) = project(rest, snapshot) {
            return Some(VoiceAction::SwitchProject {
                project: root.to_string(),
            });
        }
    }
    if let Some(rest) = starts_any(n, &["switch to", "go to"]) {
        if let Some(root) = project(rest, snapshot) {
            return Some(VoiceAction::SwitchProject {
                project: root.to_string(),
            });
        }
    }
    if let Some(rest) = starts_any(n, &["show me", "show", "open", "look at"]) {
        if has_word(rest, "worker") || role_named(rest, snapshot) {
            let all: Vec<&WorkerRef> = snapshot.workers.iter().collect();
            if let Some(worker) = resolve(rest, &all, &snapshot.workers) {
                return Some(VoiceAction::OpenWorker {
                    worker: worker.id.clone(),
                });
            }
        }
    }

    if let Some(action) = worker_command(original, n, snapshot) {
        return Some(action);
    }

    // The computer, within the safe list.
    if is_any(
        n,
        &[
            "reveal the project",
            "show the project in finder",
            "open the project in finder",
            "open the project folder",
            "open project folder",
            "show the project folder",
            "open the folder",
        ],
    ) {
        return Some(VoiceAction::RevealProject);
    }
    if starts_any(
        n,
        &[
            "open the project in",
            "open project in",
            "open this in",
            "open it in",
        ],
    )
    .is_some_and(|rest| {
        ["editor", "my editor", "code", "vs code", "vscode", "cursor"].contains(&rest)
    }) || is_any(
        n,
        &["edit the project", "open in editor", "open in my editor"],
    ) {
        return Some(VoiceAction::OpenProjectInEditor);
    }
    if let Some(rest) = starts_any(n, &["open", "show"]) {
        let folder = rest
            .trim_start_matches("my ")
            .trim_start_matches("the ")
            .trim_end_matches(" folder");
        let path = match folder {
            "downloads" => Some("~/Downloads"),
            "documents" => Some("~/Documents"),
            "desktop" => Some("~/Desktop"),
            "home" => Some("~"),
            _ => None,
        };
        if let Some(path) = path {
            return Some(VoiceAction::OpenFolder { path: path.into() });
        }
    }
    if starts_any(n, &["open", "go to", "visit", "browse to", "pull up"]).is_some() {
        if let Some(url) = url_in(original) {
            return Some(VoiceAction::OpenUrl { url });
        }
    }
    if let Some(name) = app_name(original) {
        return Some(VoiceAction::OpenApp { name });
    }
    None
}

fn role_named(words: &str, snapshot: &Snapshot) -> bool {
    snapshot
        .workers
        .iter()
        .any(|w| has_phrase(words, &w.role.replace('_', " ")))
}

fn autonomy_command(n: &str) -> Option<Autonomy> {
    let tail = |marker: &str| {
        n.split_once(marker)
            .map(|(_, rest)| rest.trim().trim_start_matches("to ").trim())
    };
    if has_word(n, "autonomy") {
        if let Some(rest) = tail("autonomy") {
            let rest = rest.trim_start_matches("level ").trim_start_matches("to ");
            if let Some(level) = level(rest) {
                return Some(level);
            }
        }
    }
    if let Some(rest) = n.strip_suffix(" mode") {
        let rest = starts_any(rest, &["switch to", "go to", "set to", "use"]).unwrap_or(rest);
        if let Some(level) = level(rest) {
            return Some(level);
        }
    }
    let rest = starts_any(n, &["switch to", "go to", "set it to"])?;
    matches!(rest, "land safe" | "landsafe" | "land most" | "landmost")
        .then(|| level(rest))
        .flatten()
}

/// Merges, delegations and stopping workers: a verb, then a reference to a worker.
fn worker_command(original: &str, n: &str, snapshot: &Snapshot) -> Option<VoiceAction> {
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
    let running: Vec<&WorkerRef> = snapshot.workers.iter().filter(|w| w.is_running()).collect();
    let reason = || original_after(original, &["because", "reason being", "since"]);

    let about_delegation = ["delegation", "task", "request", "let"]
        .iter()
        .any(|w| has_word(n, w));
    let about_merge = ["merge", "change", "changes", "branch", "pr", "work"]
        .iter()
        .any(|w| has_word(n, w));

    // Undo: the most recent landing unless a worker is named.
    if let Some(rest) = starts_any(n, &["undo", "revert", "roll back", "rollback"]) {
        let landed: Vec<&WorkerRef> = snapshot
            .landed
            .iter()
            .filter_map(|id| snapshot.workers.iter().find(|w| w.id == *id))
            .collect();
        let named = resolve(rest, &landed, &snapshot.workers)
            .filter(|_| has_word(rest, "worker") || role_named(rest, snapshot));
        let worker = named
            .map(|w| w.id.clone())
            .or_else(|| snapshot.landed.last().cloned())?;
        return Some(VoiceAction::UndoMerge { worker });
    }

    // Letting a delegation start is unambiguous with "let".
    if let Some(rest) = starts_any(n, &["let"]) {
        if rest.ends_with("start") || rest.ends_with("run") || rest.ends_with("go") {
            let worker = resolve(rest, &approvals, &snapshot.workers)?;
            return Some(VoiceAction::ApproveDelegation {
                worker: worker.id.clone(),
            });
        }
    }

    if let Some(rest) = starts_any(n, &["approve", "accept", "merge", "land", "ship", "lgtm"]) {
        let merge = resolve(rest, &merges, &snapshot.workers);
        let delegation = resolve(rest, &approvals, &snapshot.workers);
        let verb_is_merge = starts_any(n, &["merge", "land", "ship"]).is_some();
        return match (merge, delegation) {
            (Some(w), _) if (about_merge || verb_is_merge) && !about_delegation => {
                Some(VoiceAction::ApproveMerge {
                    worker: w.id.clone(),
                })
            }
            (_, Some(w)) if about_delegation && !verb_is_merge => {
                Some(VoiceAction::ApproveDelegation {
                    worker: w.id.clone(),
                })
            }
            (Some(w), None) if !about_delegation => Some(VoiceAction::ApproveMerge {
                worker: w.id.clone(),
            }),
            (None, Some(w)) if !about_merge && !verb_is_merge => {
                Some(VoiceAction::ApproveDelegation {
                    worker: w.id.clone(),
                })
            }
            _ => None,
        };
    }

    if let Some(rest) = starts_any(
        n,
        &[
            "reject",
            "discard",
            "throw away",
            "decline",
            "deny",
            "refuse",
        ],
    ) {
        let merge = resolve(rest, &merges, &snapshot.workers);
        let delegation = resolve(rest, &approvals, &snapshot.workers);
        let verb_is_delegation = starts_any(n, &["decline", "deny", "refuse"]).is_some();
        return match (merge, delegation) {
            (_, Some(w)) if about_delegation || (verb_is_delegation && !about_merge) => {
                Some(VoiceAction::DeclineDelegation {
                    worker: w.id.clone(),
                    reason: reason(),
                })
            }
            (Some(w), _) if !about_delegation => Some(VoiceAction::RejectMerge {
                worker: w.id.clone(),
                reason: reason(),
            }),
            _ => None,
        };
    }

    if let Some(rest) = starts_any(n, &["stop", "cancel", "kill", "halt"]) {
        if has_word(rest, "worker")
            || role_named(rest, snapshot)
            || is_any(rest, &["it", "that", "that worker", "the worker"])
        {
            let worker = resolve(rest, &running, &snapshot.workers)?;
            return Some(VoiceAction::StopWorker {
                worker: worker.id.clone(),
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::super::tests::{snapshot, worker};
    use super::*;

    #[test]
    fn normalizing_keeps_the_words_that_matter() {
        assert_eq!(
            normalize("Hey harness, um, what's waiting for me?"),
            "whats waiting for me"
        );
        assert_eq!(
            normalize("Approve worker three, please."),
            "approve worker 3"
        );
        assert_eq!(normalize("approve W3"), "approve worker 3");
        assert_eq!(
            normalize("Can you open the Plan tab now"),
            "open the plan tab"
        );
        assert_eq!(normalize("um"), "");
    }

    #[test]
    fn free_text_keeps_its_casing() {
        let s = slots("Tell Claude to add retries to the PriceFetcher.");
        assert_eq!(
            s.message.as_deref(),
            Some("add retries to the PriceFetcher")
        );
        assert_eq!(
            slots("reject it because it drops the cache")
                .reason
                .as_deref(),
            Some("it drops the cache")
        );
        assert_eq!(
            slots("open github dot com slash anthropics").url.as_deref(),
            Some("https://github.com/anthropics")
        );
        assert_eq!(
            slots("launch vs code").app.as_deref(),
            Some("Visual Studio Code")
        );
        assert_eq!(
            slots("open the fetcher test file").app,
            None,
            "about the code, not an app"
        );
        assert_eq!(
            slots("start a night shift to make the tests faster")
                .goal
                .as_deref(),
            Some("make the tests faster")
        );
    }

    #[test]
    fn a_worker_reference_resolves_only_when_it_is_clear() {
        let a = worker("w-a", 1, "builder", "running", "x");
        let b = worker("w-b", 2, "builder", "running", "y");
        let c = worker("w-c", 3, "tester", "running", "z");
        let everyone = vec![a.clone(), b.clone(), c.clone()];
        let all = [&a, &b, &c];
        assert_eq!(
            resolve("worker 2", &all, &everyone).map(|w| w.number),
            Some(2)
        );
        assert_eq!(
            resolve("the tester", &all, &everyone).map(|w| w.number),
            Some(3)
        );
        assert_eq!(
            resolve("the builder", &all, &everyone),
            None,
            "two builders"
        );
        assert_eq!(
            resolve("the last one", &all, &everyone).map(|w| w.number),
            Some(3)
        );
        assert_eq!(resolve("it", &all, &everyone), None, "three candidates");
        assert_eq!(resolve("it", &[&a], &everyone).map(|w| w.number), Some(1));
        assert_eq!(
            resolve("the tester", &[&a], &everyone),
            None,
            "named someone who is not a candidate"
        );
        assert_eq!(resolve("worker 9", &all, &everyone), None);
    }

    /// Spoken phrases and what they must become, against the shared snapshot. The same
    /// set, with more phrasing, lives in `voice_phrases.toml` for `harness-cli voice --eval`.
    #[test]
    fn spoken_commands_become_actions() {
        use VoiceAction::*;
        let s = snapshot();
        let cases: Vec<(&str, VoiceAction)> = vec![
            (
                "What's waiting for me?",
                Status {
                    topic: StatusTopic::Waiting,
                },
            ),
            (
                "status",
                Status {
                    topic: StatusTopic::Overview,
                },
            ),
            (
                "How's the night shift going?",
                Status {
                    topic: StatusTopic::Night,
                },
            ),
            (
                "what's running",
                Status {
                    topic: StatusTopic::Workers,
                },
            ),
            ("show me the plan", Navigate { pane: Pane::Plan }),
            (
                "open settings",
                Navigate {
                    pane: Pane::Settings,
                },
            ),
            ("go to the night shift tab", Navigate { pane: Pane::Night }),
            (
                "switch to blog",
                SwitchProject {
                    project: "/p/blog".into(),
                },
            ),
            (
                "open worker two",
                OpenWorker {
                    worker: "w-bbb".into(),
                },
            ),
            (
                "Approve the builder's merge.",
                ApproveMerge {
                    worker: "w-aaa".into(),
                },
            ),
            (
                "merge it",
                ApproveMerge {
                    worker: "w-aaa".into(),
                },
            ),
            (
                "approve worker 1",
                ApproveMerge {
                    worker: "w-aaa".into(),
                },
            ),
            (
                "approve the delegation",
                ApproveDelegation {
                    worker: "w-ccc".into(),
                },
            ),
            (
                "let the reviewer start",
                ApproveDelegation {
                    worker: "w-ccc".into(),
                },
            ),
            (
                "reject the builder's change because it drops the cache",
                RejectMerge {
                    worker: "w-aaa".into(),
                    reason: Some("it drops the cache".into()),
                },
            ),
            (
                "decline the reviewer",
                DeclineDelegation {
                    worker: "w-ccc".into(),
                    reason: None,
                },
            ),
            (
                "stop the tester",
                StopWorker {
                    worker: "w-bbb".into(),
                },
            ),
            (
                "stop worker 2",
                StopWorker {
                    worker: "w-bbb".into(),
                },
            ),
            (
                "undo that",
                UndoMerge {
                    worker: "w-old".into(),
                },
            ),
            ("stop", StopTurn),
            (
                "Tell Claude to add retries to the fetcher.",
                AskHead {
                    text: "add retries to the fetcher".into(),
                },
            ),
            (
                "set autonomy to land safe",
                SetAutonomy {
                    level: Autonomy::LandSafe,
                },
            ),
            (
                "switch to land most",
                SetAutonomy {
                    level: Autonomy::LandMost,
                },
            ),
            (
                "ask mode",
                SetAutonomy {
                    level: Autonomy::Ask,
                },
            ),
            ("run the plan", RunPlan),
            ("scrap the plan", DiscardPlan),
            (
                "plan feedback: do the docs first",
                PlanFeedback {
                    note: "do the docs first".into(),
                },
            ),
            ("stop the night shift", StopNight),
            ("propose the night's work", ProposeNight),
            (
                "start a night shift to make the tests faster",
                NightSetup {
                    goal: Some("make the tests faster".into()),
                },
            ),
            (
                "open Safari",
                OpenApp {
                    name: "Safari".into(),
                },
            ),
            (
                "launch vs code",
                OpenApp {
                    name: "Visual Studio Code".into(),
                },
            ),
            (
                "open github.com",
                OpenUrl {
                    url: "https://github.com".into(),
                },
            ),
            (
                "go to docs dot rs",
                OpenUrl {
                    url: "https://docs.rs".into(),
                },
            ),
            (
                "open my downloads folder",
                OpenFolder {
                    path: "~/Downloads".into(),
                },
            ),
            ("show the project in finder", RevealProject),
            ("open the project in vs code", OpenProjectInEditor),
        ];
        for (said, expected) in cases {
            assert_eq!(match_command(said, &s, false), Some(expected), "{said:?}");
        }
    }

    #[test]
    fn what_is_not_an_exact_command_is_left_alone() {
        let s = snapshot();
        for said in [
            "the fetcher keeps timing out, can we fix it",
            "open the fetcher test file",
            "approve",          // a merge and a delegation are both waiting
            "stop the builder", // the builder is not running
            "switch to nowhere",
        ] {
            assert_eq!(match_command(said, &s, false), None, "{said:?}");
        }
    }

    #[test]
    fn a_pending_question_is_answered_first() {
        let s = snapshot();
        assert_eq!(
            match_command("yes please", &s, true),
            Some(VoiceAction::Confirm)
        );
        assert_eq!(
            match_command("go ahead", &s, true),
            Some(VoiceAction::Confirm)
        );
        assert_eq!(
            match_command("no, cancel", &s, true),
            None,
            "not a clean answer: Laya reads it"
        );
        assert_eq!(
            match_command("never mind", &s, true),
            Some(VoiceAction::Cancel)
        );
        assert_eq!(
            match_command("stop", &s, true),
            Some(VoiceAction::Cancel),
            "not the head agent's turn"
        );
    }
}
