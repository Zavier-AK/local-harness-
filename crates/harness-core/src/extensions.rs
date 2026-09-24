//! Skills and MCP servers the user adds to the fleet, and how they reach Claude.
//!
//! Skills travel as a Claude Code plugin passed with `--plugin-dir`, so nothing is written
//! into the user's repository or into `~/.claude`. The plugin is named `harness`, which
//! makes every skill show up as `harness:<name>` next to whatever the user already has.
//!
//! The plugin directory is content-addressed: a change to the library builds a new
//! directory rather than rewriting the one a running worker may be reading from. Workers
//! pick up the new one on their next delegation; a head agent on its next start.
//!
//! On disk, under the directory handed to [`Extensions::new`]:
//!
//! ```text
//! extensions.json         which skills are off, where each came from, MCP servers
//! skills/<name>/SKILL.md  the user's library — anything a skill folder holds comes along
//! plugins/<hash>/         built plugins; only the current one is kept past startup
//! ```

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// The plugin's name, and so the namespace every harness skill appears under.
pub const PLUGIN_NAME: &str = "harness";

/// The desktop app's bundle identifier, and so its data folder's name.
const APP_IDENTIFIER: &str = "dev.harness.app";

/// Where the app keeps extensions — inside [`app_data_dir`], so the CLI and the desktop
/// app share one library.
pub fn default_dir() -> Option<PathBuf> {
    Some(app_data_dir()?.join("extensions"))
}

/// The desktop app's data folder: the same one Tauri calls the app data dir.
pub fn app_data_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let data = if cfg!(target_os = "macos") {
        home?.join("Library/Application Support")
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| home.map(|home| home.join(".local/share")))?
    };
    Some(data.join(APP_IDENTIFIER))
}

/// Reserved: the harness's own MCP server, which carries delegation.
const RESERVED_SERVER: &str = "harness";

/// Skills every install has, compiled in so they exist wherever the app is installed.
const BUNDLED: &[(&str, &str)] = &[
    (
        "worktree",
        include_str!("../../../skills/worktree/SKILL.md"),
    ),
    ("review", include_str!("../../../skills/review/SKILL.md")),
    (
        "risky-changes",
        include_str!("../../../skills/risky-changes/SKILL.md"),
    ),
];

/// How deep an import looks for skill folders. Deep enough for a repository that groups
/// skills by category (`agent-orchestration/git-worktree/SKILL.md`), shallow enough that
/// picking the wrong folder does not walk someone's home directory.
const IMPORT_DEPTH: usize = 4;

/// A skill folder is instructions and a few scripts, not a project. Anything bigger is
/// almost certainly the wrong folder.
const MAX_SKILL_FILES: usize = 200;
const MAX_SKILL_BYTES: u64 = 5 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillSource {
    Bundled,
    Library,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub source: SkillSource,
    pub enabled: bool,
    /// Where an imported skill came from, for credit and for re-importing.
    pub origin: Option<String>,
    /// The library folder, so the user can open and edit it. Bundled skills have none.
    pub path: Option<PathBuf>,
}

#[derive(Debug, Default, Serialize)]
pub struct ImportReport {
    pub imported: Vec<String>,
    /// Skill folders that were found and not taken, with the reason.
    pub skipped: Vec<(String, String)>,
}

/// What a Claude process needs from the extensions: the plugin to load and the servers
/// to connect to. Carried on the harness so every worker gets the current set.
#[derive(Debug, Clone, Default)]
pub struct WorkerExtras {
    pub plugin_dir: Option<PathBuf>,
    pub mcp_servers: BTreeMap<String, serde_json::Value>,
}

impl WorkerExtras {
    /// `--mcp-config` JSON for the user's servers, or `None` when there are none.
    pub fn mcp_config(&self) -> Option<String> {
        if self.mcp_servers.is_empty() {
            return None;
        }
        Some(serde_json::json!({ "mcpServers": self.mcp_servers }).to_string())
    }

