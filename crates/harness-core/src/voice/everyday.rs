//! Everyday things on the Mac, each a fixed recipe: search the web, play and pause music,
//! make a note, set a reminder, change the volume.
//!
//! Recognized here, run by [`super::computer`]. Nothing is ever a free-form script: each
//! action becomes one of a handful of fixed programs, with what was said passed along as
//! arguments, never spliced into code.

use serde::{Deserialize, Serialize};

use super::matcher::{
    find_app, has_phrase, has_word, is_any, normalize, original_after, starts_any,
};
use super::{Snapshot, VoiceAction};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Site {
    Web,
    Youtube,
    Amazon,
    Github,
    Maps,
    Wikipedia,
    Reddit,
}

impl Site {
    pub fn label(self) -> &'static str {
        match self {
            Site::Web => "the web",
            Site::Youtube => "YouTube",
            Site::Amazon => "Amazon",
            Site::Github => "GitHub",
            Site::Maps => "Maps",
            Site::Wikipedia => "Wikipedia",
            Site::Reddit => "Reddit",
        }
    }

    fn named(words: &str) -> Option<Site> {
        Some(match words {
            "youtube" => Site::Youtube,
            "amazon" => Site::Amazon,
            "github" => Site::Github,
            "maps" | "google maps" | "apple maps" => Site::Maps,
            "wikipedia" => Site::Wikipedia,
            "reddit" => Site::Reddit,
            "google" | "the web" | "the internet" | "online" => Site::Web,
            _ => return None,
        })
    }

    /// The results page for a query.
    pub fn url(self, query: &str) -> String {
        let q = encode(query, true);
        match self {
            Site::Web => format!("https://www.google.com/search?q={q}"),
            Site::Youtube => format!("https://www.youtube.com/results?search_query={q}"),
            Site::Amazon => format!("https://www.amazon.com/s?k={q}"),
            Site::Github => format!("https://github.com/search?q={q}"),
            Site::Maps => format!(
                "https://www.google.com/maps/search/{}",
                encode(query, false)
            ),
            Site::Wikipedia => format!("https://en.wikipedia.org/w/index.php?search={q}"),
            Site::Reddit => format!("https://www.reddit.com/search/?q={q}"),
        }
    }
}

/// Percent-encoding for a URL query (spaces as `+`) or path (spaces as `%20`).
pub fn encode(text: &str, query: bool) -> String {
    let mut out = String::new();
    for byte in text.trim().bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' if query => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Player {
    Spotify,
    Music,
}

