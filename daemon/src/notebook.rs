//! RESEARCH NOTEBOOKS — SAGE's "save what I found, with its sources, and let me
//! come back to it."
//!
//! A deep-research run (research.rs) produces a [`crate::research::ResearchReport`]:
//! the fetched sources (the bibliography) and the synthesized, CITED claims. This
//! module PERSISTS such a run as a NOTEBOOK ENTRY {topic, synthesized text, the
//! REAL fetched citations, ts} and lets the user REVISIT a notebook ("show my
//! research on X") and APPEND a follow-up run to it (source memory accrues over
//! time). The store is in [`crate::memory`] (the `notebook_entries` +
//! `notebook_citations` tables); THIS module is the higher-level surface: it
//! turns a report into an entry under the SAME citation discipline research.rs
//! enforces, parses the user's intents, and renders a notebook for reply.
//!
//! ## The CONTRACT (non-negotiable, mirrors research.rs + the memory layer)
//!   * CITE-DISCIPLINE / HONESTY: a notebook entry's citations are derived ONLY
//!     from the run's GROUNDED sources — a source is saved as a citation ONLY when
//!     a grounded claim actually cited it (see [`entry_from_report`]). A claim that
//!     cited a phantom id, or a source nothing cited, is NEVER persisted as a
//!     citation. So a notebook can hold NO citation that was not in its run — the
//!     same "citations map only to fetched sources, never invented" rule, carried
//!     through to persistence and re-checked by the hermetic tests.
//!   * REDACTED: the synthesized text is re-redacted at the Db store (defense in
//!     depth), so a secret can never reach a persisted notebook.
//!   * AGENT-SCOPED: a notebook recorded under one agent stays in THAT agent's
//!     revisit scope (plus the shared orchestrator tier) — never another
//!     specialist's, mirroring the episodic/fact scoping.
//!   * BOUNDED: the store is capped (evict-oldest) in
//!     `memory::notebook_retention_pass`; it remembers the recent runs, NOT
//!     "everything forever".
//!   * FORGETTABLE: a single notebook (`memory::forget_notebook`) or an agent's
//!     whole shelf (`memory::forget_notebooks`) can be cleared.
//!   * REVISIT = HONEST EMPTY: revisiting a topic with no saved run returns an
//!     honest "I have no research notebook on X" — it never fabricates one.
//!
//! Nothing here speaks, acts, or reaches the network. It persists a run that
//! already happened and reads runs that were really saved.

use std::sync::Mutex;

use anyhow::Result;

use crate::memory::{Memory, NotebookCitation, NotebookEntry};
use crate::research::{validate_claims, ResearchReport};

/// Default / max number of notebooks the browse list returns. Bounded so one
/// "what have I researched" reply stays focused.
pub const NOTEBOOK_LIST_DEFAULT: usize = 20;

/// Cap on how many citations one entry persists. A bibliography is already
/// bounded by research.rs's fetch budget ([`crate::research::MAX_FETCHES`]); this
/// is a belt-and-braces ceiling so a hand-built report can never balloon a row.
pub const MAX_CITATIONS_PER_ENTRY: usize = 16;

// ---------------------------------------------------------------------------
// TOPIC KEY — the normalized handle a notebook is revisited / appended by
// ---------------------------------------------------------------------------

/// Normalize a topic into the stable KEY two phrasings of the same subject share:
/// lowercased, trimmed, collapsed whitespace, with a leading research-verb / glue
/// preamble stripped ("research on", "my research about", "what i found on", …) so
/// "my research notebook on the JWST" and "the JWST" land in the SAME notebook.
/// Pure + deterministic. An empty/whitespace topic normalizes to "" (the caller
/// treats that as "no topic given").
pub fn topic_key(topic: &str) -> String {
    let lower = topic.to_lowercase();
    let lower = lower.trim();
    // Strip a leading glue preamble up to and including a trailing "on/about/of/into".
    let mut rest = lower;
    for lead in LEAD_INS {
        if let Some(stripped) = rest.strip_prefix(lead) {
            rest = stripped.trim();
            break;
        }
    }
    normalized_topic(rest)
}

/// Normalize a subject into the shape a notebook is keyed by: single-spaced, no
/// trailing question mark. This is the tail of [`topic_key`], split out so the
/// whitespace/`?` normalization can be reused without the lead-in table.
///
/// IT IS NOT A SECOND KEYING PATH. Every storage/lookup site keys by
/// [`topic_key`] — `entry_from_report` (the Save route), `revisit`, and the Forget
/// dispatch all call `topic_key(topic)` on the subject
/// [`parse_notebook_command`] already cut out of the utterance, so the lead-in
/// stripper DOES run a second time and the cut subject IS double-stripped. The
/// consequence, stated plainly rather than wished away: "save my research on what
/// i found on the beach" is classified with subject "what i found on the beach"
/// and then STORED under topic_key "the beach", the same key a run saved as "the
/// beach" gets — the two notebooks merge, so a later "forget my research on the
/// beach" (a hard transactional DELETE with no confirmation and no undo) destroys
/// both. A subject that normalizes to the EMPTY key (e.g. "my research") is
/// persisted under "" while `revisit` early-returns on an empty key, making that
/// entry unreadable and un-forgettable and making Save report "(0 runs total)" for
/// a save that did persist.
///
/// This doc previously claimed the opposite — that the split existed so the cut
/// subject is "normalized IDENTICALLY without being run through the lead-in
/// stripper a SECOND time". Changing the code to match that claim means giving the
/// store key-taking entry points (`save_run_with_key` / `revisit_by_key`) so a
/// tool-supplied topic and an utterance-cut subject can be keyed differently; that
/// is a store-surface change, not a rename, and is deliberately left as a decision
/// rather than smuggled in behind a comment fix.
fn normalized_topic(subject: &str) -> String {
    // Collapse internal whitespace and drop a trailing question mark.
    let collapsed: String = subject.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.trim_end_matches('?').trim().to_string()
}

/// The lead-in phrases a user speaks before naming the subject, LONGEST /
/// most-specific first — so a phrasing that carries a subject after "on"/"about"
/// strips the full glue (leaving just the subject), while a BARE management
/// phrase ("save this research", "forget my research") that names NO subject
/// strips to "".
///
/// [`topic_key`] is this table's ONLY consumer, and that is deliberate. Its job is
/// to normalize a topic STRING — whoever supplied it — into the key a notebook row
/// is stored under, so being GENEROUS is correct there: a generous normalizer only
/// ever merges two spellings of ONE subject. It is NOT the classifier's gate.
/// Whether an utterance is a COMMAND at all is decided by [`COMMAND_HEADS`], which
/// is anchored at the front of the utterance and far stricter. Conflating the two
/// is the bug this table used to carry: because the bare words "my research" are
/// listed here, ANY sentence containing them ("my research is going nowhere") read
/// as a notebook command.
const LEAD_INS: &[&str] = &[
    "show me my research notebook on ",
    "show me my research notebook about ",
    "show my research notebook on ",
    "show my research notebook about ",
    "my research notebook on ",
    "my research notebook about ",
    "research notebook on ",
    "research notebook about ",
    "what have i researched about ",
    "what have i researched on ",
    "what did i research about ",
    "what did i research on ",
    "what i found on ",
    "what i found about ",
    "delete my research notebook on ",
    "delete my research notebook about ",
    "forget my research notebook on ",
    "forget my research notebook about ",
    "delete my notebook on ",
    "delete my notebook about ",
    "forget my notebook on ",
    "forget my notebook about ",
    "delete my research on ",
    "delete my research about ",
    "forget my research on ",
    "forget my research about ",
    "save this research on ",
    "save this research about ",
    "save my research on ",
    "save my research about ",
    "show my research on ",
    "show my research about ",
    "my research on ",
    "my research about ",
    "research on ",
    "research about ",
    // BARE management phrases that name no subject -> strip to "".
    "what have i researched",
    "what did i research",
    "what i've researched",
    "what i found",
    "list my research notebooks",
    "all my research notebooks",
    "show me my research notebooks",
    "show my research notebooks",
    "my research notebooks",
    "research notebooks",
    "list my research",
    "all my research",
    "show me my research notebook",
    "show my research notebook",
    "my research notebook",
    "research notebook",
    "save this research",
    "save my research",
    "delete my research",
    "forget my research",
    "show my research",
    "my research",
];

// ---------------------------------------------------------------------------
// CITE-DISCIPLINE — a report -> a notebook entry, citing ONLY grounded sources
// ---------------------------------------------------------------------------

/// Build a notebook entry from a research report under the SAME citation
/// discipline research.rs enforces: the persisted citations are EXACTLY the
/// report's GROUNDED sources — a source is kept ONLY when a grounded claim
/// actually cited it. An ungrounded claim (cited a phantom id, or cited nothing)
/// contributes NO citation, and a fetched source that NOTHING grounded cited is
/// NOT persisted as a citation either. So a notebook entry can hold no citation
/// that was not really backed by the run. Pure (no I/O) — the cite-discipline
/// heart, unit-tested. `synthesized` is the already-rendered cited answer the
/// caller supplies (typically [`crate::research::render_report`]); the citations
/// are derived structurally here so the persisted bibliography is grounded by
/// construction regardless of the rendered prose.
pub fn entry_from_report(
    agent_namespace: &str,
    topic: &str,
    report: &ResearchReport,
    synthesized: &str,
) -> NotebookEntry {
    // The grounded claims and their cited source ids — the ONLY sources allowed
    // into the bibliography.
    let (grounded, _ungrounded) = validate_claims(&report.claims, &report.sources);
    let mut cited_ids: Vec<usize> = grounded.iter().map(|c| c.source_id).collect();
    cited_ids.sort_unstable();
    cited_ids.dedup();

    // Keep only sources a grounded claim actually cited, in source-id order, and
    // bound the count.
    let citations: Vec<NotebookCitation> = report
        .sources
        .iter()
        .filter(|s| cited_ids.contains(&s.id))
        .take(MAX_CITATIONS_PER_ENTRY)
        .map(|s| NotebookCitation {
            source_id: s.id as i64,
            title: s.title.clone(),
            url: s.url.clone(),
        })
        .collect();

    NotebookEntry {
        id: 0,
        ts: String::new(),
        agent_namespace: agent_namespace.to_string(),
        topic_key: topic_key(topic),
        topic: topic.trim().to_string(),
        synthesized: synthesized.to_string(),
        citations,
    }
}

