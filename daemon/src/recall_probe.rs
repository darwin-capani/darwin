//! ROUTER RECALL — the half of the routing boundary that was never measured.
//!
//! PRECISION (does an ordinary sentence get hijacked into an app?) was measured
//! and fixed: 317 of 1,897 ordinary utterances used to be captured, 1 survives.
//! Every one of those fixes moved the boundary in the direction that costs
//! RECALL, and nobody asked the other question: **how many utterances that
//! SHOULD reach a capability now reach nothing** and fall through to a generic
//! model answer? That is the failure the owner actually feels — they ask for
//! something DARWIN can do and it talks about it instead of doing it.
//!
//! This module is the standing measurement. It is TEST-ONLY (`#[cfg(test)]` at
//! the `mod` site) and has no production callers: it exists so recall is a
//! number that regresses loudly, not a thing nobody checks.
//!
//! ## What it measures
//!
//! `route()` reaches a capability ONLY through a deterministic, pure classifier
//! — every specialized handler in the chain (`daemon/src/router.rs`, the
//! `if let Some(..) = crate::<module>::classify_*(text)` arms) is gated on one.
//! If the classifier returns `None`, the turn falls through to the on-device
//! LLM classifier, whose whole taxonomy is eleven intents
//! (`inference/prompts/intent_classifier.txt`) — none of which is vision, nexus,
//! mark-forge, silicon-canvas, lumen, charts, reports, macros, runbooks,
//! notebooks, rewind, aperture or the rest. A `None` from one of these gates is
//! therefore not a soft degrade: the capability is UNREACHABLE for that
//! utterance.
//!
//! So the gate IS the capability, and gate recall IS router recall.
//!
//! ## Two fixtures, two directions
//!
//!   * `fixtures/router_recall.json` — labelled probes: natural owner phrasings
//!     for each capability (paraphrases and indirect requests, deliberately NOT
//!     the exact trigger words the classifiers were written against). Each names
//!     the gate it must reach and the variant it must resolve to.
//!   * `fixtures/router_ordinary.json` — ordinary speech that must reach NO
//!     gate. This is the precision side, kept beside recall on purpose: every
//!     recall widening risks re-opening the hijack the campaign closed, and the
//!     two tests fail in opposite directions.
//!
//! Both are CHECKED IN so the numbers are reproducible and a regression is a red
//! test rather than an anecdote.
//!
//! ## WHAT A MISS DOES TODAY, and what it cannot be turned into
//!
//! A miss is not a soft degrade. `route()` hands the turn to the on-device intent
//! classifier, whose ELEVEN intents contain nothing for these capabilities, so the
//! turn is answered as CONVERSATION: on the shipped default a cloud persona
//! completion returned to `main` and spoken there, or — offline / on a cloud error
//! — generated AND spoken inside `route()` by the streamed `converse_speak`. Either
//! way the owner hears a generic answer ABOUT the thing DARWIN could have done.
//!
//! `miss_offer.rs` measured whether a cheap on-device "did you mean" could name the
//! real capability instead, using an index of 534 phrases harvested from this
//! repo's own production source and verified to fire their gate. MEASURED NO-GO: no
//! similarity threshold is both honest (zero suggestions across all 172 ordinary
//! utterances) and useful (>= 5 correct offers on the 55 misses); the BEST
//! zero-false threshold (T=0.94) yields ONE correct offer out of 55 beside one
//! wrong-capability offer, and above T=0.96 only the wrong one is left. The
//! reason is structural and is recorded there.
//!
//! TWO OWNER DECISIONS came out of that work and are NAMED, NOT TAKEN:
//!   * the "did you mean" that WOULD work is the gate classifiers reporting their
//!     own near-miss ("you named a macro operation but no macro"), not a second
//!     weaker classifier beside them — a change to 35 classifier signatures;
//!   * eight of the gates below (`describe`, `genimage`, `sound`, `silicon`,
//!     `lumen`, `vision`, `nexus`, `markforge`) are consulted in `route()` BELOW
//!     the cloud tool loop and the conversation branch, both of which return on
//!     success. This harness fires them in ISOLATION and so cannot see it: on the
//!     shipped cloud-enabled config an utterance the intent classifier labels
//!     "conversation" never reaches them, while the same utterance offline
//!     actuates. Hoisting them would change when camera capture and screen reads
//!     can fire, which is a posture decision (see the comment at that seam in
//!     `router.rs`).

