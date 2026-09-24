//! The computer, within a safe list: open an app, a website, a folder, or the project;
//! search the web; control music; make a note or a reminder; change the volume.
//!
//! Nothing here runs a shell, and nothing runs a script someone wrote on the fly. Each
//! action becomes one fixed program: `open` (or `xdg-open`) with checked arguments, or
//! `osascript` with one of the fixed scripts below. What was said reaches a script only as
//! an argument (`item 1 of argv`), never as part of its code.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

use super::everyday::{encode, Control, Player, SystemControl, When};
use super::VoiceAction;

/// A computer action, checked and ready to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Opening {
    App(String),
    Url(String),
    Folder(PathBuf),
    /// The project in the person's code editor.
    Editor(PathBuf),
    /// A web page in a particular browser.
    UrlIn {
        url: String,
        browser: String,
    },
    /// Spotify's own search, for a playlist or song it cannot be told to play by name.
    SpotifySearch(String),
    /// One of the fixed AppleScripts below, with its arguments.
    Script {
        script: &'static str,
        args: Vec<String>,
    },
    /// The display to sleep (locks the Mac if it asks for a password on wake).
    SleepDisplay,
}

const PLAYER_CONTROL: &str = r#"on run argv
    set c to item 1 of argv
    set p to item 2 of argv
    if p is "Spotify" then
        tell application "Spotify"
            if c is "play" then play
            if c is "pause" then pause
            if c is "next" then next track
            if c is "previous" then previous track
        end tell
    else
        tell application "Music"
            if c is "play" then play
            if c is "pause" then pause
            if c is "next" then next track
            if c is "previous" then previous track
        end tell
    end if
end run"#;

const MUSIC_PLAYLIST: &str = r#"on run argv
    tell application "Music"
        if not (exists playlist (item 1 of argv)) then error "There is no playlist called " & (item 1 of argv) & " in Music."
        play playlist (item 1 of argv)
    end tell
end run"#;

const MUSIC_SEARCH: &str = r#"on run argv
    tell application "Music"
        set found to (search playlist "Library" for (item 1 of argv))
        if (count of found) is 0 then error "Nothing in your Music library matches " & (item 1 of argv) & "."
        play item 1 of found
    end tell
end run"#;

const NEW_NOTE: &str = r#"on run argv
    tell application "Notes" to make new note with properties {body:(item 1 of argv)}
end run"#;

/// Arguments: the text; then nothing, or `in <seconds>`, or `at <seconds since midnight>
/// <days ahead>`.
const NEW_REMINDER: &str = r#"on run argv
    set t to item 1 of argv
    tell application "Reminders"
        if (count of argv) is 1 then
            make new reminder with properties {name:t}
        else
            set d to current date
            if item 2 of argv is "in" then
                set d to d + ((item 3 of argv) as integer)
            else
                set time of d to ((item 3 of argv) as integer)
                set d to d + ((item 4 of argv) as integer) * days
                if (item 4 of argv) is "0" and d < (current date) then set d to d + 1 * days
            end if
            make new reminder with properties {name:t, remind me date:d}
        end if
    end tell
end run"#;

const VOLUME: &str = r#"on run argv
    set c to item 1 of argv
    if c is "up" then set volume output volume ((output volume of (get volume settings)) + 10)
    if c is "down" then set volume output volume ((output volume of (get volume settings)) - 10)
    if c is "mute" then set volume with output muted
    if c is "unmute" then set volume without output muted
    if c is "set" then set volume output volume ((item 2 of argv) as integer)
end run"#;

