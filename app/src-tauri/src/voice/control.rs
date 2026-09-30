//! The rest of the harness, by voice: settings by key, skills and MCP servers, the fleet,
//! new projects and the night shift, through the same commands as their buttons, and a
//! plain-words readout of each for the voice agent.
//!
//! Actions whose state lives in the window (which project is in front, the chat box, the
//! preview) go to the window instead; see `super::in_window`.

use harness_core::voice::projects;
use harness_core::voice::speech;
use harness_core::voice::{ExtensionChange, ExtensionKind, VoiceAction};
use tauri::{AppHandle, Emitter, Manager};

use crate::{settings, AppState};

/// Something changed outside the window's own controls; views showing it reload.
pub fn changed(app: &AppHandle, what: &str) {
    let _ = app.emit("harness://changed", what);
}

// ---------------------------------------------------------------- settings by key

/// The settings voice may change, with what each takes.
pub const SETTING_KEYS: &[(&str, &str)] = &[
    (
        "default_model",
        "the head agent's model: a model name, or default",
    ),
    ("max_turns", "the head agent's turn limit, a number"),
    ("notifications", "on or off"),
    (
        "accent",
        "a colour: #rrggbb, pink, blue, green, orange, purple, red, teal, yellow, or default",
    ),
    ("voice.enabled", "on or off (off stops voice)"),
    ("voice.hotkey", "e.g. Alt+Space"),
    ("voice.stt_model", "tiny, base or small"),
    ("voice.confidence", "0.5 to 0.99"),
    ("voice.pause_ms", "milliseconds, 300 to 2000"),
    ("voice.handsfree_idle_secs", "seconds, 5 to 600"),
    ("voice.speak_replies", "on or off"),
    ("voice.speech_engine", "natural or system"),
    (
        "voice.speech_voice",
        "George, Fable, Lewis, Daniel, Emma, Isabella, Michael or Heart",
    ),
    (
        "voice.system_voice",
        "a macOS voice name, or empty for the best British one",
    ),
    ("voice.speech_rate", "0.8 to 1.25"),
    ("voice.agent", "on or off"),
    ("voice.agent_model", "a model name, e.g. haiku"),
    ("voice.laya_first", "on or off"),
    ("voice.browser", "on or off"),
    ("voice.browser_model", "a model name, e.g. sonnet"),
    ("voice.about_me", "text about the person"),
    ("voice.laya_idle_minutes", "minutes, 1 to 240"),
];

fn flag(value: &str) -> Result<bool, String> {
    match value.trim().to_lowercase().as_str() {
        "on" | "true" | "yes" | "enable" | "enabled" | "1" => Ok(true),
        "off" | "false" | "no" | "disable" | "disabled" | "0" => Ok(false),
        other => Err(format!("“{other}” isn't on or off")),
    }
}

fn number<T: std::str::FromStr>(value: &str) -> Result<T, String> {
    value
        .trim()
        .trim_end_matches(['x', '×', 's'])
        .trim()
        .parse()
        .map_err(|_| format!("“{value}” isn't a number"))
}

fn accent(value: &str) -> Result<Option<String>, String> {
    let value = value.trim().to_lowercase();
    let named = match value.as_str() {
        "default" | "none" | "theme" | "" => return Ok(None),
        "pink" => "#ff9ebb",
        "blue" => "#7fb2ff",
        "green" => "#8fd6a8",
        "orange" => "#ffb070",
        "purple" => "#c3a3ff",
        "red" => "#ff8a94",
        "teal" => "#6fd6cf",
        "yellow" => "#f3d67c",
        hex if hex.len() == 7
            && hex.starts_with('#')
            && hex[1..].chars().all(|c| c.is_ascii_hexdigit()) =>
        {
            hex
        }
        other => return Err(format!("“{other}” isn't a colour I know")),
    };
    Ok(Some(named.to_string()))
}