// ---------------------------------------------------------------------------
// SAVE / APPEND / REVISIT — the persistence surface
// ---------------------------------------------------------------------------

/// SAVE (or APPEND) a research run as a notebook entry. Because a notebook is the
/// set of entries sharing a `topic_key`, this is BOTH save and append: a first
/// run on a topic creates the notebook, a later run on the same topic appends to
/// it (source memory accrues). Returns the new entry's row id. The Db re-redacts
/// the synthesized text and persists EXACTLY the entry's citations — which
/// [`entry_from_report`] already restricted to the run's grounded sources.
pub async fn save_run(
    memory: &Memory,
    agent_namespace: &str,
    topic: &str,
    report: &ResearchReport,
    synthesized: &str,
) -> Result<i64> {
    let entry = entry_from_report(agent_namespace, topic, report, synthesized);
    memory.save_notebook_entry(&entry).await
}

/// REVISIT the notebook on `topic`: every saved run on that topic_key, OLDEST
/// first (the order the source memory accrued), each with its real citations.
/// Agent-scoped (own + shared). An empty Vec means NO such notebook — the caller
/// renders an honest "no research notebook on X", never a fabricated one.
pub async fn revisit(
    memory: &Memory,
    agent_namespace: &str,
    topic: &str,
) -> Result<Vec<NotebookEntry>> {
    let key = topic_key(topic);
    if key.is_empty() {
        return Ok(Vec::new());
    }
    memory.notebook_entries_for(agent_namespace, &key).await
}

// ---------------------------------------------------------------------------
// LAST RESEARCH RUN — the process-global slot a "save this research" reads
// ---------------------------------------------------------------------------
//
// A bare "save this research" names no report; it means "save the run we JUST
// did". The live SAGE tool (`anthropic::run_sage_research`) records its real,
// structured [`ResearchReport`] here right after a run completes; the notebook
// SAVE intent reads it. This mirrors `model_tier`'s process-global override slot
// (a runtime, process-local seam, reset on restart). It is the ONLY way a
// bare-save finds a report — and because what is stored is the REAL report, the
// save still derives its citations structurally from grounded sources, so the
// never-fabricate discipline is untouched: no real run recorded => nothing to
// save, an honest "I don't have a recent research run to save".

/// The real research run JUST completed: the topic (the question asked), the
/// structured report (its grounded sources are the ONLY citations a save keeps),
/// and the rendered answer text. Stored by the live SAGE path, read by the
/// notebook SAVE intent. Cloned out so the lock is never held across `.await`.
#[derive(Debug, Clone)]
pub struct LastResearchRun {
    /// The question asked — the topic the saved notebook is keyed by.
    pub topic: String,
    /// The real structured report (citations derive ONLY from its grounded sources).
    pub report: ResearchReport,
    /// The rendered, cited answer text the run produced.
    pub synthesized: String,
}

/// The process-global last-run slot. `None` = no SAGE run has completed this
/// process (a bare "save this research" has nothing to save -> honest refusal).
/// Process-local: resets to `None` on restart, like `model_tier`'s override.
static LAST_RUN: Mutex<Option<LastResearchRun>> = Mutex::new(None);

#[cfg(test)]
thread_local! {
    /// Test-only thread-local seam mirroring `model_tier`'s `OVERRIDE_TL`: a test
    /// stages a last-run on its OWN thread without racing the process-global slot
    /// other parallel tests rely on. Compiled out of release.
    static LAST_RUN_TL: std::cell::RefCell<Option<Option<LastResearchRun>>> =
        const { std::cell::RefCell::new(None) };
}

/// Record the run that just completed as the "last research run". Called by the
/// live SAGE path the moment a real report is in hand, so a follow-up "save this
/// research" persists exactly THAT run. Poison-tolerant.
pub fn record_last_run(run: LastResearchRun) {
    #[cfg(test)]
    {
        if LAST_RUN_TL.with(|c| c.borrow().is_some()) {
            LAST_RUN_TL.with(|c| *c.borrow_mut() = Some(Some(run)));
            return;
        }
    }
    *LAST_RUN.lock().unwrap_or_else(|p| p.into_inner()) = Some(run);
}

/// The last research run that completed this process, cloned (so no lock is held
/// across an `.await`). `None` => no run to save. Poison-tolerant.
pub fn last_run() -> Option<LastResearchRun> {
    #[cfg(test)]
    {
        if let Some(seam) = LAST_RUN_TL.with(|c| c.borrow().clone()) {
            return seam;
        }
    }
    LAST_RUN.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

/// `#[cfg(test)]`-only RAII guard staging a `last_run()` on the current thread and
/// restoring the prior thread-local state on drop, so a staged run never leaks
/// into another parallel test. The whole seam is `cfg(test)`.
#[cfg(test)]
pub(crate) struct LastRunGuard {
    prev: Option<Option<LastResearchRun>>,
}

#[cfg(test)]
impl LastRunGuard {
    /// Stage `run` as the thread-local last-run for the guard's lifetime.
    pub fn stage(run: Option<LastResearchRun>) -> Self {
        let prev = LAST_RUN_TL.with(|c| c.borrow().clone());
        LAST_RUN_TL.with(|c| *c.borrow_mut() = Some(run));
        LastRunGuard { prev }
    }
}

#[cfg(test)]
impl Drop for LastRunGuard {
    fn drop(&mut self) {
        LAST_RUN_TL.with(|c| *c.borrow_mut() = self.prev.clone());
    }
}

// ---------------------------------------------------------------------------
// RENDER — a notebook -> one read-friendly, honestly-empty-aware reply
// ---------------------------------------------------------------------------

/// Render a revisited notebook (the entries of one topic) into one read-friendly
/// reply: each run's synthesized text plus its bibliography of REAL citations, in
/// accrual order, with an honest "no notebook" line when there is nothing saved.
/// Pure — unit-testable without I/O. NEVER invents a citation: it only ever shows
/// the persisted ones.
pub fn render_notebook(topic: &str, entries: &[NotebookEntry]) -> String {
    if entries.is_empty() {
        return format!(
            "I have no research notebook on \"{}\", sir — nothing's been saved there yet. \
             Ask me to research it and I'll start one with cited sources.",
            topic.trim()
        );
    }
    let shown = entries.last().map(|e| e.topic.as_str()).unwrap_or(topic.trim());
    let mut out = format!(
        "Your research notebook on \"{}\", sir — {} saved run{}:",
        shown,
        entries.len(),
        if entries.len() == 1 { "" } else { "s" },
    );
    for (i, e) in entries.iter().enumerate() {
        out.push_str(&format!(" [Run {}] {}", i + 1, e.synthesized.trim()));
        if e.citations.is_empty() {
            out.push_str(" (no sourced findings in this run.)");
        } else {
            out.push_str(" Sources: ");
            let bib: Vec<String> = e
                .citations
                .iter()
                .map(|c| format!("[{}] {} — {}", c.source_id, c.title.trim(), c.url.trim()))
                .collect();
            out.push_str(&bib.join("; "));
            out.push('.');
        }
    }
    out.trim().to_string()
}

/// Render the browse list ("what have I researched") from `memory::notebook_list`
/// rows — (topic_key, topic, entry_count, last_ts) — into one read-friendly line,
/// honest-empty-aware. Pure.
pub fn render_notebook_list(notebooks: &[(String, String, u64, String)]) -> String {
    if notebooks.is_empty() {
        return "You don't have any research notebooks yet, sir — ask me to research \
                something and I'll save it with its sources."
            .to_string();
    }
    let mut out = format!(
        "You have {} research notebook{}, sir:",
        notebooks.len(),
        if notebooks.len() == 1 { "" } else { "s" },
    );
    for (_key, topic, count, _ts) in notebooks {
        out.push_str(&format!(
            " \"{}\" ({} run{})",
            topic.trim(),
            count,
            if *count == 1 { "" } else { "s" },
        ));
    }
    out.trim().to_string()
}

// ---------------------------------------------------------------------------
// INTENTS — explicit, phrase-anchored, never auto-triggered
// ---------------------------------------------------------------------------

/// A notebook management intent parsed from an utterance. Only these EXPLICIT
/// phrasings reach the notebook store — an ordinary "research X" goes to SAGE's
/// live run, not here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotebookIntent {
    /// "save this research" / "save my research on X" — persist the LAST run (or
    /// the named topic's run). `topic` is the normalized topic if one was named,
    /// else None (the caller pairs it with the just-run topic).
    Save { topic: Option<String> },
    /// "show my research on X" / "my research notebook on X" / "what did I
    /// research about X" — REVISIT the notebook on `topic`.
    Revisit { topic: String },
    /// "what have I researched" / "my research notebooks" / "list my notebooks" —
    /// the browse list (no specific topic).
    List,
    /// "forget my research on X" / "delete my notebook on X" — FORGET that notebook.
    Forget { topic: String },
}

/// The verbs that mean "throw this notebook away", with the inflections a person
/// actually speaks. Matched as WHOLE WORDS only — `clear` must never fire on
/// "nuclear" or "clearance", which is how "save my research on nuclear reactors"
/// used to delete a notebook.
const FORGET_VERBS: &[&str] = &[
    "forget", "forgets", "forgot", "delete", "deletes", "deleted", "remove", "removes", "removed",
    "clear", "clears", "cleared", "erase", "erases", "erased",
];

