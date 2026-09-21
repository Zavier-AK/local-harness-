//! Surgical updates to `roles.toml`.
//!
//! The fleet file is documentation as well as configuration, so serializing a parsed
//! registry back to TOML would be destructive: comments, examples, and formatting would
//! disappear. This patcher changes only `model` and `base_url` in existing role tables,
//! then validates the complete result before atomically replacing the file.

use anyhow::{bail, Context, Result};
use std::path::Path;

use crate::detection::RoleModelPatch;
use crate::roles::RoleRegistry;

pub fn apply_role_patches(source: &str, patches: &[RoleModelPatch]) -> Result<String> {
    let mut lines: Vec<String> = source.lines().map(str::to_string).collect();
    let had_trailing_newline = source.ends_with('\n');

    for patch in patches {
        let header = format!("[roles.{}]", patch.role_name);
        let start = lines
            .iter()
            .position(|line| line.trim() == header)
            .with_context(|| format!("roles.toml has no table `{header}`"))?;
        let end = lines[start + 1..]
            .iter()
            .position(|line| line.trim_start().starts_with('['))
            .map(|offset| start + 1 + offset)
            .unwrap_or(lines.len());

        set_string_field(&mut lines, start, end, "model", &patch.model)?;

        let refreshed_end = lines[start + 1..]
            .iter()
            .position(|line| line.trim_start().starts_with('['))
            .map(|offset| start + 1 + offset)
            .unwrap_or(lines.len());
        match &patch.base_url {
            Some(base_url) => {
                set_string_field(&mut lines, start, refreshed_end, "base_url", base_url)?
            }
            None => remove_field(&mut lines, start, refreshed_end, "base_url"),
        }
    }

    let mut patched = lines.join("\n");
    if had_trailing_newline {
        patched.push('\n');
    }
    RoleRegistry::from_toml(&patched).context("validating updated roles.toml")?;
    Ok(patched)
}

fn set_string_field(
    lines: &mut Vec<String>,
    table_start: usize,
    table_end: usize,
    key: &str,
    value: &str,
) -> Result<()> {
    if value.contains('\n') || value.contains('\r') {
        bail!("role field `{key}` cannot contain a newline");
    }
    let encoded = toml::Value::String(value.to_string()).to_string();

    if let Some(index) = find_field(lines, table_start, table_end, key) {
        let indent: String = lines[index]
            .chars()
            .take_while(|c| c.is_whitespace())
            .collect();
        let comment = inline_comment(&lines[index]).unwrap_or_default();
        lines[index] = format!("{indent}{key} = {encoded}{comment}");
    } else {
        let insert_at = find_field(lines, table_start, table_end, "provider")
            .map(|index| index + 1)
            .unwrap_or(table_start + 1);
        lines.insert(insert_at, format!("{key} = {encoded}"));
    }
    Ok(())
}

fn remove_field(lines: &mut Vec<String>, table_start: usize, table_end: usize, key: &str) {
    if let Some(index) = find_field(lines, table_start, table_end, key) {
        lines.remove(index);
    }
}

fn find_field(lines: &[String], table_start: usize, table_end: usize, key: &str) -> Option<usize> {
    lines[table_start + 1..table_end]
        .iter()
        .position(|line| {
            let trimmed = line.trim_start();
            !trimmed.starts_with('#')
                && trimmed
                    .split_once('=')
                    .is_some_and(|(candidate, _)| candidate.trim() == key)
        })
        .map(|offset| table_start + 1 + offset)
}

fn inline_comment(line: &str) -> Option<String> {
    let mut quoted = false;
    let mut escaped = false;
    for (index, character) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            '#' if !quoted => return Some(format!(" {}", line[index..].trim_start())),
            _ => {}
        }
    }
    None
}

pub fn apply_role_patches_file(path: &Path, patches: &[RoleModelPatch]) -> Result<()> {
    let source =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let patched = apply_role_patches(&source, patches)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("roles path has no filename")?;
    let temp_path = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));

    std::fs::write(&temp_path, patched)
        .with_context(|| format!("writing {}", temp_path.display()))?;
    if let Err(error) = std::fs::rename(&temp_path, path) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(error).with_context(|| format!("replacing {}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = r#"# Fleet documentation stays.
[roles.builder]
provider = "claude"
model = "sonnet" # preferred default
isolation = "worktree"

# Local notes stay too.
[roles.local]
provider = "openai_compat"
base_url = "http://localhost:11434/v1"
model = "old"
isolation = "none"
"#;

    #[test]
    fn patches_only_requested_fields_and_preserves_comments() {
        let patched = apply_role_patches(
            SOURCE,
            &[RoleModelPatch {
                role_name: "local".into(),
                model: "qwen/qwen3-coder-30b".into(),
                base_url: Some("http://localhost:1234/v1".into()),
            }],
        )
        .unwrap();

        assert!(patched.contains("# Fleet documentation stays."));
        assert!(patched.contains("# Local notes stay too."));
        assert!(patched.contains("model = \"sonnet\" # preferred default"));
        assert!(patched.contains("base_url = \"http://localhost:1234/v1\""));
        assert!(patched.contains("model = \"qwen/qwen3-coder-30b\""));
    }

    #[test]
    fn inserts_missing_model_after_provider() {
        let source = r#"[roles.x]
provider = "claude"
isolation = "readonly"
"#;
        let patched = apply_role_patches(
            source,
            &[RoleModelPatch {
                role_name: "x".into(),
                model: "opus".into(),
                base_url: None,
            }],
        )
        .unwrap();
        assert!(patched.contains("provider = \"claude\"\nmodel = \"opus\"\nisolation"));
    }

    #[test]
    fn refuses_unknown_roles_without_touching_source() {
        let error = apply_role_patches(
            SOURCE,
            &[RoleModelPatch {
                role_name: "missing".into(),
                model: "opus".into(),
                base_url: None,
            }],
        )
        .unwrap_err();
        assert!(error.to_string().contains("no table"));
    }
}