impl Player {
    pub fn label(self) -> &'static str {
        match self {
            Player::Spotify => "Spotify",
            Player::Music => "Music",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Control {
    Play,
    Pause,
    Next,
    Previous,
}

impl Control {
    pub fn label(self) -> &'static str {
        match self {
            Control::Play => "Play",
            Control::Pause => "Pause",
            Control::Next => "Skip to the next song in",
            Control::Previous => "Go back a song in",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "control", rename_all = "snake_case")]
pub enum SystemControl {
    VolumeUp,
    VolumeDown,
    Mute,
    Unmute,
    SetVolume {
        percent: u8,
    },
    /// Turns the display off; the Mac locks if it asks for a password on wake.
    SleepDisplay,
}

impl SystemControl {
    pub fn label(self) -> String {
        match self {
            SystemControl::VolumeUp => "Volume up".into(),
            SystemControl::VolumeDown => "Volume down".into(),
            SystemControl::Mute => "Mute".into(),
            SystemControl::Unmute => "Unmute".into(),
            SystemControl::SetVolume { percent } => format!("Volume to {percent}%"),
            SystemControl::SleepDisplay => "Lock the screen".into(),
        }
    }
}

/// When a reminder is due. A clock time is today, or tomorrow if it has passed, unless a
/// day was said.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum When {
    In {
        seconds: u32,
    },
    At {
        hour: u8,
        minute: u8,
        days_ahead: u8,
    },
}

impl When {
    pub fn label(self) -> String {
        match self {
            When::In { seconds } if seconds % 3600 == 0 => {
                let h = seconds / 3600;
                format!("in {h} hour{}", if h == 1 { "" } else { "s" })
            }
            When::In { seconds } => {
                let m = seconds / 60;
                format!("in {m} minute{}", if m == 1 { "" } else { "s" })
            }
            When::At {
                hour,
                minute,
                days_ahead,
            } => {
                let (h12, ampm) = match hour {
                    0 => (12, "AM"),
                    1..=11 => (hour, "AM"),
                    12 => (12, "PM"),
                    _ => (hour - 12, "PM"),
                };
                let day = if days_ahead == 1 { " tomorrow" } else { "" };
                format!("at {h12}:{minute:02} {ampm}{day}")
            }
        }
    }
}

/// Split a reminder into what and when: "push the branch at 5" → ("push the branch", at 5 PM).
/// Works on normalized words. An hour from 1 to 7 without am/pm is taken as the afternoon —
/// that is what people mean by "at 5".
pub fn parse_when(words: &str) -> (String, Option<When>) {
    let tokens: Vec<&str> = words.split(' ').filter(|w| !w.is_empty()).collect();
    let mut days_ahead = 0u8;
    let mut kept: Vec<&str> = Vec::new();
    let mut when: Option<When> = None;
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i];
        let next = tokens.get(i + 1).copied();
        match t {
            "tomorrow" => {
                days_ahead = 1;
                i += 1;
                continue;
            }
            "tonight" => {
                when = Some(When::At {
                    hour: 20,
                    minute: 0,
                    days_ahead: 0,
                });
                i += 1;
                continue;
            }
            "in" if next.and_then(|n| n.parse::<u32>().ok()).is_some() => {
                let n: u32 = next.unwrap().parse().unwrap();
                let unit = tokens.get(i + 2).copied().unwrap_or("");
                let seconds = match unit {
                    "minute" | "minutes" | "min" | "mins" => Some(n * 60),
                    "hour" | "hours" => Some(n * 3600),
                    _ => None,
                };
                if let Some(seconds) = seconds {
                    when = Some(When::In { seconds });
                    i += 3;
                    continue;
                }
            }
            "at" if next
                .and_then(|n| n.parse::<u8>().ok())
                .is_some_and(|h| (1..=23).contains(&h)) =>
            {
                let mut hour: u8 = next.unwrap().parse().unwrap();
                let mut minute = 0u8;
                let mut j = i + 2;
                if let Some(m) = tokens
                    .get(j)
                    .and_then(|m| m.parse::<u8>().ok())
                    .filter(|m| *m < 60)
                {
                    minute = m;
                    j += 1;
                }
                match tokens.get(j).copied() {
                    Some("pm" | "p") => {
                        if hour < 12 {
                            hour += 12;
                        }
                        j += if tokens.get(j + 1) == Some(&"m") {
                            2
                        } else {
                            1
                        };
                    }
                    Some("am" | "a") => {
                        if hour == 12 {
                            hour = 0;
                        }
                        j += if tokens.get(j + 1) == Some(&"m") {
                            2
                        } else {
                            1
                        };
                    }
                    _ if (1..=7).contains(&hour) => hour += 12,
                    _ => {}
                }
                when = Some(When::At {
                    hour,
                    minute,
                    days_ahead: 0,
                });
                i = j;
                continue;
            }
            _ => {}
        }
        kept.push(t);
        i += 1;
    }
    let when = match when {
        Some(When::At { hour, minute, .. }) => Some(When::At {
            hour,
            minute,
            days_ahead,
        }),
        Some(other) => Some(other),
        None if days_ahead == 1 => Some(When::At {
            hour: 9,
            minute: 0,
            days_ahead: 1,
        }),
        None => None,
    };
    (kept.join(" "), when)
}

const BROWSERS: [(&str, &str); 8] = [
    ("google chrome", "Google Chrome"),
    ("chrome", "Google Chrome"),
    ("safari", "Safari"),
    ("firefox", "Firefox"),
    ("arc", "Arc"),
    ("brave", "Brave Browser"),
    ("microsoft edge", "Microsoft Edge"),
    ("edge", "Microsoft Edge"),
];

/// Words that make a "search" about the code, which is Claude's to do.
const CODE_WORDS: [&str; 10] = [
    "code",
    "codebase",
    "repo",
    "repository",
    "function",
    "file",
    "files",
    "test",
    "tests",
    "branch",
];

