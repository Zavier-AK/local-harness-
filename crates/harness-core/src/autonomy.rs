//! How much runs without the person: the autonomy slider.
//!
//! Four stops, from most to least supervised. The engine enforces them — a delegation
//! under `Ask` does not start until someone approves it, and a merge lands on its own
//! only when the person chose a level that allows it *and* the change was verified. Nothing
//! here asks the model to behave; it is what the harness will and will not do.
//!
//! Stored per project in `.harness/autonomy.json`, which is git-excluded: the setting is
//! the person's, not the repository's. The worktree hook process reads the same file, so
//! a change applies to native subagents mid-session without restarting anything.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::verify::{CheckStatus, Risk, VerificationReport};

const FILE: &str = ".harness/autonomy.json";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Autonomy {
    /// Every delegation waits for the person's approval; every merge too.
    Ask,
    /// Delegation runs freely; every merge waits. How the harness always behaved.
    #[default]
    Review,
    /// Verified, low-risk changes land by themselves; the rest wait.
    LandSafe,
    /// Verified changes land by themselves unless they are high risk.
    LandMost,
}

impl Autonomy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Review => "review",
            Self::LandSafe => "land_safe",
            Self::LandMost => "land_most",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "ask" => Some(Self::Ask),
            "review" => Some(Self::Review),
            "land_safe" => Some(Self::LandSafe),
            "land_most" => Some(Self::LandMost),
            _ => None,
        }
    }

    /// One line, for the head agent and the dial's tooltip.
    pub fn summary(self) -> &'static str {
        match self {
            Self::Ask => "every delegation waits for the person's approval, and every merge",
            Self::Review => "delegations run freely; every merge waits for the person",
            Self::LandSafe => {
                "delegations run freely; verified low-risk changes land by themselves, the rest wait"
            }
            Self::LandMost => {
                "delegations run freely; verified changes land by themselves unless high risk"
            }
        }
    }

    pub fn delegation_needs_approval(self) -> bool {
        self == Self::Ask
    }

    /// Whether a checked change may land without anyone clicking.
    ///
    /// Never for a change nothing verified, never with a failed or broken check, whatever
    /// the level — auto-landing is trusting the checks, so it needs checks to trust.
    pub fn lands(self, report: &VerificationReport) -> bool {
        let ceiling = match self {
            Self::Ask | Self::Review => return false,
            Self::LandSafe => Risk::Low,
            Self::LandMost => Risk::Medium,
        };
        report.verified
            && report.risk <= ceiling
            && !report
                .checks
                .iter()
                .any(|check| matches!(check.status, CheckStatus::Failed | CheckStatus::Error))
    }
}

fn path(project: &Path) -> PathBuf {
    project.join(FILE)
}

#[derive(Serialize, Deserialize)]
struct Stored {
    level: Autonomy,
}

/// The project's level. Missing or unreadable means `Review` — today's behaviour, and
/// never more autonomy than the person asked for.
pub fn load(project: &Path) -> Autonomy {
    std::fs::read_to_string(path(project))
        .ok()
        .and_then(|text| serde_json::from_str::<Stored>(&text).ok())
        .map(|stored| stored.level)
        .unwrap_or_default()
}

pub fn save(project: &Path, level: Autonomy) -> std::io::Result<()> {
    let target = path(project);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = target.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string(&Stored { level }).unwrap_or_default())?;
    std::fs::rename(&tmp, &target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify::{Check, CheckKind};

    fn report(risk: Risk, verified: bool, failed: Option<CheckStatus>) -> VerificationReport {
        let mut checks = Vec::new();
        if let Some(status) = failed {
            checks.push(Check {
                kind: CheckKind::Command,
                name: "t".into(),
                status,
                summary: String::new(),
                output: None,
                risk: None,
                findings: Vec::new(),
                reviewer: None,
            });
        }
        VerificationReport { risk, verified, reasons: Vec::new(), checks }
    }

    #[test]
    fn only_the_land_levels_ever_land_and_only_up_to_their_ceiling() {
        let cases = [
            (Autonomy::Ask, Risk::Low, false),
            (Autonomy::Review, Risk::Low, false),
            (Autonomy::LandSafe, Risk::Low, true),
            (Autonomy::LandSafe, Risk::Medium, false),
            (Autonomy::LandMost, Risk::Medium, true),
            (Autonomy::LandMost, Risk::High, false),
        ];
        for (level, risk, lands) in cases {
            assert_eq!(level.lands(&report(risk, true, None)), lands, "{level:?} at {risk:?}");
        }
    }

    #[test]
    fn nothing_lands_unverified_or_with_a_broken_check() {
        for level in [Autonomy::LandSafe, Autonomy::LandMost] {
            assert!(!level.lands(&report(Risk::Low, false, None)), "unverified");
            assert!(!level.lands(&report(Risk::Low, true, Some(CheckStatus::Failed))));
            assert!(!level.lands(&report(Risk::Low, true, Some(CheckStatus::Error))));
            assert!(level.lands(&report(Risk::Low, true, Some(CheckStatus::Skipped))));
        }
    }

    #[test]
    fn the_level_is_saved_per_project_and_defaults_to_review() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()), Autonomy::Review);
        save(dir.path(), Autonomy::Ask).unwrap();
        assert_eq!(load(dir.path()), Autonomy::Ask);
        std::fs::write(dir.path().join(FILE), "not json").unwrap();
        assert_eq!(load(dir.path()), Autonomy::Review, "a broken file never grants autonomy");
    }

    #[test]
    fn levels_parse_the_way_people_type_them() {
        assert_eq!(Autonomy::parse("land-safe"), Some(Autonomy::LandSafe));
        assert_eq!(Autonomy::parse(" ASK "), Some(Autonomy::Ask));
        assert_eq!(Autonomy::parse("yolo"), None);
    }
}