use serde::Deserialize;

/// One labelled recall probe: an utterance a reasonable owner would say, the
/// gate it must reach, and the variant that gate must resolve to.
#[derive(Debug, Deserialize)]
pub struct Probe {
    /// The gate id (see [`fire`]) this utterance must reach.
    pub gate: String,
    /// The normalized outcome token the gate must produce (see [`fire`]).
    pub expect: String,
    /// The utterance itself, as a person would say it out loud.
    pub text: String,
}

/// The checked-in probe set.
pub const RECALL_FIXTURE: &str = include_str!("../fixtures/router_recall.json");
/// The checked-in ordinary-speech corpus (must reach NO gate).
pub const ORDINARY_FIXTURE: &str = include_str!("../fixtures/router_ordinary.json");

/// Every gate id this harness knows how to fire, in the order `route()` (and,
/// for the last four, `main.rs`) consults them. Used to enumerate cross-gate
/// capture: a probe that fires a gate it did not name is a capability stealing
/// another capability's utterance.
pub const GATES: &[&str] = &[
    "lockdown.panic",
    "lockdown.unlock",
    "policy",
    "model_tier",
    "whisper",
    "vault",
    "macros",
    "runbook",
    "undo",
    "aperture",
    "screen_context",
    "pasteboard",
    "notebook",
    "precog",
    "report",
    "chart",
    "peek",
    "music",
    "lifelog",
    "rewind",
    "explain",
    "mirror",
    "rollcall",
    "agentquery",
    "describe",
    "genimage",
    "sound",
    "silicon",
    "lumen",
    "vision",
    "nexus",
    "markforge",
    "voiceid",
    "voiceclone",
    "guest",
];

/// Pull the `op` field out of one of the app op-lines the router builds
/// (`{"op":"select.net",..}` for Silicon Canvas / Nexus / Mark-Forge, and the
/// `{"type":"op","op":"watch.start",..}` envelope for Vision). Both carry `op`.
fn op_of(line: &str) -> String {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|v| v.get("op").and_then(|o| o.as_str()).map(str::to_string))
        .unwrap_or_else(|| "?".to_string())
}