fn browser_app(spoken: &str, snapshot: &Snapshot) -> String {
    let named = BROWSERS
        .iter()
        .find(|(said, _)| *said == spoken)
        .map(|(_, app)| *app)
        .unwrap_or(spoken);
    if snapshot.apps.is_empty() {
        return named.to_string();
    }
    find_app(&normalize(named), &snapshot.apps).unwrap_or_else(|| named.to_string())
}

/// Take a browser out of the words — "open chrome and …", "… in safari" — and say which.
fn take_browser(n: &str) -> (String, Option<&'static str>) {
    for (said, _) in BROWSERS {
        for lead in ["open", "use", "in", "on", "with", "go to"] {
            let prefix = format!("{lead} {said} and ");
            if let Some(rest) = n.strip_prefix(&prefix) {
                return (rest.to_string(), Some(said));
            }
            let prefix = format!("{lead} {said} ");
            if let Some(rest) = n.strip_prefix(&prefix) {
                return (rest.to_string(), Some(said));
            }
        }
        for lead in ["in", "on", "with", "using"] {
            let suffix = format!(" {lead} {said}");
            if let Some(rest) = n.strip_suffix(&suffix) {
                return (rest.to_string(), Some(said));
            }
        }
    }
    (n.to_string(), None)
}

fn search(n: &str, snapshot: &Snapshot) -> Option<VoiceAction> {
    let (rest, browser) = take_browser(n);
    let mut site = Site::Web;
    let mut query: Option<String> = None;

    // "search youtube for lofi"
    if let Some(after) = starts_any(&rest, &["search", "look on"]) {
        if let Some((place, q)) = after.split_once(" for ") {
            if let Some(s) = Site::named(place.trim_start_matches("the ")) {
                site = s;
                query = Some(q.to_string());
            }
        }
    }
    if query.is_none() {
        let q = starts_any(
            &rest,
            &[
                "search the web for",
                "search online for",
                "search for",
                "google",
                "search",
                "look up",
            ],
        )?;
        let mut q = q.to_string();
        // "… on amazon"
        for lead in [" on ", " in "] {
            if let Some((before, place)) = q.rsplit_once(lead) {
                if let Some(s) = Site::named(place) {
                    site = s;
                    q = before.to_string();
                    break;
                }
            }
        }
        query = Some(q);
    }
    let query = query?.trim().to_string();
    if query.is_empty() || query.split(' ').any(|w| CODE_WORDS.contains(&w)) {
        return None;
    }
    // "look up" is for looking things up in the world only when it says where.
    if starts_any(&rest, &["look up"]).is_some() && site == Site::Web && browser.is_none() {
        return None;
    }
    Some(VoiceAction::Search {
        query,
        site,
        browser: browser.map(|b| browser_app(b, snapshot)),
    })
}

fn player_in(n: &str) -> (String, Option<Player>) {
    let spotify = ["spotify"];
    let music = ["apple music", "the music app", "music app", "itunes"];
    let mut text = n.to_string();
    let mut player = None;
    for (names, which) in [(&spotify[..], Player::Spotify), (&music[..], Player::Music)] {
        for name in names {
            for pattern in [
                format!("open {name} and "),
                format!("go to {name} and "),
                format!("in {name} "),
            ] {
                if let Some(rest) = text.strip_prefix(&pattern) {
                    text = rest.to_string();
                    player = Some(which);
                }
            }
            for pattern in [
                format!(" on {name}"),
                format!(" in {name}"),
                format!(" with {name}"),
            ] {
                if let Some(rest) = text.strip_suffix(&pattern) {
                    text = rest.to_string();
                    player = Some(which);
                }
            }
            if player.is_none() && has_phrase(&text, name) {
                player = Some(which);
            }
        }
    }
    (text, player)
}

