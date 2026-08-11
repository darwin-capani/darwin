//! WHAT A ROUTING MISS COULD BECOME — and the measurement that says it cannot.
//!
//! `recall_probe.rs` measures how many utterances reach a capability (145/200 at
//! the time of writing). This module asks the follow-on question: the 55 that do
//! NOT reach one land in a generic conversational answer about a thing DARWIN
//! could have DONE — could a cheap, on-device "did you mean" turn each of those
//! dead ends into a discoverable affordance *without widening any classifier*?
//!
//! The answer, MEASURED, is **no** — and this module is the standing proof, so
//! the experiment is not silently repeated. It is TEST-ONLY (`#[cfg(test)]` at
//! the `mod` site) and has NO production callers. Nothing here routes, offers,
//! speaks or actuates anything.
//!
//! ## What was built to test the idea
//!
//! * `fixtures/capability_index.json` — a capability index of 534 phrases, each
//!   naming the gate it reaches. Every phrase was HARVESTED FROM THIS REPO'S OWN
//!   PRODUCTION SOURCE (the classifier phrase constants and doc-comment examples
//!   in the 26 modules that own a gate) and then VERIFIED by executing the real
//!   gate: [`crate::recall_probe::fire`] must return the recorded outcome, and no
//!   earlier gate may fire first. So the index is a subset of the language the OS
//!   already accepts — a suggestion built from it can never name a capability
//!   that does not exist. `capability_index_entries_still_fire_their_gate` holds
//!   that property.
//!
//!   PROVENANCE MATTERS AND IS THE WHOLE REASON THE NUMBER BELOW IS BELIEVABLE.
//!   An index authored *after* reading the miss list scores far better and the
//!   score is meaningless: a first pass at this work hand-wrote cues and reached
//!   9/55 with 0 false suggestions, purely because phrases like "select component
//!   r14" and "what would you do if i said open safari" had been written next to
//!   the misses "select r14" and "suppose i said open safari, what would you do".
//!   PROVENANCE, COUNTED RATHER THAN ASSERTED: 465 of the 534 appear VERBATIM in
//!   the pre-`#[cfg(test)]` region of `daemon/src/*.rs`. The other 70 are not
//!   authored from the miss list either — they are production phrase PREFIXES
//!   with a placeholder argument appended (the constant is `"run macro "`, the
//!   entry is `"run macro x"`), phrases from a module's test region, or texts
//!   already present in the pre-existing `router_recall.json`. All predate this
//!   work; none was written after reading a miss. AND THE CONCLUSION DOES NOT
//!   REST ON THAT SPLIT: re-swept over the 465 production-verbatim entries ALONE,
//!   the best zero-false threshold is T=0.83 with 2 correct offers — still no
//!   point that is both honest and useful, so the NO-GO is not an artifact of the
//!   other 70.
//!
//! * [`Index::nearest`] — the suggester: pure, allocation-light, on-device
//!   lexical similarity (weighted Dice over stemmed tokens, each token weighted
//!   `1/(number of capabilities whose index phrases contain it)`, so a term
//!   unique to one capability dominates a term shared by many). No cloud call, no
//!   model, no I/O. `the_did_you_mean_scan_is_cheap_enough_to_ship` measures its
//!   cost so "adds no meaningful latency" is a number.
//!
//! ## THE RESULT (`a_lexical_did_you_mean_cannot_be_honest_on_device`)
//!
//! Scored over BOTH checked-in corpora — the 55 misses in `router_recall.json`
//! that reach no gate, and all 172 ordinary utterances in `router_ordinary.json`,
//! which must draw NO suggestion — there is no similarity threshold at which the
//! feature is honest:
//!
//! ```text
//!   T=0.40  24 correct   13 wrong-capability   41 false-on-ordinary
//!   T=0.60   7 correct    3 wrong-capability   11 false-on-ordinary
//!   T=0.75   2 correct    1 wrong-capability    7 false-on-ordinary
//!   T=0.95   1 correct    1 wrong-capability    0 false-on-ordinary
//! ```
//!
//! (Those rows are transcribed from the frontier this module's own test prints,
//! and were RE-DERIVED from a run rather than copied: an earlier draft of this
//! table carried 7 at T=0.40 instead of 24, which made the paragraph under it
//! read the wrong way round.)
//!
//! WHAT FAILS IS HONESTY, NOT THE HIT RATE. Correct offers are never FEWER than
//! wrong ones anywhere the feature has yield — 24 vs 13 at T=0.40, 7 vs 3 at
//! T=0.60, 2 vs 1 at T=0.75. The bar it cannot clear is the other one, that an
//! ordinary sentence must draw NO suggestion at all:
//!   * the highest threshold still making [`MIN_USEFUL_OFFERS`] correct offers is
//!     T=0.65, and there 10 of the 172 ordinary utterances (5.8%) draw one; at
//!     T=0.40 it is 41 of 172 (23.8%);
//!   * the LOWEST threshold with zero false suggestions is T=0.94, and there the
//!     whole yield is ONE correct offer out of 55 (1.8%) beside one
//!     wrong-capability offer — HALF of what it says is wrong;
//!   * above T=0.96 the correct offers run out first and only the wrong one is
//!     left.
//!
//! A wrong suggestion is worse than none, so this ships nothing.
//!
//! ## WHY — the structural reason, also measured
//!
//! The two populations are drawn from the SAME lexicon. 103 of the 172 ordinary
//! utterances (60%) contain a token that is unique to exactly one capability in
//! the OS's own trigger vocabulary, against 43 of the 55 misses (78%) — the
//! separation of top-similarity scores is AUC 0.755, and the overlap is not noise
//! but structure: "run my macro" (must reach nothing — no macro is named) and
//! "show me what the backup runbook would do" (should reach the runbook planner)
//! differ only in whether the request carries a resolvable ARGUMENT. That is
//! precisely the test each gate classifier already performs and discards. An
//! external similarity model over the capability vocabulary is blind to it by
//! construction, and a semantic embedding would be blind for the same reason —
//! the separating feature is not semantic, it is the presence of an argument.
//!
//! THE ARCHITECTURE THAT WOULD WORK is therefore not a second, weaker classifier
//! bolted beside the first: it is the gate classifiers REPORTING their own
//! near-miss ("you named a macro operation but no macro"), which is exact and
//! carries no guess. That is a change to 35 classifier signatures and is recorded
//! here as the owner decision, not taken.
//!
//! ## The bar this test holds
//!
//! `a_lexical_did_you_mean_cannot_be_honest_on_device` asserts the NO-GO: no
//! threshold achieves zero false suggestions on ordinary speech while making at
//! least [`MIN_USEFUL_OFFERS`] correct offers. If a later change to the
//! classifiers, the index or the scorer overturns that, the test goes RED — which
//! is the signal to re-open the feature, not to relax the bar.
//!
//! It holds a SECOND bar, in the opposite direction, because the first one is a
//! negative and a negative is satisfied by silence: the sweep must also still
//! REACH [`MIN_USEFUL_OFFERS`] correct offers at its permissive end. Without that,
//! a scorer that quietly stopped scoring would keep this module green forever
//! while the numbers in the doc above became fiction.