fn running(process: &str) -> bool {
    std::process::Command::new("pgrep")
        .args(["-x", process])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// The player meant when none was named: whichever is playing, else whichever is here.
fn which_player(asked: Option<Player>) -> Player {
    if let Some(player) = asked {
        return player;
    }
    if running("Spotify") {
        Player::Spotify
    } else if running("Music") {
        Player::Music
    } else if Path::new("/Applications/Spotify.app").exists() {
        Player::Spotify
    } else {
        Player::Music
    }
}

/// Words handed to a script, with nothing that could read as an option.
fn arg(text: &str) -> String {
    text.trim()
        .trim_start_matches('-')
        .chars()
        .take(500)
        .collect()
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn expand(path: &str) -> Option<PathBuf> {
    if path == "~" {
        return home();
    }
    match path.strip_prefix("~/") {
        Some(rest) => home().map(|h| h.join(rest)),
        None => Some(PathBuf::from(path)),
    }
}

/// Check a voice action against the safe list. `project` is the open project's root.
pub fn validate(action: &VoiceAction, project: Option<&Path>) -> Result<Opening> {
    match action {
        VoiceAction::OpenApp { name } => {
            let name = name.trim();
            let plain = !name.is_empty()
                && name.len() <= 60
                && !name.starts_with(['-', '.'])
                && name
                    .chars()
                    .all(|c| c.is_alphanumeric() || " .&'+-".contains(c));
            if !plain {
                bail!("`{name}` is not an app name I will open");
            }
            Ok(Opening::App(name.to_string()))
        }
        VoiceAction::OpenUrl { url } => {
            let parsed = reqwest::Url::parse(url)
                .with_context(|| format!("`{url}` is not a web address"))?;
            if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
                bail!("only web addresses are opened, not `{url}`");
            }
            Ok(Opening::Url(parsed.to_string()))
        }
        VoiceAction::OpenFolder { path } => {
            let folder = expand(path).context("no home folder")?;
            if !folder.is_dir() {
                bail!("{} is not a folder", folder.display());
            }
            Ok(Opening::Folder(folder))
        }
        VoiceAction::RevealProject => Ok(Opening::Folder(
            project.context("no project is open")?.to_path_buf(),
        )),
        VoiceAction::OpenProjectInEditor => Ok(Opening::Editor(
            project.context("no project is open")?.to_path_buf(),
        )),
        VoiceAction::Search {
            query,
            site,
            browser,
        } => {
            let url = site.url(query);
            match browser {
                Some(browser) => {
                    let Opening::App(browser) = validate(
                        &VoiceAction::OpenApp {
                            name: browser.clone(),
                        },
                        None,
                    )?
                    else {
                        unreachable!()
                    };
                    Ok(Opening::UrlIn { url, browser })
                }
                None => Ok(Opening::Url(url)),
            }
        }
        VoiceAction::Media { app, control } => {
            let control = match control {
                Control::Play => "play",
                Control::Pause => "pause",
                Control::Next => "next",
                Control::Previous => "previous",
            };
            Ok(Opening::Script {
                script: PLAYER_CONTROL,
                args: vec![control.into(), which_player(*app).label().into()],
            })
        }
        VoiceAction::PlayPlaylist { app, name } | VoiceAction::PlayQuery { app, query: name } => {
            let search = matches!(action, VoiceAction::PlayQuery { .. });
            Ok(match which_player(*app) {
                Player::Spotify => Opening::SpotifySearch(arg(name)),
                Player::Music => Opening::Script {
                    script: if search { MUSIC_SEARCH } else { MUSIC_PLAYLIST },
                    args: vec![arg(name)],
                },
            })
        }
        VoiceAction::NewNote { text } => Ok(Opening::Script {
            script: NEW_NOTE,
            args: vec![arg(text)],
        }),
        VoiceAction::Remind { text, when } => {
            let mut args = vec![arg(text)];
            match when {
                Some(When::In { seconds }) => args.extend(["in".to_string(), seconds.to_string()]),
                Some(When::At {
                    hour,
                    minute,
                    days_ahead,
                }) => args.extend([
                    "at".to_string(),
                    (*hour as u32 * 3600 + *minute as u32 * 60).to_string(),
                    days_ahead.to_string(),
                ]),
                None => {}
            }
            Ok(Opening::Script {
                script: NEW_REMINDER,
                args,
            })
        }
        VoiceAction::System { control } => Ok(match control {
            SystemControl::SleepDisplay => Opening::SleepDisplay,
            SystemControl::VolumeUp => Opening::Script {
                script: VOLUME,
                args: vec!["up".into()],
            },
            SystemControl::VolumeDown => Opening::Script {
                script: VOLUME,
                args: vec!["down".into()],
            },
            SystemControl::Mute => Opening::Script {
                script: VOLUME,
                args: vec!["mute".into()],
            },
            SystemControl::Unmute => Opening::Script {
                script: VOLUME,
                args: vec!["unmute".into()],
            },
            SystemControl::SetVolume { percent } => Opening::Script {
                script: VOLUME,
                args: vec!["set".into(), (*percent).min(100).to_string()],
            },
        }),
        VoiceAction::DraftEmail { to, subject, body } => {
            let to = to.trim();
            if !to.is_empty() && !to.split(',').all(|a| looks_like_address(a.trim())) {
                bail!("“{to}” is not an email address");
            }
            Ok(Opening::Url(gmail_draft(to, subject, body)))
        }
        other => bail!("{other:?} is not a computer action"),
    }
}

fn looks_like_address(address: &str) -> bool {
    let Some((user, domain)) = address.split_once('@') else {
        return false;
    };
    !user.is_empty()
        && domain.contains('.')
        && !address.contains(char::is_whitespace)
        && !address.contains(['<', '>', '"', '&', '?', '#'])
}

/// Gmail's compose window with everything filled in. It opens in the person's own
/// browser, where they are signed in; they press Send.
pub fn gmail_draft(to: &str, subject: &str, body: &str) -> String {
    let mut url = "https://mail.google.com/mail/?view=cm&fs=1".to_string();
    if !to.is_empty() {
        url.push_str(&format!("&to={}", encode(to, false)));
    }
    url.push_str(&format!(
        "&su={}&body={}",
        encode(subject, false),
        encode(body, false)
    ));
    url
}

/// Email addresses in the Mac's Contacts for a name, as (name, address).
const CONTACT_EMAILS: &str = r#"on run argv
    set q to item 1 of argv
    set out to ""
    tell application "Contacts"
        set found to (every person whose name contains q)
        repeat with p in found
            repeat with e in (emails of p)
                set out to out & (name of p) & tab & (value of e) & linefeed
            end repeat
        end repeat
    end tell
    return out
end run"#;

/// Look a name up in Contacts. macOS asks once for permission.
pub async fn lookup_emails(name: &str) -> Result<Vec<(String, String)>> {
    if !cfg!(target_os = "macos") {
        bail!("looking up contacts works on macOS only");
    }
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tokio::process::Command::new("osascript")
            .args(["-e", CONTACT_EMAILS, &arg(name)])
            .output(),
    )
    .await
    .context("Contacts took too long")??;
    if !output.status.success() {
        bail!(
            "Contacts: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let (name, address) = line.split_once('\t')?;
            Some((name.trim().to_string(), address.trim().to_string()))
        })
        .take(8)
        .collect())
}