fn media(n: &str) -> Option<VoiceAction> {
    let (rest, app) = player_in(n);
    let rest = rest.as_str();
    let control = if is_any(
        rest,
        &[
            "pause",
            "pause the music",
            "pause music",
            "pause it",
            "pause the song",
            "stop the music",
            "stop music",
            "stop playing",
            "pause spotify",
        ],
    ) {
        Some(Control::Pause)
    } else if is_any(
        rest,
        &[
            "play",
            "resume",
            "play music",
            "play the music",
            "resume the music",
            "resume music",
            "keep playing",
            "unpause",
        ],
    ) {
        Some(Control::Play)
    } else if is_any(
        rest,
        &[
            "next",
            "next song",
            "next track",
            "skip",
            "skip this song",
            "skip this",
            "skip it",
            "skip the song",
            "skip song",
            "play the next song",
        ],
    ) {
        Some(Control::Next)
    } else if is_any(
        rest,
        &[
            "previous",
            "previous song",
            "previous track",
            "last song",
            "go back a song",
            "play the previous song",
            "back one song",
        ],
    ) {
        Some(Control::Previous)
    } else {
        None
    };
    if let Some(control) = control {
        return Some(VoiceAction::Media { app, control });
    }

    let what = starts_any(rest, &["play", "put on", "start playing"])?;
    if what.is_empty() {
        return None;
    }
    // "my playlist called top 200", "the top 200 playlist", "playlist top 200"
    let playlist = starts_any(
        what,
        &[
            "my playlist called",
            "my playlist named",
            "the playlist called",
            "the playlist named",
            "a playlist called",
            "playlist called",
            "playlist named",
            "my playlist",
            "the playlist",
            "playlist",
        ],
    )
    .map(str::to_string)
    .or_else(|| {
        what.strip_suffix(" playlist").map(|name| {
            name.trim_start_matches("my ")
                .trim_start_matches("the ")
                .to_string()
        })
    });
    Some(match playlist {
        Some(name) if !name.is_empty() => VoiceAction::PlayPlaylist { app, name },
        _ => VoiceAction::PlayQuery {
            app,
            query: what.trim_start_matches("some ").to_string(),
        },
    })
}

fn system(n: &str) -> Option<VoiceAction> {
    let control = if is_any(
        n,
        &[
            "volume up",
            "turn the volume up",
            "turn up the volume",
            "turn it up",
            "louder",
            "increase the volume",
            "make it louder",
        ],
    ) {
        SystemControl::VolumeUp
    } else if is_any(
        n,
        &[
            "volume down",
            "turn the volume down",
            "turn down the volume",
            "turn it down",
            "quieter",
            "decrease the volume",
            "make it quieter",
            "lower the volume",
        ],
    ) {
        SystemControl::VolumeDown
    } else if is_any(
        n,
        &[
            "mute",
            "mute the sound",
            "mute the volume",
            "mute my mac",
            "mute the mac",
        ],
    ) {
        SystemControl::Mute
    } else if is_any(n, &["unmute", "unmute the sound", "unmute my mac"]) {
        SystemControl::Unmute
    } else if is_any(
        n,
        &[
            "lock the screen",
            "lock my screen",
            "lock the computer",
            "lock my computer",
            "lock my mac",
            "lock the mac",
            "sleep the screen",
            "sleep the display",
            "turn off the screen",
            "turn off the display",
        ],
    ) {
        SystemControl::SleepDisplay
    } else if let Some(rest) = starts_any(
        n,
        &[
            "set the volume to",
            "set volume to",
            "volume to",
            "turn the volume to",
        ],
    ) {
        let percent: u8 = rest
            .trim_end_matches(" percent")
            .trim_end_matches('%')
            .trim()
            .parse()
            .ok()?;
        SystemControl::SetVolume {
            percent: percent.min(100),
        }
    } else {
        return None;
    };
    Some(VoiceAction::System { control })
}

fn note(original: &str, n: &str) -> Option<VoiceAction> {
    let markers = [
        "create a new note that",
        "create a new note saying",
        "create a new note",
        "make a new note that",
        "make a new note",
        "start a new note",
        "add a new note",
        "write a new note",
        "new note saying",
        "make a note that",
        "make a note saying",
        "make a note",
        "take a note that",
        "take a note",
        "create a note",
        "add a note",
        "write a note",
        "jot down",
        "note to self",
        "new note",
    ];
    starts_any(n, &markers)?;
    let text = original_after(original, &markers)?;
    let text = text
        .trim_start_matches("that ")
        .trim_start_matches("saying ")
        .trim()
        .to_string();
    (!text.is_empty()).then_some(VoiceAction::NewNote { text })
}

