//! REPORT GENERATION (#40) — "generate a report on X" over already-cited sources.
//!
//! SAGE's notebooks (notebook.rs) and the deep-research engine (research.rs)
//! produce CITED material: a claim is only ever "backed" when it cites a source
//! that was actually fetched ([`crate::research::Claim::is_grounded`] /
//! [`crate::research::validate_claims`]). This module ASSEMBLES that already-cited
//! material into one structured, ordered, BOUNDED markdown report — a title,
//! sections, and a citations list — REUSING the very same citation discipline:
//! it NEVER invents a citation, it only ever carries forward the REAL source ref
//! an input claim already carried.
//!
//! ## The CONTRACT (non-negotiable, mirrors notebook.rs + research.rs)
//!   * CITE-ONLY-REAL / HONESTY: every citation in the report is a REAL source ref
//!     carried by an INPUT claim. A claim with no usable citation (an empty/blank
//!     ref) is DROPPED — never presented, never given a fabricated source. So the
//!     report can hold NO citation that was not already grounded by an input.
//!   * BOUNDED: the report caps the number of sections ([`MAX_SECTIONS`]), the
//!     claims per section ([`MAX_CLAIMS_PER_SECTION`]), and the total distinct
//!     citations ([`MAX_CITATIONS`]). A report, not a dump.
//!   * HONEST-EMPTY: with NO citable sources, the report is honestly empty
//!     (`empty: true`, a "no sources to report on" body) — NEVER a fabricated body
//!     with an invented citation.
//!   * PURE: [`build_report`] and [`render_markdown`] do NO I/O — assembly + the
//!     cite-discipline are the unit-tested heart. The live wiring (the router op)
//!     pulls the already-cited sources from the notebook / a research run and feeds
//!     them in; this module never fetches, never calls a model, never networks.
//!
//! Nothing here speaks, acts, or reaches the network. It folds material that was
//! already gathered + cited into a read-friendly structured document.

/// Hard cap on how many sections one report holds. A report is a focused
/// synthesis, not an unbounded dump.
pub const MAX_SECTIONS: usize = 8;

/// Cap on how many claims a single section renders. Keeps each section a
/// readable paragraph-group, not a wall.
pub const MAX_CLAIMS_PER_SECTION: usize = 12;

/// Whole-report cap on the number of DISTINCT citations the bibliography lists.
/// Belt-and-braces ceiling so even a large input set yields a bounded report.
pub const MAX_CITATIONS: usize = 24;

// ---------------------------------------------------------------------------
// INPUT — a sourced claim (the already-cited unit the report assembles)
// ---------------------------------------------------------------------------

/// A REAL source ref an input claim carries: a stable id, a title, and the URL it
/// was fetched from — the SAME locator vocabulary [`crate::research::Source`] /
/// [`crate::notebook::NotebookCitation`] use. A citation exists ONLY because a real
/// source was fetched; this type carries that forward, never invents one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRef {
    /// Stable id (the run-local source id the input cited).
    pub id: usize,
    /// The source's title.
    pub title: String,
    /// The real URL the source was fetched from.
    pub url: String,
}

impl SourceRef {
    /// Build a source ref.
    pub fn new(id: usize, title: impl Into<String>, url: impl Into<String>) -> Self {
        SourceRef { id, title: title.into(), url: url.into() }
    }

    /// True when this ref is USABLE as a citation: it must carry a non-blank URL.
    /// A ref with an empty/whitespace URL is NOT a real locator and a claim
    /// carrying only it is dropped (never given a fabricated source). The id and
    /// title are presentational; the URL is what makes a citation real.
    pub fn is_usable(&self) -> bool {
        !self.url.trim().is_empty()
    }
}

/// One already-cited claim fed into the report: a heading bucket (which section it
/// belongs under), the claim text, and the REAL source ref it carries (or `None`
/// when the input had no usable citation — such a claim is DROPPED, never
/// fabricated a source). This is the input vocabulary the live op produces from
/// the notebook / research material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcedClaim {
    /// The section heading this claim belongs under (claims sharing a heading are
    /// grouped into one section, in first-seen order).
    pub heading: String,
    /// The claim text (one sentence/point of the report body).
    pub text: String,
    /// The REAL source ref this claim cites. `None` (or a blank-URL ref) => the
    /// claim is UNCITED and is dropped from the report — never given a source.
    pub citation: Option<SourceRef>,
}

impl SourcedClaim {
    /// Build a sourced claim with a citation.
    pub fn cited(
        heading: impl Into<String>,
        text: impl Into<String>,
        citation: SourceRef,
    ) -> Self {
        SourcedClaim {
            heading: heading.into(),
            text: text.into(),
            citation: Some(citation),
        }
    }

    /// Build an UNCITED claim — one with no usable source. The report DROPS these;
    /// the constructor exists so a caller (and the tests) can express "this input
    /// had no citation" honestly rather than synthesizing a fake one. The live op
    /// produces only cited claims from grounded notebook citations, so this reads as
    /// unused in the binary while the drop-uncited discipline is proven by the tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn uncited(heading: impl Into<String>, text: impl Into<String>) -> Self {
        SourcedClaim { heading: heading.into(), text: text.into(), citation: None }
    }

    /// The claim's USABLE citation, if any: the carried ref when it is real
    /// ([`SourceRef::is_usable`]), else `None`. The single chokepoint deciding
    /// whether a claim contributes to the report.
    fn usable_citation(&self) -> Option<&SourceRef> {
        self.citation.as_ref().filter(|c| c.is_usable())
    }
}