/// Change one setting in place. Range checks are `Settings::validated`'s, on save.
pub fn apply_setting(s: &mut settings::Settings, key: &str, value: &str) -> Result<(), String> {
    let v = &mut s.voice;
    let text = value.trim().to_string();
    match key.trim().to_lowercase().as_str() {
        "default_model" => {
            s.default_model = match text.to_lowercase().as_str() {
                "default" | "none" | "" => None,
                _ => Some(text),
            }
        }
        "max_turns" => s.max_turns = number(value)?,
        "notifications" => s.notifications = flag(value)?,
        "accent" => s.accent = accent(value)?,
        "voice.enabled" => v.enabled = flag(value)?,
        "voice.hotkey" => v.hotkey = text,
        "voice.stt_model" => {
            let size = text.to_lowercase();
            let size = if size.ends_with(".en") {
                size
            } else {
                format!("{size}.en")
            };
            v.stt_model = serde_json::from_value(serde_json::Value::String(size))
                .map_err(|_| "the speech model is tiny, base or small".to_string())?;
        }
        "voice.confidence" => v.confidence = number(value)?,
        "voice.pause_ms" => {
            let n: f64 = number(value)?;
            // "one second" arrives as 1.
            v.pause_ms = if n < 10.0 {
                (n * 1000.0) as u32
            } else {
                n as u32
            };
        }
        "voice.handsfree_idle_secs" => v.handsfree_idle_secs = number(value)?,
        "voice.speak_replies" => v.speak_replies = flag(value)?,
        "voice.speech_engine" => {
            v.speech_engine = match text.to_lowercase().as_str() {
                "natural" | "kokoro" => "natural".into(),
                "system" | "macos" | "mac" => "system".into(),
                other => return Err(format!("“{other}” isn't natural or system")),
            }
        }
        "voice.speech_voice" => {
            let wanted = text.to_lowercase();
            let found = speech::VOICES
                .iter()
                .find(|(id, name, _)| *id == wanted || name.to_lowercase() == wanted)
                .ok_or_else(|| format!("there's no voice called “{text}”"))?;
            v.speech_voice = found.0.to_string();
            v.speech_engine = "natural".into();
        }
        "voice.system_voice" => v.system_voice = text,
        "voice.speech_rate" => v.speech_rate = number(value)?,
        "voice.agent" => v.agent = flag(value)?,
        "voice.agent_model" => v.agent_model = text,
        "voice.laya_first" => v.laya_first = flag(value)?,
        "voice.browser" => v.browser = flag(value)?,
        "voice.browser_model" => v.browser_model = text,
        "voice.about_me" => v.about_me = text,
        "voice.laya_idle_minutes" => v.laya_idle_minutes = number(value)?,
        other => return Err(format!("there's no setting called “{other}”")),
    }
    Ok(())
}

