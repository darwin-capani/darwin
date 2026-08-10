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
        let ut = tokens(text);
        if ut.is_empty() {
            return None;
        }
        let ut_mass: f64 = ut.iter().map(|t| self.weight(t)).sum();
        let mut best: Option<Offer> = None;
        for c in &self.cues {
            let inter: f64 = c
                .toks
                .iter()
                .filter(|t| ut.contains(*t))
                .map(|t| self.weight(t))
                .sum();
            if inter <= 0.0 {
                continue;
            }
            let cue_mass: f64 = c.toks.iter().map(|t| self.weight(t)).sum();
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
        best
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
        assert!(ms.len() >= 40, "too few misses to conclude: {}", ms.len());
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
        assert!(
            best_correct >= MIN_USEFUL_OFFERS,
            "the suggester makes at most {best_correct} correct offer(s) at ANY \
             swept threshold (bar {MIN_USEFUL_OFFERS}), so the NO-GO asserted \
             below would pass VACUOUSLY — what died is the measurement, not the \
             feature"
        );

        assert!(
            honest_and_useful.is_empty(),
            "the lexical did-you-mean NO-GO has been OVERTURNED — a threshold is now both \
             honest (0 suggestions on ordinary speech) and useful (>= {MIN_USEFUL_OFFERS} \
             correct offers). Re-open the feature; do not relax this bar:\n{}",
            honest_and_useful.join("\n")
        );
    }

    /// COST, so "cheap and on-device" is a number rather than a claim. The whole
    /// 534-phrase scan is pure string work; this bounds it well under a single
    /// audio frame so the NO-GO above is about ACCURACY, never about speed.
    #[test]
    fn the_did_you_mean_scan_is_cheap_enough_to_ship() {
        let idx = Index::build();
        let corpus = ordinary();
        assert!(corpus.len() >= 150, "corpus too small to time");
        // Warm the allocator/branch predictors so the measured pass is steady.
        for t in &corpus {
            let _ = idx.nearest(t);
        }
        let start = Instant::now();
        let rounds = 5u32;
        for _ in 0..rounds {
            for t in &corpus {
                let _ = idx.nearest(t);
            }
        }
        let total = start.elapsed();
        let per = total / (rounds * corpus.len() as u32);
        eprintln!(
            "\n=== MISS-OFFER SCAN COST: {:?} per utterance over {} indexed phrases ===",
            per,
            idx.len()
        );
        assert!(
            per.as_micros() < 5_000,
            "the capability scan costs {per:?} per utterance — no longer negligible \
             beside the ~1-2s STT+route+converse turn it would ride"
        );
    }
}
