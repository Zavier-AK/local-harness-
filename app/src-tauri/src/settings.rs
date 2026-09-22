//! App-wide preferences from the Settings tab, kept as JSON in the app's data folder.
//!
//! Per-project choices (the fleet, its tools) stay in each project's `roles.toml`; this
//! holds only what applies to the app as a whole.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const FILE: &str = "settings.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The head agent's model when a project is opened without choosing one. `None`
    /// leaves it to the Claude CLI's own default.
    pub default_model: Option<String>,
    /// The head agent's turn limit — the runaway-loop rail for a whole conversation.
    pub max_turns: u32,
    /// Notify when a worker finishes or a merge waits, while the window is in the back.
    pub notifications: bool,
    /// Accent colour as `#rrggbb`. `None` keeps the theme's own.
    pub accent: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            default_model: None,
            max_turns: 200,
            notifications: true,
            accent: None,
        }
    }
}

impl Settings {
    /// Normalize, and refuse what would break a session rather than store it.
    pub fn validated(mut self) -> Result<Self, String> {
        self.default_model = self
            .default_model
            .map(|model| model.trim().to_string())
            .filter(|model| !model.is_empty());
        if let Some(model) = &self.default_model {
            // Never starting with `-`: it is passed as the value of `--model`, and must not
            // be mistaken for a flag of its own.
            let valid = model.len() <= 100
                && !model.starts_with('-')
                && model
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '[' | ']' | '/'));
            if !valid {
                return Err(format!("`{model}` does not look like a model name"));
            }
        }
        if !(1..=1000).contains(&self.max_turns) {
            return Err("max turns must be between 1 and 1000".into());
        }
        self.accent = self.accent.filter(|accent| !accent.is_empty());
        if let Some(accent) = &self.accent {
            let valid = accent.len() == 7
                && accent.starts_with('#')
                && accent[1..].chars().all(|c| c.is_ascii_hexdigit());
            if !valid {
                return Err(format!("`{accent}` is not a #rrggbb colour"));
            }
        }
        Ok(self)
    }
}

pub fn path() -> Option<PathBuf> {
    Some(harness_core::extensions::app_data_dir()?.join(FILE))
}

/// A missing or unreadable file means defaults: settings are never a reason not to start.
pub fn load_from(path: &Path) -> Settings {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Settings>(&text).ok())
        .and_then(|settings| settings.validated().ok())
        .unwrap_or_default()
}

pub fn load() -> Settings {
    path().map(|path| load_from(&path)).unwrap_or_default()
}

pub fn save_to(path: &Path, settings: &Settings) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let text = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_or_broken_file_means_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_from(&dir.path().join("none.json")), Settings::default());
        std::fs::write(dir.path().join("bad.json"), "{ not json").unwrap();
        assert_eq!(load_from(&dir.path().join("bad.json")), Settings::default());
    }

    #[test]
    fn settings_round_trip_and_fill_in_missing_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let settings = Settings {
            default_model: Some("opus".into()),
            max_turns: 80,
            notifications: false,
            accent: Some("#e0567a".into()),
        };
        save_to(&path, &settings).unwrap();
        assert_eq!(load_from(&path), settings);

        // A file from an older version, missing newer fields, still loads.
        std::fs::write(&path, r#"{"max_turns": 50}"#).unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.max_turns, 50);
        assert!(loaded.notifications);
    }

    #[test]
    fn values_that_would_break_a_session_are_refused() {
        let bad = [
            Settings { max_turns: 0, ..Default::default() },
            Settings { max_turns: 5000, ..Default::default() },
            Settings { accent: Some("red".into()), ..Default::default() },
            Settings { default_model: Some("--dangerously-skip".into()), ..Default::default() },
            Settings { default_model: Some("has space".into()), ..Default::default() },
        ];
        for settings in bad {
            assert!(settings.clone().validated().is_err(), "{settings:?}");
        }
        let blank = Settings { default_model: Some("  ".into()), ..Default::default() };
        assert_eq!(blank.validated().unwrap().default_model, None);
    }
}
