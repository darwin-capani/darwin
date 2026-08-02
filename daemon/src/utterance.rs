//! WHOLE-WORD MATCHING FOR SPOKEN UTTERANCES.
//!
//! Intent classifiers all over the daemon ask "did the user say this word?", and
//! the cheap way to ask — `lower.contains("clear")` — is wrong in a way that is
//! invisible until it destroys something: `clear` lives inside `nuclear`,
//! `clearance`, and `unclear`; `remove` inside `removal`; `erase` inside
//! `erasure`. A destructive branch guarded that way fires on an utterance that
//! never asked for it.
//!
//! This module holds the one primitive those classifiers share. It is
//! deliberately tiny and deliberately central: a second private copy is how the
//! rule drifts apart again.
//!
//! NOT for shell commands — [`crate::shell`] has its own `word_present` that
//! treats `;|&()/` as boundaries so a path-qualified `/bin/rm` matches as `rm`.
//! Applying that here would split ordinary speech on slashes; applying this
//! there would miss glued obfuscation like `rm-rf`. The two rules are different
//! on purpose.

/// Does `lower` contain `word` as a WHOLE WORD?
///
/// Splits on every non-alphanumeric character and compares tokens exactly, so
/// "clear" matches in "clear the world" and in "clear, please" but never in
/// "nuclear" or "clearance". `lower` is expected already lowercased; `word` must
/// be a single alphanumeric token (a `word` containing punctuation can never
/// match, since it would straddle a split point).
pub fn mentions_word(lower: &str, word: &str) -> bool {
    lower.split(|c: char| !c.is_alphanumeric()).any(|w| w == word)
}

/// Does `lower` contain ANY of `words` as a whole word? The plural form, for the
/// common "any of these verbs means the same intent" check.
pub fn mentions_any_word(lower: &str, words: &[&str]) -> bool {
    words.iter().any(|w| mentions_word(lower, w))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The substring trap, at the level it is enforced. `clear` is the verb that
    /// hides inside the most ordinary English, so it gets the sharpest test:
    /// every one of these fired a destructive branch under `contains`.
    #[test]
    fn a_verb_hiding_inside_a_longer_word_is_not_a_match() {
        for hay in [
            "nuclear reactors",
            "clearance rates",
            "unclear results",
            "nuclear clearance policy",
        ] {
            assert!(
                !mentions_word(hay, "clear"),
                "{hay:?} contains the letters of `clear` but nobody said it"
            );
        }
        assert!(!mentions_word("removal costs", "remove"));
        assert!(!mentions_word("erasure coding", "erase"));
        assert!(!mentions_word("deletion policy", "delete"));
        assert!(!mentions_word("superhero", "super"));
    }

    /// ...and the word itself still matches, wherever it sits and whatever
    /// punctuation abuts it. A rule that stopped matching real speech would pass
    /// the test above and be just as broken.
    #[test]
    fn the_real_word_still_matches() {
        assert!(mentions_word("clear the world", "clear"));
        assert!(mentions_word("please clear, now", "clear"));
        assert!(mentions_word("world clear", "clear"));
        assert!(mentions_word("clear", "clear"));
        assert!(mentions_word("(clear)", "clear"));
        // Contractions split on the apostrophe, which is what the callers want:
        // "i've" is "i" + "ve", neither of which is a verb any classifier lists.
        assert!(mentions_word("what i've researched", "i"));
    }

    #[test]
    fn mentions_any_word_is_the_or_of_its_parts() {
        const VERBS: &[&str] = &["forget", "clear", "wipe"];
        assert!(mentions_any_word("wipe the timeline", VERBS));
        assert!(mentions_any_word("forget it", VERBS));
        assert!(!mentions_any_word("nuclear timeline", VERBS));
        assert!(!mentions_any_word("", VERBS));
        assert!(!mentions_any_word("clear it", &[]));
    }
}