    /// Arguments for a Claude process. The head agent merges the servers into its own
    /// config instead, since it already passes one for delegation.
    pub fn claude_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(dir) = &self.plugin_dir {
            args.push("--plugin-dir".into());
            args.push(dir.to_string_lossy().into_owned());
        }
        if let Some(config) = self.mcp_config() {
            args.push("--mcp-config".into());
            args.push(config);
        }
        args
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    disabled_skills: BTreeSet<String>,
    #[serde(default)]
    origins: BTreeMap<String, String>,
    #[serde(default)]
    mcp_servers: BTreeMap<String, serde_json::Value>,
}

pub struct Extensions {
    dir: PathBuf,
}

impl Extensions {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn library(&self) -> PathBuf {
        self.dir.join("skills")
    }

    fn state_path(&self) -> PathBuf {
        self.dir.join("extensions.json")
    }

    fn load(&self) -> Result<State> {
        match std::fs::read_to_string(self.state_path()) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("reading {}", self.state_path().display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.state_path().display())),
        }
    }

    /// Written whole to a temporary file and renamed, so a crash cannot leave half a file.
    fn save(&self, state: &State) -> Result<()> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        let tmp = self
            .dir
            .join(format!("extensions.json.{}.tmp", std::process::id()));
        std::fs::write(&tmp, serde_json::to_string_pretty(state)?)?;
        std::fs::rename(&tmp, self.state_path())?;
        Ok(())
    }

    // ---------------------------------------------------------------- skills

    pub fn skills(&self) -> Result<Vec<SkillInfo>> {
        let state = self.load()?;
        let mut skills: Vec<SkillInfo> = BUNDLED
            .iter()
            .map(|(name, body)| SkillInfo {
                name: name.to_string(),
                description: frontmatter(body).1.unwrap_or_default(),
                source: SkillSource::Bundled,
                enabled: !state.disabled_skills.contains(*name),
                origin: None,
                path: None,
            })
            .collect();

        for (name, path) in self.library_skills()? {
            let body = std::fs::read_to_string(path.join("SKILL.md")).unwrap_or_default();
            skills.push(SkillInfo {
                description: frontmatter(&body).1.unwrap_or_default(),
                source: SkillSource::Library,
                enabled: !state.disabled_skills.contains(&name),
                origin: state.origins.get(&name).cloned(),
                path: Some(path),
                name,
            });
        }
        Ok(skills)
    }

    fn library_skills(&self) -> Result<Vec<(String, PathBuf)>> {
        let mut found = Vec::new();
        let entries = match std::fs::read_dir(self.library()) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(found),
            Err(e) => return Err(e.into()),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if path.join("SKILL.md").is_file() && check_skill_name(&name).is_ok() {
                found.push((name, path));
            }
        }
        found.sort();
        Ok(found)
    }

    pub fn set_skill_enabled(&self, name: &str, enabled: bool) -> Result<()> {
        if !self.skills()?.iter().any(|skill| skill.name == name) {
            bail!("there is no skill called `{name}`");
        }
        let mut state = self.load()?;
        if enabled {
            state.disabled_skills.remove(name);
        } else {
            state.disabled_skills.insert(name.to_string());
        }
        self.save(&state)
    }

    /// A blank skill in the library, ready to be written. Returns its folder.
    pub fn create_skill(&self, name: &str, description: &str) -> Result<PathBuf> {
        check_skill_name(name)?;
        if is_bundled(name) || self.library().join(name).exists() {
            bail!("a skill called `{name}` already exists");
        }
        let description = description.trim();
        if description.is_empty() {
            bail!("a skill needs a description — it is how the agent decides to use it");
        }
        let dir = self.library().join(name);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(
            dir.join("SKILL.md"),
            format!(
                "---\nname: {name}\ndescription: {}\n---\n\n# {name}\n\nWrite the instructions here.\n",
                yaml_line(description)
            ),
        )?;
        Ok(dir)
    }

    /// Copy skills in from a folder: either one skill (a folder with a `SKILL.md`) or a
    /// folder of them, such as a cloned skills repository.
    ///
    /// A skill that is already in the library is replaced, which is how an update from
    /// the same source is pulled in. One that would shadow a bundled skill is skipped.
    pub fn import_skills(&self, source: &Path, origin: Option<&str>) -> Result<ImportReport> {
        let source = source
            .canonicalize()
            .with_context(|| format!("{} does not exist", source.display()))?;
        let mut folders = Vec::new();
        find_skill_folders(&source, IMPORT_DEPTH, &mut folders);
        if folders.is_empty() {
            bail!(
                "no SKILL.md found in {} or the folders beneath it",
                source.display()
            );
        }

        let mut report = ImportReport::default();
        let mut state = self.load()?;
        for folder in folders {
            let dir_name = folder
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let body = std::fs::read_to_string(folder.join("SKILL.md")).unwrap_or_default();
            let (declared, description) = frontmatter(&body);
            let name = declared.unwrap_or(dir_name.clone());

            let refusal = if let Err(e) = check_skill_name(&name) {
                Some(e.to_string())
            } else if is_bundled(&name) {
                Some("has the same name as a built-in skill".to_string())
            } else if description.is_none() {
                Some("has no description, so an agent would never pick it".to_string())
            } else {
                None
            };
            if let Some(reason) = refusal {
                report.skipped.push((dir_name, reason));
                continue;
            }

            let target = self.library().join(&name);
            let staging = self.library().join(format!(".{name}.importing"));
            let _ = std::fs::remove_dir_all(&staging);
            if let Err(e) = copy_skill(&folder, &staging) {
                let _ = std::fs::remove_dir_all(&staging);
                report.skipped.push((name, format!("{e:#}")));
                continue;
            }
            let _ = std::fs::remove_dir_all(&target);
            std::fs::rename(&staging, &target)?;

            match origin {
                Some(origin) => state.origins.insert(name.clone(), origin.to_string()),
                None => state.origins.remove(&name),
            };
            report.imported.push(name);
        }
        self.save(&state)?;
        Ok(report)
    }

    /// Clone a git repository (shallow) and import every skill in it.
    pub async fn import_skills_from_git(&self, url: &str) -> Result<ImportReport> {
        let url = url.trim();
        // `git clone` takes options before the URL; one starting with `-` would be read
        // as an option, and local paths belong to the folder import.
        if !(url.starts_with("https://") || url.starts_with("git@")) {
            bail!("give an https:// or git@ URL to a git repository");
        }
        std::fs::create_dir_all(&self.dir)?;
        let checkout = self
            .dir
            .join(format!(".clone-{}", uuid::Uuid::new_v4().simple()));
        let output = tokio::process::Command::new("git")
            .args(["clone", "--depth", "1", "--quiet", "--", url])
            .arg(&checkout)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .await
            .context("running git")?;
        let result = if output.status.success() {
            self.import_skills(&checkout, Some(url))
        } else {
            Err(anyhow::anyhow!(
                "git clone failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        };
        let _ = std::fs::remove_dir_all(&checkout);
        result
    }

    /// Delete a library skill. Bundled skills can only be turned off.
    pub fn remove_skill(&self, name: &str) -> Result<()> {
        check_skill_name(name)?;
        if is_bundled(name) {
            bail!("`{name}` is built in; turn it off instead");
        }
        let dir = self.library().join(name);
        if !dir.join("SKILL.md").is_file() {
            bail!("there is no skill called `{name}` in the library");
        }
        std::fs::remove_dir_all(&dir)?;
        let mut state = self.load()?;
        state.disabled_skills.remove(name);
        state.origins.remove(name);
        self.save(&state)
    }

    // ----------------------------------------------------------- MCP servers

    pub fn mcp_servers(&self) -> Result<BTreeMap<String, serde_json::Value>> {
        Ok(self.load()?.mcp_servers)
    }

    /// Add or replace a server, in Claude Code's own `mcpServers` shape: `command` (with
    /// optional `args` and `env`) for a local process, or `url` (with optional `type`
    /// and `headers`) for a remote one.
    pub fn set_mcp_server(&self, name: &str, config: serde_json::Value) -> Result<()> {
        check_server_name(name)?;
        let object = config
            .as_object()
            .context("a server's config must be an object")?;
        let has_command = object
            .get("command")
            .and_then(|v| v.as_str())
            .is_some_and(|c| !c.trim().is_empty());
        let has_url = object
            .get("url")
            .and_then(|v| v.as_str())
            .is_some_and(|u| u.starts_with("https://") || u.starts_with("http://"));
        if has_command == has_url {
            bail!("a server needs either a command to run or an http(s) URL, not both");
        }
        if let Some(args) = object.get("args") {
            if !args
                .as_array()
                .is_some_and(|a| a.iter().all(|v| v.is_string()))
            {
                bail!("`args` must be a list of strings");
            }
        }
        let mut state = self.load()?;
        state.mcp_servers.insert(name.to_string(), config);
        self.save(&state)
    }

    pub fn remove_mcp_server(&self, name: &str) -> Result<()> {
        let mut state = self.load()?;
        if state.mcp_servers.remove(name).is_none() {
            bail!("there is no MCP server called `{name}`");
        }
        self.save(&state)
    }

    // --------------------------------------------------------------- plugin

    /// Build (or reuse) the plugin holding every enabled skill, and return what a Claude
    /// process needs. No enabled skills means no plugin at all.
    pub fn worker_extras(&self) -> Result<WorkerExtras> {
        let state = self.load()?;
        Ok(WorkerExtras {
            plugin_dir: self.build_plugin(&state)?,
            mcp_servers: state.mcp_servers,
        })
    }

    fn build_plugin(&self, state: &State) -> Result<Option<PathBuf>> {
        let mut files: Vec<(String, PathBuf, Vec<u8>)> = Vec::new();
        for (name, body) in BUNDLED {
            if !state.disabled_skills.contains(*name) {
                files.push((
                    name.to_string(),
                    PathBuf::from("SKILL.md"),
                    body.as_bytes().to_vec(),
                ));
            }
        }
        for (name, dir) in self.library_skills()? {
            if state.disabled_skills.contains(&name) {
                continue;
            }
            for relative in list_files(&dir)? {
                let bytes = std::fs::read(dir.join(&relative))?;
                files.push((name.clone(), relative, bytes));
            }
        }
        if files.is_empty() {
            return Ok(None);
        }

        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        files.hash(&mut hasher);
        let plugins = self.dir.join("plugins");
        let target = plugins.join(format!("{:016x}", hasher.finish()));
        if target.join(".claude-plugin/plugin.json").is_file() {
            return Ok(Some(target));
        }

        let staging = plugins.join(format!(".building-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(staging.join(".claude-plugin"))?;
        std::fs::write(
            staging.join(".claude-plugin/plugin.json"),
            serde_json::json!({
                "name": PLUGIN_NAME,
                "version": "1.0.0",
                "description": "Skills managed by local-harness."
            })
            .to_string(),
        )?;
        for (name, relative, bytes) in &files {
            let path = staging.join("skills").join(name).join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, bytes)?;
        }
        // Another process may have built the same plugin meanwhile; theirs is identical.
        if std::fs::rename(&staging, &target).is_err() {
            let _ = std::fs::remove_dir_all(&staging);
        }
        Ok(Some(target))
    }

    /// Remove built plugins other than `keep`. Only safe at startup, before any worker
    /// could be reading an older one.
    pub fn prune_plugins(&self, keep: Option<&Path>) {
        let Ok(entries) = std::fs::read_dir(self.dir.join("plugins")) else {
            return;
        };
        for entry in entries.flatten() {
            if Some(entry.path().as_path()) != keep {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
}

fn is_bundled(name: &str) -> bool {
    BUNDLED.iter().any(|(bundled, _)| *bundled == name)
}

/// Claude Code's own rule for skill names: lowercase letters, digits and hyphens.
fn check_skill_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !valid {
        bail!("`{name}` is not a valid skill name — use lowercase letters, digits and hyphens");
    }
    Ok(())
}

fn check_server_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !valid {
        bail!("`{name}` is not a valid server name — use letters, digits, `-` and `_`");
    }
    if name == RESERVED_SERVER {
        bail!("`{RESERVED_SERVER}` is the harness's own server");
    }
    Ok(())
}

/// `name` and `description` from a SKILL.md's frontmatter. Only the two flat keys a
/// skill needs, so no YAML parser: a value may be quoted, and anything else is ignored.
fn frontmatter(body: &str) -> (Option<String>, Option<String>) {
    let mut lines = body.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (None, None);
    }
    let (mut name, mut description) = (None, None);
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value
            .trim()
            .trim_matches(|c| c == '"' || c == '\'')
            .trim()
            .to_string();
        if value.is_empty() {
            continue;
        }
        match key.trim() {
            "name" => name = Some(value),
            "description" => description = Some(value),
            _ => {}
        }
    }
    (name, description)
}

/// A description as one YAML-safe line.
fn yaml_line(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    serde_json::to_string(&flat).unwrap_or(flat)
}

fn find_skill_folders(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    if dir.join("SKILL.md").is_file() {
        found.push(dir.to_path_buf());
        return;
    }
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            !name.starts_with('.') && name != "node_modules" && name != "target"
        })
        .map(|entry| entry.path())
        .collect();
    children.sort();
    for child in children {
        find_skill_folders(&child, depth - 1, found);
    }
}