fn reminder(n: &str) -> Option<VoiceAction> {
    let rest = starts_any(
        n,
        &[
            "remind me to",
            "remind me",
            "set a reminder to",
            "set a reminder for",
            "add a reminder to",
            "create a reminder to",
            "add a reminder",
            "set a reminder",
        ],
    )?;
    let (text, when) = parse_when(rest.trim_start_matches("to "));
    let text = text.trim_start_matches("to ").trim().to_string();
    if text.is_empty() {
        return None;
    }
    let mut chars = text.chars();
    let text = chars
        .next()
        .map(|c| c.to_uppercase().collect::<String>() + chars.as_str())
        .unwrap_or(text);
    Some(VoiceAction::Remind { text, when })
}

/// One of the everyday recipes, if the words are one.
pub fn match_everyday(original: &str, snapshot: &Snapshot) -> Option<VoiceAction> {
    let n = normalize(original);
    if n.is_empty() {
        return None;
    }
    system(&n)
        .or_else(|| note(original, &n))
        .or_else(|| reminder(&n))
        .or_else(|| search(&n, snapshot))
        .or_else(|| media(&n))
        .filter(|_| !has_word(&n, "worker"))
}

/// Several commands in one breath — "open notes and show me the plan" — as their parts, if
/// every part is a command on its own. Otherwise `None`, and the words are read whole.
pub fn split_commands(original: &str, snapshot: &Snapshot) -> Option<Vec<String>> {
    // "Open chrome and search for shoes" is one thing — a search in Chrome — not two.
    if match_everyday(original, snapshot).is_some() {
        return None;
    }
    let lower = original.to_lowercase();
    let mut cuts: Vec<(usize, usize)> = Vec::new();
    for separator in [
        ", and then ",
        " and then ",
        ", then ",
        " then ",
        ", and ",
        " and ",
    ] {
        let mut from = 0;
        while let Some(found) = lower[from..].find(separator) {
            let start = from + found;
            let end = start + separator.len();
            if !cuts.iter().any(|(s, e)| start < *e && end > *s) {
                cuts.push((start, end));
            }
            from = end;
        }
    }
    if cuts.is_empty() {
        return None;
    }
    cuts.sort();
    let mut parts = Vec::new();
    let mut from = 0;
    for (start, end) in cuts {
        if !original.is_char_boundary(start) || !original.is_char_boundary(end) {
            return None;
        }
        parts.push(original[from..start].trim().to_string());
        from = end;
    }
    parts.push(original[from..].trim().to_string());
    let parts: Vec<String> = parts.into_iter().filter(|p| !p.is_empty()).collect();
    if parts.len() < 2 || parts.len() > 4 {
        return None;
    }
    parts
        .iter()
        .all(|part| super::matcher::match_command(part, snapshot, false).is_some())
        .then_some(parts)
}

#[cfg(test)]
mod tests {
    use super::super::eval::fixture;
    use super::*;

    fn heard(said: &str) -> Option<VoiceAction> {
        super::super::matcher::match_command(said, &fixture(), false)
    }

    #[test]
    fn searches_go_to_the_right_place() {
        assert_eq!(
            heard("open chrome and search for shoes"),
            Some(VoiceAction::Search {
                query: "shoes".into(),
                site: Site::Web,
                browser: Some("Google Chrome".into())
            })
        );
        assert_eq!(
            heard("search youtube for lofi beats"),
            Some(VoiceAction::Search {
                query: "lofi beats".into(),
                site: Site::Youtube,
                browser: None
            })
        );
        assert_eq!(
            heard("search for running shoes on amazon"),
            Some(VoiceAction::Search {
                query: "running shoes".into(),
                site: Site::Amazon,
                browser: None
            })
        );
        assert_eq!(
            heard("google salt and pepper grinders"),
            Some(VoiceAction::Search {
                query: "salt and pepper grinders".into(),
                site: Site::Web,
                browser: None
            }),
            "an and inside a query is not two commands"
        );
        assert_eq!(
            heard("search for the retry function in the code"),
            None,
            "that is Claude's"
        );
        assert_eq!(
            Site::Web.url("running shoes"),
            "https://www.google.com/search?q=running+shoes"
        );
        assert_eq!(
            Site::Maps.url("coffee near me"),
            "https://www.google.com/maps/search/coffee%20near%20me"
        );
        assert_eq!(encode("c++ & rust?", true), "c%2B%2B+%26+rust%3F");
    }