/// Run ONE gate against `text`, returning the normalized outcome token when the
/// gate fires and `None` when the turn falls through it.
///
/// The token vocabulary is the gate's own variant set, lowercased — `on`/`off`,
/// `recall`/`forget`, the app op name (`watch.start`, `select.net`, …), or the
/// gate id itself for the boolean gates that carry no variant.
///
/// PURE: every arm is a side-effect-free classifier, exactly as
/// `guest_denied_fast_path` relies on. Clock-taking classifiers are given the
/// real local clock (the same one `route()` hands them).
pub fn fire(gate: &str, text: &str) -> Option<String> {
    let now = chrono::Local::now();
    match gate {
        "lockdown.panic" => crate::lockdown::is_panic_intent(text).then(|| "panic".to_string()),
        "lockdown.unlock" => crate::lockdown::is_unlock_intent(text).then(|| "unlock".to_string()),
        "policy" => crate::policy::classify_policy_command(text).map(|_| "policy".to_string()),
        "model_tier" => crate::model_tier::classify_model_swap(text).map(|i| {
            use crate::model_tier::ModelSwapIntent as M;
            match i {
                M::Heavy => "heavy",
                M::Fast => "fast",
                M::Local => "local",
                M::Auto => "auto",
            }
            .to_string()
        }),
        "whisper" => crate::prosody::parse_whisper_command(text).map(|c| {
            use crate::prosody::WhisperCommand as W;
            match c {
                W::On => "on",
                W::Off => "off",
            }
            .to_string()
        }),
        "vault" => crate::vault::classify_vault_command(text).map(|c| {
            use crate::vault::VaultCommand as V;
            match c {
                V::On => "on",
                V::Off => "off",
            }
            .to_string()
        }),
        "macros" => crate::macros::classify_macro_command(text).map(|c| {
            use crate::macros::MacroCommand as M;
            match c {
                M::StartRecording { .. } => "record",
                M::StopRecording => "stop",
                M::Replay { .. } => "replay",
                M::List => "list",
                M::Forget { .. } => "forget",
            }
            .to_string()
        }),
        "runbook" => crate::runbook::classify_runbook_command(text).map(|c| {
            use crate::runbook::RunbookCommand as R;
            match c {
                R::Plan { .. } => "plan",
                R::Run { .. } => "run",
            }
            .to_string()
        }),
        "undo" => crate::journal::classify_undo_command(text).map(|c| {
            use crate::journal::UndoCommand as U;
            match c {
                U::UndoLast => "undo",
                U::Status => "status",
            }
            .to_string()
        }),
        "aperture" => crate::aperture::classify_aperture_intent(text, &now).map(|i| {
            use crate::aperture::ApertureIntent as A;
            match i {
                A::Recall(_) => "recall",
                A::Forget => "forget",
            }
            .to_string()
        }),
        "screen_context" => crate::screen_context::classify_screen_context_intent(text).map(|i| {
            use crate::screen_context::ScreenContextIntent as S;
            match i {
                S::Recall { .. } => "recall",
                S::Forget => "forget",
            }
            .to_string()
        }),
        "pasteboard" => crate::pasteboard::classify_pasteboard_intent(text).map(|i| {
            use crate::pasteboard::PasteboardIntent as P;
            match i {
                P::Recall { .. } => "recall",
                P::Forget => "forget",
            }
            .to_string()
        }),
        "notebook" => crate::notebook::classify_notebook_intent(text).map(|i| {
            use crate::notebook::NotebookIntent as N;
            match i {
                N::Save { .. } => "save",
                N::Revisit { .. } => "revisit",
                N::List => "list",
                N::Forget { .. } => "forget",
            }
            .to_string()
        }),
        "precog" => crate::simulate::extract_hypothetical(text).map(|_| "hypothetical".to_string()),
        "report" => crate::report::classify_report_intent(text).map(|_| "report".to_string()),
        "chart" => crate::chart::classify_chart_intent(text).map(|_| "chart".to_string()),
        "peek" => crate::artifact::classify_peek_intent(text).then(|| "peek".to_string()),
        "music" => crate::router::classify_music_intent(text).map(|_| "music".to_string()),
        "lifelog" => crate::lifelog::classify_lifelog_intent(text).map(|i| {
            let crate::lifelog::LifeLogIntent::Digest(p) = i;
            match p {
                crate::lifelog::Period::Day => "day",
                crate::lifelog::Period::Week => "week",
            }
            .to_string()
        }),
        "rewind" => crate::rewind::classify_rewind_intent(text, now.fixed_offset())
            .map(|_| "rewind".to_string()),
        "explain" => crate::explain::classify_explain_intent(text).map(|q| {
            use crate::explain::ExplainQuery as E;
            match q {
                E::Last => "last",
                E::Agent(_) => "agent",
            }
            .to_string()
        }),
        "mirror" => crate::user_model::classify_mirror_intent(text).map(|i| {
            use crate::user_model::MirrorIntent as M;
            match i {
                M::Explain(_) => "explain",
                M::Contest(_) => "contest",
                M::Clear(_) => "clear",
            }
            .to_string()
        }),
        "rollcall" => crate::agents::is_roll_call(text).then(|| "rollcall".to_string()),
        "agentquery" => crate::agents::is_agent_query(text).then(|| "agentquery".to_string()),
        "describe" => crate::router::describe_command(text).map(|r| {
            use crate::router::DescribeRequest as D;
            match r {
                D::Screen { .. } => "screen",
                D::Image { .. } => "image",
            }
            .to_string()
        }),
        "genimage" => crate::router::generate_image_command(text).map(|_| "genimage".to_string()),
        "sound" => crate::router::is_identify_sound_request(text).then(|| "sound".to_string()),
        "silicon" => crate::router::silicon_canvas_command(text).map(|c| {
            use crate::router::SiliconCanvasCommand as S;
            match c {
                S::Launch => "launch".to_string(),
                S::Op(line) => op_of(&line),
            }
        }),
        "lumen" => crate::router::lumen_command(text).map(|c| {
            use crate::router::LumenCommand as L;
            match c {
                L::Read => "read",
                L::Act(_) => "act",
            }
            .to_string()
        }),
        "vision" => crate::router::vision_command(text).map(|c| {
            use crate::router::VisionCommand as V;
            match c {
                V::Launch => "launch".to_string(),
                V::Op(line) => op_of(&line),
            }
        }),
        "nexus" => crate::router::nexus_command(text).map(|c| {
            use crate::router::NexusCommand as N;
            match c {
                N::Launch => "launch".to_string(),
                N::Op(line) => op_of(&line),
            }
        }),
        "markforge" => crate::router::mark_forge_command(text).map(|c| {
            use crate::router::MarkForgeCommand as M;
            match c {
                M::Launch => "launch".to_string(),
                M::Op(line) => op_of(&line),
            }
        }),
        "voiceid" => crate::voiceid::classify_intent(text).map(|i| {
            use crate::voiceid::VoiceIntent as V;
            match i {
                V::Enroll => "enroll",
                V::Forget => "forget",
            }
            .to_string()
        }),
        "voiceclone" => crate::voiceclone::classify_intent(text).map(|i| {
            use crate::voiceclone::CloneIntent as C;
            match i {
                C::Clone => "clone",
                C::Forget => "forget",
            }
            .to_string()
        }),
        "guest" => crate::threshold::classify_guest_toggle(text).map(|t| {
            use crate::threshold::GuestToggle as G;
            match t {
                G::On => "on",
                G::Off => "off",
            }
            .to_string()
        }),
        other => {
            // A fixture naming a gate this harness cannot fire would otherwise
            // count as a silent miss forever. Make it loud.
            panic!("recall fixture names an unknown gate {other:?}; add it to fire()/GATES");
        }
    }
}