// ---------------------------------------------------------------------------
// OUTPUT — the structured report
// ---------------------------------------------------------------------------

/// One CITATION the report carries, as the rendered bibliography lists it: the
/// REPORT-LOCAL id (dense + unique across every input run — see [`build_report`]),
/// the title, and the real URL — exactly the locator the input claim carried.
/// NEVER fabricated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportCitation {
    pub id: usize,
    pub title: String,
    pub url: String,
}

/// One section of the report: a heading and an ordered body of cited points, each
/// carrying the REPORT-LOCAL id of the source it cites (so the rendered body can
/// bracket `[n]` and have it resolve to exactly one bibliography entry). A section
/// exists ONLY because at least one CITED claim landed under its heading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportSection {
    /// The section heading.
    pub heading: String,
    /// The cited body points: (text, cited source id), in first-seen order,
    /// bounded by [`MAX_CLAIMS_PER_SECTION`].
    pub points: Vec<(String, usize)>,
}

/// A built report: the title, the ordered sections, the distinct citations list
/// (every entry a REAL source ref an input claim carried), and the honest-empty
/// flag. Pure data — [`render_markdown`] turns it into the markdown string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The report title (the topic, as asked).
    pub title: String,
    /// The ordered sections — each with at least one cited point. Empty when there
    /// were no citable sources.
    pub sections: Vec<ReportSection>,
    /// The DISTINCT citations the report rests on, in id order. Every one is a real
    /// source ref carried by an input claim; bounded by [`MAX_CITATIONS`].
    pub all_citations: Vec<ReportCitation>,
    /// True when there was NOTHING citable to report — an honest empty report.
    pub empty: bool,
}

// ---------------------------------------------------------------------------
// BUILD — assemble cited claims into a bounded, cite-only-real report
// ---------------------------------------------------------------------------

/// Assemble `sources` (already-cited claims) into a structured [`Report`] under
/// the SAME citation discipline research.rs / notebook.rs enforce:
///   * CITE-ONLY-REAL: a claim with NO usable citation (`None`, or a blank-URL ref)
///     is DROPPED — it contributes neither a body point nor a citation. The report
///     therefore holds NO citation that was not carried by an input claim, and no
///     body point that was not backed by a real source. A citation is never
///     synthesized.
///   * BOUNDED: at most [`MAX_SECTIONS`] sections (in first-seen heading order), at
///     most [`MAX_CLAIMS_PER_SECTION`] points per section, at most [`MAX_CITATIONS`]
///     distinct citations. A claim whose source could not be LISTED (past the
///     citation cap) is bounded out of the BODY too — never a `[n]` marker with no
///     matching `## Sources` entry.
///   * REPORT-LOCAL IDS: incoming source ids are RUN-local (each research run
///     numbers its own sources from 1), so distinct sources collide across runs.
///     Every distinct `(incoming id, url)` is renumbered to a unique report-local
///     id used for BOTH the body marker and the bibliography key — so `[n]`
///     resolves to exactly one source.
///   * HONEST-EMPTY: when NO claim carries a usable citation, the result is
///     `empty: true` with no sections and no citations — the caller renders a plain
///     "no sources to report on", never a fabricated body.
///     Pure (no I/O) — the cite-discipline + assembly heart, unit-tested.
pub fn build_report(title: &str, sources: &[SourcedClaim], _cfg: &ReportConfig) -> Report {
    // Group CITED claims by heading in first-seen order. A claim with no usable
    // citation is dropped here (never reaches a section, never a citation).
    let mut headings: Vec<String> = Vec::new();
    let mut grouped: Vec<Vec<(String, usize)>> = Vec::new();
    // Distinct citations keyed by (incoming id, url) so the same real source cited
    // twice is listed once; collected in first-seen order, then sorted by id.
    let mut citations: Vec<ReportCitation> = Vec::new();
    // (incoming id, url) -> the REPORT-LOCAL id it was renumbered to.
    //
    // WHY renumber: incoming ids are RUN-LOCAL (research.rs assigns `sources.len()
    // + 1` per run, so every saved run numbers its sources 1,2,3…). Folding two
    // runs into one report therefore produced two different sources both carrying
    // id 1 — the bibliography listed `[1]` twice and the `[1]` marker under "Run 2"
    // resolved to run 1's source. Report-local ids are dense and unique, so every
    // body marker resolves to exactly ONE bibliography entry.
    let mut id_map: Vec<((usize, String), usize)> = Vec::new();

    for claim in sources {
        let Some(cit) = claim.usable_citation() else {
            // UNCITED -> dropped. Never fabricate a source for it.
            continue;
        };

        // Record the distinct citation (by incoming id + url) under its new
        // report-local id — bounded.
        let key = (cit.id, cit.url.trim().to_string());
        let local_id = match id_map.iter().find(|(k, _)| *k == key) {
            Some((_, id)) => *id,
            None => {
                if citations.len() >= MAX_CITATIONS {
                    // Past the citation cap: this source cannot be LISTED, so its
                    // claim is bounded out of the BODY too. Emitting the point
                    // anyway would render a `[n]` marker with no `## Sources` entry
                    // — a dangling citation pointing at nothing.
                    continue;
                }
                let next = citations.len() + 1;
                citations.push(ReportCitation {
                    id: next,
                    title: cit.title.trim().to_string(),
                    url: cit.url.trim().to_string(),
                });
                id_map.push((key, next));
                next
            }
        };

        // Find or create the section for this heading (bounded by MAX_SECTIONS).
        let heading = claim.heading.trim().to_string();
        let idx = match headings.iter().position(|h| h == &heading) {
            Some(i) => i,
            None => {
                if headings.len() >= MAX_SECTIONS {
                    // Past the section cap: this claim's heading is a NEW section we
                    // can't open. Drop the claim from the body (it is bounded out),
                    // but its citation already recorded above stays honest — every
                    // listed citation is still real. We do NOT silently expand.
                    continue;
                }
                headings.push(heading.clone());
                grouped.push(Vec::new());
                headings.len() - 1
            }
        };
        let text = claim.text.trim().to_string();
        if grouped[idx].len() < MAX_CLAIMS_PER_SECTION {
            // The MAPPED id — the marker must name the bibliography entry, not the
            // input's run-local id.
            grouped[idx].push((text, local_id));
        }
    }

    // Drop any heading that ended up with zero points (its only claims were bounded
    // out of the body) — a section must carry at least one cited point.
    let sections: Vec<ReportSection> = headings
        .into_iter()
        .zip(grouped)
        .filter(|(_, points)| !points.is_empty())
        .map(|(heading, points)| ReportSection { heading, points })
        .collect();

    // Citations the body actually references — drop any recorded for a claim whose
    // body point was bounded out, so the bibliography matches the body. Then sort
    // by id for a stable, readable bibliography.
    let referenced: std::collections::BTreeSet<usize> = sections
        .iter()
        .flat_map(|s| s.points.iter().map(|(_, id)| *id))
        .collect();
    let mut all_citations: Vec<ReportCitation> = citations
        .into_iter()
        .filter(|c| referenced.contains(&c.id))
        .collect();
    all_citations.sort_by_key(|c| c.id);

    let empty = sections.is_empty();
    Report {
        title: title.trim().to_string(),
        sections,
        all_citations,
        empty,
    }
}

