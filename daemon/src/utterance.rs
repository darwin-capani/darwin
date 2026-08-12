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

/// Tokens that may legally sit BEFORE the WH-word of a spoken QUESTION: a
/// vocative, a politeness, a discourse opener, a request frame ("can you …",
/// "i need you to …"), and the DIRECT-QUESTION verbs ("tell me what …", "show
/// me what …", "read what …", "watch what …", "describe what …", "do you know
/// what …").
///
/// WHY THIS IS NOT [`crate::router`]'s command frame, which it otherwise
/// resembles: a WH-question has no verb of its own to put in command position
/// ("what's on my screen" contains none at all), so the analogous rule has to
/// bound what precedes the WH-WORD instead. That admits a class the command
/// frame never needs — the speech-act verbs `tell`/`show`/`know`/`see` — and it
/// is exactly those that separate a request from a report, so the two lists
/// cannot be one list. Sharing them would have to widen one of the two.
///
/// DENY BY DEFAULT, like the command-position sibling: anything else in front of
/// the WH-word makes the question an EMBEDDED content clause ("we talk about
/// what's on my screen", "i forgot what was on the screen", "she wondered
/// what …") rather than a request, so the gate fails toward a missed question —
/// which the owner can rephrase — and never toward capturing the screen and
/// SPEAKING it at somebody who was only telling a story.
const INTERROGATIVE_FRAME: &[&str] = &[
    // Vocative / politeness / discourse openers.
    "darwin", "hey", "hi", "ok", "okay", "alright", "please", "so", "and",
    "then", "now", "also", "just", "actually", "maybe", "well", "um", "uh",
    "sorry", "excuse", "quickly", "quick", "first", "simply", "kindly", "right",
    "yes", "yeah", "sure", "wait", "go", "ahead", "back", "on", "out",
    // The request frame — modals, the addressee, and the "i want/need to …"
    // periphrastics.
    "can", "could", "will", "would", "should", "do", "does", "did", "i", "we",
    "you", "us", "me", "my", "let", "lets", "want", "wants", "need", "needs",
    "like", "try", "help", "gonna", "going", "to",
    // The DIRECT-QUESTION verbs. These are the whole reason this list is not the
    // command frame: "tell me what's on the screen", "show me what's on my
    // screen", "read what's on screen", "watch what's on the screen" and
    // "describe what's on my screen" are harvested real phrasings, and each
    // leads with a word no command frame admits.
    "tell", "show", "know", "see", "remind", "check", "read", "watch",
    "watching", "describe", "say", "list", "narrate", "explain", "find",
    // The ASPECT verbs, so a continued read keeps its question ("keep watching
    // what's on the screen").
    "keep", "keeps", "keeping", "start", "starts", "continue", "resume", "begin",
    "get", "gets", "carry",
    // The fragments an apostrophe leaves once the utterance is split on
    // non-alphanumerics ("i'd" -> "i" + "d"), plus the apostrophe-free dictation
    // spellings of the same contractions.
    "m", "s", "t", "d", "ll", "re", "ve", "im", "id", "ill", "ive",
];

/// The frame tokens that make what follows a REQUEST rather than a report, so a
/// bare subject BEHIND one of them is the addressee ("can YOU tell me what's on
/// my screen") rather than the speaker ("YOU know what's on my screen").
///
/// `d` and `ll` are deliberately NOT here even though they are in the frame: "i'd
/// like to know what's on my screen" already passes on `like`, while "i'll tell
/// you what's on my screen" is a promise, not a question, and listing `ll` would
/// forgive its subject.
const INTERROGATIVE_REQUEST_MODALS: &[&str] = &[
    "can", "could", "will", "would", "should", "do", "does", "did", "please",
    "want", "wants", "need", "needs", "like", "let", "lets", "try", "help",
    "gonna", "going", "darwin", "hey", "ok", "okay",
];

/// The request modals that legitimately sit BEHIND their subject, because the
/// request they build is PERIPHRASTIC rather than inverted: "i WANT to know
/// what's on my screen", "i NEED to know …", "i'd LIKE to know …". Every other
/// modal in [`INTERROGATIVE_REQUEST_MODALS`] has to come BEFORE the subject to
/// make a question, because English marks a matrix question by AUX INVERSION.
const INTERROGATIVE_PERIPHRASTIC_MODALS: &[&str] =
    &["want", "wants", "need", "needs", "like"];