/// Every regular file under `dir`, relative to it. Symlinks are left out: following one
/// could copy something from outside the skill folder into every Claude session.
fn list_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(relative) = stack.pop() {
        for entry in std::fs::read_dir(dir.join(&relative))?.flatten() {
            let kind = entry.file_type()?;
            let name = entry.file_name();
            if name.to_string_lossy().starts_with(".git") {
                continue;
            }
            let path = relative.join(&name);
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file() {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

fn copy_skill(from: &Path, to: &Path) -> Result<()> {
    let files = list_files(from)?;
    if files.len() > MAX_SKILL_FILES {
        bail!("has {} files — too many for a skill", files.len());
    }
    let mut total = 0;
    for relative in &files {
        total += std::fs::metadata(from.join(relative))?.len();
    }
    if total > MAX_SKILL_BYTES {
        bail!("is {} MB — too big for a skill", total / (1024 * 1024));
    }
    for relative in files {
        let target = to.join(&relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(from.join(&relative), target)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(dir: &Path, name: &str, description: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\nbody\n"),
        )
        .unwrap();
    }

    #[test]
    fn the_bundled_skills_are_listed_and_on_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let skills = Extensions::new(dir.path()).skills().unwrap();
        let names: Vec<_> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["worktree", "review", "risky-changes"]);
        assert!(skills
            .iter()
            .all(|s| s.enabled && s.source == SkillSource::Bundled));
        assert!(
            skills.iter().all(|s| !s.description.is_empty()),
            "descriptions come from frontmatter"
        );
    }

    #[test]
    fn the_plugin_carries_enabled_skills_under_the_harness_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let extensions = Extensions::new(dir.path());
        extensions.set_skill_enabled("review", false).unwrap();

        let plugin = extensions.worker_extras().unwrap().plugin_dir.unwrap();
        let manifest: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(plugin.join(".claude-plugin/plugin.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest["name"], PLUGIN_NAME);
        assert!(plugin.join("skills/worktree/SKILL.md").is_file());
        assert!(
            !plugin.join("skills/review").exists(),
            "a disabled skill stays out"
        );
    }

    #[test]
    fn a_change_builds_a_new_plugin_and_leaves_the_old_one_for_running_workers() {
        let dir = tempfile::tempdir().unwrap();
        let extensions = Extensions::new(dir.path());
        let before = extensions.worker_extras().unwrap().plugin_dir.unwrap();
        assert_eq!(
            extensions.worker_extras().unwrap().plugin_dir.unwrap(),
            before,
            "unchanged means reused"
        );

        extensions
            .create_skill("deploy-notes", "How we deploy.")
            .unwrap();
        let after = extensions.worker_extras().unwrap().plugin_dir.unwrap();
        assert_ne!(before, after);
        assert!(
            before.join("skills/worktree/SKILL.md").is_file(),
            "the old plugin is intact"
        );
        assert!(after.join("skills/deploy-notes/SKILL.md").is_file());

        extensions.prune_plugins(Some(&after));
        assert!(!before.exists());
        assert!(after.exists());
    }

    #[test]
    fn no_enabled_skills_means_no_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let extensions = Extensions::new(dir.path());
        for name in ["worktree", "review", "risky-changes"] {
            extensions.set_skill_enabled(name, false).unwrap();
        }
        assert!(extensions.worker_extras().unwrap().plugin_dir.is_none());
    }

    #[test]
    fn importing_a_repository_takes_every_skill_folder_in_it() {
        let dir = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        write_skill(
            &repo.path().join("orchestration/git-worktree"),
            "git-worktree",
            "Worktrees.",
        );
        write_skill(&repo.path().join("ops/risky"), "risky", "Risky changes.");
        std::fs::write(repo.path().join("ops/risky/check.sh"), "echo ok\n").unwrap();
        // One that would shadow a built-in, and one no agent would ever pick.
        write_skill(&repo.path().join("review"), "review", "Mine.");
        std::fs::create_dir_all(repo.path().join("vague")).unwrap();
        std::fs::write(
            repo.path().join("vague/SKILL.md"),
            "---\nname: vague\n---\n",
        )
        .unwrap();

        let extensions = Extensions::new(dir.path().join("ext"));
        let report = extensions
            .import_skills(repo.path(), Some("https://github.com/someone/skills"))
            .unwrap();
        assert_eq!(
            report.imported,
            ["risky", "git-worktree"],
            "walked in sorted folder order"
        );
        let skipped: Vec<_> = report
            .skipped
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(skipped, ["review", "vague"]);

        let risky = extensions
            .skills()
            .unwrap()
            .into_iter()
            .find(|s| s.name == "risky")
            .unwrap();
        assert_eq!(
            risky.origin.as_deref(),
            Some("https://github.com/someone/skills")
        );
        assert!(
            risky.path.unwrap().join("check.sh").is_file(),
            "the whole folder comes along"
        );
    }

    #[test]
    fn a_folder_without_skills_is_an_error_not_an_empty_success() {
        let dir = tempfile::tempdir().unwrap();
        let empty = tempfile::tempdir().unwrap();
        assert!(Extensions::new(dir.path())
            .import_skills(empty.path(), None)
            .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_in_a_skill_are_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let skill = tempfile::tempdir().unwrap();
        write_skill(skill.path(), "linky", "Has a link.");
        let secret = dir.path().join("secret.txt");
        std::fs::write(&secret, "do not copy").unwrap();
        std::os::unix::fs::symlink(&secret, skill.path().join("leak.txt")).unwrap();

        let extensions = Extensions::new(dir.path().join("ext"));
        extensions.import_skills(skill.path(), None).unwrap();
        let imported = dir.path().join("ext/skills/linky");
        assert!(imported.join("SKILL.md").is_file());
        assert!(!imported.join("leak.txt").exists());
    }

    #[test]
    fn skill_names_follow_claude_codes_rules() {
        let dir = tempfile::tempdir().unwrap();
        let extensions = Extensions::new(dir.path());
        for bad in ["", "Caps", "../escape", "-dash", "has space", "worktree"] {
            assert!(
                extensions.create_skill(bad, "x").is_err(),
                "{bad:?} should be refused"
            );
        }
        assert!(
            extensions.create_skill("fine-name-2", "").is_err(),
            "description required"
        );
        assert!(extensions
            .create_skill("fine-name-2", "Does a thing.")
            .is_ok());
    }

    #[test]
    fn a_created_skill_has_loadable_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let extensions = Extensions::new(dir.path());
        let path = extensions
            .create_skill("notes", "Use when: taking notes.")
            .unwrap();
        let body = std::fs::read_to_string(path.join("SKILL.md")).unwrap();
        assert_eq!(
            frontmatter(&body),
            (Some("notes".into()), Some("Use when: taking notes.".into())),
            "a colon in the description must not break it"
        );
    }

    #[test]
    fn bundled_skills_can_be_turned_off_but_not_removed() {
        let dir = tempfile::tempdir().unwrap();
        let extensions = Extensions::new(dir.path());
        assert!(extensions.remove_skill("review").is_err());
        extensions.create_skill("mine", "Mine.").unwrap();
        extensions.set_skill_enabled("mine", false).unwrap();
        extensions.remove_skill("mine").unwrap();
        assert!(extensions
            .skills()
            .unwrap()
            .iter()
            .all(|s| s.name != "mine"));
        assert!(extensions.set_skill_enabled("mine", true).is_err());
    }

    #[test]
    fn mcp_servers_are_kept_in_claude_codes_shape() {
        let dir = tempfile::tempdir().unwrap();
        let extensions = Extensions::new(dir.path());
        extensions
            .set_mcp_server(
                "github",
                serde_json::json!({ "command": "npx", "args": ["-y", "gh-mcp"] }),
            )
            .unwrap();
        extensions
            .set_mcp_server(
                "docs",
                serde_json::json!({ "type": "http", "url": "https://example.com/mcp" }),
            )
            .unwrap();

        let extras = extensions.worker_extras().unwrap();
        let config: serde_json::Value =
            serde_json::from_str(&extras.mcp_config().unwrap()).unwrap();
        assert_eq!(config["mcpServers"]["github"]["command"], "npx");
        assert_eq!(
            config["mcpServers"]["docs"]["url"],
            "https://example.com/mcp"
        );
        let args = extras.claude_args();
        assert!(args.contains(&"--mcp-config".to_string()));
        assert!(args.contains(&"--plugin-dir".to_string()));

        extensions.remove_mcp_server("docs").unwrap();
        assert_eq!(extensions.mcp_servers().unwrap().len(), 1);
    }

    #[test]
    fn a_bad_mcp_server_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let extensions = Extensions::new(dir.path());
        let refused = [
            ("harness", serde_json::json!({ "command": "x" })),
            ("has space", serde_json::json!({ "command": "x" })),
            ("neither", serde_json::json!({ "args": ["x"] })),
            (
                "both",
                serde_json::json!({ "command": "x", "url": "https://x" }),
            ),
            ("file", serde_json::json!({ "url": "file:///etc/passwd" })),
            (
                "args",
                serde_json::json!({ "command": "x", "args": "not a list" }),
            ),
        ];
        for (name, config) in refused {
            assert!(
                extensions.set_mcp_server(name, config).is_err(),
                "{name} should be refused"
            );
        }
        assert!(extensions.mcp_servers().unwrap().is_empty());
    }

    #[tokio::test]
    async fn git_import_refuses_what_is_not_a_remote_url() {
        let dir = tempfile::tempdir().unwrap();
        let extensions = Extensions::new(dir.path());
        for bad in [
            "--upload-pack=touch /tmp/x",
            "/etc",
            "file:///etc",
            "ext::sh -c x",
        ] {
            assert!(
                extensions.import_skills_from_git(bad).await.is_err(),
                "{bad} should be refused"
            );
        }
    }
}
