//! The computer, within a safe list: open an app, a website, a folder, or the project.
//!
//! Nothing here runs a shell. Each action becomes one `open` (macOS) or `xdg-open` call
//! with fixed arguments, after checks that make the argument mean only what it says: an
//! app name of plain characters, an `http(s)` URL, a folder that exists.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

use super::VoiceAction;

/// A computer action, checked and ready to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Opening {
    App(String),
    Url(String),
    Folder(PathBuf),
    /// The project in the person's code editor.
    Editor(PathBuf),
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
        other => bail!("{other:?} is not a computer action"),
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
}