/// The aspect verbs that license a bare participle after them, so "keep watching
/// what's on the screen" stays a request while "just watching what's on the
/// screen" — the same participle with its subject elided — does not.
const INTERROGATIVE_ASPECT_VERBS: &[&str] =
    &["keep", "keeps", "keeping", "start", "starts", "continue", "resume", "begin"];

/// The tokens after which a bare `you` is DARWIN, the ADDRESSEE — the inverting
/// auxiliaries and the object-control desideratives. Anywhere else in the prefix,
/// `you` is the RECIPIENT of what the speaker is about to say ("let me tell YOU
/// what's on my screen", "i want to let YOU know what's on my screen"), which
/// makes the clause a report however many request modals are in front of it.
///
/// AN ALLOW-LIST, NOT A VERB LIST, because the verb side is open: a first cut
/// enumerated tell/show/read/say/narrate/describe/explain/list and eleven more
/// spellings walked straight past it — let / remind / get / find / keep / start /
/// try / check / see / know / go, each MEASURED reaching a Lumen read and a
/// vision `read.screen`. What is closed is the other end: the handful of tokens a
/// REQUEST puts in front of its addressee. `let` is deliberately absent even
/// though it is a request modal — "let ME know" is the request and "let YOU know"
/// is the report, and they differ only here.
const INTERROGATIVE_ADDRESSEE_LICENSORS: &[&str] = &[
    "can", "could", "will", "would", "should", "do", "does", "did",
    "want", "wants", "need", "needs", "like",
];

/// Bare subject pronouns — a subject in front of the WH-word with no request
/// modal ahead of it means the speaker is REPORTING the question, not asking it.
/// This is what tells "do you know what's on my screen" (a request) from "i know
/// what's on my screen" (a boast) and "you know what's on my screen" (a
/// reassurance), which share every other word.
///
/// THE APOSTROPHE-FREE DICTATION SPELLINGS ARE SUBJECTS TOO, and leaving them
/// out made the rule answer differently depending on whether the dictation
/// engine emitted an apostrophe. "i'm" splits into `i` + `m` and is refused on
/// the `i`; dictation writes it "im", which sat in the frame and in no other
/// list, so "im watching what's on the screen" read the screen ALOUD while
/// "i'm watching what's on the screen" did not. Same asymmetry for id / ive /
/// ill. MEASURED on both spellings.
const INTERROGATIVE_BARE_SUBJECTS: &[&str] =
    &["i", "we", "you", "im", "id", "ive", "ill"];

