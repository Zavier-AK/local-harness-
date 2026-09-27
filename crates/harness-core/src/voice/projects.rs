//! Where a project made by voice goes, and what it may be called.
//!
//! "Make a new project called weather app" becomes `~/Projects/weather-app` (or a folder
//! beside the project in front), with a git repository and the default fleet. The name
//! can't reach outside that folder, and an existing folder is never taken over.

use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

/// A folder name from what was said: lower case, words joined by hyphens, and only
/// letters, digits, `-`, `_` and `.`, so it can't be a path.
pub fn folder_name(name: &str) -> Result<String> {
    let mut out = String::new();
    for word in name.split_whitespace() {
        let cleaned: String = word
            .chars()
            .filter(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
            .collect::<String>()
            .to_lowercase();
        if cleaned.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('-');
        }
        out.push_str(&cleaned);
    }
    let out = out.trim_matches(|c| c == '.' || c == '-').to_string();
    if out.is_empty() {
        bail!("“{name}” isn't a name a folder can have");
    }
    if out.len() > 64 {
        bail!("that name is too long for a folder");
    }
    Ok(out)
}

/// `~/…` and plain paths, made absolute.
pub fn expand(path: &str, home: &Path) -> PathBuf {
    let path = path.trim();
    if path == "~" {
        return home.to_path_buf();
    }
    match path.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                path
            } else {
                home.join(path)
            }
        }
    }
}

/// Projects made or opened by voice stay inside the home folder, and aren't the home
/// folder itself.
pub fn inside_home(path: &Path, home: &Path) -> Result<()> {
    let within = path
        .components()
        .all(|c| !matches!(c, std::path::Component::ParentDir))
        && path.starts_with(home)
        && path != home;
    if !within {
        bail!(
            "{} isn't a folder inside {}; by voice, projects stay in your home folder",
            path.display(),
            home.display()
        );
    }
    Ok(())
}

/// Where the new project goes. `parent` if given; otherwise beside the project in front;
/// otherwise `~/Projects`. It must not exist yet.
pub fn new_project_folder(
    name: &str,
    parent: Option<&str>,
    active: Option<&Path>,
    home: &Path,
) -> Result<PathBuf> {
    let folder = folder_name(name)?;
    let parent = match parent {
        Some(parent) => expand(parent, home),
        None => active
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| home.join("Projects")),
    };
    let path = parent.join(&folder);
    inside_home(&path, home)?;
    if path.exists() {
        bail!(
            "{} already exists; say “open the project at …” to open it",
            path.display()
        );
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spoken_names_become_folder_names() {
        assert_eq!(folder_name("Weather App").unwrap(), "weather-app");
        assert_eq!(folder_name("my_site.v2").unwrap(), "my_site.v2");
        assert_eq!(folder_name("../../etc").unwrap(), "etc");
        assert_eq!(folder_name("a/b\\c").unwrap(), "abc");
        assert!(folder_name("   ").is_err());
        assert!(folder_name("..").is_err());
    }

    #[test]
    fn it_goes_beside_the_current_project_or_in_projects() {
        let home = Path::new("/Users/z");
        let beside =
            new_project_folder("Weather", None, Some(Path::new("/Users/z/code/shop")), home)
                .unwrap();
        assert_eq!(beside, PathBuf::from("/Users/z/code/weather"));
        let fresh = new_project_folder("Weather", None, None, home).unwrap();
        assert_eq!(fresh, PathBuf::from("/Users/z/Projects/weather"));
        let chosen = new_project_folder("Weather", Some("~/Desktop"), None, home).unwrap();
        assert_eq!(chosen, PathBuf::from("/Users/z/Desktop/weather"));
    }

    #[test]
    fn projects_stay_in_the_home_folder() {
        let home = Path::new("/Users/z");
        assert!(new_project_folder("x", Some("/etc"), None, home).is_err());
        assert!(new_project_folder("x", Some("~/../other"), None, home).is_err());
        assert!(inside_home(Path::new("/Users/z"), home).is_err());
        assert!(inside_home(Path::new("/Users/z/code/site"), home).is_ok());
    }

    #[test]
    fn an_existing_folder_is_never_taken_over() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("Projects/shop")).unwrap();
        assert!(new_project_folder("shop", None, None, home.path()).is_err());
    }
}
