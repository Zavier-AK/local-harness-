//! What the person has said the head agent may always do in a project.
//!
//! The head agent is approved up front only for reading. Anything else it tries (a shell
//! command, a file write) is put to the person in the chat: allow once, always allow, or
//! deny. "Always" rules are kept here, in `.harness/allowed-tools.json` (git-excluded, as
//! the rest of `.harness/`), and passed to `--allowedTools` next time the session starts.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const FILE: &str = ".harness/allowed-tools.json";

fn path(project: &Path) -> PathBuf {
    project.join(FILE)
}

#[derive(Default, Serialize, Deserialize)]
struct Stored {
    rules: Vec<String>,
}

/// Whether `rule` is a tool name or `Tool(content)`: nothing that could smuggle another
/// flag or rule onto the command line. No commas either, since `--allowedTools` gets the
/// rules joined by them.
fn well_formed(rule: &str) -> bool {
    let name_end = rule.find('(').unwrap_or(rule.len());
    let name = &rule[..name_end];
    let rest = &rule[name_end..];
    name.starts_with(|c: char| c.is_ascii_alphabetic())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && (rest.is_empty() || (rest.ends_with(')') && rest.len() >= 2))
        && !rule.contains('\n')
        && !rule.contains(',')
}

/// The project's remembered rules. Missing or unreadable means none.
pub fn load(project: &Path) -> Vec<String> {
    std::fs::read_to_string(path(project))
        .ok()
        .and_then(|text| serde_json::from_str::<Stored>(&text).ok())
        .map(|stored| stored.rules)
        .unwrap_or_default()
        .into_iter()
        .filter(|rule| well_formed(rule))
        .collect()
}

/// Add `rules`, keeping those already there. Returns the full list.
pub fn remember(project: &Path, rules: &[String]) -> std::io::Result<Vec<String>> {
    let mut all = load(project);
    for rule in rules {
        if well_formed(rule) && !all.contains(rule) {
            all.push(rule.clone());
        }
    }
    let target = path(project);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = target.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(
        &tmp,
        serde_json::to_string_pretty(&Stored { rules: all.clone() }).unwrap_or_default(),
    )?;
    std::fs::rename(&tmp, &target)?;
    Ok(all)
}

/// Forget every remembered rule for the project.
pub fn forget_all(project: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path(project)) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_are_remembered_once_and_survive_a_reload() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).is_empty());
        remember(dir.path(), &["Bash(git add:*)".into()]).unwrap();
        let all = remember(dir.path(), &["Bash(git add:*)".into(), "Write".into()]).unwrap();
        assert_eq!(all, vec!["Bash(git add:*)".to_string(), "Write".into()]);
        assert_eq!(load(dir.path()), all);
        forget_all(dir.path()).unwrap();
        assert!(load(dir.path()).is_empty());
        forget_all(dir.path()).unwrap();
    }

    #[test]
    fn odd_rules_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let all = remember(
            dir.path(),
            &[
                "--dangerously-skip-permissions".into(),
                "Bash(ok)".into(),
                "Bash(".into(),
                "Bash(a)\nWrite".into(),
                "Bash(echo a,b)".into(),
                "".into(),
            ],
        )
        .unwrap();
        assert_eq!(all, vec!["Bash(ok)".to_string()]);
    }
}
