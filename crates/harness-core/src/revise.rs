//! Sending a worker's change back: the person's review comments, turned into the message
//! the worker gets for another pass on its own branch.

use serde::{Deserialize, Serialize};

/// One comment on a line of a worker's diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewComment {
    pub file: String,
    /// The line in the changed file, or in the old one for a removed line.
    #[serde(default)]
    pub line: Option<u32>,
    /// Whether `line` counts in the old file: the comment is on a removed line.
    #[serde(default)]
    pub removed: bool,
    /// The line itself, so the comment still makes sense if the numbers have moved.
    #[serde(default)]
    pub excerpt: Option<String>,
    pub text: String,
}

/// The longest excerpt quoted back; a minified line should not swamp the message.
const EXCERPT_CHARS: usize = 160;

/// What the worker is told. `note` is the person's overall message, if any.
pub fn feedback(note: &str, comments: &[ReviewComment]) -> String {
    let mut out = String::from(
        "The person reviewed your change and wants another pass. Your earlier work is \
         already committed on this branch: build on it rather than starting again, and \
         change only what the review asks for.",
    );
    let note = note.trim();
    if !note.is_empty() {
        out.push_str("\n\nWhat they said:\n");
        out.push_str(note);
    }
    let comments: Vec<&ReviewComment> = comments
        .iter()
        .filter(|c| !c.text.trim().is_empty())
        .collect();
    if !comments.is_empty() {
        out.push_str("\n\nComments on specific lines:");
        for comment in comments {
            let place = match (comment.line, comment.removed) {
                (Some(line), false) => format!("{} line {line}", comment.file),
                (Some(line), true) => format!("{} (removed line, was {line})", comment.file),
                (None, _) => comment.file.clone(),
            };
            out.push_str(&format!("\n- {place}"));
            if let Some(excerpt) = comment.excerpt.as_deref().map(str::trim) {
                if !excerpt.is_empty() {
                    let short: String = excerpt.chars().take(EXCERPT_CHARS).collect();
                    let cut = if short.len() < excerpt.len() {
                        "…"
                    } else {
                        ""
                    };
                    out.push_str(&format!(" — `{}{cut}`", short.replace('`', "'")));
                }
            }
            out.push_str(&format!(": {}", comment.text.trim()));
        }
    }
    out.push_str("\n\nWhen you are done, reply with a short summary of what you changed.");
    out
}

/// Whether there is anything to send.
pub fn is_empty(note: &str, comments: &[ReviewComment]) -> bool {
    note.trim().is_empty() && comments.iter().all(|c| c.text.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(
        file: &str,
        line: Option<u32>,
        removed: bool,
        excerpt: &str,
        text: &str,
    ) -> ReviewComment {
        ReviewComment {
            file: file.into(),
            line,
            removed,
            excerpt: Some(excerpt.into()).filter(|e: &String| !e.is_empty()),
            text: text.into(),
        }
    }

    #[test]
    fn comments_name_the_place_and_quote_the_line() {
        let text = feedback(
            "Close, but keep the old name.",
            &[
                comment(
                    "src/lib.rs",
                    Some(42),
                    false,
                    "let retries = 3;",
                    "make this a constant",
                ),
                comment(
                    "src/old.rs",
                    Some(7),
                    true,
                    "fn legacy() {}",
                    "keep this one",
                ),
                comment("README.md", None, false, "", "mention the flag"),
                comment("x", Some(1), false, "", "   "),
            ],
        );
        assert!(text.contains("build on it"));
        assert!(text.contains("What they said:\nClose, but keep the old name."));
        assert!(text.contains("- src/lib.rs line 42 — `let retries = 3;`: make this a constant"));
        assert!(
            text.contains("- src/old.rs (removed line, was 7) — `fn legacy() {}`: keep this one")
        );
        assert!(text.contains("- README.md: mention the flag"));
        assert!(!text.contains("- x line 1"), "blank comments are dropped");
    }

    #[test]
    fn long_lines_are_cut_and_backticks_cannot_break_the_quote() {
        let long = format!("`{}`", "a".repeat(400));
        let text = feedback("", &[comment("f", Some(1), false, &long, "shorter")]);
        assert!(text.contains("…`: shorter"));
        assert!(!text.contains(&"a".repeat(200)));
        assert!(!text.contains("What they said"));
    }

    #[test]
    fn nothing_to_send_is_noticed() {
        assert!(is_empty("  ", &[comment("f", Some(1), false, "", " ")]));
        assert!(!is_empty("fix it", &[]));
    }
}
