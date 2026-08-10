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
/// GATES THAT `route()` CONSULTS ONLY BELOW ITS TWO CLOUD EARLY-RETURNS.
///
/// This measurement calls [`fire`] directly, which is the right way to measure a
/// CLASSIFIER — but it is not the same question as "can the owner reach this on
/// the shipped config". On `[router].conversation_route = "cloud_heavy"` (the
/// default) with a reachable cloud, an utterance the on-device classifier labels
/// "conversation" is answered by `complete_persona` and RETURNS, and this seam is
/// never reached; the SAME utterance offline falls through and actuates. The
/// cloud tool catalogue carries `open_app`/`quit_app` but no describe,
/// generate-image, identify-sound, Silicon-Canvas, Lumen-read, Vision, Nexus or
/// Mark-Forge op, so nothing upstream substitutes for it (router.rs documents the
/// trace).
///
/// So the headline number is CLASSIFIER recall. Printing the preempted share
/// beside it stops that number from being read as shipped reachability — which is
/// exactly the "right and misleading" shape this campaign keeps finding.
pub const CLOUD_PREEMPTED: &[&str] = &[
    "describe", "genimage", "sound", "silicon", "lumen", "vision", "nexus", "markforge",
];

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

    /// HIJACKS THAT ARE STILL OPEN, AS A RATCHET.
    ///
    /// These fire TODAY. They are invisible on the shipped config because the
    /// cloud conversation branch answers first (see [`CLOUD_PREEMPTED`]) — but
    /// OFFLINE, in VAULT mode, and for a GUEST, `route()` falls through and these
    /// gates are consulted, so an ordinary sentence renders an image or drives the
    /// physics engine. That is precisely the population that cannot fall back on
    /// the cloud answering first.
    ///
    /// They are NOT in `router_ordinary.json` because that fixture asserts ZERO
    /// captures and these are known-bad; burying them there would turn the suite
    /// red without telling anyone what to fix. Instead this PRINTS them and
    /// ratchets: the count may shrink, never grow. Closing one means deleting its
    /// line and moving the sentence into the ordinary corpus.
    ///
    /// FOUR OF THE ORIGINAL SEVEN ARE NOW CLOSED and live in
    /// `router_ordinary.json`, where the corpus enforces them: the two markforge
    /// STEP/PAUSE sentences and the two markforge LAUNCH sentences. Their branches
    /// got the closed-vocabulary context their siblings (world reset, bare spawn)
    /// already carried. The same pass enumerated all 14 branches of the three
    /// classifiers named here and found 9 defective; 25 further ordinary sentences
    /// went into the corpus with them (markforge launch/step/pause/gravity/spawn/
    /// state, silicon launch), 29 in all.
    ///
    /// RECALL WAS UNCHANGED AT 191/202, PER-GATE TOO — AND THAT IS A STATEMENT
    /// ABOUT THE FIXTURE, NOT ABOUT THE BRANCHES. `router_recall.json` carries 6
    /// markforge probes for 7 branches and 10 silicon probes, and every one of
    /// them either NAMES the app ("open mark forge", "reset the physics world") or
    /// is exactly the bare idiom the new closed vocabularies were written around
    /// ("spawn a cube", "advance 5 frames"), so the fixture cannot see what those
    /// vocabularies drop. Measured separately, against 49 constructed commands the
    /// same branches SERVED at HEAD: 27 are now refused. One adjective or one
    /// trailing "right now" beside the loose noun is enough —
    ///   "drop a big box in the sandbox"        "spawn a red cube in the sandbox"
    ///   "add a crate to the sandbox"           "pause the simulation for a second"
    ///   "step the simulation forward by two frames"   (digits pass, number WORDS
    ///                                                  are not in the step list)
    ///   "turn off gravity in the sandbox"      (the gravity list holds no locus
    ///                                           noun at all: 5 of 8 lost)
    ///   "what is the current physics state"    "show me the sandbox right now"
    ///   "open the schematic right now"         (`NEXUS_BARE_GAIN_VOCAB` in
    ///                                           router.rs records this exact
    ///                                           lesson: "set the gain to -6 RIGHT
    ///                                           NOW died while set the gain to -6
    ///                                           worked")
    /// Naming the engine still works in every one of those cases ("drop a big box
    /// in the PHYSICS sandbox"). That is the price of the precision; it is not
    /// zero, and it is not visible in the number above. Buying some of it back is
    /// cheap and safe in shape — each of these lists is ANDed AFTER its branch's
    /// existing gate, so a word added to one can only move behaviour back toward
    /// HEAD, never past it — but which phrasings are worth re-admitting on a branch
    /// that WRITES A GRAVITY VECTOR or SPAWNS A BODY is an owner judgement, not an
    /// agent's, so it is recorded here rather than taken.
    ///
    /// WHAT REMAINS IS GENIMAGE, AND IT IS NOT A VOCABULARY PROBLEM.
    /// A request to DARWIN is an IMPERATIVE; present-tense narration reuses the
    /// SAME base verb form in the SAME verb-object shape, so "draw a picture of X"
    /// and "we draw a picture of X every christmas" are lexically identical up to
    /// the subject. Rule 2 in `image_noun_is_commanded` already drops the inflected
    /// forms (drew / draws / painted / created), which is why only 1st/2nd-person
    /// and plural narration survives — exactly these.
    ///
    /// A subject-pronoun rule ("we"/"you"/"they"/"I" immediately before the verb,
    /// absent subject-auxiliary inversion) would close all three. IT WAS MEASURED
    /// AND NOT TAKEN: it closes the PRONOUN class and leaves the equally ordinary
    /// PLURAL-NOUN class wide open — "the kids draw a picture of the dog every
    /// week" and "they make art with recycled bottles at the co-op" still fire, as
    /// does "i paint a picture of the coast every summer" if the pronoun list is
    /// bounded any tighter. Closing three named sentences while an identical
    /// sentence one noun away still renders an image would make this ratchet read
    /// CLOSED while the hole stays open — the "right and misleading" shape this
    /// campaign keeps finding. Separating them needs a POS tagger (or the
    /// classifier reporting its own near-miss), not another list. Left visible on
    /// purpose.
    const KNOWN_OPEN_HIJACKS: &[&str] = &[
        "we make art with the kids on saturdays",
        "you cannot paint a picture with only one color",
        "we draw a picture of the family every christmas",
        // NOT one of the original seven — measured by the same enumeration and
        // added here rather than left unstated, because it is the class the
        // pronoun rule above would NOT have closed. The list is honest about its
        // own breadth or it is not a ratchet.
        "the kids draw a picture of the dog every week",
    ];

    #[test]
    fn known_open_hijacks_only_ever_shrink() {
        let still: Vec<&str> = KNOWN_OPEN_HIJACKS
            .iter()
            .copied()
            .filter(|s| !super::all_hits(s).is_empty())
            .collect();
        eprintln!("\n=== KNOWN-OPEN HIJACKS still firing: {}/{} ===", still.len(), KNOWN_OPEN_HIJACKS.len());
        for s in &still {
            eprintln!("  {:?} -> {:?}", s, super::all_hits(s));
        }
        assert!(
            still.len() <= KNOWN_OPEN_HIJACKS.len(),
            "the known-open list grew; add the sentence or close the gate"
        );
        // ...and the list must not rot into a lie in the other direction: a
        // sentence that no longer fires belongs in the ordinary corpus, where it
        // is actually enforced, not sitting here looking unfixed.
        let fixed: Vec<&&str> = KNOWN_OPEN_HIJACKS
            .iter()
            .filter(|s| super::all_hits(s).is_empty())
            .collect();
        assert!(
            fixed.is_empty(),
            "these no longer hijack — move them to router_ordinary.json so the \
             corpus enforces it, and delete them here: {fixed:?}"
        );
    }

    /// CLOUD_PREEMPTED must name real gates, or the caveat printed beside the
    /// headline is measuring nothing. A typo or a renamed gate would silently
    /// drop that gate out of the warning while it stays just as unreachable —
    /// the caveat quietly becoming smaller than the truth.
    #[test]
    fn every_cloud_preempted_name_is_a_real_gate() {
        assert!(!super::CLOUD_PREEMPTED.is_empty(), "the caveat covers nothing");
        for g in super::CLOUD_PREEMPTED {
            assert!(
                super::GATES.contains(g),
                "CLOUD_PREEMPTED names {g:?}, which is not a gate in GATES — the \
                 printed caveat is silently smaller than the truth"
            );
        }
        // And every one must actually carry probes, or it is not part of the
        // percentage it claims to describe.
        let probes = super::tests::probes();
        for g in super::CLOUD_PREEMPTED {
            assert!(
                probes.iter().any(|p| &p.gate.as_str() == g),
                "CLOUD_PREEMPTED names {g:?} but no probe targets it"
            );
        }
    }
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
        // ...and the share of that number the owner cannot actually reach with a
        // cloud key set. Reported every run so the headline is never quoted alone.
        let pre_t: usize = per
            .iter()
            .filter(|(g, _)| CLOUD_PREEMPTED.contains(g))
            .map(|(_, (_, t))| *t)
            .sum();
        let pre_h: usize = per
            .iter()
            .filter(|(g, _)| CLOUD_PREEMPTED.contains(g))
            .map(|(_, (h, _))| *h)
            .sum();
        eprintln!(
            "    of which CLOUD-PREEMPTED (classifier hits, but route() never \
             consults these gates when the cloud answers): {pre_h}/{pre_t} \
             probes = {:.1}% of the fixture — {:?}. A hoist of the four \
             non-capture gates was measured and REFUSED (see the block above \
             `needs_deep_reasoning` in router.rs): five of the six branches that \
             blocked it are now closed, ONE (genimage's grammatical-person hole) \
             is not, and the other four gates each actuate a CAPTURE, which is an \
             owner consent decision",
            pre_t as f64 / total as f64 * 100.0,
            CLOUD_PREEMPTED,
        );
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
        // the phrasings people use) took it to 142/200, then 145/200 (72.5%).
        //
        // THE MISS-LIST SWEEP took it to 192/202 (95.0%). Every one of the 55
        // misses the harness printed was read against the code that produced it.
        // 45 of them are now hits — 43 closed by a classifier fix, and 2 by a
        // LABEL correction alone (the utterance already reached the capability
        // that is the right answer; see `no_recall_hit_is_shadowed_by_an_earlier
        // _gate` for all four label corrections). The remaining 10 are named here
        // rather than left to be rediscovered.
        //
        // The ordinary-speech corpus grew 172 -> 244 in the same change: every
        // widening contributed the ordinary sentences it could newly swallow, and
        // `ordinary_speech_reaches_no_gate` is still at 0 captures. Two probes
        // were ADDED for `MirrorIntent::Clear`, whose only two probes turned out
        // to be mislabelled contests — the variant would otherwise have been left
        // with no coverage at all.
        //
        // MEASURED, not inferred: HEAD (39c8101 + ff17b40) run against THIS
        // 202-probe fixture scores 149/202. So the code fixes are worth +43 and
        // the fixture changes account for the other +4 of the 145 -> 192 move.
        //
        // THE 10 THAT REMAIN, and why:
        //   * POSTURE, DELIBERATELY REFUSED (7). "turn on the camera" / "keep an
        //     eye on the camera" (arming a lens is a consent decision);
        //     "stand down" / "disengage lockdown" (loosening the emergency stop);
        //     "come out of vault mode" / "turn private mode off" (re-enabling
        //     cloud egress); "make a voice that sounds like me" (ships a sample
        //     to a third party). Each is a decision for the owner, not a recall
        //     fix. NOTE the SHIPPED direction of each pair IS closed: "turn the
        //     camera off" now stops the watch, because turning capture off
        //     removes a capability rather than granting one.
        //   * CAPABILITY DOES NOT COVER IT (2). "plot the last week of battery
        //     life" — `chart_from_snapshot` reads ONE telemetry snapshot (cpu %
        //     and memory %); there is no battery series and no history, so firing
        //     the gate would answer a battery question with a two-bar "System
        //     load" chart. "play some lo-fi music" — the only music capability is
        //     COMPOSE (`classify_music_intent` + an ElevenLabs generation);
        //     nothing plays a library, and the router's own doc refuses play
        //     requests on purpose. Both are honest NO-GOs, not phrase gaps.
        //   * ANCHORLESS (1). "who would handle a markets question" carries
        //     neither the word "agent" nor an agent's name, and "who handles X"
        //     is ordinary English ("who handles the payroll at your company").
        //     Admitting it would answer a question about the owner's colleagues
        //     out of DARWIN's roster.
        //
        // NAMED, NOT FIXED — "disengage lockdown" is worse than a miss and is an
        // OWNER DECISION. It does not merely fail to unlock: it fires
        // `lockdown::is_panic_intent` and ENGAGES the panic lockdown. The token
        // "lockdown" is a PANIC phrase, and the unlock veto inside
        // `is_panic_intent` only spares the spellings listed in `UNLOCK_PHRASES`
        // ("unlock", "resume normal", "end/lift/exit lockdown"). "disengage" is
        // not one of them, so the utterance that means STOP LOCKING lands as LOCK.
        // The miss list prints it as `[also fires: [("lockdown.panic","panic")]]`.
        // Closing it means adding a phrase to the EMERGENCY STOP's exit path,
        // which is a posture decision — reported here, not taken.
        //
        // A LABEL CORRECTION PUT ONE MISS BACK: 192 -> 191, and the -1 is not a
        // code regression.
        //
        // "private mode on" was labelled `vault`/`on`. The miss-list sweep
        // relabelled it `model_tier`/`local` and counted it as a hit. NO BEHAVIOUR
        // CHANGED — the utterance reached exactly what it reached before; the
        // RIGHT ANSWER was redefined to whatever it already hit, which is the one
        // move a recall number must never be allowed to make.
        //
        // It also settles, silently, what "private mode" MEANS — and the reason
        // recorded for that must be the code's, not the nearest reassuring
        // sentence. model_tier's `mod` banner does say it is "swap-only" and
        // "changes NO safety gate", but that banner is NOT the whole of `Local`
        // and quoting it alone gets this backwards: the same file calls `Local`
        // "a PRIVACY control" and warns that a missed "turn on private mode"
        // "sends the turn to the CLOUD after the user asked it not to", and
        // `resolve_tier`'s doc says a `Local` override means "NO cloud call is
        // made (the privacy path)". So model_tier/local DOES keep this turn's
        // completion off the cloud. It is not nothing.
        //
        // What vault adds on top is the part the phrase is worth: `deny_cloud` is
        // folded into BOTH cloud seams — the ACTUATING tool-loop gate as well as
        // the turn's `cloud_reachable` — and `boundary::gate_and_trim` forces
        // CUSTOMS to `TrimSpec::maximal()`, so vault also trims WHAT LEAVES on the
        // egress that remains, and it is a durable posture rather than a per-turn
        // model choice.
        //
        // THE LABEL IS BACK ON `vault` FOR THE BOOKKEEPING REASON, WHICH STANDS ON
        // ITS OWN: a recall number may not be raised by redefining the right
        // answer to whatever the utterance already reached, and the fixture's own
        // twin entry ("turn private mode off") is still `vault`/`off`. Which of
        // the two gates SHOULD own the phrase is the owner decision named below;
        // it is not settled here, and this comment does not pretend the losing
        // side does nothing.
        //
        // The fixture was also INTERNALLY INCONSISTENT, which is what makes the
        // relabel legible as score-keeping rather than a reading of the code:
        // "turn private mode off" is STILL labelled `vault`/`off` two entries
        // later, and is one of the excused misses below. The same two words owned
        // by two different gates, split exactly where it made ON a hit and left
        // OFF an excused miss.
        //
        // OWNER DECISION, NAMED NOT TAKEN: making "private mode" reach the vault
        // needs `model_tier` to stop claiming the phrase (it is consulted first
        // and would have to yield). That redirects a spoken phrase from a model
        // swap to a control that suppresses cloud egress. It TIGHTENS rather than
        // widens, but it is still a posture decision about what a word actuates,
        // so it is reported here rather than made.
        //
        // Raise this ONLY with a fresh measurement. Lowering it for any reason
        // other than a label correction of this kind is the regression this test
        // exists to catch.
        const FLOOR_HIT: usize = 191;
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
    /// THE MISS-LIST SWEEP corrected three more labels the same way, each because
    /// the utterance already reached a capability that IS the right answer:
    ///   * "what's on my screen" was labelled `describe`/`screen` (the VLM
    ///     caption). It is an OCR read — `vision_command`'s own doc says so
    ///     ("Checked before the presence status so 'what's on my screen' is an
    ///     OCR read") — and Lumen takes it first, for the same control-read
    ///     reason as "read my screen". Relabelled `lumen`/`read`.
    ///   * "private mode on" was labelled `vault`/`on`. `model_tier` owns
    ///     "private mode" deliberately and resolves it to `Local` ("Work offline
    ///     / on-device / privately — NO cloud call"), and `model_tier` is
    ///     consulted first. Relabelled `model_tier`/`local`.
    ///   * "forget what you think you know about my music taste" and "clear your
    ///     model of my interests" were labelled `mirror`/`clear`. `Clear` is the
    ///     UN-contest (it LIFTS a suppression so a belief may be re-derived);
    ///     both utterances ask for the belief to be DROPPED, which is `Contest`.
    ///     Relabelled, and two real `clear` probes added so the variant keeps
    ///     coverage.
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