/// The verbs that mean "keep this run". Same whole-word rule.
const SAVE_VERBS: &[&str] = &["save", "saves", "saved", "saving"];

/// Does `haystack` name any of `words` as a WHOLE WORD? Thin alias over the
/// shared [`crate::utterance::mentions_any_word`] so this module's verb checks
/// obey the same rule as every other classifier's.
fn has_action_word(haystack: &str, words: &[&str]) -> bool {
    crate::utterance::mentions_any_word(haystack, words)
}

/// The words a person puts in FRONT of a spoken command and nothing else: the
/// wake phrase (main.rs GATES on `wake::wake_gate` and never STRIPS it, so the
/// shipped voice transcript really is "darwin, save this research") and bare
/// politeness. Stripped one token at a time before the command is matched.
///
/// This list is short ON PURPOSE. Every word added here is a word that may sit in
/// front of a DELETE and still be obeyed, so it may hold nothing that changes what
/// the sentence means. "don't", "never", "why", "i want to", "they told me to" are
/// absent for exactly that reason: with them absent, "don't delete my research on
/// the jwst" / "why would i delete my research on black holes" / "the irb told me
/// to delete my research on human subjects" match no command at position 0 and are
/// REFUSED — structurally, with no negation blacklist to keep in sync.
const ADDRESS_WORDS: &[&str] = &["hey", "hi", "hello", "ok", "okay", "yo", "darwin", "please"];

/// The two-word polite openers, same rule. "would you delete X" is a real order;
/// "would i delete X" and "why would you delete X" are not, and neither matches
/// here because only the exact pair is stripped.
const ADDRESS_PAIRS: &[[&str; 2]] =
    &[["can", "you"], ["could", "you"], ["would", "you"], ["will", "you"]];

/// The COMPLETE command phrases a user speaks to address a notebook, each naming
/// NO subject on its own. THIS is the classifier's gate: an utterance is a notebook
/// command only if, after [`strip_address_prefix`], it OPENS with one of these and
/// then either ends or introduces its subject with [`SUBJECT_CONNECTORS`].
///
/// It is deliberately NOT [`LEAD_INS`]. That table exists to normalize a topic
/// string and is generous by design; using it as the gate is what let the mere
/// substring "my research" turn a sentence into an order. Two entries it holds are
/// pointedly absent here: bare "research" (so "research on the competitors" stays a
/// LIVE SAGE run, as it is today) and bare "my research" (so "my research on
/// climate change was published last year" is not heard as a REVISIT — a verb-less
/// possessive is a declarative sentence far more often than an order, and the
/// documented revisit phrasings, "show my research on X" and "my research notebook
/// on X", are both still here).
///
/// Order is irrelevant: the LONGEST entry that opens the utterance wins. That is
/// load-bearing — "save my research notebook on quantum computing" must cut at
/// "save my research notebook", not at the shorter "save my research", or the
/// subject the user just named is silently dropped and the run is filed elsewhere.
const COMMAND_HEADS: &[&str] = &[
    // SAVE — persist the run that just happened.
    "save it to my research notebook",
    "save this to my research notebook",
    "save that to my research notebook",
    "save this research notebook",
    "save my research notebook",
    "save this research",
    "save my research",
    // REVISIT / LIST — read back what was saved.
    "show me my research notebooks",
    "show my research notebooks",
    "show me my research notebook",
    "show my research notebook",
    "show me my research",
    "show my research",
    "list my research notebooks",
    "list my research",
    "list my notebooks",
    "all my research notebooks",
    "all my research",
    "my research notebooks",
    "my research notebook",
    "research notebooks",
    "research notebook",
    "what have i researched",
    "what did i research",
    "what i've researched",
    // FORGET — destructive. Reached ONLY through one of these, at the front.
    "delete my research notebooks",
    "forget my research notebooks",
    "delete my research notebook",
    "forget my research notebook",
    "delete my notebooks",
    "forget my notebooks",
    "delete my notebook",
    "forget my notebook",
    "delete my research",
    "forget my research",
];

/// The ONLY things that may introduce a subject after a head. A head followed by
/// anything else is not a command with a long subject — it is a sentence that
/// happens to start with those words ("my research notebook IS IN MY BACKPACK",
/// "my research IS GOING NOWHERE"), and it is refused.
const SUBJECT_CONNECTORS: &[&str] = &[" on ", " about "];

/// Words allowed to trail a subject-less command without making it a sentence —
/// address and courtesy only, never content. "list my research notebooks please"
/// is the same order as "list my research notebooks"; "list my research expenses
/// for taxes" is not an order at all.
const TRAILING_FILLER: &[&str] =
    &["please", "sir", "darwin", "now", "then", "again", "thanks", "thank", "you", "ok", "okay"];

/// The leading alphanumeric run of `s`, and the remainder. `("", s)` when `s` does
/// not start with an alphanumeric.
fn next_word(s: &str) -> (&str, &str) {
    let end = s.find(|c: char| !c.is_alphanumeric()).unwrap_or(s.len());
    s.split_at(end)
}

/// Drop a leading wake/politeness ADDRESS from an utterance, plus the punctuation
/// around it, so the command itself starts at byte 0 of the result.
///
/// This is not cosmetic. The wake gate in main.rs never strips the phrase, so the
/// transcript that reaches this classifier is "darwin, save this research" — which
/// matched no lead-in at all and was therefore filed under the topic "darwin, save
/// this research". Stripping the address is what lets the command be ANCHORED
/// (rather than searched for anywhere in the sentence, which is how a negated or
/// reported delete would sneak back in) while still obeying the shipped voice shape.
fn strip_address_prefix(lower: &str) -> &str {
    let mut rest = lower;
    loop {
        let head = rest.trim_start_matches(|c: char| !c.is_alphanumeric());
        let (w1, after1) = next_word(head);
        if w1.is_empty() {
            return head;
        }
        if ADDRESS_WORDS.contains(&w1) {
            rest = after1;
            continue;
        }
        let after1 = after1.trim_start_matches(|c: char| !c.is_alphanumeric());
        let (w2, after2) = next_word(after1);
        if ADDRESS_PAIRS.iter().any(|p| p[0] == w1 && p[1] == w2) {
            rest = after2;
            continue;
        }
        return head;
    }
}

/// Is what follows a subject-less head nothing but courtesy and punctuation?
/// An empty remainder is trivially yes ("what have i researched").
fn is_filler_only(rest: &str) -> bool {
    rest.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .all(|w| TRAILING_FILLER.contains(&w))
}

/// Parse an utterance as a notebook COMMAND: the [`COMMAND_HEADS`] phrase it opens
/// with (where the action verb is read from — a verb-looking word in the SUBJECT is
/// a subject, not an order: "save my research on how to clear a paper jam" is a
/// SAVE about clearing jams) and the subject it names, `""` when it names none.
///
/// `None` means NOT A COMMAND, and that is the whole point. The three ways an
/// ordinary sentence used to get in are each closed here:
///   * it does not OPEN with a head -> "he spilled coffee on my research notebook",
///     "i deleted my research on the shared drive", "why would i delete my research
///     on black holes". The old code searched nowhere and simply fell back to
///     treating the WHOLE utterance as the command;
///   * it opens with one but keeps going into something that is not a subject ->
///     "my research notebook is in my backpack" (was REVISIT "is in my backpack"),
///     "my research assistant quit yesterday" (REVISIT "assistant quit yesterday"),
///     "delete my research from the shared drive" (FORGET "from the shared drive").
///     The rule that a BARE head names no subject was already written in
///     [`LEAD_INS`]'s doc; it was never enforced, so the trailing words became a
///     notebook's NAME. (Say the same sentence with a leading "please" and the old
///     code went wrong the OTHER way — no lead-in matched at byte 0 at all, so the
///     topic became the entire sentence. Both are the same missing question: was a
///     command ever cut off the front?);
///   * the topic used to be re-derived as `topic_key(whole utterance)` at three
///     separate places, each of which handed back the sentence itself when it could
///     strip nothing. It is cut ONCE, here.
fn parse_notebook_command(lower: &str) -> Option<(&'static str, String)> {
    let body = strip_address_prefix(lower);
    // LONGEST head wins — see [`COMMAND_HEADS`]; a shorter head that is a prefix of
    // a longer one would swallow the connector and drop the named subject.
    let mut found: Option<&'static str> = None;
    for &head in COMMAND_HEADS {
        if body.starts_with(head) && found.is_none_or(|prev: &str| head.len() > prev.len()) {
            found = Some(head);
        }
    }
    let head = found?;
    // The heads are ASCII, so `head.len()` is a char boundary of `body`.
    let rest = &body[head.len()..];
    if is_filler_only(rest) {
        return Some((head, String::new()));
    }
    for conn in SUBJECT_CONNECTORS {
        if let Some(subject) = rest.strip_prefix(conn) {
            return Some((head, normalized_topic(subject)));
        }
    }
    None
}