    #[test]
    fn music_is_controlled_and_playlists_found() {
        assert_eq!(
            heard("pause the music"),
            Some(VoiceAction::Media {
                app: None,
                control: Control::Pause
            })
        );
        assert_eq!(
            heard("skip this song"),
            Some(VoiceAction::Media {
                app: None,
                control: Control::Next
            })
        );
        assert_eq!(
            heard("open spotify and play my playlist called top 200"),
            Some(VoiceAction::PlayPlaylist {
                app: Some(Player::Spotify),
                name: "top 200".into()
            })
        );
        assert_eq!(
            heard("play the chill mix playlist in apple music"),
            Some(VoiceAction::PlayPlaylist {
                app: Some(Player::Music),
                name: "chill mix".into()
            })
        );
        assert_eq!(
            heard("play some jazz on spotify"),
            Some(VoiceAction::PlayQuery {
                app: Some(Player::Spotify),
                query: "jazz".into()
            })
        );
        assert_eq!(
            heard("next song on spotify"),
            Some(VoiceAction::Media {
                app: Some(Player::Spotify),
                control: Control::Next
            })
        );
        assert_eq!(
            heard("stop"),
            Some(VoiceAction::StopTurn),
            "plain stop is still the head agent's"
        );
        assert_eq!(
            heard("open spotify"),
            Some(VoiceAction::OpenApp {
                name: "Spotify".into()
            }),
            "just opening it"
        );
    }

    #[test]
    fn notes_and_reminders_keep_what_was_said() {
        assert_eq!(
            heard("Make a note: call the dentist about Thursday."),
            Some(VoiceAction::NewNote {
                text: "call the dentist about Thursday".into()
            })
        );
        assert_eq!(
            heard("create a new note saying buy milk and eggs"),
            Some(VoiceAction::NewNote {
                text: "buy milk and eggs".into()
            })
        );
        assert_eq!(
            heard("remind me to push the branch at 5"),
            Some(VoiceAction::Remind {
                text: "Push the branch".into(),
                when: Some(When::At {
                    hour: 17,
                    minute: 0,
                    days_ahead: 0
                })
            })
        );
        assert_eq!(
            heard("remind me to call mum in 20 minutes"),
            Some(VoiceAction::Remind {
                text: "Call mum".into(),
                when: Some(When::In { seconds: 1200 })
            })
        );
        assert_eq!(
            heard("remind me tomorrow at 9:30 am to renew the domain"),
            Some(VoiceAction::Remind {
                text: "Renew the domain".into(),
                when: Some(When::At {
                    hour: 9,
                    minute: 30,
                    days_ahead: 1
                })
            })
        );
        assert_eq!(
            heard("set a reminder to water the plants"),
            Some(VoiceAction::Remind {
                text: "Water the plants".into(),
                when: None
            })
        );
        assert_eq!(
            When::At {
                hour: 17,
                minute: 5,
                days_ahead: 1
            }
            .label(),
            "at 5:05 PM tomorrow"
        );
    }

    #[test]
    fn the_mac_itself() {
        assert_eq!(
            heard("turn it up"),
            Some(VoiceAction::System {
                control: SystemControl::VolumeUp
            })
        );
        assert_eq!(
            heard("mute"),
            Some(VoiceAction::System {
                control: SystemControl::Mute
            })
        );
        assert_eq!(
            heard("set the volume to 30 percent"),
            Some(VoiceAction::System {
                control: SystemControl::SetVolume { percent: 30 }
            })
        );
        assert_eq!(
            heard("lock my screen"),
            Some(VoiceAction::System {
                control: SystemControl::SleepDisplay
            })
        );
    }

    #[test]
    fn two_commands_in_one_breath_are_two() {
        let s = fixture();
        assert_eq!(
            split_commands("open notes and show me the plan", &s),
            Some(vec![
                "open notes".to_string(),
                "show me the plan".to_string()
            ])
        );
        assert_eq!(
            split_commands("pause the music, then what's waiting for me", &s),
            Some(vec![
                "pause the music".to_string(),
                "what's waiting for me".to_string()
            ])
        );
        assert_eq!(
            split_commands("add retries and a test for the fetcher", &s),
            None,
            "not commands"
        );
        assert_eq!(split_commands("open notes", &s), None);
        assert_eq!(
            split_commands("open chrome and search for shoes", &s),
            None,
            "one search, in Chrome"
        );
        assert_eq!(
            split_commands("open spotify and play my playlist called top 200", &s),
            None
        );
    }
}