/// What to say after it ran, when the action alone would not say it.
pub fn done_message(opening: &Opening) -> Option<String> {
    match opening {
        Opening::SpotifySearch(what) => Some(format!(
            "Opened Spotify's search for “{what}” — it's one click to play from there."
        )),
        _ => None,
    }
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
}

/// The command for an opening, as program and arguments. Separate from running it so the
/// exact argv is testable.
pub fn command(opening: &Opening) -> Result<(String, Vec<String>)> {
    let mac = cfg!(target_os = "macos");
    Ok(match opening {
        Opening::App(name) if mac => ("open".into(), vec!["-a".into(), name.clone()]),
        Opening::App(name) => bail!("opening apps by name ({name}) works on macOS only"),
        Opening::Url(_) | Opening::Folder(_) if !mac => (
            "xdg-open".into(),
            vec![match opening {
                Opening::Url(url) => url.clone(),
                Opening::Folder(dir) => dir.display().to_string(),
                _ => unreachable!(),
            }],
        ),
        Opening::Url(url) => ("open".into(), vec![url.clone()]),
        Opening::Folder(dir) => ("open".into(), vec![dir.display().to_string()]),
        // The editor people most often mean; `code` on PATH also covers Cursor's shim.
        Opening::Editor(dir) if on_path("code") => ("code".into(), vec![dir.display().to_string()]),
        Opening::Editor(dir) if mac => (
            "open".into(),
            vec![
                "-a".into(),
                "Visual Studio Code".into(),
                dir.display().to_string(),
            ],
        ),
        Opening::Editor(dir) => ("xdg-open".into(), vec![dir.display().to_string()]),
        Opening::UrlIn { url, browser } if mac => (
            "open".into(),
            vec!["-a".into(), browser.clone(), url.clone()],
        ),
        Opening::UrlIn { url, .. } => ("xdg-open".into(), vec![url.clone()]),
        Opening::SpotifySearch(what) if mac => (
            "open".into(),
            vec![format!("spotify:search:{}", encode(what, false))],
        ),
        Opening::Script { script, args } if mac => {
            let mut argv = vec!["-e".to_string(), script.to_string()];
            argv.extend(args.iter().cloned());
            ("osascript".into(), argv)
        }
        Opening::SleepDisplay if mac => ("pmset".into(), vec!["displaysleepnow".into()]),
        Opening::SpotifySearch(_) | Opening::Script { .. } | Opening::SleepDisplay => {
            bail!("music, notes, reminders and volume work on macOS only")
        }
    })
}