// ---------------------------------------------------------------------------
// RENDER — a report -> a markdown string (honest-empty-aware)
// ---------------------------------------------------------------------------

/// Render a [`Report`] into a markdown string: a `#` title, each section as a `##`
/// heading with its cited points (`- point [n]`), and a `## Sources` bibliography
/// listing every REAL citation (`[n] title — url`). An honest-empty report renders
/// a plain "no sources to report on" line under the title — NEVER a fabricated
/// body or citation. Pure — unit-testable without I/O. When a point's text ALREADY
/// ends with its own `[n]` citation marker (e.g. a notebook entry's pre-rendered
/// cited synthesis), the marker is NOT appended again — the existing one already
/// points at the same real source, so re-stamping it would only double the marker,
/// never add a citation.
pub fn render_markdown(report: &Report) -> String {
    let mut out = String::new();
    let title = if report.title.is_empty() { "Report" } else { report.title.as_str() };
    out.push_str(&format!("# {title}\n"));

    if report.empty {
        out.push_str("\n_No sources to report on — nothing cited was available, so there's nothing to assemble._\n");
        return out;
    }

    for section in &report.sections {
        out.push_str(&format!("\n## {}\n", section.heading));
        for (text, id) in &section.points {
            let marker = format!("[{id}]");
            if text.trim_end().ends_with(&marker) {
                // The point already carries its own marker for this source — keep it
                // as-is rather than doubling it.
                out.push_str(&format!("- {}\n", text));
            } else {
                out.push_str(&format!("- {} {}\n", text, marker));
            }
        }
    }

    if !report.all_citations.is_empty() {
        out.push_str("\n## Sources\n");
        for c in &report.all_citations {
            out.push_str(&format!("[{}] {} — {}\n", c.id, c.title, c.url));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// CONFIG — the ON-by-default flag the live op reads
// ---------------------------------------------------------------------------

/// The report op's runtime knobs, mirrored from [`crate::config::ReportConfig`] so
/// the pure builder stays config-shaped without depending on the whole `Config`.
/// `enabled` is the master gate (ships ON — full-power default; the report op is
/// READ-ONLY and folds already-cited material into a bounded report, safe to enable
/// outright): with it false the live "generate a report on X" op declines (it never
/// builds). The pure [`build_report`] does not consult `enabled` — the gate lives at
/// the op boundary; the builder is always available to tests. In lockstep with
/// `config::ReportConfig::default()`.
#[derive(Debug, Clone, Copy)]
pub struct ReportConfig {
    /// Whether the live report op is enabled. ON by default.
    pub enabled: bool,
}

impl Default for ReportConfig {
    fn default() -> Self {
        // SHIPS ON (full-power default) — the report op is READ-ONLY (folds saved,
        // already-cited material into a bounded report; honest-empty when none),
        // safe to enable outright. In lockstep with config::ReportConfig::default().
        Self { enabled: true }
    }
}

// ---------------------------------------------------------------------------
// INTENT — "generate a report on X" (explicit, phrase-anchored)
// ---------------------------------------------------------------------------

/// A report-generation intent parsed from an utterance: build a report on `topic`.
/// CONSERVATIVE — only an explicit "report on / write a report about X" phrasing
/// trips it; an ordinary question never does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportIntent {
    /// The topic the report is on, as the user named it (normalized via
    /// [`crate::notebook::topic_key`] downstream to find the saved material).
    pub topic: String,
}

/// Detect a "generate a report on X" intent. CONSERVATIVE and phrase-anchored: the
/// utterance must carry "report" together with a generate/write/build/make verb (or
/// the canonical "generate a report on / report on" lead-in) AND name a topic after
/// "on/about". So "what's in the quarterly report" (a question, no generate verb)
/// and "research X" never trip it. Pure — unit-tested.
pub fn classify_report_intent(utterance: &str) -> Option<ReportIntent> {
    let lower = utterance.to_lowercase();
    let lower = lower.trim();
    if !lower.contains("report") {
        return None;
    }
    // Must carry an explicit build verb so a mere mention of "report" (a question
    // about an existing report) does not trip it.
    const VERBS: &[&str] = &["generate", "write", "build", "make", "create", "produce", "draft", "compile"];
    if !VERBS.iter().any(|v| lower.contains(v)) {
        return None;
    }
    // Pull the topic after the LAST " on " / " about " that follows "report".
    let topic = extract_report_topic(lower)?;
    if topic.is_empty() {
        return None;
    }
    Some(ReportIntent { topic })
}

/// Extract the topic from a report-generation utterance: the text after the
/// "report on"/"report about" (or a trailing "on"/"about"). Returns the trimmed,
/// question-mark-stripped subject, or `None` when no topic follows.
fn extract_report_topic(lower: &str) -> Option<String> {
    // Prefer the segment right after "report on"/"report about".
    for anchor in [" report on ", " report about ", "report on ", "report about "] {
        if let Some(pos) = lower.find(anchor) {
            let rest = &lower[pos + anchor.len()..];
            let topic = rest.trim().trim_end_matches('?').trim();
            if !topic.is_empty() {
                return Some(topic.to_string());
            }
        }
    }
    // Fallback: a trailing " on X" / " about X" anywhere (e.g. "write me a report,
    // on X"). Take the last occurrence so "report on the budget" still works.
    for anchor in [" on ", " about "] {
        if let Some(pos) = lower.rfind(anchor) {
            let rest = &lower[pos + anchor.len()..];
            let topic = rest.trim().trim_end_matches('?').trim();
            if !topic.is_empty() {
                return Some(topic.to_string());
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// FROM NOTEBOOK ENTRIES — the live op's already-cited input source
// ---------------------------------------------------------------------------

/// Turn the agent-scoped, already-cited notebook entries on a topic into the
/// [`SourcedClaim`]s [`build_report`] assembles. Each saved run becomes a section
/// (heading "Run N — <topic>"), its already-redacted synthesized text the body,
/// and EACH of the run's REAL persisted citations a sourced claim under that
/// heading. The cite discipline carries straight through: a notebook entry holds
/// ONLY grounded citations (enforced by [`crate::notebook::entry_from_report`]), so
/// every claim here carries a real source ref — and an entry with NO citations
/// contributes NO claim (its run was honestly unsourced), so the report stays
/// cite-only-real and honestly empty when nothing was sourced. Pure (no I/O).
pub fn sourced_claims_from_entries(
    entries: &[crate::memory::NotebookEntry],
) -> Vec<SourcedClaim> {
    let mut out = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        let heading = if entry.topic.trim().is_empty() {
            format!("Run {}", i + 1)
        } else {
            format!("Run {} — {}", i + 1, entry.topic.trim())
        };
        // STRIP the run's OWN citation apparatus before the body becomes a
        // report claim (sweep residual). `synthesized` is a rendered cited
        // answer: it carries RUN-LOCAL "[n]" markers and may carry an inline
        // "Sources: [1] Title — url" tail. build_report renumbers citations
        // REPORT-locally, so those embedded markers would point at the wrong
        // bibliography row — the exact collision the renumbering fixed at the
        // report level, leaking back in through the body text. The structured
        // citations below carry the real provenance, so the prose does not
        // need (and must not keep) its own stale numbering.
        let body = strip_run_local_citations(entry.synthesized.trim());
        if body.trim().is_empty() {
            continue;
        }
        // ONE claim per citation (build_report lists only citations a body point
        // actually references, so a textless claim would silently drop its
        // source from the bibliography). The prose is emitted with the FIRST
        // citation; the rest carry a short pointer instead of repeating the
        // whole paragraph — before this a 4-source run printed the same
        // paragraph four times.
        for (n, cit) in entry.citations.iter().enumerate() {
            let text = if n == 0 {
                body.clone()
            } else {
                // A short, DISTINCT pointer — not a copy of the prose. It exists
                // only so build_report references (and therefore lists) this
                // run's remaining real sources; repeating the paragraph per
                // citation is what this fix removes.
                format!("Additional source for this run: {}.", cit.title.trim())
            };
            out.push(SourcedClaim::cited(
                heading.clone(),
                text,
                SourceRef::new(cit.source_id.max(0) as usize, cit.title.clone(), cit.url.clone()),
            ));
        }
    }
    out
}


/// PURE: strip a synthesized answer's OWN citation apparatus — the trailing
/// "Sources: [1] … ; [2] …" tail and any inline "[n]" markers — so the prose can
/// be renumbered report-locally without carrying stale run-local ids. Text that
/// has no apparatus is returned unchanged (minus surrounding whitespace).
fn strip_run_local_citations(body: &str) -> String {
    // Drop the inline bibliography tail the notebook renderer appends.
    let cut = body
        .rfind("Sources: [")
        .or_else(|| body.rfind("Sources:["))
        .unwrap_or(body.len());
    let head = &body[..cut];
    // Drop bare "[n]" markers (digits only — never touches "[TODO]" or a real
    // bracketed phrase), then tidy the doubled spaces they leave behind.
    let mut out = String::with_capacity(head.len());
    let bytes = head.as_bytes();
    let mut i = 0;
    while i < head.len() {
        if bytes[i] == b'[' {
            if let Some(close) = head[i..].find(']') {
                let inner = &head[i + 1..i + close];
                if !inner.is_empty() && inner.bytes().all(|b| b.is_ascii_digit()) {
                    i += close + 1;
                    continue;
                }
            }
        }
        out.push(head[i..].chars().next().unwrap());
        i += head[i..].chars().next().unwrap().len_utf8();
    }
    // Collapse the whitespace the removals leave (" ,", "  ", " .").
    let collapsed = out
        .replace(" ,", ",")
        .replace(" .", ".")
        .replace("  ", " ");
    collapsed.trim().to_string()
}

// ---------------------------------------------------------------------------
// DISPATCH — a classified intent -> a built + rendered report (read-only)
// ---------------------------------------------------------------------------

/// The outcome of handling a report intent: the rendered markdown to display, a
/// short telemetry verb naming WHAT happened, and the structured [`Report`] the HUD
/// can render (the title, sections, and REAL citations). Honest throughout — an
/// empty report carries `empty: true` and the honest-empty markdown.
pub struct ReportOutcome {
    /// The rendered markdown report (honest-empty-aware).
    pub markdown: String,
    /// A short telemetry verb: "report" (built, with or without content) /
    /// "report_empty" (nothing citable) / "report_off" (the op is disabled).
    pub verb: &'static str,
    /// The structured report (for the HUD). `None` only for the disabled case.
    pub report: Option<Report>,
}

/// Handle a [`ReportIntent`] against the agent-scoped notebook store: pull the
/// already-cited runs on the topic and fold them into a bounded markdown report.
/// READ-ONLY — it reads runs that were really saved and assembles them; it never
/// fetches, never calls a model, never persists. GATED: with the op disabled it
/// declines honestly (verb "report_off") and reads nothing. HONEST-EMPTY: a topic
/// with no saved cited runs yields an honest empty report (never a fabricated one).
pub async fn dispatch(
    memory: &crate::memory::Memory,
    namespace: &str,
    intent: ReportIntent,
    cfg: &ReportConfig,
) -> anyhow::Result<ReportOutcome> {
    if !cfg.enabled {
        return Ok(ReportOutcome {
            markdown: "Report generation is off, sir — turn on [report].enabled and I'll \
                       assemble one from your saved research, with its real sources."
                .to_string(),
            verb: "report_off",
            report: None,
        });
    }
    // Pull the agent-scoped, already-cited notebook runs on the topic (own +
    // shared). An empty list -> an honest-empty report.
    let entries = crate::notebook::revisit(memory, namespace, &intent.topic).await?;
    let claims = sourced_claims_from_entries(&entries);
    let report = build_report(&intent.topic, &claims, cfg);
    let markdown = render_markdown(&report);
    let verb = if report.empty { "report_empty" } else { "report" };
    Ok(ReportOutcome { markdown, verb, report: Some(report) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ReportConfig {
        ReportConfig::default()
    }

    fn sref(id: usize, url: &str) -> SourceRef {
        SourceRef::new(id, format!("Source {id}"), url)
    }

    // ---- assembly: cited claims -> ordered, grouped sections ---------------

    #[test]
    fn assembles_cited_claims_into_ordered_grouped_sections() {
        let sources = vec![
            SourcedClaim::cited("Overview", "X is a thing", sref(1, "https://a.test")),
            SourcedClaim::cited("Overview", "X has parts", sref(2, "https://b.test")),
            SourcedClaim::cited("Details", "A part does Y", sref(1, "https://a.test")),
        ];
        let r = build_report("All about X", &sources, &cfg());
        assert!(!r.empty);
        assert_eq!(r.title, "All about X");
        // Two sections, in first-seen order.
        assert_eq!(r.sections.len(), 2);
        assert_eq!(r.sections[0].heading, "Overview");
        assert_eq!(r.sections[1].heading, "Details");
        // Overview grouped both its claims.
        assert_eq!(r.sections[0].points.len(), 2);
        assert_eq!(r.sections[0].points[0], ("X is a thing".to_string(), 1));
        // Distinct citations: source 1 (cited twice) listed once, plus source 2.
        assert_eq!(r.all_citations.len(), 2, "{:?}", r.all_citations);
        assert_eq!(r.all_citations[0].id, 1);
        assert_eq!(r.all_citations[1].id, 2);
    }

    // ---- cite-only-real: an uncited claim is dropped, never given a source -

    #[test]
    fn drops_uncited_claims_never_fabricates_a_citation() {
        let sources = vec![
            SourcedClaim::cited("S", "a backed point", sref(1, "https://real.test")),
            // No citation at all -> dropped.
            SourcedClaim::uncited("S", "an unbacked assertion"),
            // A citation with a BLANK url -> not usable -> dropped.
            SourcedClaim::cited("S", "a blank-url point", SourceRef::new(9, "Phantom", "   ")),
        ];
        let r = build_report("topic", &sources, &cfg());
        // Only the one real, backed point survives.
        assert_eq!(r.sections.len(), 1);
        assert_eq!(r.sections[0].points.len(), 1, "only the cited point: {:?}", r.sections[0].points);
        assert_eq!(r.sections[0].points[0].0, "a backed point");
        // Exactly the one real citation — never the blank/phantom one.
        assert_eq!(r.all_citations.len(), 1);
        assert_eq!(r.all_citations[0].url, "https://real.test");
        assert!(
            !r.all_citations.iter().any(|c| c.id == 9),
            "a blank-url citation must never be carried: {:?}",
            r.all_citations
        );
        // The dropped assertions never reach the rendered body either.
        let md = render_markdown(&r);
        assert!(!md.contains("unbacked assertion"), "uncited body leaked: {md}");
        assert!(!md.contains("blank-url point"), "blank-url body leaked: {md}");
    }

    // ---- honest-empty: no citable source -> an honest empty report ---------

    #[test]
    fn no_citable_source_is_honest_empty_never_fabricates() {
        // All inputs are uncited / blank-url -> nothing citable.
        let sources = vec![
            SourcedClaim::uncited("S", "claim one"),
            SourcedClaim::cited("S", "claim two", SourceRef::new(1, "t", "")),
        ];
        let r = build_report("nothing to say", &sources, &cfg());
        assert!(r.empty, "no citable source -> empty");
        assert!(r.sections.is_empty());
        assert!(r.all_citations.is_empty());
        let md = render_markdown(&r);
        assert!(
            md.to_lowercase().contains("no sources to report on"),
            "honest-empty render: {md}"
        );
        // The empty render carries NO fabricated body or citation.
        assert!(!md.contains("claim one") && !md.contains("claim two"), "{md}");
        assert!(!md.contains("## Sources"), "no bibliography in an empty report: {md}");
    }

    #[test]
    fn empty_input_is_honest_empty() {
        let r = build_report("nothing", &[], &cfg());
        assert!(r.empty);
        let md = render_markdown(&r);
        assert!(md.to_lowercase().contains("no sources to report on"), "{md}");
    }

    // ---- bounded: sections, points-per-section, citations all capped -------

    #[test]
    fn bounds_the_number_of_sections() {
        // MAX_SECTIONS + 3 distinct headings, each with one cited claim.
        let mut sources = Vec::new();
        for i in 0..(MAX_SECTIONS + 3) {
            sources.push(SourcedClaim::cited(
                format!("Section {i}"),
                format!("point {i}"),
                sref(i + 1, &format!("https://s{i}.test")),
            ));
        }
        let r = build_report("big", &sources, &cfg());
        assert_eq!(r.sections.len(), MAX_SECTIONS, "sections bounded to the cap");
        // The bibliography only lists citations the kept body references.
        assert!(r.all_citations.len() <= MAX_SECTIONS, "{:?}", r.all_citations.len());
        assert!(
            r.all_citations.iter().all(|c| c.id <= MAX_SECTIONS),
            "no citation beyond the kept sections: {:?}",
            r.all_citations
        );
    }

    #[test]
    fn bounds_the_points_per_section() {
        let mut sources = Vec::new();
        for i in 0..(MAX_CLAIMS_PER_SECTION + 5) {
            sources.push(SourcedClaim::cited(
                "One Section",
                format!("point {i}"),
                sref(i + 1, &format!("https://s{i}.test")),
            ));
        }
        let r = build_report("crowded", &sources, &cfg());
        assert_eq!(r.sections.len(), 1);
        assert_eq!(
            r.sections[0].points.len(),
            MAX_CLAIMS_PER_SECTION,
            "points per section bounded to the cap"
        );
    }

    #[test]
    fn bounds_the_total_citations() {
        // One section, but far more than MAX_CITATIONS distinct sources — the body
        // is capped at MAX_CLAIMS_PER_SECTION so the referenced-citation count is
        // bounded by that, never exceeding MAX_CITATIONS.
        let mut sources = Vec::new();
        for i in 0..(MAX_CITATIONS + 10) {
            sources.push(SourcedClaim::cited(
                format!("Section {}", i % MAX_SECTIONS),
                format!("point {i}"),
                sref(i + 1, &format!("https://s{i}.test")),
            ));
        }
        let r = build_report("huge", &sources, &cfg());
        assert!(
            r.all_citations.len() <= MAX_CITATIONS,
            "citations bounded to the cap: {}",
            r.all_citations.len()
        );
    }

    // ---- render: exactly the cited points + a real bibliography ------------

    #[test]
    fn render_carries_only_real_citations_in_the_bibliography() {
        let sources = vec![
            SourcedClaim::cited("Findings", "the sky is blue", sref(1, "https://sky.test")),
            SourcedClaim::uncited("Findings", "an uncited rumor"),
        ];
        let r = build_report("Sky report", &sources, &cfg());
        let md = render_markdown(&r);
        assert!(md.contains("# Sky report"), "title rendered: {md}");
        assert!(md.contains("## Findings"), "section heading rendered: {md}");
        assert!(md.contains("- the sky is blue [1]"), "cited point with marker: {md}");
        assert!(md.contains("## Sources"), "bibliography present: {md}");
        assert!(md.contains("[1] Source 1 — https://sky.test"), "real source listed: {md}");
        assert!(!md.contains("uncited rumor"), "dropped claim must not render: {md}");
    }

    // ---- intent classification ---------------------------------------------

    #[test]
    fn classifies_explicit_report_requests() {
        assert_eq!(
            classify_report_intent("generate a report on the JWST"),
            Some(ReportIntent { topic: "the jwst".to_string() })
        );
        assert_eq!(
            classify_report_intent("write me a report about black holes"),
            Some(ReportIntent { topic: "black holes".to_string() })
        );
        assert_eq!(
            classify_report_intent("build a report on quantum computing?"),
            Some(ReportIntent { topic: "quantum computing".to_string() })
        );
        // A question about an existing report (no build verb) is NOT an intent.
        assert_eq!(classify_report_intent("what's in the quarterly report"), None);
        // A bare research request never trips it.
        assert_eq!(classify_report_intent("research the competitors"), None);
        // "report" with a verb but no topic -> None.
        assert_eq!(classify_report_intent("generate the report"), None);
    }

    // ---- from notebook entries: cite-only-real carries through -------------

    fn entry(topic: &str, synth: &str, cites: Vec<(i64, &str, &str)>) -> crate::memory::NotebookEntry {
        crate::memory::NotebookEntry {
            id: 0,
            ts: String::new(),
            agent_namespace: "agent.darwin".into(),
            topic_key: topic.to_lowercase(),
            topic: topic.into(),
            synthesized: synth.into(),
            citations: cites
                .into_iter()
                .map(|(id, title, url)| crate::memory::NotebookCitation {
                    source_id: id,
                    title: title.into(),
                    url: url.into(),
                })
                .collect(),
        }
    }

    #[test]
    fn builds_a_report_from_notebook_entries_citing_only_their_real_sources() {
        let entries = vec![
            entry("the JWST", "It sees infrared", vec![(1, "NASA", "https://nasa.test")]),
            entry("the JWST", "Launched 2021", vec![(2, "ESA", "https://esa.test")]),
        ];
        let claims = sourced_claims_from_entries(&entries);
        let r = build_report("the JWST", &claims, &cfg());
        assert!(!r.empty);
        // One section per run.
        assert_eq!(r.sections.len(), 2);
        assert!(r.sections[0].heading.contains("Run 1"));
        // Both real notebook sources carried, never invented.
        assert_eq!(r.all_citations.len(), 2);
        let urls: Vec<&str> = r.all_citations.iter().map(|c| c.url.as_str()).collect();
        assert!(urls.contains(&"https://nasa.test") && urls.contains(&"https://esa.test"), "{urls:?}");
    }

    /// REGRESSION (sweep residual): a saved run's `synthesized` text carries its
    /// OWN run-local "[n]" markers and may end with an inline
    /// "Sources: [1] Title — url" tail. build_report renumbers citations
    /// REPORT-locally, so those embedded markers would point at the wrong
    /// bibliography row — the collision the renumbering fixed, leaking back in
    /// through the prose. They must be stripped before the body becomes a claim.
    #[test]
    fn a_runs_own_citation_markers_are_stripped_from_the_report_body() {
        let entries = vec![entry(
            "X",
            "JWST saw early galaxies [1] and confirmed the redshift [2]. Sources: [1] NASA — https://nasa.test; [2] ESA — https://esa.test.",
            vec![(1, "NASA", "https://nasa.test"), (2, "ESA", "https://esa.test")],
        )];
        let claims = sourced_claims_from_entries(&entries);
        let r = build_report("X", &claims, &cfg());
        let md = render_markdown(&r);
        // The run's own inline bibliography is gone from the BODY...
        let body_end = md.find("## Sources").unwrap_or(md.len());
        let body = &md[..body_end];
        assert!(!body.contains("Sources: ["), "the run's inline tail leaked: {body}");
        assert!(!body.contains("[1] NASA"), "the run's own bibliography leaked: {body}");
        // ...and both real sources are still listed, renumbered report-locally.
        assert_eq!(r.all_citations.len(), 2, "{:?}", r.all_citations);
        let urls: Vec<&str> = r.all_citations.iter().map(|c| c.url.as_str()).collect();
        assert!(urls.contains(&"https://nasa.test") && urls.contains(&"https://esa.test"));
        // The prose is not repeated once per citation.
        let occurrences = body.matches("saw early galaxies").count();
        assert_eq!(occurrences, 1, "the paragraph must appear once, got {occurrences}: {body}");
    }

    /// REGRESSION: two saved runs each number their OWN sources from 1, so folding
    /// them into one report used to emit a bibliography with two `[1]` entries and
    /// a `[1]` marker under "Run 2" that resolved to run 1's source. Report-local
    /// renumbering makes every marker resolve to exactly one listed source.
    #[test]
    fn citation_ids_are_report_local_so_runs_never_collide() {
        let entries = vec![
            entry("X", "run one", vec![(1, "A1", "https://a1.test"), (2, "A2", "https://a2.test")]),
            entry("X", "run two", vec![(1, "B1", "https://b1.test"), (2, "B2", "https://b2.test")]),
        ];
        let claims = sourced_claims_from_entries(&entries);
        let r = build_report("X", &claims, &cfg());

        // Four distinct real sources -> four DISTINCT ids.
        assert_eq!(r.all_citations.len(), 4, "{:?}", r.all_citations);
        let ids: Vec<usize> = r.all_citations.iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![1, 2, 3, 4], "ids are dense + unique: {ids:?}");
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "no duplicate bibliography id: {ids:?}");

        // Run 2's body markers point at run 2's sources, NOT run 1's.
        let run2 = r.sections.iter().find(|s| s.heading.contains("Run 2")).expect("run 2 section");
        let run2_ids: Vec<usize> = run2.points.iter().map(|(_, id)| *id).collect();
        for id in &run2_ids {
            let cited = r.all_citations.iter().find(|c| c.id == *id).expect("marker resolves");
            assert!(
                cited.url.contains("b1.test") || cited.url.contains("b2.test"),
                "run 2's marker [{id}] must resolve to a run-2 source, got {}",
                cited.url
            );
        }

        // …and every marker in the rendered markdown has exactly one bibliography line.
        let md = render_markdown(&r);
        for id in 1..=4 {
            assert_eq!(
                md.matches(&format!("\n[{id}] ")).count(),
                1,
                "exactly one `## Sources` line for [{id}]: {md}"
            );
        }
    }

    /// REGRESSION: a claim whose citation is refused by [`MAX_CITATIONS`] must be
    /// bounded out of the BODY too. It used to still emit its `[n]` marker while
    /// the bibliography stopped at the cap — a dangling marker pointing at nothing.
    #[test]
    fn a_claim_past_the_citation_cap_is_bounded_out_of_the_body_too() {
        let mut sources = Vec::new();
        for i in 0..(MAX_CITATIONS + 1) {
            sources.push(SourcedClaim::cited(
                format!("H{}", i % MAX_SECTIONS),
                format!("claim {i}"),
                sref(i + 1, &format!("https://e{i}.test")),
            ));
        }
        let r = build_report("capped", &sources, &cfg());
        let listed: std::collections::BTreeSet<usize> =
            r.all_citations.iter().map(|c| c.id).collect();
        let body: std::collections::BTreeSet<usize> = r
            .sections
            .iter()
            .flat_map(|s| s.points.iter().map(|(_, id)| *id))
            .collect();
        let dangling: Vec<usize> = body.difference(&listed).copied().collect();
        assert!(dangling.is_empty(), "body cites unlisted sources: {dangling:?}");
        // The claim past the cap never reached the rendered body.
        let md = render_markdown(&r);
        assert!(
            !md.contains(&format!("claim {}", MAX_CITATIONS)),
            "the over-cap claim must not render: {md}"
        );
    }

    #[test]
    fn render_does_not_double_a_pre_existing_citation_marker() {
        // A notebook entry's synthesized text already ends with its own [1] marker;
        // the render must NOT append a second one (it would only duplicate, never
        // add a real citation).
        let entries = vec![entry("X", "Key finding [1]", vec![(1, "Src", "https://x.test")])];
        let claims = sourced_claims_from_entries(&entries);
        let r = build_report("X", &claims, &cfg());
        let md = render_markdown(&r);
        assert!(md.contains("- Key finding [1]\n"), "single marker kept: {md}");
        assert!(!md.contains("[1] [1]"), "no doubled marker: {md}");
    }

    #[test]
    fn an_unsourced_notebook_run_contributes_nothing_honest_empty() {
        // A notebook entry with NO citations (an honestly-unsourced run) yields no
        // sourced claim, so the report is honestly empty — never a fabricated body.
        let entries = vec![entry("dry topic", "I found nothing sourced", vec![])];
        let claims = sourced_claims_from_entries(&entries);
        assert!(claims.is_empty(), "an uncited run contributes no claim");
        let r = build_report("dry topic", &claims, &cfg());
        assert!(r.empty, "no citable source -> honest empty");
        let md = render_markdown(&r);
        assert!(md.to_lowercase().contains("no sources to report on"), "{md}");
    }

    // ---- config: ON by default (full-power; read-only report op) -----------

    #[test]
    fn report_config_ships_on_by_default() {
        assert!(ReportConfig::default().enabled, "the report op ships ON (full-power default)");
    }
}
