//! Where one spoken request ends and the next begins, for listening that goes on.
//!
//! Pauses cut speech into pieces, but people pause mid-thought too: "look through the
//! results for … 4K monitors". An exact command acts at once. Anything else waits a
//! moment for more, and longer if it trails off on a word like "and" or "for", so the
//! voice agent gets the whole request instead of its first half.

use std::time::{Duration, Instant};

use super::matcher::normalize;

/// How long to wait for more after a piece that isn't an exact command.
pub const JOIN_WAIT: Duration = Duration::from_millis(800);
/// How long to wait when the piece plainly isn't finished ("…and", "…for the").
pub const TRAILING_WAIT: Duration = Duration::from_millis(2500);

/// Words a finished request doesn't end on.
const TRAILING: &[&str] = &[
    "and", "then", "so", "but", "or", "to", "for", "with", "the", "a", "an", "of", "in", "on",
    "at", "my", "your", "because", "about", "from", "into", "that", "which", "also", "plus",
    "like", "um", "uh", "er",
];

/// Whether `text` stops mid-sentence.
pub fn trails_off(text: &str) -> bool {
    let trimmed = text.trim_end();
    if trimmed.ends_with(',') || trimmed.ends_with("...") || trimmed.ends_with('…') {
        return true;
    }
    normalize(trimmed)
        .split_whitespace()
        .last()
        .is_some_and(|last| TRAILING.contains(&last))
}

/// How long to wait for the rest of `text`.
pub fn wait_for_more(text: &str) -> Duration {
    if trails_off(text) {
        TRAILING_WAIT
    } else {
        JOIN_WAIT
    }
}

/// Short, and one thing: safe to hand to a quick single-action reader (Laya) instead of
/// the voice agent. "Pause Spotify" is; "open Notes and remind me at six" is not.
pub fn single_intent(text: &str) -> bool {
    let normalized = normalize(text);
    let words: Vec<&str> = normalized.split_whitespace().collect();
    if words.is_empty() || words.len() > 12 {
        return false;
    }
    let joined = format!(" {} ", words.join(" "));
    ![" and ", " then ", " also ", " after that ", " plus "]
        .iter()
        .any(|joiner| joined.contains(joiner))
        && !text.contains(';')
}

/// "Stop listening", "that's all": ends hands-free listening.
pub fn is_stop_listening(text: &str) -> bool {
    let normalized = normalize(text);
    let said = normalized
        .trim_start_matches("ok ")
        .trim_start_matches("okay ")
        .trim_start_matches("thanks ")
        .trim_start_matches("thank you ")
        .trim();
    [
        "stop listening",
        "thats all",
        "that is all",
        "thats it",
        "that is it",
        "go to sleep",
        "im done",
        "i am done",
        "were done",
        "we are done",
        "goodbye",
        "bye",
    ]
    .contains(&said)
        || said.ends_with("stop listening")
}

/// Whether a piece that just ended overlapped the assistant speaking, so is probably its
/// own voice coming back through the microphone. `spoke_until` is when it last stopped.
pub fn over_speech(
    ended: Instant,
    length: Duration,
    pause: Duration,
    speaking: bool,
    spoke_until: Option<Instant>,
) -> bool {
    if speaking {
        return true;
    }
    // The piece began `length` before it was cut, which was `pause` after the last word;
    // allow a little for the room's echo.
    let echo = Duration::from_millis(300);
    let began = ended.checked_sub(length + pause + echo);
    match (spoke_until, began) {
        (Some(until), Some(began)) => until > began,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_sentences_wait_longer() {
        assert!(trails_off("look through the search results for"));
        assert!(trails_off("open notes and"));
        assert!(trails_off("find the cheapest monitor,"));
        assert!(!trails_off("find the cheapest 4K monitor"));
        assert_eq!(wait_for_more("email Sam and"), TRAILING_WAIT);
        assert_eq!(wait_for_more("email Sam that I'm late"), JOIN_WAIT);
    }

    #[test]
    fn one_short_thing_is_single() {
        assert!(single_intent("ship the builder's change"));
        assert!(single_intent("turn it down a bit"));
        assert!(!single_intent("open notes and remind me at six"));
        assert!(!single_intent("pause the music then open safari"));
        assert!(!single_intent(
            "look through the search results for 4K monitors for MacBooks and find the cheapest"
        ));
        assert!(!single_intent(""));
    }

    #[test]
    fn ways_to_stop_listening() {
        for said in [
            "Stop listening.",
            "That's all",
            "OK, that's it.",
            "Thanks, goodbye",
            "Go to sleep",
        ] {
            assert!(is_stop_listening(said), "{said}");
        }
        for said in [
            "stop",
            "stop the builder",
            "stop the music",
            "that's all the tests passing?",
        ] {
            assert!(!is_stop_listening(said), "{said}");
        }
    }

    #[test]
    fn its_own_voice_is_not_a_command() {
        let now = Instant::now() + Duration::from_secs(60);
        let pause = Duration::from_millis(700);
        let length = Duration::from_secs(2);
        // Still speaking.
        assert!(over_speech(now, length, pause, true, None));
        // Stopped speaking a moment before the piece ended: the piece is its tail.
        assert!(over_speech(
            now,
            length,
            pause,
            false,
            Some(now - Duration::from_secs(1))
        ));
        // Stopped well before the person started talking.
        assert!(!over_speech(
            now,
            length,
            pause,
            false,
            Some(now - Duration::from_secs(10))
        ));
        assert!(!over_speech(now, length, pause, false, None));
    }
}