/// Whether one of `wh` is the FIRST content token of `lower` — i.e. the WH-word
/// opens the question, preceded by nothing but [`INTERROGATIVE_FRAME`], with no
/// bare subject ahead of it unless an INVERTED request modal came first, and
/// with no OTHER wh-word anywhere behind a content word.
///
/// This is the INTERROGATIVE analogue of the command-position rule the imperative
/// capture gates already carry. WHAT WENT WRONG without it: every question-shaped
/// capture cue was matched ANYWHERE in the utterance, so an owner telling a story
/// ABOUT the question had their screen captured and read back ALOUD — "on sundays
/// we talk about what's on my screen", "i forgot what was on the screen", "on
/// sundays we joke about what was i doing all week", "she wondered what was on
/// the screen", "the kids talk about what's on my screen".
///
/// ANYWHERE, NOT ADJACENT, for the subject — the same lesson the command-position
/// sibling had to learn twice. Checking only the token immediately before the
/// WH-word is bypassed by one adverb ("i just know what's on my screen"), so the
/// subject stays seen for the rest of the prefix once it appears. A later modal
/// USED TO forgive unconditionally, and that was the hole: "i CAN see what's on
/// my screen", "you DO know what's on my screen" and "i'll LET you know what's
/// on my screen" are reports, not requests, and all three MEASURED as a Lumen
/// read plus a SPOKEN vision `read.screen`. What forgives now is AUX INVERSION —
/// a modal in front of the subject ("DO you know …", "CAN you tell me …") — plus
/// the [`INTERROGATIVE_PERIPHRASTIC_MODALS`], which are declarative by
/// construction and are what keeps "i just need to know what's on my screen"
/// alive. Note what that costs, so it is not rediscovered
/// as a bug: an adverb OUTSIDE the frame refuses the whole utterance, so "i
/// really need to know what's on my screen" no longer asks. It fails toward a
/// rephraseable miss, which is the direction a capture gate must fail in.
///
/// Whole-word by construction (the split is the same alphanumeric-boundary rule
/// [`mentions_word`] uses), and single-pass with no allocation, so an oversize
/// junk utterance stays cheap.
pub fn wh_word_in_interrogative_position(lower: &str, wh: &[&str]) -> bool {
    let mut modal_seen = false;
    let mut subject_seen = false;
    let mut prev = "";
    let mut opened = false;
    let mut left_the_frame = false;
    for w in lower.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()) {
        if wh.contains(&w) {
            // AN EMBEDDED WH-WORD ANYWHERE REFUSES THE WHOLE UTTERANCE. This rule
            // is asked about the UTTERANCE while the cue it guards is a
            // `contains(...)` matched ANYWHERE in it, so testing only the FIRST
            // wh-word let one leading wh clear an embedded one behind it. MEASURED
            // with the ledger's rule in place: "what a day, on sundays we talk
            // about what's on my screen" -> lumen read + vision read.screen,
            // "what did you say, i forgot what was on the screen" -> read.screen,
            // "what's for dinner, we asked who is there and nobody answered" ->
            // the CAMERA. Once a token outside the frame has gone by, any wh-word
            // after it sits inside somebody's clause.
            if left_the_frame {
                return false;
            }
            if subject_seen && !modal_seen {
                return false;
            }
            opened = true;
            prev = w;
            continue;
        }
        if !INTERROGATIVE_FRAME.contains(&w) {
            if !opened {
                return false;
            }
            left_the_frame = true;
            prev = w;
            continue;
        }
        // "TELL YOU" IS NOT "TELL ME", and the same for every other verb that can
        // take a recipient. The direct-question verbs are in the frame because
        // "tell me what's on the screen" and "show me what's on my screen" are
        // real requests — but with `you` as the RECIPIENT the speaker is the one
        // ANSWERING, and a request modal in front ("LET me tell you …", "i WANT
        // to let you know …") forgives the subject, so the whole family reached a
        // SPOKEN screen read. MEASURED, all of them: "let me tell you what's on
        // my screen", "let me show you what's on my screen", "i want to tell you
        // what's on my screen", "i want to let you know what's on my screen",
        // "let me remind you what's on my screen", "let me get you what's on my
        // screen", "let me find you what's on my screen" — plus "let me tell you
        // what was that sound" and "i want to let you know who is there", which
        // classify the mic clip and open the CAMERA.
        //
        // DENY BY DEFAULT, like everything else here, and for a measured reason:
        // a verb list was tried first and eleven spellings walked past it (see
        // [`INTERROGATIVE_ADDRESSEE_LICENSORS`]). A request to DARWIN only ever
        // puts `you` behind an inverting aux or an object-control desiderative —
        // "CAN you tell me …", "DO you know …", "i NEED you to tell me …" — so
        // that is the closed end. Utterance-initial `you` is left to the subject
        // rule below ("you can see what's on my screen" is refused there), and
        // once the question has OPENED this is its own body, not a prefix, so
        // "what ARE you seeing" and "what DO you hear" are untouched.
        if !opened
            && w == "you"
            && !prev.is_empty()
            && !INTERROGATIVE_ADDRESSEE_LICENSORS.contains(&prev)
        {
            return false;
        }
        // AUX INVERSION IS THE TELL, and forgiving a bare subject on ANY later
        // modal was the hole. English marks a matrix question by putting the aux
        // in FRONT of the subject — "DO you know what's on my screen" — while the
        // same words with the subject first are a REPORT: "i CAN see what's on my
        // screen", "you CAN see what's on my screen", "you DO know what's on my
        // screen", "i DID know what was on the screen", "i'll LET you know what's
        // on my screen". Every one of those MEASURED reaching a Lumen read AND a
        // vision `read.screen` — a capture whose readout is SPOKEN — with the
        // interrogative-position rule in place, and "i do know …" is exactly ONE
        // WORD from the "i know …" the same rule is tested on as inert.
        //
        // So a modal only forgives a subject it came BEFORE. The exception is the
        // PERIPHRASTIC request, which is declarative by construction and is what
        // keeps "i want to know what's on my screen" / "i need to know …" /
        // "i'd like to know …" alive.
        if INTERROGATIVE_REQUEST_MODALS.contains(&w)
            && (!subject_seen || INTERROGATIVE_PERIPHRASTIC_MODALS.contains(&w))
        {
            modal_seen = true;
        }
        if INTERROGATIVE_BARE_SUBJECTS.contains(&w) {
            subject_seen = true;
        }
        // A BARE PARTICIPLE IS A DROPPED SUBJECT. `watching` is in the frame only
        // so the aspectual request "KEEP watching what's on the screen" survives;
        // with no aspect verb in front of it, it is the answer to "what are you
        // doing" with the subject elided — "just watching what's on the screen" —
        // and that MEASURED as a spoken screen read. Treating it as a subject
        // costs nothing the aspect verbs do not buy straight back.
        if w == "watching" && !INTERROGATIVE_ASPECT_VERBS.contains(&prev) {
            subject_seen = true;
        }
        prev = w;
    }
    opened
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

    /// THE EMBEDDED QUESTION IS NOT A REQUEST. Every sentence here reached a
    /// capture gate at HEAD — the screen, the camera, the mic, the rings — and
    /// each is an owner telling a story ABOUT the question rather than asking it.
    /// The embedding verbs are varied (talk/joke/wonder/forget/remember/ask/tell/
    /// know/argue/laugh/discuss) and so are the subjects, because subjects are an
    /// OPEN class and no list could enumerate them: what closes this is the
    /// DENY-BY-DEFAULT frame, which refuses anything in front of the WH-word that
    /// is not a vocative, a politeness, a request frame or a direct-question verb.
    #[test]
    fn an_embedded_wh_question_is_not_in_interrogative_position() {
        for text in [
            "on sundays we talk about what's on my screen",
            "i forgot what was on the screen",
            "she wondered what was on the screen",
            "the kids talk about what's on my screen",
            "nobody knows what's on my screen",
            "he asked what's on my screen",
            "i told her what's on my screen",
            "we argued about what are the buttons on this screen for",
            "we laughed about what was on the screen",
            "my brother knows what's on my screen",
            "we never discuss what's on my screen",
            "i cannot remember what was on the screen",
            "i remember what's on my screen",
        ] {
            assert!(
                !wh_word_in_interrogative_position(text, &["what", "whats"]),
                "{text:?} embeds the question and would capture"
            );
        }
        // WHO-shaped, the camera's presence gate.
        assert!(!wh_word_in_interrogative_position(
            "we asked who is there and nobody answered",
            &["who"]
        ));
    }

    /// ...AND ONE ADVERB IS NOT A BOUNDARY. This is the defect the
    /// command-position sibling shipped and had to fix twice: checking only the
    /// token immediately in front is cleared by a single frame word. Both
    /// dimensions are varied one at a time — the adverb between the subject and
    /// the embedding verb, then the subject itself with the adverb held fixed.
    #[test]
    fn an_adverb_between_the_subject_and_the_embedding_verb_does_not_clear_it() {
        for text in [
            "on sundays we sometimes talk about what's on my screen",
            "we always talk about what's on my screen",
            "we also joke about what's on my screen",
            "we now talk about what's on my screen",
            "we then talk about what's on my screen",
            "we actually talk about what's on my screen",
            "we maybe talk about what's on my screen",
            "we first talk about what's on my screen",
            "we quickly read about what's on my screen",
            "we go and talk about what's on my screen",
            "i just forgot what was on the screen",
            "i simply forgot what was on the screen",
            "i already know what's on my screen",
        ] {
            assert!(
                !wh_word_in_interrogative_position(text, &["what", "whats"]),
                "{text:?} is narration and an adverb cleared the rule"
            );
        }
        // THE STICKY SUBJECT IS WHAT DOES IT, and it must be exactly what
        // separates the boast from the request. These three share every word but
        // the frame in front of `know`.
        assert!(!wh_word_in_interrogative_position("i know what's on my screen", &["what"]));
        assert!(!wh_word_in_interrogative_position("you know what's on my screen", &["what"]));
        assert!(wh_word_in_interrogative_position("do you know what's on my screen", &["what"]));
        assert!(wh_word_in_interrogative_position("i want to know what's on my screen", &["what"]));
        // The modal is tested AT the WH-word, not where the subject was, so a
        // frame adverb between subject and modal still forgives...
        assert!(wh_word_in_interrogative_position(
            "i just need to know what's on my screen",
            &["what"]
        ));
        // ...and the MEASURED COST of deny-by-default, stated as a test rather
        // than left to be rediscovered: an adverb outside the frame refuses.
        assert!(!wh_word_in_interrogative_position(
            "i really need to know what's on my screen",
            &["what"]
        ));
    }

    /// BOTH SIDES, or the rule is just a capability deletion. Every opener,
    /// dictation spelling, politeness prefix, vocative, aux-inverted request and
    /// direct-question frame that a real owner uses has to survive it.
    #[test]
    fn every_real_way_of_asking_a_question_is_in_interrogative_position() {
        for text in [
            "what's on my screen",                  // bare opener
            "whats on my screen",                   // dictation, no apostrophe
            "what is displayed on the screen",
            "what's on my second screen",
            "darwin what's on my screen",           // vocative
            "hey darwin, what's on my screen",
            "please tell me what's on my screen",   // politeness + frame verb
            "ok what's on my screen",
            "so what's on my screen",
            "quickly, what's on my screen",
            "actually, what's on my screen",
            "tell me what's on the screen",         // direct-question frames
            "show me what's on my screen",
            "read what's on screen",
            "watch what's on the screen",
            "describe what's on my screen",
            "let me know what's on my screen",
            "can you tell me what's on my screen",  // aux inversion, bare subject
            "could you tell me what's on the screen",
            "would you tell me what's on my screen",
            "i need to know what's on my screen",   // periphrastic request
            "i'd like to know what's on my screen",
            "keep watching what's on the screen",   // aspect
        ] {
            assert!(
                wh_word_in_interrogative_position(text, &["what", "whats"]),
                "{text:?} is a real question and stopped being one"
            );
        }
        for text in ["who is there", "who's there", "darwin who is there", "tell me who is there"] {
            assert!(wh_word_in_interrogative_position(text, &["who"]), "{text:?}");
        }
    }

    /// THE DEGENERATE CASES, because a bound that is never walked to its end is
    /// a bound nobody has checked. An empty utterance, a WH-word with nothing
    /// after it, an utterance with no WH-word at all, an empty WH list (which
    /// must refuse rather than admit), and a WH-word that is only a SUBSTRING of
    /// a real token.
    #[test]
    fn the_interrogative_rule_holds_at_its_edges() {
        assert!(!wh_word_in_interrogative_position("", &["what"]));
        assert!(wh_word_in_interrogative_position("what", &["what"]));
        assert!(!wh_word_in_interrogative_position("read my screen", &["what"]));
        assert!(!wh_word_in_interrogative_position("what's on my screen", &[]));
        // Whole-word by construction: "whatever" and "somewhat" are not "what".
        assert!(!wh_word_in_interrogative_position("whatever is on my screen", &["what"]));
        assert!(!wh_word_in_interrogative_position("somewhat on my screen", &["what"]));
        // A frame that runs to the end of the utterance without ever reaching a
        // WH-word is a refusal, not a fall-through.
        assert!(!wh_word_in_interrogative_position("please can you", &["what"]));
    }

    /// A REPORT THAT HAPPENS TO CONTAIN A MODAL IS STILL A REPORT. The rule above
    /// forgave a bare subject on ANY request modal in the prefix, so the whole
    /// declarative can/do/did/will family walked straight through it — and every
    /// sentence here was MEASURED reaching a capture gate WITH the position rule
    /// in place, most of them a Lumen read AND a vision `read.screen`, whose
    /// readout is SPOKEN. AUX INVERSION is the discriminator: English puts the aux
    /// in FRONT of the subject to make a matrix question.
    ///
    /// Note the pairs. Each narration line below is ONE WORD from a request line
    /// in the next block, which is the whole point: "i know" / "i do know", "let
    /// me know" / "let me tell you", "i need to know" / "i can see".
    #[test]
    fn a_request_modal_behind_its_subject_does_not_make_a_report_a_question() {
        for text in [
            "i can see what's on my screen",
            "we can see what's on my screen",
            "you can see what's on my screen",
            "i could see what's on my screen",
            "you will see what's on my screen",
            "you should see what's on my screen",
            "i do know what's on my screen",
            "you do know what's on my screen",
            "i did know what was on the screen",
            "i'll let you know what's on my screen",
            "i can show you what's on my screen",
            // ...and the RECIPIENT family, where `you` is who the answer is FOR:
            // the speaker is answering, not asking. The verb side of this is an
            // OPEN class — a verb list was tried first and let / remind / get /
            // find / keep / check / try walked past it — so what is closed is the
            // set of tokens a REQUEST puts in front of its addressee.
            "let me tell you what's on my screen",
            "let me show you what's on my screen",
            "let me read you what's on my screen",
            "i want to tell you what's on my screen",
            "i just want to tell you what's on my screen",
            "im going to tell you what's on my screen",
            "i want to let you know what's on my screen",
            "i need to let you know what's on my screen",
            "let me let you know what's on my screen",
            "let me remind you what's on my screen",
            "i want to remind you what's on my screen",
            "let me get you what's on my screen",
            "let me find you what's on my screen",
            "let me check you what's on my screen",
            "let me keep you what's on my screen",
            "let me try you what's on my screen",
            // ...and the dictation contraction spellings, which sat in the frame
            // and in no subject list, so "im" answered differently from "i'm".
            "im watching what's on the screen",
            "i'm watching what's on the screen",
            "id know what's on my screen",
            // ...and the bare participle with its subject elided.
            "just watching what's on the screen",
            "watching what's on the screen",
        ] {
            assert!(
                !wh_word_in_interrogative_position(text, &["what", "whats"]),
                "{text:?} is a report and would capture"
            );
        }
        assert!(!wh_word_in_interrogative_position("you can see who is there", &["who"]));
    }

    /// ...AND THE REQUESTS THE INVERSION RULE HAS TO KEEP. Deny-by-default plus an
    /// inversion rule is exactly the shape that would delete the periphrastic
    /// request ("i want to know what's on my screen" has its subject first), so
    /// each surviving shape is pinned here beside the report it is one word from.
    #[test]
    fn an_inverted_or_periphrastic_request_is_still_in_interrogative_position() {
        for text in [
            "do you know what's on my screen",
            "did you see what's on my screen",
            "can you tell me what's on my screen",
            "could you tell me what's on the screen",
            "would you tell me what's on my screen",
            "can i see what's on my screen",
            "i want to know what's on my screen",
            "i need to know what's on my screen",
            "i just need to know what's on my screen",
            "i'd like to know what's on my screen",
            "id like to know what's on my screen",
            "let me know what's on my screen",
            "let me see what's on my screen",
            "tell me what's on the screen",
            "show me what's on my screen",
            "remind me what's on my screen",
            "get me what's on my screen",
            "find me what's on my screen",
            "read me what's on the screen",
            "can you let me know what's on my screen",
            "i need you to tell me what's on my screen",
            "i want you to tell me what's on my screen",
            "im gonna need you to tell me what's on my screen",
            "keep watching what's on the screen",
            "what's on my screen",
            "whats on my screen",
        ] {
            assert!(
                wh_word_in_interrogative_position(text, &["what", "whats"]),
                "{text:?} is a real request and stopped being one"
            );
        }
    }

    /// A LEADING WH-WORD DOES NOT CLEAR AN EMBEDDED ONE BEHIND IT. This rule is
    /// asked about the UTTERANCE while every cue it guards is a `contains(...)`
    /// matched ANYWHERE in it, so returning at the FIRST wh-word meant any opener
    /// at all licensed the narration that followed. All six MEASURED reaching a
    /// capture gate with the position rule in place.
    #[test]
    fn a_leading_wh_word_does_not_license_an_embedded_one() {
        for text in [
            "what a day, on sundays we talk about what's on my screen",
            "what did you say, i forgot what was on the screen",
            "what a mess, we joked about what's on my screen",
            "what a week, on sundays we joke about what did i do all week",
            "whats up, i forgot what was on the screen",
            "sorry i forgot what was on the screen",
        ] {
            assert!(
                !wh_word_in_interrogative_position(text, &["what", "whats"]),
                "{text:?} embeds a second question and a leading wh-word cleared it"
            );
        }
        assert!(!wh_word_in_interrogative_position(
            "what's for dinner, we asked who is there and nobody answered",
            &["what", "who"]
        ));
        // ...and the degenerate end of that bound: a question whose OWN body runs
        // past the frame is not "embedded". Every real question does this — the
        // screen noun, the aux, the object are all outside the frame — so if the
        // bound were off by one, nothing would fire at all.
        for text in [
            "what's on my screen",
            "what is displayed on the screen",
            "what does the whiteboard say",
            "what was i doing at lunch",
            "what buttons are on this screen",
            "read what's on screen",
            // ...and the addressee bound is a PREFIX bound, not a whole-utterance
            // one: once the question has opened, `you` is inside its own body.
            // "what ARE you seeing" has `you` behind a token that is not even in
            // the frame, so a bound that ran past the opener would refuse it.
            "what are you seeing",
            "what do you hear",
            "what can you see",
        ] {
            assert!(
                wh_word_in_interrogative_position(text, &["what", "whats"]),
                "{text:?} is one question, not two, and the bound ate it"
            );
        }
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