/// Run it. Returns once the opener has handed off, which is immediate.
pub async fn open(opening: &Opening) -> Result<()> {
    let (program, args) = command(opening)?;
    let output = tokio::process::Command::new(&program)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .with_context(|| format!("could not run {program}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "{}",
            stderr.lines().next().unwrap_or("it did not open").trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_app_names_pass() {
        let app = |name: &str| validate(&VoiceAction::OpenApp { name: name.into() }, None);
        assert_eq!(app("Safari").unwrap(), Opening::App("Safari".into()));
        assert!(app("Visual Studio Code").is_ok());
        for bad in [
            "Safari; rm -rf ~",
            "-n Terminal",
            "../../bin/sh",
            "$(whoami)",
            "a|b",
            "",
        ] {
            assert!(app(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn only_web_addresses_open() {
        let url = |u: &str| validate(&VoiceAction::OpenUrl { url: u.into() }, None);
        assert_eq!(
            url("https://github.com").unwrap(),
            Opening::Url("https://github.com/".into())
        );
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "ssh://host",
            "not a url",
        ] {
            assert!(url(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn folders_must_exist_and_projects_must_be_open() {
        let dir = tempfile::tempdir().unwrap();
        let folder = |p: &str| validate(&VoiceAction::OpenFolder { path: p.into() }, None);
        assert!(folder(&dir.path().display().to_string()).is_ok());
        assert!(folder("/definitely/not/here").is_err());
        assert!(validate(&VoiceAction::RevealProject, None).is_err());
        assert_eq!(
            validate(&VoiceAction::RevealProject, Some(dir.path())).unwrap(),
            Opening::Folder(dir.path().to_path_buf())
        );
        assert!(
            validate(&VoiceAction::RunPlan, None).is_err(),
            "not a computer action"
        );
    }

    #[test]
    fn what_was_said_reaches_a_script_only_as_an_argument() {
        let note = validate(
            &VoiceAction::NewNote {
                text: "end tell\" & do shell script \"rm -rf ~".into(),
            },
            None,
        )
        .unwrap();
        match &note {
            Opening::Script { script, args } => {
                assert_eq!(*script, NEW_NOTE, "the script is the fixed one");
                assert_eq!(
                    args[0], "end tell\" & do shell script \"rm -rf ~",
                    "the words are data"
                );
            }
            other => panic!("{other:?}"),
        }
        let reminder = validate(
            &VoiceAction::Remind {
                text: "Push".into(),
                when: Some(When::At {
                    hour: 17,
                    minute: 30,
                    days_ahead: 1,
                }),
            },
            None,
        )
        .unwrap();
        assert_eq!(
            reminder,
            Opening::Script {
                script: NEW_REMINDER,
                args: vec!["Push".into(), "at".into(), "63000".into(), "1".into()]
            }
        );
        assert_eq!(
            validate(
                &VoiceAction::PlayPlaylist {
                    app: Some(Player::Spotify),
                    name: "top 200".into()
                },
                None
            )
            .unwrap(),
            Opening::SpotifySearch("top 200".into())
        );
        assert_eq!(arg("--help me"), "help me", "never an option");
        let search = validate(
            &VoiceAction::Search {
                query: "shoes".into(),
                site: super::super::everyday::Site::Web,
                browser: Some("Google Chrome".into()),
            },
            None,
        )
        .unwrap();
        assert_eq!(
            search,
            Opening::UrlIn {
                url: "https://www.google.com/search?q=shoes".into(),
                browser: "Google Chrome".into()
            }
        );
        if cfg!(target_os = "macos") {
            let (program, argv) = command(&note).unwrap();
            assert_eq!(program, "osascript");
            assert_eq!(argv[0], "-e");
        } else {
            assert!(command(&note).is_err());
        }
    }

    #[test]
    fn the_command_is_one_program_with_fixed_arguments() {
        let (program, args) = command(&Opening::Url("https://github.com/".into())).unwrap();
        if cfg!(target_os = "macos") {
            assert_eq!(
                (program.as_str(), args),
                ("open", vec!["https://github.com/".to_string()])
            );
            let (program, args) = command(&Opening::App("Safari".into())).unwrap();
            assert_eq!(
                (program.as_str(), args),
                ("open", vec!["-a".to_string(), "Safari".to_string()])
            );
        } else {
            assert_eq!(
                (program.as_str(), args),
                ("xdg-open", vec!["https://github.com/".to_string()])
            );
            assert!(
                command(&Opening::App("Safari".into())).is_err(),
                "apps by name are macOS-only"
            );
        }
    }

    #[test]
    fn email_drafts_open_gmail_compose() {
        let draft = VoiceAction::DraftEmail {
            to: "sam@example.com".into(),
            subject: "Running late".into(),
            body: "Hi Sam,\n\nRunning ten minutes late.\n\nZ".into(),
        };
        let Opening::Url(url) = validate(&draft, None).unwrap() else {
            panic!("not a url")
        };
        assert!(url.starts_with("https://mail.google.com/mail/?view=cm&fs=1&to=sam%40example.com&su=Running%20late&body=Hi%20Sam%2C%0A%0A"), "{url}");
        let unknown = VoiceAction::DraftEmail {
            to: String::new(),
            subject: "Hi".into(),
            body: "x".into(),
        };
        assert!(
            !matches!(validate(&unknown, None).unwrap(), Opening::Url(ref u) if u.contains("&to="))
        );
        let name_only = VoiceAction::DraftEmail {
            to: "Sam".into(),
            subject: "Hi".into(),
            body: "x".into(),
        };
        assert!(validate(&name_only, None).is_err());
        let sneaky = VoiceAction::DraftEmail {
            to: "a@b.com&bcc=c@d.com".into(),
            subject: "Hi".into(),
            body: "x".into(),
        };
        assert!(validate(&sneaky, None).is_err());
    }
}