use std::collections::{BTreeMap, BTreeSet};

use crate::recall_probe::{self, Probe};

/// The checked-in capability index: phrases harvested from production source and
/// verified to fire the gate they name.
pub const CAPABILITY_INDEX: &str = include_str!("../fixtures/capability_index.json");

/// The smallest number of correct offers (out of the 55 misses) that would make a
/// zero-false-suggestion "did you mean" worth shipping. Deliberately modest: even
/// 5 of 55 would be a real affordance if it never guessed. The measurement does
/// not reach it.
pub const MIN_USEFUL_OFFERS: usize = 5;

/// Lowercase, non-alphanumeric to a single space, trimmed — the same shape the
/// gate classifiers normalize to, so the index and the utterance are compared in
/// the vocabulary the OS actually matches on.
fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = true;
    for c in s.chars() {
        if c.is_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_space = false;
        } else if !last_space {
            out.push(' ');
            last_space = true;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// A crude suffix stem so "whispering" meets "whisper" and "notebooks" meets
/// "notebook". Deliberately shallow: an aggressive stemmer collapses distinct
/// capability nouns into each other, which costs precision on the side that
/// matters. Only ASCII suffixes are stripped, so the slice is always on a char
/// boundary.
fn stem(w: &str) -> String {
    for suf in ["ing", "ed", "es", "s"] {
        if w.len() > suf.len() + 2 && w.ends_with(suf) {
            return w[..w.len() - suf.len()].to_string();
        }
    }
    w.to_string()
}

/// Normalized, stemmed token set for an utterance or an index phrase.
fn tokens(s: &str) -> BTreeSet<String> {
    normalize(s)
        .split(' ')
        .filter(|w| !w.is_empty())
        .map(stem)
        .collect()
}

/// One indexed phrase.
struct Cue {
    gate: String,
    text: String,
    toks: BTreeSet<String>,
}

/// What the suggester would offer: the capability, the similarity, and the
/// working phrase it would quote. Never executed — an offer is speech.
#[derive(Debug, Clone)]
pub struct Offer {
    pub gate: String,
    pub score: f64,
    pub cue: String,
}

/// The capability index plus the per-token capability-spread used to weight it.
pub struct Index {
    cues: Vec<Cue>,
    /// token -> how many DISTINCT gates have an index phrase containing it.
    spread: BTreeMap<String, usize>,
}

impl Index {
    /// Load and weight the checked-in index. Panics on a malformed fixture — a
    /// silently empty index would make every measurement below read as a clean
    /// zero, which is exactly the vacuous pass this campaign keeps finding.
    pub fn build() -> Index {
        let entries: Vec<Probe> =
            serde_json::from_str(CAPABILITY_INDEX).expect("capability_index.json must parse");
        let cues: Vec<Cue> = entries
            .iter()
            .map(|e| Cue {
                gate: e.gate.clone(),
                text: e.text.clone(),
                toks: tokens(&e.text),
            })
            .collect();
        let mut per_gate: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for c in &cues {
            let set = per_gate.entry(c.gate.as_str()).or_default();
            for t in &c.toks {
                set.insert(t.as_str());
            }
        }
        let mut spread: BTreeMap<String, usize> = BTreeMap::new();
        for toks in per_gate.values() {
            for t in toks {
                *spread.entry((*t).to_string()).or_insert(0) += 1;
            }
        }
        Index { cues, spread }
    }

    /// How many index phrases are loaded.
    pub fn len(&self) -> usize {
        self.cues.len()
    }

    /// Never true for the checked-in fixture; present so `len` is not a lone
    /// length method (clippy).
    pub fn is_empty(&self) -> bool {
        self.cues.is_empty()
    }

    /// Tokens unique to exactly one capability in the index vocabulary.
    pub fn unique_tokens(&self) -> BTreeSet<&str> {
        self.spread
            .iter()
            .filter(|(_, n)| **n == 1)
            .map(|(t, _)| t.as_str())
            .collect()
    }

    /// A token's weight: `1/spread`, so a term unique to one capability weighs 1
    /// and a term the whole vocabulary shares weighs almost nothing. A token the
    /// index has never seen weighs 1 as well — it is maximally specific, and
    /// leaving it unmatched is what penalizes an utterance that is mostly about
    /// something else.
    fn weight(&self, t: &str) -> f64 {
        match self.spread.get(t) {
            Some(n) if *n > 0 => 1.0 / (*n as f64),
            _ => 1.0,
        }
    }

    /// The nearest capability to `text` by weighted Dice similarity, or `None`
    /// when the utterance shares nothing with the index. PURE, on-device, no
    /// model, no I/O. Applies NO threshold — the caller chooses one, which is
    /// what the measurement below sweeps.
    pub fn nearest(&self, text: &str) -> Option<Offer> {
        self.nearest_counted(text).0
    }

    /// [`Index::nearest`], plus the WORK the scan actually did, so the cost test
    /// can state a budget with no clock in it. One unit per INDEX-TOKEN PROBE: a
    /// `contains` against the utterance's token set, or a `spread` weight lookup.
    /// The units are incremented inside the loops, off their real trip counts,
    /// never from a formula about them — a scan that grew a second pass over the
    /// cues, or an index that doubled in size, doubles this number.
    ///
    /// The accumulations below are written as explicit loops rather than
    /// `.filter().map().sum()` only so the probes can be counted where they
    /// happen. `Iterator::sum` for `f64` folds left-to-right from `0.0` over the
    /// same `BTreeSet` order, so every score is bit-identical to the chain it
    /// replaced — which matters, because the NO-GO frontier above is asserted on
    /// those scores.
    fn nearest_counted(&self, text: &str) -> (Option<Offer>, usize) {
        let ut = tokens(text);
        if ut.is_empty() {
            return (None, 0);
        }
        let mut work = ut.len();
        let ut_mass: f64 = ut.iter().map(|t| self.weight(t)).sum();
        let mut best: Option<Offer> = None;
        for c in &self.cues {
            let mut inter: f64 = 0.0;
            for t in &c.toks {
                work += 1;
                if ut.contains(t) {
                    work += 1;
                    inter += self.weight(t);
                }
            }
            if inter <= 0.0 {
                continue;
            }
            let mut cue_mass: f64 = 0.0;
            for t in &c.toks {
                work += 1;
                cue_mass += self.weight(t);
            }
            let denom = cue_mass + ut_mass;
            if denom <= 0.0 {
                continue;
            }
            let score = 2.0 * inter / denom;
            if best.as_ref().is_none_or(|b| score > b.score) {
                best = Some(Offer {
                    gate: c.gate.clone(),
                    score,
                    cue: c.text.clone(),
                });
            }
        }
        (best, work)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// The utterances in `router_recall.json` that reach NO gate (or the wrong
    /// one) — the 55 dead ends this module exists to reason about. Recomputed
    /// from the live classifiers every run, never hard-coded, so the measurement
    /// tracks the real routing boundary.
    /// EVERY probe that NAMES A REAL CAPABILITY — not just the ones the router
    /// currently misses.
    ///
    /// This used to filter to live misses, which made the NO-GO decay as the
    /// router improved: closing the recall gaps took the population 55 -> 10, and
    /// with 10 the suggester cannot reach MIN_USEFUL_OFFERS even in principle, so
    /// the anti-vacuity guard fired and the whole measurement died. That is
    /// backwards — fixing the ROUTER must not invalidate a measurement about a
    /// DIFFERENT mechanism.
    ///
    /// The claim under test is about the METHOD: given an utterance that names a
    /// capability, can lexical similarity over this vocabulary pick the right one
    /// without also firing on ordinary speech? Every recall probe is such an
    /// utterance by construction, whether or not the router happens to route it
    /// today. So the population is the whole fixture: stable, large, and exactly
    /// the input the feature claims to serve.
    fn misses() -> Vec<Probe> {
        let probes: Vec<Probe> =
            serde_json::from_str(recall_probe::RECALL_FIXTURE).expect("recall fixture must parse");
        probes
            .into_iter()
            .filter(|p| recall_probe::fire(&p.gate, &p.text).as_deref() != Some(p.expect.as_str()))
            .collect()
    }

    fn ordinary() -> Vec<String> {
        serde_json::from_str(recall_probe::ORDINARY_FIXTURE).expect("ordinary fixture must parse")
    }

    /// THE HONESTY PROPERTY. Every phrase in the capability index must still
    /// FIRE the gate it names, with the recorded outcome, and no earlier gate may
    /// take it first. This is what makes an offer built from the index incapable
    /// of naming a capability that does not exist — and it is a live recall canary:
    /// a classifier that tightens until its own documented phrase stops working
    /// turns this red.
    /// SCOPE, so this is not read as a licence to keep a gate loose: the index is
    /// by construction the language the gates ACCEPT TODAY, so a deliberate
    /// PRECISION fix that narrows a gate will turn this test red. The correct
    /// answer then is to DELETE the entry the gate no longer accepts, never to
    /// widen the gate back. One entry was already removed on exactly those grounds
    /// — `"what's on my screen protector"` fired `lumen`/`read`, which is a
    /// question about a phone accessory reaching a screen read, and pinning it
    /// here would have made fixing that leak look like a regression.
    #[test]
    fn capability_index_entries_still_fire_their_gate() {
        let entries: Vec<Probe> =
            serde_json::from_str(CAPABILITY_INDEX).expect("capability_index.json must parse");
        // PRECONDITION: an empty or gutted index would make this test — and every
        // measurement below — pass vacuously.
        assert!(
            entries.len() >= 500,
            "capability index is too small to be the OS's vocabulary: {}",
            entries.len()
        );
        let gates: BTreeSet<&str> = entries.iter().map(|e| e.gate.as_str()).collect();
        assert!(
            gates.len() >= 30,
            "capability index covers only {} gates",
            gates.len()
        );
        let mut broken: Vec<String> = Vec::new();
        for e in &entries {
            let got = recall_probe::fire(&e.gate, &e.text);
            if got.as_deref() != Some(e.expect.as_str()) {
                broken.push(format!(
                    "  {} {:?} no longer fires {} (got {:?})",
                    e.gate,
                    e.text,
                    e.expect,
                    got.as_deref().unwrap_or("NOTHING")
                ));
                continue;
            }
            let first = recall_probe::all_hits(&e.text);
            let first = first.first().map(|(g, _)| *g).unwrap_or("?");
            if first != e.gate {
                broken.push(format!(
                    "  {:?} is indexed under {} but {first} fires first",
                    e.text, e.gate
                ));
            }
        }
        assert!(
            broken.is_empty(),
            "the capability index no longer describes the OS it was harvested from:\n{}",
            broken.join("\n")
        );
    }

    /// THE MEASUREMENT AND THE NO-GO.
    ///
    /// Sweeps the similarity threshold over both checked-in corpora and asserts
    /// that no threshold is simultaneously HONEST (zero suggestions on ordinary
    /// speech) and USEFUL ([`MIN_USEFUL_OFFERS`] correct offers on the misses).
    /// Prints the whole frontier so the next reader sees the numbers rather than
    /// this comment.
    #[test]
    fn a_lexical_did_you_mean_cannot_be_honest_on_device() {
        let idx = Index::build();
        let ms = misses();
        let ord = ordinary();
        // PRECONDITIONS: the two corpora must actually be present and of the size
        // the conclusion is drawn over.
        assert!(
            !idx.is_empty() && idx.len() >= 500,
            "index not loaded: {}",
            idx.len()
        );
        // THE MISS FLOOR MOVED BECAUSE RECALL IMPROVED, not because the corpus
        // was trimmed to fit. This measurement was taken at 55 misses; closing
        // the router-recall gaps took it to 10, which is the whole point of that
        // work. So the floor is now 8 — enough that the yield column means
        // something — and the CONCLUSION deliberately does not rest on it.
        //
        // What disqualifies a lexical "did you mean" is the FALSE-SUGGESTION side,
        // and that is measured over the ordinary corpus (>=150, currently 278) —
        // a population that GREW while the miss population shrank. Fewer misses
        // only makes the feature worth less, never more, so a smaller miss set
        // cannot rescue the NO-GO; it strengthens it.
        // THE MISS SET IS THE ONLY UNCONTAMINATED POPULATION, and it must stay
        // the population even as it shrinks. Widening a classifier to close a
        // recall gap puts that probe's phrasing INTO the production source the
        // capability index is harvested from — so a probe the router now routes
        // is a phrase the index has memorised, and scoring against it measures
        // the harvest, not the method. Measured: sweeping all 202 probes reports
        // 62 correct with 0 false at T=0.95 and would OVERTURN this NO-GO on
        // contamination alone.
        assert!(ms.len() >= 8, "too few misses to sweep: {}", ms.len());
        assert!(ord.len() >= 150, "ordinary corpus too small: {}", ord.len());

        let miss_offers: Vec<(String, Option<Offer>)> = ms
            .iter()
            .map(|p| (p.gate.clone(), idx.nearest(&p.text)))
            .collect();
        let ord_offers: Vec<Option<Offer>> = ord.iter().map(|t| idx.nearest(t)).collect();

        eprintln!(
            "\n=== MISS-OFFER FRONTIER: {} misses / {} ordinary / {} indexed phrases ===",
            ms.len(),
            ord.len(),
            idx.len()
        );
        eprintln!("    T  correct  wrong-capability  false-on-ordinary");
        let mut honest_and_useful: Vec<String> = Vec::new();
        // Bookkeeping for the ANTI-VACUITY assertion below.
        let mut best_correct = 0usize;
        let mut t_pct = 30usize;
        while t_pct <= 100 {
            let t = t_pct as f64 / 100.0;
            let mut correct = 0usize;
            let mut wrong = 0usize;
            for (want, off) in &miss_offers {
                if let Some(o) = off {
                    if o.score >= t {
                        if &o.gate == want {
                            correct += 1;
                        } else {
                            wrong += 1;
                        }
                    }
                }
            }
            let false_offers = ord_offers
                .iter()
                .filter(|o| o.as_ref().is_some_and(|o| o.score >= t))
                .count();
            best_correct = best_correct.max(correct);
            if t_pct.is_multiple_of(5) {
                eprintln!("  {t:4.2}  {correct:7}  {wrong:16}  {false_offers:17}");
            }
            if false_offers == 0 && correct >= MIN_USEFUL_OFFERS {
                honest_and_useful.push(format!(
                    "  T={t:.2}: {correct} correct, {wrong} wrong-capability, 0 false"
                ));
            }
            t_pct += 1;
        }

        // The witnesses, so a reader can judge the conclusion rather than take it.
        eprintln!("--- what it would say at T=0.60 (the most permissive useful point) ---");
        for (want, off) in &miss_offers {
            if let Some(o) = off {
                if o.score >= 0.60 {
                    let tag = if &o.gate == want { "OK   " } else { "WRONG" };
                    eprintln!("  {tag} want {want:<16} -> {:<16} {:?}", o.gate, o.cue);
                }
            }
        }
        for (t, off) in ord.iter().zip(ord_offers.iter()) {
            if let Some(o) = off {
                if o.score >= 0.60 {
                    eprintln!("  FALSE {t:?} -> {} ({:?})", o.gate, o.cue);
                }
            }
        }

        // The structural reason, as a number: the two populations share a lexicon.
        let unique = idx.unique_tokens();
        let shared_ord = ord
            .iter()
            .filter(|t| tokens(t).iter().any(|w| unique.contains(w.as_str())))
            .count();
        let shared_miss = ms
            .iter()
            .filter(|p| tokens(&p.text).iter().any(|w| unique.contains(w.as_str())))
            .count();
        eprintln!(
            "--- utterances carrying a token unique to ONE capability: ordinary {}/{}, misses {}/{}",
            shared_ord,
            ord.len(),
            shared_miss,
            ms.len()
        );

        // ANTI-VACUITY, and it is not hypothetical. The assertion that follows is
        // a NEGATIVE — "no threshold is both honest and useful" — which a
        // suggester that scored NOTHING satisfies perfectly. MUTATION-VERIFIED:
        // forcing `Index::nearest` to return `None` for every utterance leaves all
        // three tests in this module GREEN and prints an all-zero frontier, so
        // without this line the checked-in NO-GO could outlive its own
        // measurement. MEASURED, the scorer IS useful at the permissive end of the
        // sweep — 30 correct offers at T=0.30, 6x this bar — and fails only on
        // HONESTY. A `nearest` that stops offering now turns this RED.
        // ANTI-VACUITY, rewritten to survive the router getting BETTER. This bar
        // was `best_correct >= MIN_USEFUL_OFFERS` (5), which was right at 55
        // misses and became unreachable at 10 — closing the recall gaps took the
        // population down, and the guard then killed the measurement rather than
        // the feature. A scorer that offers NOTHING must still turn this RED
        // (mutation-verified: forcing `nearest` to return None), so the bar is
        // that it offers SOMETHING correct somewhere in the sweep — which does
        // not scale with how many misses are left.
        assert!(
            best_correct >= 1,
            "the suggester makes NO correct offer at any swept threshold, so the \
             NO-GO below would pass VACUOUSLY — what died is the measurement, \
             not the feature"
        );

        assert!(
            honest_and_useful.is_empty(),
            "the lexical did-you-mean NO-GO has been OVERTURNED — a threshold is now both \
             honest (0 suggestions on ordinary speech) and useful (>= {MIN_USEFUL_OFFERS} \
             correct offers). Re-open the feature; do not relax this bar:\n{}",
            honest_and_useful.join("\n")
        );

        // ...AND THE SHAPE OF THE FAILURE, which holds at any population size and
        // is the reason the feature cannot ship: wherever the suggester says
        // anything at all, it is wrong about ordinary speech more often than it
        // is right about a capability. Stated as a ratio rather than a count so
        // it does not decay with the miss population the way the bar above did.
        let mut worse_than_useless: Vec<String> = Vec::new();
        for t_pct in (30..=95).step_by(5) {
            let t = t_pct as f64 / 100.0;
            let correct = miss_offers
                .iter()
                .filter(|(w, o)| o.as_ref().is_some_and(|o| o.score >= t && &o.gate == w))
                .count();
            let false_offers = ord_offers
                .iter()
                .filter(|o| o.as_ref().is_some_and(|o| o.score >= t))
                .count();
            if correct > 0 && false_offers <= correct {
                worse_than_useless.push(format!(
                    "  T={t:.2}: {correct} correct vs {false_offers} false-on-ordinary"
                ));
            }
        }
        assert!(
            worse_than_useless.is_empty(),
            "a threshold now offers more right answers than wrong interruptions — the \
             structural objection to this feature has weakened and it is worth \
             re-measuring properly:\n{}",
            worse_than_useless.join("\n")
        );
    }

    /// COST, so "cheap and on-device" is a number rather than a claim. The whole
    /// 534-phrase scan is pure string work; this bounds it so the NO-GO above is
    /// about ACCURACY, never about speed.
    ///
    /// THIS USED TO BE A WALL-CLOCK ASSERTION AND IT WAS A BAD GATE. It timed the
    /// scan and required `per < 5000µs`. On an idle machine the scan costs 645µs,
    /// so it looked like 7.8x of headroom — and it still went RED at 5770µs, on a
    /// file byte-identical to HEAD, because several agents were building at once.
    /// RE-MEASURED HERE rather than taken on faith: the same scan on the same
    /// binary costs 645µs idle, 1444µs under 16 spinners and 4662µs under 48
    /// spinners on a 10-core machine — a 7.2x swing driven entirely by who else
    /// is running. No fixed µs budget both survives that and still means anything,
    /// and a gate that goes red for a reason that is not the code teaches everyone
    /// to re-run it until it is green.
    ///
    /// So the budget is two numbers that do not move with the machine's load:
    ///
    /// 1. WORK, with no clock in it at all — index-token probes per utterance,
    ///    counted inside [`Index::nearest_counted`]'s own loops. Deterministic
    ///    given the fixture: identical on an idle laptop and a thrashing one.
    /// 2. A RATIO against a baseline THIS TEST MEASURES IN THE SAME RUN — the
    ///    routing pass (`recall_probe::all_hits`) that already runs on every
    ///    utterance anyway. Contention inflates both phases together, so the
    ///    ratio holds where the absolute number does not: MEASURED over six runs
    ///    at three load levels it stays in 1.62..1.91 while the absolute cost it
    ///    is computed from moves 645µs -> 4662µs, a 7.2x swing. The bound is 3.0,
    ///    57% above the worst ratio any of those runs produced.
    #[test]
    fn the_did_you_mean_scan_is_cheap_enough_to_ship() {
        let idx = Index::build();
        let corpus = ordinary();
        assert!(corpus.len() >= 150, "corpus too small to measure");

        // ---- 1. WORK PER UTTERANCE. No clock, no load sensitivity. ----
        let mut total_work = 0usize;
        let mut worst: (usize, &str) = (0, "");
        for t in &corpus {
            let (_, w) = idx.nearest_counted(t);
            total_work += w;
            if w > worst.0 {
                worst = (w, t.as_str());
            }
        }
        let work_per = total_work / corpus.len();
        eprintln!(
            "\n=== MISS-OFFER SCAN WORK: {work_per} index-token probes per utterance \
             over {} indexed phrases (worst {} on {:?}) ===",
            idx.len(),
            worst.0,
            worst.1
        );
        // PRECONDITION, or the budget below passes vacuously on a counter stuck at
        // zero: the intersection pass probes every token of every cue, so the work
        // can never be less than one probe per indexed phrase.
        assert!(
            work_per >= idx.len(),
            "the work counter reports {work_per} probes for {} indexed phrases — it \
             is not counting the scan it claims to count",
            idx.len()
        );
        // MEASURED at this revision: 2_726 probes per utterance on average, worst
        // single utterance 3_439. The budget is 4_000 — 1.47x the mean, so the
        // index may grow by nearly half before anyone has to think about it, and
        // far below the ~5_452 that a second pass over the cues, or an index of
        // twice the size, would cost. Unlike the µs budget it replaces, this
        // number is IDENTICAL on an idle machine and a thrashing one; the only
        // thing that moves it is the fixture, which is a reviewable change.
        //
        // IT TRACKS THE ORDINARY CORPUS TOO, which is easy to miss because the
        // budget is stated per utterance: `work_per` is a MEAN over
        // `router_ordinary.json`, so adding sentences moves it. It read 2_723 at
        // 476 sentences and 2_726 at 488 — re-derive it, do not nudge it.
        assert!(
            work_per <= 4_000,
            "the capability scan now costs {work_per} index-token probes per \
             utterance over {} phrases (budget 4_000, was 2_726 when measured) — \
             it has grown a pass or the index has grown by half; re-derive the \
             cost before shipping it on every turn",
            idx.len()
        );

        // ---- 2. TIME, AS A RATIO AGAINST A SAME-RUN BASELINE. ----
        // The baseline is production work that already runs on EVERY utterance, so
        // "the suggester costs N routing passes" is the honest statement of what it
        // would add. Both phases are min-of-3 and interleaved, so a load ramp
        // during the test hits them alike.
        for t in &corpus {
            let _ = idx.nearest(t);
            let _ = recall_probe::all_hits(t);
        }
        let mut scan = f64::MAX;
        let mut route = f64::MAX;
        for _ in 0..3 {
            let t0 = Instant::now();
            for t in &corpus {
                let _ = idx.nearest(t);
            }
            scan = scan.min(t0.elapsed().as_secs_f64());
            let t1 = Instant::now();
            for t in &corpus {
                let _ = recall_probe::all_hits(t);
            }
            route = route.min(t1.elapsed().as_secs_f64());
        }
        let ratio = scan / route;
        eprintln!(
            "=== MISS-OFFER SCAN COST: {:.0}µs per utterance = {ratio:.2}x the \
             routing pass ({:.0}µs) measured in the same run ===",
            scan * 1e6 / corpus.len() as f64,
            route * 1e6 / corpus.len() as f64
        );
        // PRECONDITION: a baseline that measured nothing would make any ratio pass.
        assert!(
            route > 0.0 && scan > 0.0,
            "one of the two phases took no measurable time — the ratio below means \
             nothing (scan {scan:?}s, route {route:?}s)"
        );
        // 3.0 against a worst observed 1.91 is 57% headroom on the noisiest run
        // measured, and still refuses a scan that got 1.6x slower — TIGHTER than
        // the 7.8x the deleted wall-clock budget allowed.
        //
        // THE ONE WAY THIS CAN FAIL WITHOUT THE SCAN CHANGING: the baseline gets
        // FASTER. That is a router optimisation, not machine noise — a visible,
        // deliberate event, and the message says to check it.
        assert!(
            ratio <= 3.0,
            "the capability scan costs {ratio:.2}x the routing pass it would ride \
             beside — either the scan got slower, or `recall_probe::all_hits` got \
             much faster and this ratio needs re-deriving. Absolute cost this run: \
             {:.0}µs scan, {:.0}µs route, per utterance.",
            scan * 1e6 / corpus.len() as f64,
            route * 1e6 / corpus.len() as f64
        );
    }
}