/// Every gate that fires on `text`, as `(gate, token)` pairs in [`GATES`] order.
pub fn all_hits(text: &str) -> Vec<(&'static str, String)> {
    GATES
        .iter()
        .filter_map(|g| fire(g, text).map(|tok| (*g, tok)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn probes() -> Vec<Probe> {
        serde_json::from_str(RECALL_FIXTURE).expect("router_recall.json must parse")
    }

    fn ordinary() -> Vec<String> {
        serde_json::from_str(ORDINARY_FIXTURE).expect("router_ordinary.json must parse")
    }

    /// The fixture must be non-trivial and must cover EVERY gate the harness
    /// knows about — a recall number computed over a hand-picked subset of
    /// capabilities is not a recall number. (A hand-picked subset already
    /// misled this campaign once: "6 of 6 vacuous" became 5 of 79 on full
    /// enumeration.)
    #[test]
    fn recall_fixture_covers_every_gate() {
        let ps = probes();
        assert!(ps.len() >= 200, "probe set is too small: {}", ps.len());
        let covered: std::collections::BTreeSet<&str> =
            ps.iter().map(|p| p.gate.as_str()).collect();
        let missing: Vec<&&str> = GATES.iter().filter(|g| !covered.contains(**g)).collect();
        assert!(missing.is_empty(), "gates with no probe at all: {missing:?}");
    }

    /// THE MEASUREMENT. Prints per-capability recall and asserts the floor.
    ///
    /// The floor is the number MEASURED when this test was written, not an
    /// aspiration: it exists so a later "precision" tightening that quietly
    /// costs recall turns this test red instead of shipping.
    #[test]
    fn router_recall_does_not_regress() {
        let ps = probes();
        let mut per: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
        let mut misses: Vec<String> = Vec::new();
        for p in &ps {
            let got = fire(&p.gate, &p.text);
            let ok = got.as_deref() == Some(p.expect.as_str());
            let e = per.entry(GATES.iter().find(|g| **g == p.gate).copied().unwrap_or("?"))
                .or_insert((0, 0));
            e.1 += 1;
            if ok {
                e.0 += 1;
            } else {
                let stolen = all_hits(&p.text);
                misses.push(format!(
                    "  {gate:<16} want {want:<16} got {got:<16} :: {text:?}{extra}",
                    gate = p.gate,
                    want = p.expect,
                    got = got.as_deref().unwrap_or("NOTHING"),
                    text = p.text,
                    extra = if stolen.is_empty() {
                        String::new()
                    } else {
                        format!("  [also fires: {stolen:?}]")
                    },
                ));
            }
        }
        let hit: usize = per.values().map(|(h, _)| *h).sum();
        let total: usize = per.values().map(|(_, t)| *t).sum();
        eprintln!("\n=== ROUTER RECALL: {hit}/{total} ===");
        for (gate, (h, t)) in &per {
            eprintln!("  {gate:<16} {h}/{t}");
        }
        if !misses.is_empty() {
            eprintln!("--- misses ---");
            for m in &misses {
                eprintln!("{m}");
            }
        }
        // MEASURED FLOOR. At HEAD 39c8101 this was 129/200 (64.5%) — the first
        // time router recall had a number at all. Three fixes (macros + runbooks
        // accepting the name BEFORE the noun, and the undo phrase lists holding
        // the phrasings people use) took it to 142/200 (71.0%) with the
        // ordinary-speech corpus still at 0 captures. The remaining 58 misses are
        // documented, not forgotten: several are deliberate refusals (the camera
        // watch is not widened on purpose — turning capture ON is a posture
        // decision, not a recall fix), and several are cross-gate precedence
        // rather than a dead end.
        //
        // NAMED, NOT FIXED — one entry in the miss list above is worse than a
        // miss and is an OWNER DECISION rather than a recall fix. "disengage
        // lockdown" does not merely fail to unlock: it fires
        // `lockdown::is_panic_intent` and ENGAGES the panic lockdown. The token
        // "lockdown" is a PANIC phrase, and the unlock veto inside
        // `is_panic_intent` only spares the spellings listed in `UNLOCK_PHRASES`
        // ("unlock", "resume normal", "end/lift/exit lockdown"). "disengage" is
        // not one of them, so the utterance that means STOP LOCKING lands as LOCK.
        // The miss list prints it as `[also fires: [("lockdown.panic","panic")]]`.
        // Closing it means adding a phrase to the EMERGENCY STOP's exit path,
        // which is a posture decision — reported here, not taken.
        //
        // Raise this ONLY with a fresh measurement. Lowering it is the regression
        // this test exists to catch.
        const FLOOR_HIT: usize = 142;
        assert!(
            hit >= FLOOR_HIT,
            "router recall regressed: {hit}/{total} < {FLOOR_HIT}"
        );
    }

    /// A "HIT" MUST ALSO WIN.
    ///
    /// [`GATES`] is `route()`'s own order and `route()` takes the FIRST gate that
    /// fires, but [`router_recall_does_not_regress`] scores each probe against its
    /// named gate IN ISOLATION. Those two facts disagree whenever an EARLIER gate
    /// also fires: the probe counts as a hit while the utterance actually reaches
    /// somebody else. The recall number is optimistic by exactly the number of
    /// shadowed hits, and this test is what holds that number at zero.
    ///
    /// MEASURED: one existed when the fixture was written. "read my screen" was
    /// labelled `vision`/`read.screen`, and both `lumen_command` and
    /// `vision_command` fire on it — with `route()` dispatching Lumen BEFORE
    /// Vision deliberately ("a control read/act is Lumen's"). The LABEL was
    /// corrected, not the router; the total is unchanged because the probe still
    /// hits under its true owner.
    ///
    /// CAVEAT, so this is not read as more than it is: `voiceid`, `voiceclone` and
    /// `guest` are consulted in `main.rs` BEFORE `route()` yet sit at the END of
    /// [`GATES`]. No probe currently straddles that difference — but this test
    /// would not see it if one did, so their position is documentation debt.
    #[test]
    fn no_recall_hit_is_shadowed_by_an_earlier_gate() {
        let mut shadowed: Vec<String> = Vec::new();
        for p in probes() {
            if fire(&p.gate, &p.text).as_deref() != Some(p.expect.as_str()) {
                continue;
            }
            let hits = all_hits(&p.text);
            let first = hits.first().map(|(g, _)| *g).unwrap_or("?");
            if first != p.gate {
                shadowed.push(format!(
                    "  {:?} is labelled {} but {first} fires first :: {hits:?}",
                    p.text, p.gate
                ));
            }
        }
        assert!(
            shadowed.is_empty(),
            "a probe scored as a hit is shadowed by an earlier gate, so the recall \
             number is optimistic:\n{}",
            shadowed.join("\n")
        );
    }

    /// THE PRECISION SIDE. Ordinary speech must reach NO gate. This is the test
    /// that goes red when a recall widening re-opens the hijack the campaign
    /// closed (317 ordinary utterances routed into an app the user never named;
    /// a tornado-watch question turned the CAMERA ON).
    #[test]
    fn ordinary_speech_reaches_no_gate() {
        let mut captured: Vec<String> = Vec::new();
        for text in ordinary() {
            let hits = all_hits(&text);
            if !hits.is_empty() {
                captured.push(format!("  {text:?} -> {hits:?}"));
            }
        }
        assert!(
            captured.is_empty(),
            "ordinary speech was captured by a capability gate:\n{}",
            captured.join("\n")
        );
    }
}