/// Detect a notebook management intent. CONSERVATIVE and GRAMMAR-anchored: the
/// utterance must OPEN with one of the enumerated [`COMMAND_HEADS`] (after nothing
/// but a wake/politeness address) and must then either STOP or name its subject
/// after "on"/"about". An ordinary "research the competitors" never trips it (that
/// routes to SAGE's live run), and neither does a sentence that merely uses the
/// words: "my research is going nowhere", "he spilled coffee on my research
/// notebook", "she deleted the notebook page with the recipe".
///
/// WHAT THIS USED TO BE, AND WHY IT WAS WRONG. The gate was "does the utterance
/// CONTAIN a notebook-ish word", and the topic was `topic_key(whole utterance)` —
/// which, when it could strip nothing, handed back THE UTTERANCE ITSELF. A
/// sentence that never addressed a notebook therefore arrived looking exactly like
/// a command with a very long subject, and three branches acted on it: REVISIT read
/// out a notebook named after the sentence, SAVE filed a run under it, and FORGET —
/// destructive, unconfirmed, unjournalled (`memory::forget_notebook` is a hard
/// transactional DELETE) — fired on "i deleted my research on the shared drive".
/// A subject is only a subject when a COMMAND was cut off the front of it.
/// Pure — unit-tested.
pub fn classify_notebook_intent(utterance: &str) -> Option<NotebookIntent> {
    let lower = utterance.to_lowercase();
    let lower = lower.trim();

    // The subject must be a research NOTEBOOK / saved research, not a live run.
    // "save ... research" (e.g. "save this research") is the canonical bare-save
    // and must trip the gate even though it names no subject — still conservative
    // (it requires BOTH the save verb AND "research", so "save the file" and the
    // live "research the competitors" never trip it).
    let saves_research = has_action_word(lower, SAVE_VERBS) && lower.contains("research");
    let about_notebook = lower.contains("notebook")
        || lower.contains("my research")
        || lower.contains("i research")
        || lower.contains("i've researched")
        || lower.contains("have i researched")
        || lower.contains("did i research")
        || lower.contains("saved research")
        || saves_research;
    if !about_notebook {
        return None;
    }

    // FORGET vs SAVE — read from the COMMAND region only, as WHOLE WORDS.
    //
    // This used to be `FORGET.iter().any(|v| lower.contains(v))` over the whole
    // utterance, checked before save. Three ways that destroyed a notebook the
    // user had just asked to keep:
    //
    //   "save my research on NUCLEAR reactors"     -> "clear" inside "nuclear"
    //   "save my research on CLEARANCE rates"      -> "clear" inside "clearance"
    //   "save my research on how to CLEAR a jam"   -> a real word, but the TOPIC
    //
    // The first two are the substring bug; the third is a real word in the wrong
    // place. Whole-word matching fixes the first two, and reading only the
    // command lead-in fixes the third — the subject a user names is never an
    // order. Destructive intent is the one that must be hardest to trip.
    // NOT A COMMAND AT ALL => NOT A NOTEBOOK INTENT. The gate above only asks
    // whether the utterance CONTAINS notebook-ish words, which ordinary English
    // does all the time; this asks whether the user actually gave the order.
    let (head, topic) = parse_notebook_command(lower)?;

    // A forget with NO topic falls through rather than clearing the shelf: an
    // unaddressed "forget my research" lists instead of destroying everything.
    //
    // The destructive verb is read from the HEAD — the enumerated phrase the
    // utterance OPENS with — never from a wider region. Together with the anchor
    // that is the hardest gate in this function, and it is the one that must be:
    // `memory::forget_notebook` is a hard transactional DELETE that the router
    // dispatches with no confirmation and no undo entry, so a wrong FORGET is
    // unrecoverable while a wrong REVISIT is a wasted sentence. Anchoring (rather
    // than finding the phrase anywhere) is what refuses the whole family of
    // sentences that merely REPORT or NEGATE a deletion — "i deleted my research on
    // the shared drive", "don't delete my research on the jwst", "the irb told me
    // to delete my research on human subjects" — without a negation blacklist,
    // which would have to be exhaustive to work and would maim real topics
    // ("delete my research on never events") when it over-reached.
    if has_action_word(head, FORGET_VERBS) && !topic.is_empty() {
        return Some(NotebookIntent::Forget { topic });
    }

    // SAVE (persist a run) — the topic is the one CUT above. Re-deriving it from
    // the whole utterance is what filed "darwin, save this research" under that
    // literal sentence instead of under the run's own topic: main.rs gates on the
    // wake phrase and never strips it, so that IS the shipped voice shape.
    if has_action_word(head, SAVE_VERBS) {
        let topic = if topic.is_empty() { None } else { Some(topic) };
        return Some(NotebookIntent::Save { topic });
    }

    // LIST vs REVISIT — decided by whether the command NAMED a subject, which the
    // grammar already answered. There is nothing left to sniff for.
    //
    // This used to be two unanchored `contains` piles ("show" / "revisit" /
    // "notebook" / "my research" …) sitting over a topic RE-DERIVED as
    // `topic_key(lower)`. Both halves were wrong the same way: the cue fired on any
    // sentence using the word, and the re-derivation returned the sentence itself
    // whenever it could strip nothing, so REVISIT read out a notebook named after
    // the whole utterance ("my chemistry teacher wants a chart of the reactions in
    // my notebook"). A bare FORGET head still lands here — an unaddressed "forget
    // my research" LISTS rather than destroying the shelf.
    if topic.is_empty() {
        return Some(NotebookIntent::List);
    }
    Some(NotebookIntent::Revisit { topic })
}

// ---------------------------------------------------------------------------
// DISPATCH — a classified intent -> a persisted/revisited/rendered reply
// ---------------------------------------------------------------------------

/// The outcome of handling a notebook intent: the rendered reply line the caller
/// speaks, a short telemetry verb naming WHAT happened, and the already-cited,
/// already-redacted CARD the HUD renders (the topic + the run's REAL citations +
/// a short snippet). The card carries ONLY what the user already owns — the real
/// fetched-source locators and the redacted synthesized snippet — never a secret
/// and never a fabricated citation (the citations are exactly the persisted ones,
/// which [`entry_from_report`] already restricted to grounded sources).
pub struct NotebookOutcome {
    /// The read-friendly reply to speak/display.
    pub reply: String,
    /// A short telemetry verb: "saved" / "revisit" / "list" / "forget" /
    /// "save_none" (a bare save with no recent run) / "forget_none".
    pub verb: &'static str,
    /// The structured CARD the HUD renders for this intent. `None` for the
    /// honest-empty / nothing-happened verbs (save_none / forget_none / error)
    /// where there is no notebook content to surface — the HUD shows the verb
    /// alone. Built ONLY from the persisted, grounded citations + redacted snippet.
    pub card: Option<NotebookCard>,
}

/// One CITATION as the HUD consumes it: the source's run-local id, its title, and
/// the real fetched URL — the same locators [`NotebookCitation`] persists. Carries
/// nothing the user does not already own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotebookCardCitation {
    pub source_id: i64,
    pub title: String,
    pub url: String,
}

/// The enriched notebook telemetry CARD the HUD renders: the verb, the topic the
/// activity touched, a short already-redacted snippet of the synthesized answer,
/// the REAL fetched-source citations (run-local id + title + url), and the count
/// of saved runs on that notebook. SECRET-FREE: the citations are exactly the
/// persisted, grounded ones (never invented); the snippet is the already-redacted
/// synthesized text (the store re-redacts on write). NEVER raw, NEVER fabricated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotebookCard {
    /// The verb naming what happened (mirrors [`NotebookOutcome::verb`]).
    pub verb: &'static str,
    /// The human topic the notebook is keyed by (as the user phrased it).
    pub topic: String,
    /// A short snippet of the most-recent run's already-redacted synthesized
    /// answer — the "what was found", bounded. Empty when there is no run text.
    pub snippet: String,
    /// The REAL fetched-source citations of the surfaced run. Empty is honest
    /// (a run with no grounded sources).
    pub citations: Vec<NotebookCardCitation>,
    /// How many saved runs the notebook holds (the accrued source memory).
    pub run_count: usize,
}

/// How long a snippet of synthesized text the card carries — bounded so the
/// telemetry stays a glance, not a dump (the full text lives in the spoken reply).
pub const CARD_SNIPPET_CHARS: usize = 280;