fn setting_value(s: &settings::Settings, key: &str) -> String {
    let v = &s.voice;
    let on = |b: bool| if b { "on" } else { "off" }.to_string();
    match key {
        "default_model" => s.default_model.clone().unwrap_or_else(|| "default".into()),
        "max_turns" => s.max_turns.to_string(),
        "notifications" => on(s.notifications),
        "accent" => s.accent.clone().unwrap_or_else(|| "default".into()),
        "voice.enabled" => on(v.enabled),
        "voice.hotkey" => v.hotkey.clone(),
        "voice.stt_model" => v.stt_model.as_str().trim_end_matches(".en").to_string(),
        "voice.confidence" => format!("{:.2}", v.confidence),
        "voice.pause_ms" => v.pause_ms.to_string(),
        "voice.handsfree_idle_secs" => v.handsfree_idle_secs.to_string(),
        "voice.speak_replies" => on(v.speak_replies),
        "voice.speech_engine" => v.speech_engine.clone(),
        "voice.speech_voice" => v.speech_voice.clone(),
        "voice.system_voice" => {
            if v.system_voice.is_empty() {
                "(best British)".into()
            } else {
                v.system_voice.clone()
            }
        }
        "voice.speech_rate" => format!("{:.2}", v.speech_rate),
        "voice.agent" => on(v.agent),
        "voice.agent_model" => v.agent_model.clone(),
        "voice.laya_first" => on(v.laya_first),
        "voice.browser" => on(v.browser),
        "voice.browser_model" => v.browser_model.clone(),
        "voice.about_me" => {
            if v.about_me.is_empty() {
                "(empty)".into()
            } else {
                format!("“{}”", v.about_me.chars().take(80).collect::<String>())
            }
        }
        "voice.laya_idle_minutes" => v.laya_idle_minutes.to_string(),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------- readouts

/// Part of the harness, in plain words, for the voice agent.
pub async fn inventory(app: &AppHandle, topic: &str) -> String {
    let state = app.state::<AppState>();
    match topic.trim() {
        "skills" | "skill" => match crate::list_skills() {
            Ok(skills) if skills.is_empty() => "No skills yet.".into(),
            Ok(skills) => format!(
                "Skills: {}",
                skills
                    .iter()
                    .map(|s| format!("{} ({})", s.name, if s.enabled { "on" } else { "off" }))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Err(e) => format!("Could not list skills: {e}"),
        },
        "mcp" | "mcp servers" | "servers" => match crate::list_mcp_servers() {
            Ok(servers) if servers.is_empty() => "No MCP servers yet.".into(),
            Ok(servers) => format!(
                "MCP servers: {}",
                servers.keys().cloned().collect::<Vec<_>>().join(", ")
            ),
            Err(e) => format!("Could not list MCP servers: {e}"),
        },
        "roles" | "fleet" | "models" => match crate::session_roles(state, None).await {
            Ok(roles) => format!(
                "Roles: {}",
                roles
                    .iter()
                    .map(|r| {
                        format!(
                            "{} on {} {}{}",
                            r.name,
                            r.provider,
                            r.model.as_deref().unwrap_or("(its default model)"),
                            if r.available { "" } else { " (not available)" }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            Err(e) => format!("No project is open, so no roles: {e}"),
        },
        "settings" | "setting" => {
            let s = settings::load();
            format!(
                "Settings (key = value — what it takes): {}",
                SETTING_KEYS
                    .iter()
                    .map(|(key, takes)| format!("{key} = {} — {takes}", setting_value(&s, key)))
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        }
        "projects" | "project" => {
            let (snapshot, _, _) = super::snapshot(&state).await;
            if snapshot.projects.is_empty() {
                "No projects are open.".into()
            } else {
                format!(
                    "Open projects: {}",
                    snapshot
                        .projects
                        .iter()
                        .map(|p| {
                            let front = snapshot.active_project.as_ref() == Some(&p.root);
                            format!(
                                "{} at {}{}",
                                p.name,
                                p.root,
                                if front { " (in front)" } else { "" }
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("; ")
                )
            }
        }
        _ => "Topics: skills, mcp, roles, settings, projects.".into(),
    }
}

// ---------------------------------------------------------------- doing it

fn home() -> Result<std::path::PathBuf, String> {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| "no home folder".to_string())
}

/// A folder ready to open as a project: it exists, and has a fleet.
pub async fn prepare_project(path: &str) -> Result<String, String> {
    let home = home()?;
    let root = projects::expand(path, &home);
    projects::inside_home(&root, &home).map_err(|e| format!("{e:#}"))?;
    if !root.is_dir() {
        return Err(format!("{} isn't a folder", root.display()));
    }
    let root = root.display().to_string();
    if !std::path::Path::new(&root).join("roles.toml").is_file() {
        crate::write_default_roles(root.clone()).await?;
    }
    Ok(root)
}

/// A new folder with a git repository and the default fleet.
pub async fn make_project(
    app: &AppHandle,
    name: &str,
    parent: Option<&str>,
) -> Result<String, String> {
    let (snapshot, _, _) = super::snapshot(&app.state::<AppState>()).await;
    let active = snapshot.active_project.as_deref().map(std::path::Path::new);
    let path = projects::new_project_folder(name, parent, active, &home()?)
        .map_err(|e| format!("{e:#}"))?;
    std::fs::create_dir_all(&path).map_err(|e| format!("making {}: {e}", path.display()))?;
    let git = tokio::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&path)
        .output()
        .await;
    match git {
        Ok(out) if out.status.success() => {}
        Ok(out) => tracing::warn!(
            "git init in {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(e) => tracing::warn!("git init in {}: {e} — is git installed?", path.display()),
    }
    let root = path.display().to_string();
    crate::write_default_roles(root.clone()).await?;
    Ok(root)
}

/// Harness actions carried out here, through the same commands as their buttons. `None`
/// if the action isn't one of these.
pub async fn run(app: &AppHandle, action: &VoiceAction) -> Option<Result<Option<String>, String>> {
    let state = app.state::<AppState>();
    let result = match action {
        VoiceAction::SetRoleModel {
            role,
            model,
            provider,
        } => set_role_model(app, role, model, provider.clone()).await,
        VoiceAction::Extension { kind, name, change } => {
            let done = match (kind, change) {
                (ExtensionKind::Skill, ExtensionChange::Remove) => {
                    crate::remove_skill(state, name.clone()).await.map(|_| ())
                }
                (ExtensionKind::Skill, change) => crate::set_skill_enabled(
                    state,
                    name.clone(),
                    *change == ExtensionChange::Enable,
                )
                .await
                .map(|_| ()),
                (ExtensionKind::Mcp, ExtensionChange::Remove) => {
                    crate::remove_mcp_server(state, name.clone())
                        .await
                        .map(|_| ())
                }
                (ExtensionKind::Mcp, _) => {
                    Err("MCP servers can be added or removed, but not switched off and on".into())
                }
            };
            changed(app, "tools");
            done.map(|_| None)
        }
        VoiceAction::ImportSkills { url } => {
            let done = crate::import_skills_git(state, url.clone()).await;
            changed(app, "tools");
            done.map(|report| Some(format!("Imported: {}", describe_import(&report))))
        }
        VoiceAction::NewSkill { name, description } => {
            let done = crate::create_skill(state, name.clone(), description.clone()).await;
            changed(app, "tools");
            done.map(|path| Some(format!("Made the skill; its instructions are in {path}")))
        }
        VoiceAction::AddMcpServer {
            name,
            command,
            args,
            url,
        } => {
            let config = match (url, command) {
                (Some(url), _) => serde_json::json!({ "type": "http", "url": url }),
                (None, Some(command)) => serde_json::json!({ "command": command, "args": args }),
                (None, None) => return Some(Err("give a command or a URL".into())),
            };
            let done = crate::set_mcp_server(state, name.clone(), config).await;
            changed(app, "tools");
            done.map(|_| None)
        }
        VoiceAction::SetSetting { key, value } => {
            let mut s = settings::load();
            match apply_setting(&mut s, key, value) {
                Ok(()) => {
                    let saved = crate::save_settings(app.clone(), s);
                    changed(app, "settings");
                    saved.map(|s| Some(format!("{key} is now {}", setting_value(&s, key))))
                }
                Err(e) => Err(e),
            }
        }
        VoiceAction::StartNight {
            goal,
            metric,
            higher_is_better,
            guard,
            role,
        } => start_night(app, goal, metric, *higher_is_better, guard, role.clone()).await,
        _ => return None,
    };
    Some(result)
}

fn describe_import(report: &harness_core::extensions::ImportReport) -> String {
    let mut line = if report.imported.is_empty() {
        "nothing new".to_string()
    } else {
        report.imported.join(", ")
    };
    if !report.skipped.is_empty() {
        line.push_str(&format!(" (skipped {})", report.skipped.len()));
    }
    line
}

async fn set_role_model(
    app: &AppHandle,
    role: &str,
    model: &str,
    provider: Option<String>,
) -> Result<Option<String>, String> {
    let state = app.state::<AppState>();
    let (snapshot, _, _) = super::snapshot(&state).await;
    let root = snapshot.active_project.ok_or("no project is open")?;
    let roles = crate::session_roles(app.state::<AppState>(), None).await?;
    let wanted = role.trim().to_lowercase();
    let found = roles
        .iter()
        .find(|r| r.name.to_lowercase() == wanted)
        .ok_or_else(|| {
            format!(
                "there's no role called “{role}”; the roles are {}",
                roles
                    .iter()
                    .map(|r| r.name.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
    let patch = harness_core::detection::RoleModelPatch {
        role_name: found.name.to_string(),
        model: model.trim().to_string(),
        base_url: None,
        provider,
        provider_opts: Default::default(),
    };
    crate::save_role_assignments(app.state::<AppState>(), root, None, vec![patch]).await?;
    changed(app, "roles");
    Ok(None)
}

async fn start_night(
    app: &AppHandle,
    goal: &str,
    metric: &str,
    higher_is_better: bool,
    guard: &Option<String>,
    role: Option<String>,
) -> Result<Option<String>, String> {
    let role = match role {
        Some(role) => role,
        None => crate::session_roles(app.state::<AppState>(), None)
            .await?
            .into_iter()
            .find(|r| r.can_edit_files)
            .map(|r| r.name.to_string())
            .ok_or("no role can edit files, so the night shift has nobody to make changes")?,
    };
    let config: harness_core::night::NightConfig = serde_json::from_value(serde_json::json!({
        "goal": goal,
        "metric": metric,
        "direction": if higher_is_better { "higher" } else { "lower" },
        "guard": guard,
        "role": role,
    }))
    .map_err(|e| e.to_string())?;
    crate::start_night(app.state::<AppState>(), config, None).await?;
    Ok(Some("The night shift has started.".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_change_by_key_in_plain_words() {
        let mut s = settings::Settings::default();
        apply_setting(&mut s, "notifications", "off").unwrap();
        assert!(!s.notifications);
        apply_setting(&mut s, "voice.speech_rate", "1.1x").unwrap();
        assert!((s.voice.speech_rate - 1.1).abs() < 1e-9);
        apply_setting(&mut s, "voice.speech_voice", "Lewis").unwrap();
        assert_eq!(s.voice.speech_voice, "bm_lewis");
        apply_setting(&mut s, "voice.pause_ms", "1").unwrap();
        assert_eq!(s.voice.pause_ms, 1000);
        apply_setting(&mut s, "voice.stt_model", "small").unwrap();
        assert_eq!(s.voice.stt_model.as_str(), "small.en");
        apply_setting(&mut s, "accent", "teal").unwrap();
        assert_eq!(s.accent.as_deref(), Some("#6fd6cf"));
        apply_setting(&mut s, "default_model", "opus").unwrap();
        assert_eq!(s.default_model.as_deref(), Some("opus"));
        apply_setting(&mut s, "default_model", "default").unwrap();
        assert_eq!(s.default_model, None);
        assert!(apply_setting(&mut s, "notifications", "maybe").is_err());
        assert!(apply_setting(&mut s, "voice.speech_voice", "Daniel Craig").is_err());
        assert!(apply_setting(&mut s, "accent", "url(javascript:x)").is_err());
        assert!(apply_setting(&mut s, "no.such", "1").is_err());
        // Every key listed can be read back.
        for (key, _) in SETTING_KEYS {
            assert!(
                !setting_value(&s, key).is_empty() || *key == "voice.hotkey",
                "{key}"
            );
        }
    }
}