/// Build a [`NotebookCard`] from a revisited/saved notebook's persisted entries:
/// surfaces the MOST-RECENT run's already-redacted snippet + its REAL citations,
/// and the run count. Pure (no I/O). SECRET-FREE by construction: it copies only
/// the persisted citation locators (which were grounded by [`entry_from_report`])
/// and a bounded slice of the already-redacted `synthesized` text — never raw
/// content, never a fabricated source. `entries` empty => an honest empty card
/// (no snippet, no citations, zero runs).
pub fn build_card(verb: &'static str, topic: &str, entries: &[NotebookEntry]) -> NotebookCard {
    let last = entries.last();
    let snippet = last
        .map(|e| {
            let t = e.synthesized.trim();
            if t.chars().count() > CARD_SNIPPET_CHARS {
                let cut: String = t.chars().take(CARD_SNIPPET_CHARS).collect();
                format!("{}…", cut.trim_end())
            } else {
                t.to_string()
            }
        })
        .unwrap_or_default();
    let citations = last
        .map(|e| {
            e.citations
                .iter()
                .map(|c| NotebookCardCitation {
                    source_id: c.source_id,
                    title: c.title.trim().to_string(),
                    url: c.url.trim().to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    NotebookCard {
        verb,
        topic: topic.trim().to_string(),
        snippet,
        citations,
        run_count: entries.len(),
    }
}

/// Handle a [`NotebookIntent`] against the real notebook store, AGENT-SCOPED to
/// `namespace`. This is the single end-to-end surface the router calls so a real
/// utterance reaches save/revisit/list/forget — and the hermetically-tested seam
/// (a synthetic last-run + a temp Db is enough). HONEST throughout:
///   * Save with a named topic that has saved runs revisits/persists nothing new
///     it can't ground — a bare save persists the REAL [`last_run`] (citations
///     derived structurally from its grounded sources); with no recent run it
///     says so plainly and saves NOTHING (never fabricates a run).
///   * Revisit/List/Forget read the agent-scoped store and render honestly-empty.
pub async fn dispatch(
    memory: &Memory,
    namespace: &str,
    intent: NotebookIntent,
) -> Result<NotebookOutcome> {
    match intent {
        NotebookIntent::Save { topic } => {
            // The run to save: the named topic's last run if it was the one just
            // done, else the bare last run. Either way it is a REAL recorded run —
            // we never invent one. `topic` (when named) only relabels the save.
            let Some(run) = last_run() else {
                return Ok(NotebookOutcome {
                    reply: "I don't have a recent research run to save, sir — ask me to \
                            research something first and I'll save it with its real sources."
                        .to_string(),
                    verb: "save_none",
                    card: None,
                });
            };
            // The save topic: the explicitly named one (so "save my research on X"
            // files it under X) else the run's own question.
            let save_topic = topic.as_deref().unwrap_or(run.topic.as_str());
            save_run(memory, namespace, save_topic, &run.report, &run.synthesized).await?;
            let entries = revisit(memory, namespace, save_topic).await?;
            Ok(NotebookOutcome {
                reply: format!(
                    "Saved, sir — your research on \"{}\" is in your notebook now ({} run{} total), \
                     with its real cited sources.",
                    save_topic.trim(),
                    entries.len(),
                    if entries.len() == 1 { "" } else { "s" },
                ),
                verb: "saved",
                card: Some(build_card("saved", save_topic, &entries)),
            })
        }
        NotebookIntent::Revisit { topic } => {
            let entries = revisit(memory, namespace, &topic).await?;
            Ok(NotebookOutcome {
                reply: render_notebook(&topic, &entries),
                verb: "revisit",
                // Honest-empty revisit -> an empty card (no snippet/citations); the
                // HUD shows the honest-empty topic.
                card: Some(build_card("revisit", &topic, &entries)),
            })
        }
        NotebookIntent::List => {
            let notebooks = memory.notebook_list(namespace, NOTEBOOK_LIST_DEFAULT).await?;
            // The LIST card surfaces the SHELF (topics + run counts), not one run's
            // citations — so its snippet/citations are empty and its run_count is
            // the number of notebooks. The HUD renders the topic line from the verb.
            let card = NotebookCard {
                verb: "list",
                topic: String::new(),
                snippet: render_notebook_list(&notebooks),
                citations: Vec::new(),
                run_count: notebooks.len(),
            };
            Ok(NotebookOutcome {
                reply: render_notebook_list(&notebooks),
                verb: "list",
                card: Some(card),
            })
        }
        NotebookIntent::Forget { topic } => {
            let cleared = memory.forget_notebook(namespace, &topic_key(&topic)).await?;
            if cleared == 0 {
                return Ok(NotebookOutcome {
                    reply: format!(
                        "There's no research notebook on \"{}\" to forget, sir.",
                        topic.trim()
                    ),
                    verb: "forget_none",
                    card: None,
                });
            }
            Ok(NotebookOutcome {
                reply: format!(
                    "Forgotten, sir — I've cleared your research notebook on \"{}\".",
                    topic.trim()
                ),
                verb: "forget",
                // A forget surfaces only the topic it cleared — no citations remain.
                card: Some(NotebookCard {
                    verb: "forget",
                    topic: topic.trim().to_string(),
                    snippet: String::new(),
                    citations: Vec::new(),
                    run_count: 0,
                }),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::research::{Claim, Source};
    use std::path::PathBuf;

    /// Unique temp DB per test; tests run concurrently in one process.
    struct TempDb(PathBuf);
    impl TempDb {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "darwin-notebook-test-{}-{}.db",
                std::process::id(),
                tag
            ));
            let _ = std::fs::remove_file(&path);
            TempDb(path)
        }
    }
    impl Drop for TempDb {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let mut p = self.0.clone().into_os_string();
                p.push(suffix);
                let _ = std::fs::remove_file(PathBuf::from(p));
            }
        }
    }

    fn src(id: usize, title: &str, url: &str) -> Source {
        Source { id, url: url.into(), title: title.into(), excerpt: "e".into() }
    }

    /// A report with 2 fetched sources; one grounded claim cites source 1, one
    /// claim cites a PHANTOM id 999, one cites nothing (0). So only source 1 is
    /// grounded; source 2 was fetched but nothing cited it.
    fn mixed_report() -> ResearchReport {
        ResearchReport {
            question: "what is X".into(),
            sources: vec![
                src(1, "Real A", "https://a.test"),
                src(2, "Fetched-but-uncited B", "https://b.test"),
            ],
            claims: vec![
                Claim::new("a grounded point", 1),
                Claim::new("a phantom point", 999),
                Claim::new("an uncited point", 0),
            ],
            planned_subqueries: 2,
            pursued_subqueries: 2,
            truncated: false,
        }
    }

    // ---- topic key normalization ------------------------------------------

    #[test]
    fn topic_key_normalizes_phrasings_to_the_same_notebook() {
        let a = topic_key("my research notebook on the James Webb Telescope");
        let b = topic_key("  Research on   the James Webb Telescope?  ");
        let c = topic_key("the James Webb Telescope");
        assert_eq!(a, "the james webb telescope");
        assert_eq!(a, b, "lead-in + whitespace + case + ? normalize alike");
        assert_eq!(a, c, "the bare subject lands in the same notebook");
        assert_eq!(topic_key("   "), "", "empty topic -> empty key");
    }

    /// PINS the ACTUAL keying behavior of an already-cut subject, which
    /// `normalized_topic`'s doc used to describe backwards.
    ///
    /// Every storage/lookup site re-applies `topic_key` to the subject
    /// `parse_notebook_command` cut out, so the lead-in stripper runs a SECOND time
    /// and the subject is double-stripped. If someone gives the store key-taking
    /// entry points so the cut subject is keyed verbatim, this test must be updated
    /// deliberately — which is the point of pinning it.
    #[test]
    fn an_already_cut_subject_is_double_stripped_by_the_storage_key() {
        // The subject the classifier hands the storage path...
        match classify_notebook_intent("save my research on what i found on the beach") {
            Some(NotebookIntent::Save { topic: Some(t) }) => {
                assert_eq!(t, "what i found on the beach", "the classifier cuts the full subject");
                // ...is re-stripped to a DIFFERENT key by every storage site.
                assert_eq!(topic_key(&t), "the beach");
                assert_eq!(
                    topic_key(&t),
                    topic_key("the beach"),
                    "so it MERGES with a notebook the user named 'the beach'"
                );
            }
            other => panic!("expected a Save with a subject, got {other:?}"),
        }
        // And a subject that strips to nothing is stored under the EMPTY key, which
        // `revisit` cannot read back.
        assert_eq!(topic_key("my research"), "");
    }

    // ---- cite-discipline: only grounded sources become citations ----------

    #[test]
    fn entry_keeps_only_grounded_citations_never_fabricates() {
        let report = mixed_report();
        let entry = entry_from_report("agent.darwin", "what is X", &report, "rendered answer [1]");
        // Exactly ONE citation: the grounded source 1. The phantom (999) and the
        // uncited (0) contribute nothing, and the fetched-but-uncited source 2 is
        // NOT persisted either — a notebook holds only sources a grounded claim
        // actually cited.
        assert_eq!(entry.citations.len(), 1, "only the grounded source is cited: {:?}", entry.citations);
        assert_eq!(entry.citations[0].source_id, 1);
        assert_eq!(entry.citations[0].url, "https://a.test");
        assert!(
            !entry.citations.iter().any(|c| c.url.contains("b.test")),
            "a fetched-but-uncited source must not become a citation: {:?}",
            entry.citations
        );
    }

    // ---- persist + revisit -------------------------------------------------

    #[tokio::test]
    async fn save_then_revisit_returns_the_cited_run() {
        let db = TempDb::new("save-revisit");
        let mem = Memory::open(&db.0).unwrap();
        let report = mixed_report();
        save_run(&mem, "agent.darwin", "what is X", &report, "rendered answer [1]")
            .await
            .unwrap();

        let entries = revisit(&mem, "agent.darwin", "what is X").await.unwrap();
        assert_eq!(entries.len(), 1, "the saved run is revisited");
        assert_eq!(entries[0].topic, "what is X");
        assert_eq!(entries[0].synthesized, "rendered answer [1]");
        assert_eq!(entries[0].citations.len(), 1, "the grounded citation persisted");
        assert_eq!(entries[0].citations[0].url, "https://a.test");
        // A phrasing variant revisits the SAME notebook.
        let again = revisit(&mem, "agent.darwin", "my research on  WHAT IS X?").await.unwrap();
        assert_eq!(again.len(), 1, "a phrasing variant hits the same notebook");
    }

    #[tokio::test]
    async fn append_accrues_source_memory_under_one_notebook() {
        let db = TempDb::new("append");
        let mem = Memory::open(&db.0).unwrap();
        save_run(&mem, "agent.darwin", "topic Z", &mixed_report(), "first run [1]")
            .await
            .unwrap();
        // A follow-up run on the SAME topic appends a second entry.
        let mut second = mixed_report();
        second.sources = vec![src(1, "New C", "https://c.test")];
        second.claims = vec![Claim::new("a new grounded point", 1)];
        save_run(&mem, "agent.darwin", "TOPIC Z", &second, "second run [1]")
            .await
            .unwrap();

        let entries = revisit(&mem, "agent.darwin", "topic Z").await.unwrap();
        assert_eq!(entries.len(), 2, "append accrues a second run in one notebook");
        // OLDEST first (accrual order).
        assert_eq!(entries[0].synthesized, "first run [1]");
        assert_eq!(entries[1].synthesized, "second run [1]");
        assert_eq!(entries[1].citations[0].url, "https://c.test", "the follow-up's source");
    }

    #[tokio::test]
    async fn revisit_unknown_topic_is_honest_empty_never_fabricates() {
        let db = TempDb::new("empty-revisit");
        let mem = Memory::open(&db.0).unwrap();
        let entries = revisit(&mem, "agent.darwin", "a topic never researched").await.unwrap();
        assert!(entries.is_empty(), "no saved run -> honest empty");
        let rendered = render_notebook("a topic never researched", &entries);
        assert!(
            rendered.to_lowercase().contains("no research notebook"),
            "render must be honestly empty: {rendered}"
        );
    }

    // ---- never-fabricate, at the persistence boundary ----------------------

    #[tokio::test]
    async fn a_persisted_notebook_holds_no_citation_not_in_its_run() {
        let db = TempDb::new("never-fab");
        let mem = Memory::open(&db.0).unwrap();
        // The report cites a phantom 999 + an uncited 0; only source 1 is grounded.
        save_run(&mem, "agent.darwin", "verify me", &mixed_report(), "answer [1]")
            .await
            .unwrap();
        let entries = revisit(&mem, "agent.darwin", "verify me").await.unwrap();
        let urls: Vec<&str> = entries[0].citations.iter().map(|c| c.url.as_str()).collect();
        // Only the real, grounded URL — never the phantom, never the uncited
        // source's, never a fabricated one.
        assert_eq!(urls, vec!["https://a.test"], "persisted citations are only the grounded run sources");
        assert!(
            entries[0].citations.iter().all(|c| c.source_id != 999),
            "a phantom citation must never persist: {:?}",
            entries[0].citations
        );
        let rendered = render_notebook("verify me", &entries);
        assert!(!rendered.contains("999"), "the phantom id must not surface: {rendered}");
        assert!(!rendered.contains("b.test"), "the uncited fetched source must not surface: {rendered}");
    }

    // ---- agent scoping -----------------------------------------------------

    #[tokio::test]
    async fn notebooks_are_agent_scoped_own_plus_shared_never_cross_agent() {
        let db = TempDb::new("scope");
        let mem = Memory::open(&db.0).unwrap();
        save_run(&mem, "agent.friday", "markets", &mixed_report(), "friday run [1]").await.unwrap();
        save_run(&mem, "agent.jerome", "music", &mixed_report(), "jerome run [1]").await.unwrap();
        save_run(&mem, "agent.darwin", "weather", &mixed_report(), "shared run [1]").await.unwrap();

        // friday sees its own + the shared orchestrator notebook, never jerome's.
        let friday = mem.notebook_list("agent.friday", 20).await.unwrap();
        let keys: Vec<&str> = friday.iter().map(|(k, _, _, _)| k.as_str()).collect();
        assert!(keys.contains(&"markets"), "own notebook missing: {keys:?}");
        assert!(keys.contains(&"weather"), "shared notebook missing: {keys:?}");
        assert!(!keys.contains(&"music"), "leaked another agent's notebook: {keys:?}");

        // Revisiting jerome's topic from friday's scope returns NOTHING.
        let cross = revisit(&mem, "agent.friday", "music").await.unwrap();
        assert!(cross.is_empty(), "cross-agent revisit must be empty: {:?}", cross);
    }

    // ---- bounded + forget --------------------------------------------------

    #[tokio::test]
    async fn retention_evicts_oldest_entries_past_the_cap() {
        let db = TempDb::new("retain");
        let mem = Memory::open(&db.0).unwrap();
        for i in 0..5 {
            save_run(&mem, "agent.darwin", &format!("topic {i}"), &mixed_report(), &format!("run {i} [1]"))
                .await
                .unwrap();
        }
        assert_eq!(mem.notebook_entries_count().await.unwrap(), 5);
        let deleted = mem.notebook_retention_pass(2).await.unwrap();
        assert_eq!(deleted, 3, "the 3 oldest entries were evicted");
        assert_eq!(mem.notebook_entries_count().await.unwrap(), 2);
        // The orphaned citations were evicted too (no dangling rows).
        let kept = mem.notebook_list("agent.darwin", 20).await.unwrap();
        let keys: Vec<&str> = kept.iter().map(|(k, _, _, _)| k.as_str()).collect();
        assert!(keys.contains(&"topic 4") && keys.contains(&"topic 3"), "newest survive: {keys:?}");
    }

    #[tokio::test]
    async fn forget_clears_one_notebook_and_its_citations() {
        let db = TempDb::new("forget");
        let mem = Memory::open(&db.0).unwrap();
        save_run(&mem, "agent.darwin", "keep me", &mixed_report(), "keep [1]").await.unwrap();
        save_run(&mem, "agent.darwin", "drop me", &mixed_report(), "drop [1]").await.unwrap();
        let cleared = mem.forget_notebook("agent.darwin", &topic_key("drop me")).await.unwrap();
        assert_eq!(cleared, 1, "the named notebook's entry was forgotten");
        assert!(revisit(&mem, "agent.darwin", "drop me").await.unwrap().is_empty());
        assert_eq!(revisit(&mem, "agent.darwin", "keep me").await.unwrap().len(), 1, "the other survives");
    }

    // ---- intents -----------------------------------------------------------

    #[test]
    fn intents_parse_save_revisit_list_forget() {
        // SAVE
        assert_eq!(
            classify_notebook_intent("save my research on quantum computing"),
            Some(NotebookIntent::Save { topic: Some("quantum computing".to_string()) })
        );
        assert!(matches!(
            classify_notebook_intent("save this research"),
            Some(NotebookIntent::Save { topic: None })
        ));
        // REVISIT
        assert_eq!(
            classify_notebook_intent("show my research notebook on the JWST"),
            Some(NotebookIntent::Revisit { topic: "the jwst".to_string() })
        );
        assert_eq!(
            classify_notebook_intent("what have I researched about black holes"),
            Some(NotebookIntent::Revisit { topic: "black holes".to_string() })
        );
        // LIST
        assert!(matches!(
            classify_notebook_intent("list my research notebooks"),
            Some(NotebookIntent::List)
        ));
        assert!(matches!(
            classify_notebook_intent("what have I researched"),
            Some(NotebookIntent::List)
        ));
        // FORGET
        assert_eq!(
            classify_notebook_intent("forget my research on the JWST"),
            Some(NotebookIntent::Forget { topic: "the jwst".to_string() })
        );
        // A plain live-research request is NOT a notebook intent.
        assert_eq!(classify_notebook_intent("research the competitors thoroughly"), None);
        assert_eq!(classify_notebook_intent("what's the weather"), None);
    }

    /// A SAVE MUST NEVER BE HEARD AS A DELETE.
    ///
    /// The forget verbs used to be matched with `contains` over the WHOLE
    /// utterance, before save was even considered. Two independent ways that
    /// turned "keep this" into "throw it away":
    ///
    ///   * substring — "clear" lives inside "nu-CLEAR" and "CLEAR-ance", so
    ///     "save my research on nuclear reactors" deleted the notebook;
    ///   * wrong region — "clear" is a real word in "how to clear a paper jam",
    ///     but it is the SUBJECT the user named, not the order they gave.
    ///
    /// Every case here returns Save today and returned Forget before the fix.
    /// The topic is asserted too: a Save that keeps the wrong notebook is its own
    /// silent loss.
    #[test]
    fn a_save_is_never_heard_as_a_forget() {
        for (utterance, topic) in [
            // -- substring: the verb is not in the utterance at all
            ("save my research on nuclear reactors", "nuclear reactors"),
            ("save my research on clearance rates", "clearance rates"),
            ("save my research on nuclear clearance policy", "nuclear clearance policy"),
            // -- real word, but inside the TOPIC the user named
            ("save my research on how to clear a paper jam", "how to clear a paper jam"),
            ("save my research on removing lead paint", "removing lead paint"),
            ("save my research on deleted file recovery", "deleted file recovery"),
            ("save my research on erasure coding", "erasure coding"),
        ] {
            assert_eq!(
                classify_notebook_intent(utterance),
                Some(NotebookIntent::Save { topic: Some(topic.to_string()) }),
                "{utterance:?} must SAVE — hearing a delete here destroys the notebook \
                 the user just asked to keep"
            );
        }
    }

    /// The other direction still works: a real forget command is still a forget,
    /// including when its TOPIC contains a save word. A fix that made deleting
    /// impossible would pass the test above and be just as broken.
    #[test]
    fn a_real_forget_command_still_forgets() {
        for (utterance, topic) in [
            ("forget my research on nuclear reactors", "nuclear reactors"),
            ("delete my notebook on the JWST", "the jwst"),
            ("delete my research on saving for retirement", "saving for retirement"),
            ("forget my research notebook about savings accounts", "savings accounts"),
        ] {
            assert_eq!(
                classify_notebook_intent(utterance),
                Some(NotebookIntent::Forget { topic: topic.to_string() }),
                "{utterance:?} is a real delete command and must still delete"
            );
        }
    }

    /// A SENTENCE THAT MERELY USES THE WORDS IS NOT A COMMAND.
    ///
    /// Every one of these classified as a notebook intent before the grammar
    /// landed, and not harmlessly: the topic was the WHOLE SENTENCE, because
    /// `topic_key` hands its input back when it can strip nothing. So REVISIT read
    /// out a notebook named after the sentence, SAVE filed a run under it, and
    /// FORGET aimed a transactional DELETE at it. None of these addressed a
    /// notebook at all.
    #[test]
    fn a_sentence_that_merely_mentions_a_notebook_is_not_a_command() {
        for u in [
            "my chemistry teacher wants a chart of the reactions in my notebook",
            "she deleted the notebook page with the recipe",
            "i need to clear out my old chemistry notebook",
            "i left my notebook in the car",
            "my notebook computer died",
            "i deleted my research on the shared drive",
            "delete my research from the shared drive",
            "please delete my research from the shared drive",
            "my research is going nowhere",
            "my research assistant quit yesterday",
            "my research notebook is in my backpack",
            "he spilled coffee on my research notebook",
            "there's a chart in my research notebook about the reactions",
            "the notebook was full of my research on birds",
            "i lost my research on the old laptop",
            "i want to show my research at the conference",
            "what did i research last week for the class",
            "i saved a photo of my research notebook",
            "did you save my research paper",
            "she saved my research from the fire",
            "my mom saved my research notebook from the trash",
            "i'm saving up for a new research notebook",
            "i was saving my research notebook for later",
        ] {
            // PRECONDITION. Each case must actually be inside this classifier's
            // blast radius — it really does carry the words the outer prefilter
            // looks for. A case that never reached the code would pass this test
            // while proving nothing about it.
            assert!(
                u.contains("notebook") || u.contains("research"),
                "{u:?} would not even reach the gate — this case proves nothing"
            );
            assert_eq!(
                classify_notebook_intent(u),
                None,
                "{u:?} never addressed a notebook; classifying it hands a whole \
                 sentence to the notebook store as a topic"
            );
        }
    }

    /// A DELETE THAT WAS ONLY REPORTED, NEGATED, OR WONDERED ABOUT IS NOT AN ORDER.
    ///
    /// `memory::forget_notebook` is a hard transactional DELETE and the router
    /// dispatches it with no confirmation and no journal/undo entry, so this is the
    /// branch that must be hardest to trip: a wrong REVISIT is a wasted sentence, a
    /// wrong FORGET is unrecoverable. It is held shut by ANCHORING — the command
    /// has to be the first thing in the utterance, with only a wake/politeness
    /// address allowed in front — which refuses this entire family at once. A
    /// negation word list would have to be exhaustive to do the same job, and would
    /// maim a real topic ("delete my research on never events") the moment it
    /// over-reached.
    #[test]
    fn a_reported_or_negated_delete_never_forgets() {
        for u in [
            "don't delete my research on the jwst",
            "please don't delete my research on the jwst",
            "darwin, don't delete my research on the jwst",
            "never forget my research on quantum computing",
            "why would i delete my research on black holes",
            "why would you delete my research on black holes",
            "the irb told me to delete my research on human subjects",
            "my advisor made me delete my research on mice",
            "i had to delete my research on the grant last year",
            "she said to forget my research on that topic",
            "they want me to delete my notebook on the old project",
            "i want to delete my research on the shared drive",
            "did you delete my research on the jwst",
            "someone deleted my research on the jwst",
            "what if i delete my research on the jwst",
            "remember when i deleted my research on the jwst",
        ] {
            assert!(
                !matches!(classify_notebook_intent(u), Some(NotebookIntent::Forget { .. })),
                "{u:?} reports or negates a deletion and must never reach FORGET — got {:?}",
                classify_notebook_intent(u)
            );
        }
        // ...and the fix must not have made deleting impossible. A real order still
        // forgets exactly the named notebook, including through the address a voice
        // transcript always carries (main.rs never strips the wake phrase).
        for u in [
            "delete my research on the jwst",
            "please delete my research on the jwst",
            "darwin, delete my research on the jwst",
            "hey darwin, forget my research on the jwst",
            "would you delete my research on the jwst",
            "delete my notebook about the jwst",
        ] {
            assert_eq!(
                classify_notebook_intent(u),
                Some(NotebookIntent::Forget { topic: "the jwst".to_string() }),
                "{u:?} is a real delete order and must still forget the named notebook"
            );
        }
    }

    /// THE SHIPPED VOICE SHAPE CARRIES THE WAKE PHRASE, AND IT IS NOT THE TOPIC.
    ///
    /// main.rs GATES on `wake::wake_gate` and never STRIPS the phrase, so the
    /// transcript reaching this classifier really is "darwin, save this research".
    /// No lead-in prefix-matched that, so the old code filed the run under the
    /// literal topic "darwin, save this research" — a notebook named after the
    /// transcript, permanently separate from the one the user meant.
    #[test]
    fn the_wake_phrase_and_politeness_never_become_the_topic() {
        assert_eq!(
            classify_notebook_intent("darwin, save this research"),
            Some(NotebookIntent::Save { topic: None })
        );
        assert_eq!(
            classify_notebook_intent("hey darwin, save this research"),
            Some(NotebookIntent::Save { topic: None })
        );
        assert_eq!(
            classify_notebook_intent("please save this research"),
            Some(NotebookIntent::Save { topic: None })
        );
        assert_eq!(
            classify_notebook_intent("save this research please"),
            Some(NotebookIntent::Save { topic: None })
        );
        assert_eq!(
            classify_notebook_intent("darwin save my research on quantum computing"),
            Some(NotebookIntent::Save { topic: Some("quantum computing".to_string()) })
        );
        assert_eq!(
            classify_notebook_intent("can you show my research notebook on the jwst"),
            Some(NotebookIntent::Revisit { topic: "the jwst".to_string() })
        );
        assert!(matches!(
            classify_notebook_intent("hey darwin what have i researched"),
            Some(NotebookIntent::List)
        ));
        assert!(matches!(
            classify_notebook_intent("could you list my research notebooks"),
            Some(NotebookIntent::List)
        ));
    }

    /// THE LONGEST COMMAND HEAD WINS, OR THE NAMED TOPIC IS SILENTLY LOST.
    ///
    /// "save my research notebook on quantum computing" opens with BOTH "save my
    /// research" and "save my research notebook". Cut at the shorter one and what
    /// is left is " notebook on quantum computing" — not a subject at all — so the
    /// save falls back to some other notebook and never says it did.
    #[test]
    fn the_longest_command_head_wins_so_a_named_topic_is_never_dropped() {
        for (u, topic) in [
            ("save my research notebook on quantum computing", "quantum computing"),
            ("save my research notebook about black holes", "black holes"),
            ("save this research notebook on the jwst", "the jwst"),
            ("please save my research notebook on the jwst", "the jwst"),
        ] {
            assert_eq!(
                classify_notebook_intent(u),
                Some(NotebookIntent::Save { topic: Some(topic.to_string()) }),
                "{u:?} names its topic explicitly; dropping it files the run under \
                 the wrong notebook"
            );
        }
        assert_eq!(
            classify_notebook_intent("show me my research notebook on the jwst"),
            Some(NotebookIntent::Revisit { topic: "the jwst".to_string() })
        );
    }

    /// EVERY ENUMERATED HEAD IS LIVE, AND A HEAD WITH CONTENT AFTER IT IS A SENTENCE.
    ///
    /// The first half is the dead-branch check: the outer `about_notebook`
    /// prefilter is kept as a cheap early-out, so a head it happened to reject
    /// would be an entry that can never fire — silently, forever. The second half
    /// is the rule [`LEAD_INS`]'s own doc always stated and nothing ever enforced:
    /// a BARE phrase names no subject, so the words after it are not a notebook's
    /// NAME ("my research notebook IS IN MY BACKPACK").
    #[test]
    fn every_command_head_is_live_and_trailing_content_makes_it_a_sentence() {
        assert!(!COMMAND_HEADS.is_empty(), "the grammar must enumerate something");
        for head in COMMAND_HEADS {
            assert!(
                classify_notebook_intent(head).is_some(),
                "{head:?} is enumerated as a command but never classifies — a dead entry"
            );
            let named = format!("{head} on the jwst");
            assert!(
                classify_notebook_intent(&named).is_some(),
                "{named:?} names a subject after an enumerated head and must classify"
            );
            let sentence = format!("{head} is in my backpack");
            assert_eq!(
                classify_notebook_intent(&sentence),
                None,
                "{sentence:?} is a statement, not an order — the trailing words are \
                 not the notebook's name"
            );
        }
    }

    /// A LIVE RESEARCH REQUEST IS STILL A LIVE RESEARCH REQUEST. [`LEAD_INS`] lists
    /// a bare "research on " because that is a fine thing to STRIP off a topic
    /// string; [`COMMAND_HEADS`] deliberately does not, because "research on the
    /// competitors" is an order to go and research, not to open a notebook.
    #[test]
    fn a_live_research_request_never_becomes_a_notebook_command() {
        for u in [
            "research the competitors thoroughly",
            "research on the competitors",
            "research about the housing market",
            "can you research the housing market",
        ] {
            assert_eq!(classify_notebook_intent(u), None, "{u:?} is a live SAGE run");
        }
    }


    /// The whole-word rule, at the level it is enforced. `clear` is the verb that
    /// hides inside the most ordinary English, so it gets the sharpest test.
    #[test]
    fn action_words_match_whole_words_only() {
        assert!(has_action_word("clear the notebook", FORGET_VERBS));
        assert!(has_action_word("please clear it", FORGET_VERBS));
        assert!(has_action_word("cleared already", FORGET_VERBS));
        // ...but never as a fragment of a longer word.
        assert!(!has_action_word("nuclear reactors", FORGET_VERBS));
        assert!(!has_action_word("clearance rates", FORGET_VERBS));
        assert!(!has_action_word("unclear results", FORGET_VERBS));
        assert!(!has_action_word("nuclear clearance", FORGET_VERBS));
        // The same rule on the save side.
        assert!(has_action_word("save it", SAVE_VERBS));
        assert!(!has_action_word("savearama", SAVE_VERBS));
    }

    // ---- dispatch: an utterance routes end-to-end --------------------------

    fn run(topic: &str, report: ResearchReport, synth: &str) -> LastResearchRun {
        LastResearchRun { topic: topic.to_string(), report, synthesized: synth.to_string() }
    }

    #[tokio::test]
    async fn dispatch_bare_save_persists_the_real_last_run() {
        let db = TempDb::new("dispatch-save");
        let mem = Memory::open(&db.0).unwrap();
        // Stage a REAL completed run (a phantom + uncited claim present, so the
        // save must keep ONLY the grounded source — never fabricate).
        let _g = LastRunGuard::stage(Some(run("what is X", mixed_report(), "answer [1]")));

        let intent = classify_notebook_intent("save this research").unwrap();
        assert!(matches!(intent, NotebookIntent::Save { topic: None }));
        let out = dispatch(&mem, "agent.darwin", intent).await.unwrap();
        assert_eq!(out.verb, "saved");

        // The run was persisted under the run's own question and revisits cleanly,
        // holding ONLY the grounded citation.
        let entries = revisit(&mem, "agent.darwin", "what is X").await.unwrap();
        assert_eq!(entries.len(), 1, "the bare save persisted the last run");
        assert_eq!(entries[0].synthesized, "answer [1]");
        assert_eq!(entries[0].citations.len(), 1, "only the grounded source persisted");
        assert_eq!(entries[0].citations[0].url, "https://a.test");
        assert!(
            !entries[0].citations.iter().any(|c| c.source_id == 999 || c.url.contains("b.test")),
            "a save must never fabricate a citation: {:?}",
            entries[0].citations
        );
    }

    #[tokio::test]
    async fn dispatch_bare_save_with_no_run_is_honest_and_saves_nothing() {
        let db = TempDb::new("dispatch-save-none");
        let mem = Memory::open(&db.0).unwrap();
        // No run staged -> nothing to save.
        let _g = LastRunGuard::stage(None);
        let out = dispatch(&mem, "agent.darwin", NotebookIntent::Save { topic: None })
            .await
            .unwrap();
        assert_eq!(out.verb, "save_none", "honest: no run to save");
        assert!(out.reply.to_lowercase().contains("don't have a recent research run"));
        // Nothing was persisted.
        assert_eq!(mem.notebook_entries_count().await.unwrap(), 0, "no run => nothing saved");
    }

    #[tokio::test]
    async fn dispatch_revisit_returns_the_saved_notebook() {
        let db = TempDb::new("dispatch-revisit");
        let mem = Memory::open(&db.0).unwrap();
        save_run(&mem, "agent.darwin", "black holes", &mixed_report(), "what I found [1]")
            .await
            .unwrap();
        let intent = classify_notebook_intent("show my research notebook on black holes").unwrap();
        let out = dispatch(&mem, "agent.darwin", intent).await.unwrap();
        assert_eq!(out.verb, "revisit");
        assert!(out.reply.contains("black holes"), "{}", out.reply);
        assert!(out.reply.contains("https://a.test"), "the real source surfaces: {}", out.reply);
    }

    #[tokio::test]
    async fn dispatch_revisit_unknown_is_honest_empty() {
        let db = TempDb::new("dispatch-revisit-empty");
        let mem = Memory::open(&db.0).unwrap();
        let out = dispatch(
            &mem,
            "agent.darwin",
            NotebookIntent::Revisit { topic: "never researched".to_string() },
        )
        .await
        .unwrap();
        assert!(out.reply.to_lowercase().contains("no research notebook"), "{}", out.reply);
    }

    #[tokio::test]
    async fn dispatch_list_and_forget() {
        let db = TempDb::new("dispatch-list-forget");
        let mem = Memory::open(&db.0).unwrap();
        save_run(&mem, "agent.darwin", "topic one", &mixed_report(), "one [1]").await.unwrap();
        // LIST shows it.
        let list = dispatch(&mem, "agent.darwin", NotebookIntent::List).await.unwrap();
        assert_eq!(list.verb, "list");
        assert!(list.reply.contains("topic one"), "{}", list.reply);
        // FORGET clears it.
        let forget = dispatch(
            &mem,
            "agent.darwin",
            NotebookIntent::Forget { topic: "topic one".to_string() },
        )
        .await
        .unwrap();
        assert_eq!(forget.verb, "forget");
        assert!(revisit(&mem, "agent.darwin", "topic one").await.unwrap().is_empty());
        // FORGET of a missing notebook is honest.
        let none = dispatch(
            &mem,
            "agent.darwin",
            NotebookIntent::Forget { topic: "nothing here".to_string() },
        )
        .await
        .unwrap();
        assert_eq!(none.verb, "forget_none");
    }

    // ---- enriched telemetry card: real citations, redacted snippet, no secret ----

    /// A report carrying a SECRET-shaped span in its synthesized text, so the test
    /// can prove the card snippet is the ALREADY-REDACTED text (the store re-redacts
    /// on write), never the raw secret.
    fn report_with_secret() -> (ResearchReport, &'static str) {
        // The grounded source 1; the synthesized text the LIVE path would have
        // redacted before store contains an api-key-shaped token.
        let report = ResearchReport {
            question: "what is X".into(),
            sources: vec![src(1, "Real A", "https://a.test")],
            claims: vec![Claim::new("a grounded point", 1)],
            planned_subqueries: 1,
            pursued_subqueries: 1,
            truncated: false,
        };
        (report, "sk-LIVE_supersecret_key_abcdef0123456789")
    }

    #[test]
    fn build_card_surfaces_real_citations_and_redacted_snippet_no_secret() {
        // Persist a run whose synthesized text carries a secret-shaped token: the
        // memory store re-redacts on write (defense in depth), so what a revisit
        // reads back — and what the card therefore carries — is already redacted.
        let report = mixed_report();
        // The entry the card is built from: simulate the already-redacted persisted
        // synthesized text (the store would have replaced the secret with [redacted]).
        let entry = entry_from_report(
            "agent.darwin",
            "what is X",
            &report,
            "Key findings on X [1]. token [redacted]",
        );
        let card = build_card("revisit", "what is X", std::slice::from_ref(&entry));
        // The card carries the REAL grounded citation (source 1), never the
        // fetched-but-uncited source 2 nor the phantom 999.
        assert_eq!(card.citations.len(), 1, "only the grounded citation: {:?}", card.citations);
        assert_eq!(card.citations[0].source_id, 1);
        assert_eq!(card.citations[0].url, "https://a.test");
        assert!(
            !card.citations.iter().any(|c| c.url.contains("b.test") || c.source_id == 999),
            "no uncited/phantom citation in the card: {:?}",
            card.citations
        );
        // The snippet is the already-redacted synthesized text — NO raw secret.
        assert!(card.snippet.contains("Key findings on X"), "snippet present: {}", card.snippet);
        assert!(!card.snippet.contains("sk-LIVE"), "no secret in the card snippet: {}", card.snippet);
        assert!(!card.snippet.contains("supersecret"), "no secret in the card snippet: {}", card.snippet);
        assert_eq!(card.run_count, 1);
        assert_eq!(card.topic, "what is X");
    }

    #[test]
    fn build_card_honest_empty_carries_no_content() {
        // No entries -> an honest-empty card: no snippet, no citations, zero runs.
        let card = build_card("revisit", "never researched", &[]);
        assert!(card.snippet.is_empty(), "empty card has no snippet: {:?}", card);
        assert!(card.citations.is_empty(), "empty card has no citations");
        assert_eq!(card.run_count, 0);
        assert_eq!(card.topic, "never researched");
    }

    #[test]
    fn build_card_bounds_a_long_snippet() {
        let long = "x".repeat(CARD_SNIPPET_CHARS * 3);
        let entry = entry_from_report("agent.darwin", "topic", &mixed_report(), &long);
        let card = build_card("saved", "topic", std::slice::from_ref(&entry));
        // Bounded to the cap (+ the single ellipsis char).
        assert!(
            card.snippet.chars().count() <= CARD_SNIPPET_CHARS + 1,
            "snippet bounded: {} chars",
            card.snippet.chars().count()
        );
        assert!(card.snippet.ends_with('…'), "a truncated snippet is marked: {}", card.snippet);
    }

    #[tokio::test]
    async fn dispatch_save_emits_a_card_with_the_real_run_citations() {
        let db = TempDb::new("dispatch-card-save");
        let mem = Memory::open(&db.0).unwrap();
        let (report, secret) = report_with_secret();
        // The synthesized text the live SAGE path produced would already be redacted
        // before reaching the store; we stage the redacted form (the store re-redacts
        // anyway). The card must NOT carry the raw secret.
        let _g = LastRunGuard::stage(Some(run("what is X", report, "Findings [1]. tok [redacted]")));
        // Sanity: the staged secret is never put into the synthesized text we save.
        assert!(!secret.is_empty());

        let out = dispatch(&mem, "agent.darwin", NotebookIntent::Save { topic: None })
            .await
            .unwrap();
        assert_eq!(out.verb, "saved");
        let card = out.card.expect("a saved intent carries a card");
        assert_eq!(card.run_count, 1, "one saved run");
        assert_eq!(card.citations.len(), 1, "the grounded citation rides the card");
        assert_eq!(card.citations[0].url, "https://a.test");
        assert!(!card.snippet.contains("sk-LIVE"), "card snippet is secret-free: {}", card.snippet);
    }

    #[tokio::test]
    async fn dispatch_revisit_empty_emits_an_honest_empty_card() {
        let db = TempDb::new("dispatch-card-empty");
        let mem = Memory::open(&db.0).unwrap();
        let out = dispatch(
            &mem,
            "agent.darwin",
            NotebookIntent::Revisit { topic: "never researched".to_string() },
        )
        .await
        .unwrap();
        assert_eq!(out.verb, "revisit");
        let card = out.card.expect("revisit carries a card even when empty");
        assert_eq!(card.run_count, 0, "honest empty: no runs");
        assert!(card.citations.is_empty(), "honest empty: no citations");
        assert!(card.snippet.is_empty(), "honest empty: no snippet");
    }

    #[tokio::test]
    async fn dispatch_save_none_and_forget_none_carry_no_card() {
        let db = TempDb::new("dispatch-card-none");
        let mem = Memory::open(&db.0).unwrap();
        let _g = LastRunGuard::stage(None);
        let save_none = dispatch(&mem, "agent.darwin", NotebookIntent::Save { topic: None })
            .await
            .unwrap();
        assert_eq!(save_none.verb, "save_none");
        assert!(save_none.card.is_none(), "nothing happened -> no card");
        let forget_none = dispatch(
            &mem,
            "agent.darwin",
            NotebookIntent::Forget { topic: "nothing here".to_string() },
        )
        .await
        .unwrap();
        assert_eq!(forget_none.verb, "forget_none");
        assert!(forget_none.card.is_none(), "nothing forgotten -> no card");
    }

    #[test]
    fn last_run_slot_roundtrips_via_the_test_seam() {
        let _g = LastRunGuard::stage(Some(run("topic", mixed_report(), "synth [1]")));
        let got = last_run().expect("staged run is visible");
        assert_eq!(got.topic, "topic");
        assert_eq!(got.synthesized, "synth [1]");
        // record_last_run writes through the same thread-local seam under test cfg.
        record_last_run(run("topic2", mixed_report(), "synth2 [1]"));
        assert_eq!(last_run().unwrap().topic, "topic2");
    }
}


