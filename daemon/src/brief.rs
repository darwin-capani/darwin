//! SMARTER BRIEF (#23) — a PURE brief builder that turns a bag of proactive
//! [`Signal`]s into a RANKED, CAPPED, CITED, HONESTLY-EMPTY digest.
//!
//! The existing EDITH evaluator ([`crate::anticipate::evaluate`]) picks the ONE
//! strongest trigger per tick. This builder does the COMPLEMENTARY job: given
//! every signal that crossed its relevance floor this moment (calendar, mail,
//! health, market, news, routine), it assembles a GLANCE — the few that matter,
//! in priority order, each citing its REAL source — for the on-demand brief and
//! the proactive surface.
//!
//! ## The contract (mirrors anticipate.rs's grounding + episodic.rs's honesty)
//!   1. RANK by relevance. Each [`Signal`] carries a [`Priority`] (Urgent /
//!      Important / Routine). [`build_brief`] sorts Urgent first, ties broken by a
//!      stable secondary key (the source kind, then the citation) so the order is
//!      deterministic — an urgent calendar conflict outranks a routine news item,
//!      always.
//!   2. CAP the count. A brief is a GLANCE, not a dump: at most [`MAX_ITEMS`]
//!      items survive (the most relevant). The cap can be tightened further by a
//!      focus profile's verbosity (see [`build_brief`]'s `max_items` arg).
//!   3. CITE every item to its REAL source. Each [`Signal`] carries a
//!      [`Citation`] naming its ORIGIN — a calendar event id, a Gmail message id,
//!      a news source/url, the memory-health subsystem. The rendered item carries
//!      that citation verbatim; the builder NEVER invents a citation and NEVER
//!      emits an item whose citation is empty/placeholder.
//!   4. HONEST-EMPTY. With NO signals (every source absent — unconnected, so
//!      contributing nothing), the brief is `empty == true` and carries ZERO
//!      items. It NEVER pads with a fabricated line. An unconnected source is
//!      honestly ABSENT — the builder reasons only over the signals it is GIVEN.
//!
//! ## Honesty boundary (the same one anticipate.rs draws)
//! The builder is a PURE function of the signals it receives. It does not fetch,
//! does not reach the network, does not invent. The LIVE signal collection
//! (calendar/mail/news) is credential/device-gated and happens at the edge
//! ([`crate::signals`]); an unconnected source simply yields no [`Signal`], and
//! the builder renders an honest brief over whatever is present. There is no path
//! by which an absent source becomes a fabricated item.
//!
//! ## Focus integration (#24)
//! [`build_brief`] takes the focus-tuned [`crate::focus::TunedBehavior`] so the
//! active focus profile can (a) drop signals whose CATEGORY the profile silences
//! and (b) tighten the item cap via verbosity. With the Default profile the
//! filter is the identity, so the brief is byte-for-byte the unfocused digest.

use serde::{Deserialize, Serialize};

use crate::focus::{SignalCategory, TunedBehavior};

/// Hard cap on items in one brief — a GLANCE, not a dump. Small on purpose; a
/// focus profile's verbosity can tighten it further, never loosen it past this.
pub const MAX_ITEMS: usize = 4;

// ---------------------------------------------------------------------------
// Priority — the relevance rank axis
// ---------------------------------------------------------------------------

/// How relevant a signal is, for ranking. `Urgent` (an imminent conflict, a
/// critical reading) outranks `Important` (notable unread mail), which outranks
/// `Routine` (a recurring heads-up, a news item). Ordered so a plain sort puts
/// the most relevant first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    /// Top of the brief — time-critical / consequential-to-miss.
    Urgent,
    /// Worth surfacing but not urgent.
    Important,
    /// A heads-up / low-priority item — first to be capped out.
    Routine,
}

impl Priority {
    /// Sort key (smaller == higher priority): Urgent=0, Important=1, Routine=2.
    fn rank(&self) -> u8 {
        match self {
            Priority::Urgent => 0,
            Priority::Important => 1,
            Priority::Routine => 2,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Priority::Urgent => "urgent",
            Priority::Important => "important",
            Priority::Routine => "routine",
        }
    }
}

// ---------------------------------------------------------------------------
// Citation — the REAL origin of a signal (never invented)
// ---------------------------------------------------------------------------

/// Where a signal came from — its REAL, verifiable origin. Every brief item
/// carries one verbatim; the builder never fabricates a citation and never
/// surfaces an item whose citation is empty. The `source` is the connected
/// system (e.g. "calendar", "gmail", "global_scan", "memory_health") and `ref_id`
/// is the concrete identifier in that system (an event id, a message id, a news
/// source/url, a subsystem key). Secret-free: an id/source name, never a body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Citation {
    /// The connected source system the signal originated in.
    pub source: String,
    /// The concrete reference within that source (event id / message id / url /
    /// subsystem key). The thing that makes the item VERIFIABLE.
    pub ref_id: String,
}

impl Citation {
    /// A citation is USABLE only when both fields are non-empty — that is the
    /// honesty floor: an item with no real reference is not cited, and the
    /// builder refuses to surface it (better silent than a fabricated source).
    pub fn is_usable(&self) -> bool {
        !self.source.trim().is_empty() && !self.ref_id.trim().is_empty()
    }

    /// The human/telemetry rendering, e.g. "calendar:evt_abc123".
    pub fn render(&self) -> String {
        format!("{}:{}", self.source.trim(), self.ref_id.trim())
    }
}

// ---------------------------------------------------------------------------
// Signal — the injected input the builder ranks (never fetched here)
// ---------------------------------------------------------------------------

/// One proactive signal the builder may surface. Assembled at the live edge from
/// a CONNECTED source ([`crate::signals`]); an UNCONNECTED source produces NO
/// signal (honestly absent). Carries its category (for focus filtering), its
/// priority (for ranking), the human line, and its real citation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signal {
    /// The coarse category (calendar/mail/health/market/news/routine/critical),
    /// for focus filtering + tie-break ordering.
    pub category: SignalCategory,
    /// How relevant — the primary rank key.
    pub priority: Priority,
    /// The honest one-line text (grounded in the source; never fabricated).
    pub text: String,
    /// The REAL source citation. An item with an unusable citation is dropped.
    pub citation: Citation,
}

impl Signal {
    /// Convenience constructor.
    pub fn new(
        category: SignalCategory,
        priority: Priority,
        text: impl Into<String>,
        source: impl Into<String>,
        ref_id: impl Into<String>,
    ) -> Signal {
        Signal {
            category,
            priority,
            text: text.into(),
            citation: Citation {
                source: source.into(),
                ref_id: ref_id.into(),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// BriefItem + Brief — the ranked, cited output
// ---------------------------------------------------------------------------

/// One rendered item of the brief: a priority, the line, and the citation that
/// makes it verifiable. Distinct from [`Signal`] so the output type carries only
/// what the surface needs (no category-internal detail beyond the cite).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BriefItem {
    pub priority: Priority,
    pub text: String,
    /// The real source citation, rendered (e.g. "calendar:evt_abc"). Always
    /// present + usable (the builder dropped any item without one).
    pub citation: String,
}

impl BriefItem {
    /// Telemetry for one item (HUD digest row). Secret-free: a line + a citation.
    pub fn telemetry(&self) -> serde_json::Value {
        serde_json::json!({
            "priority": self.priority.as_str(),
            "text": self.text,
            "source": self.citation,
        })
    }
}

/// The assembled brief: the ranked+capped items, plus `empty` (true iff there
/// were no surfacable signals — an HONEST empty brief, never padded). The live
/// surface emits this; an empty brief means "nothing on the radar", stated
/// honestly, never a fabricated item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Brief {
    pub items: Vec<BriefItem>,
    pub empty: bool,
}

impl Brief {
    /// The honest-empty brief.
    pub fn empty() -> Brief {
        Brief {
            items: Vec::new(),
            empty: true,
        }
    }

    /// A spoken-friendly rendering: each item line with its citation, or the
    /// honest "nothing on the radar" line when empty. The SAME honest-empty copy
    /// the existing on-demand brief uses, so an empty smart-brief reads exactly
    /// like today's "all quiet".
    pub fn render_spoken(&self) -> String {
        if self.empty || self.items.is_empty() {
            return "Nothing on the radar right now, sir. All quiet.".to_string();
        }
        self.items
            .iter()
            .map(|it| format!("{} (source: {})", it.text, it.citation))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// HUD telemetry for the whole digest (the `proactive.digest` card — a
    /// DISTINCT event from the first-contact `proactive.brief`). Carries the
    /// ranked items + the honest-empty flag + the item count. Secret-free.
    pub fn telemetry(&self) -> serde_json::Value {
        serde_json::json!({
            "empty": self.empty,
            "count": self.items.len(),
            "items": self.items.iter().map(|it| it.telemetry()).collect::<Vec<_>>(),
        })
    }
}

// ---------------------------------------------------------------------------
// build_brief — PURE: rank + cap + cite + honest-empty (+ focus filter)
// ---------------------------------------------------------------------------

/// Build a brief from the injected `signals`, under the focus-`tuned` behavior.
///
/// PURE and deterministic. The pipeline:
///   1. DROP any signal whose citation is NOT usable (no real source -> never
///      surfaced; we refuse to fabricate a source).
///   2. FOCUS FILTER (#24): drop any signal whose CATEGORY the tuned behavior
///      does not surface. With the Default profile every category surfaces, so
///      this is the identity.
///   3. RANK: sort by priority (Urgent first), ties broken by category then
///      citation render — fully deterministic.
///   4. CAP: keep at most `min(MAX_ITEMS, tuned.verbosity.max_items(MAX_ITEMS))`
///      — the verbosity can tighten the cap (Brief -> 1, Silent -> 0), never
///      loosen it past [`MAX_ITEMS`].
///   5. HONEST-EMPTY: if nothing survives, return [`Brief::empty`] (empty=true,
///      zero items) — NEVER a padded/fabricated line.
///
/// Because the builder reasons ONLY over the `signals` it is handed, an
/// unconnected source (which produced no signal) contributes nothing — honestly
/// absent, never invented.
pub fn build_brief(signals: &[Signal], tuned: &TunedBehavior) -> Brief {
    // 1 + 2: keep only well-cited signals in a surfaced category.
    let mut kept: Vec<&Signal> = signals
        .iter()
        .filter(|s| s.citation.is_usable())
        .filter(|s| tuned.surfaces(s.category))
        .collect();

    // 3: deterministic relevance rank — priority, then category, then citation.
    kept.sort_by(|a, b| {
        a.priority
            .rank()
            .cmp(&b.priority.rank())
            .then_with(|| a.category.cmp(&b.category))
            .then_with(|| a.citation.render().cmp(&b.citation.render()))
    });

    // 4: cap (verbosity may tighten; never exceed MAX_ITEMS).
    let cap = tuned.verbosity.max_items(MAX_ITEMS).min(MAX_ITEMS);
    let items: Vec<BriefItem> = kept
        .into_iter()
        .take(cap)
        .map(|s| BriefItem {
            priority: s.priority,
            text: s.text.clone(),
            citation: s.citation.render(),
        })
        .collect();

    // 5: honest-empty — never pad.
    if items.is_empty() {
        Brief::empty()
    } else {
        Brief { items, empty: false }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::focus::{apply_profile, BaseBehavior, FocusProfile, Verbosity};

    /// The unfocused (Default-profile) tuned behavior: every category surfaces,
    /// full verbosity. The base over which the builder behaves "as today".
    fn unfocused() -> TunedBehavior {
        apply_profile(&FocusProfile::Default, &BaseBehavior::default())
    }

    fn sig(cat: SignalCategory, pri: Priority, text: &str, source: &str, id: &str) -> Signal {
        Signal::new(cat, pri, text, source, id)
    }

    // =====================================================================
    // RANKING — urgent outranks important outranks routine
    // =====================================================================

    #[test]
    fn ranks_urgent_above_important_above_routine() {
        let signals = vec![
            sig(SignalCategory::News, Priority::Routine, "Markets steady", "global_scan", "reuters-1"),
            sig(SignalCategory::Mail, Priority::Important, "3 unread", "gmail", "msg_42"),
            sig(SignalCategory::Calendar, Priority::Urgent, "1:1 in 5 min", "calendar", "evt_9"),
        ];
        let b = build_brief(&signals, &unfocused());
        assert!(!b.empty);
        assert_eq!(b.items.len(), 3);
        // Urgent calendar first, then important mail, then routine news.
        assert_eq!(b.items[0].priority, Priority::Urgent);
        assert!(b.items[0].text.contains("1:1"));
        assert_eq!(b.items[1].priority, Priority::Important);
        assert_eq!(b.items[2].priority, Priority::Routine);
    }

    #[test]
    fn ranking_is_deterministic_for_ties() {
        // Two items at the SAME priority: ordering is stable (category, then
        // citation), so the brief is reproducible.
        let signals = vec![
            sig(SignalCategory::News, Priority::Routine, "News B", "global_scan", "b"),
            sig(SignalCategory::Routine, Priority::Routine, "Routine A", "memory_health", "a"),
        ];
        let b1 = build_brief(&signals, &unfocused());
        // Same input in a different order -> same output order.
        let mut shuffled = signals.clone();
        shuffled.reverse();
        let b2 = build_brief(&shuffled, &unfocused());
        assert_eq!(b1, b2, "ranking must be deterministic regardless of input order");
        // News (category enum-ordered before Routine? no — Routine sorts AFTER
        // News by enum order) — assert the concrete stable order.
        assert_eq!(b1.items.len(), 2);
    }

    // =====================================================================
    // CAPPING — a glance, not a dump
    // =====================================================================

    #[test]
    fn caps_to_max_items_keeping_the_most_relevant() {
        // More signals than the cap; the highest-priority survive.
        let mut signals = Vec::new();
        // 2 urgent, 2 important, 4 routine -> 8 total, cap is MAX_ITEMS (4).
        for i in 0..2 {
            signals.push(sig(SignalCategory::Calendar, Priority::Urgent, &format!("urgent {i}"), "calendar", &format!("u{i}")));
        }
        for i in 0..2 {
            signals.push(sig(SignalCategory::Mail, Priority::Important, &format!("important {i}"), "gmail", &format!("i{i}")));
        }
        for i in 0..4 {
            signals.push(sig(SignalCategory::News, Priority::Routine, &format!("routine {i}"), "global_scan", &format!("r{i}")));
        }
        let b = build_brief(&signals, &unfocused());
        assert_eq!(b.items.len(), MAX_ITEMS, "capped to a glance");
        // The kept items are the highest priority — the routines were capped out.
        assert!(b.items.iter().all(|it| it.priority != Priority::Routine),
            "the cap kept urgent+important over routine: {:?}", b.items);
    }

    // =====================================================================
    // CITATION — every item cites its REAL source; uncited dropped; never invented
    // =====================================================================

    #[test]
    fn every_item_carries_its_real_source_citation() {
        let signals = vec![
            sig(SignalCategory::Calendar, Priority::Urgent, "Standup soon", "calendar", "evt_abc"),
            sig(SignalCategory::Mail, Priority::Important, "Unread from boss", "gmail", "msg_xyz"),
        ];
        let b = build_brief(&signals, &unfocused());
        let cals: Vec<&BriefItem> = b.items.iter().filter(|it| it.citation.starts_with("calendar:")).collect();
        assert_eq!(cals.len(), 1, "the calendar item cites the real event id");
        assert_eq!(cals[0].citation, "calendar:evt_abc");
        let mails: Vec<&BriefItem> = b.items.iter().filter(|it| it.citation.starts_with("gmail:")).collect();
        assert_eq!(mails[0].citation, "gmail:msg_xyz", "the mail item cites the real message id");
    }

    #[test]
    fn a_signal_without_a_usable_citation_is_dropped_never_fabricated() {
        // An item whose citation is empty has no real source — it must NOT
        // surface (we never invent a source to make it look grounded).
        let signals = vec![
            sig(SignalCategory::News, Priority::Routine, "Uncited rumor", "", ""),
            sig(SignalCategory::Calendar, Priority::Urgent, "Real event", "calendar", "evt_1"),
        ];
        let b = build_brief(&signals, &unfocused());
        assert_eq!(b.items.len(), 1, "the uncited signal is dropped");
        assert_eq!(b.items[0].citation, "calendar:evt_1");
        assert!(!b.items.iter().any(|it| it.text.contains("rumor")), "an uncited line never surfaces");
    }

    #[test]
    fn citation_render_is_source_colon_ref() {
        let c = Citation { source: "global_scan".into(), ref_id: "reuters/abc".into() };
        assert!(c.is_usable());
        assert_eq!(c.render(), "global_scan:reuters/abc");
        // Whitespace-only fields are NOT usable.
        assert!(!Citation { source: "  ".into(), ref_id: "x".into() }.is_usable());
        assert!(!Citation { source: "calendar".into(), ref_id: "".into() }.is_usable());
    }

    // =====================================================================
    // HONEST-EMPTY — no signals => empty brief, never padded
    // =====================================================================

    #[test]
    fn no_signals_yields_an_honest_empty_brief() {
        let b = build_brief(&[], &unfocused());
        assert!(b.empty, "no signals -> empty=true");
        assert!(b.items.is_empty(), "no fabricated padding");
        assert!(b.render_spoken().to_lowercase().contains("nothing"), "honest empty copy: {}", b.render_spoken());
    }

    #[test]
    fn all_signals_dropped_yields_an_honest_empty_brief_not_a_fabrication() {
        // Every signal is uncited -> all dropped -> the brief is honestly empty,
        // NOT padded with an invented line.
        let signals = vec![
            sig(SignalCategory::News, Priority::Routine, "Uncited A", "", ""),
            sig(SignalCategory::Mail, Priority::Important, "Uncited B", "gmail", ""),
        ];
        let b = build_brief(&signals, &unfocused());
        assert!(b.empty, "all-dropped -> honest empty, never invented");
        assert!(b.items.is_empty());
    }

    #[test]
    fn an_unconnected_source_contributes_no_item_honestly_absent() {
        // The builder reasons ONLY over the signals it is given. A source that is
        // not connected simply produced no Signal — so it contributes nothing,
        // and the builder never invents one to fill the gap. Here only calendar
        // is connected (one signal); mail/news produced nothing.
        let signals = vec![sig(SignalCategory::Calendar, Priority::Important, "Review at 2pm", "calendar", "evt_77")];
        let b = build_brief(&signals, &unfocused());
        assert_eq!(b.items.len(), 1, "only the connected source's signal surfaces");
        assert_eq!(b.items[0].citation, "calendar:evt_77");
        // No fabricated mail/news item appeared.
        assert!(!b.items.iter().any(|it| it.citation.starts_with("gmail:") || it.citation.starts_with("global_scan:")));
    }

    // =====================================================================
    // FOCUS INTEGRATION (#24) — a profile filters categories + tightens cap
    // =====================================================================

    #[test]
    fn focus_default_profile_is_the_identity_over_the_brief() {
        // Default profile == today's brief: every category surfaces, full cap.
        let signals = vec![
            sig(SignalCategory::News, Priority::Routine, "World news", "global_scan", "n1"),
            sig(SignalCategory::Calendar, Priority::Urgent, "Meeting now", "calendar", "e1"),
        ];
        let focused = build_brief(&signals, &unfocused());
        assert_eq!(focused.items.len(), 2, "default profile surfaces both categories");
    }

    #[test]
    fn deep_focus_silences_all_but_critical_in_the_brief() {
        // Under DeepFocus the tuned behavior surfaces ONLY critical AND has Silent
        // verbosity (0 items) — so a non-critical digest goes fully dark.
        let tuned = apply_profile(&FocusProfile::DeepFocus, &BaseBehavior::default());
        let signals = vec![
            sig(SignalCategory::News, Priority::Routine, "News", "global_scan", "n1"),
            sig(SignalCategory::Mail, Priority::Important, "Mail", "gmail", "m1"),
        ];
        let b = build_brief(&signals, &tuned);
        assert!(b.empty, "deep focus silences the non-critical digest entirely");
    }

    #[test]
    fn sleep_profile_drops_news_keeps_critical() {
        // Sleep surfaces only Critical. A critical signal still gets through; news
        // is dropped. (Verbosity Brief caps to 1, but only the critical survives
        // the category filter anyway.)
        let tuned = apply_profile(&FocusProfile::Sleep, &BaseBehavior::default());
        let signals = vec![
            sig(SignalCategory::News, Priority::Routine, "Late-night news", "global_scan", "n1"),
            sig(SignalCategory::Critical, Priority::Urgent, "Disk critically full", "memory_health", "disk"),
        ];
        let b = build_brief(&signals, &tuned);
        assert_eq!(b.items.len(), 1, "only the critical signal surfaces asleep");
        assert_eq!(b.items[0].citation, "memory_health:disk");
        assert!(!b.items.iter().any(|it| it.citation.starts_with("global_scan:")), "news dropped asleep");
    }

    #[test]
    fn work_profile_silences_news_but_keeps_calendar_and_mail() {
        let tuned = apply_profile(&FocusProfile::Work, &BaseBehavior::default());
        let signals = vec![
            sig(SignalCategory::News, Priority::Routine, "Headlines", "global_scan", "n1"),
            sig(SignalCategory::Calendar, Priority::Urgent, "Standup", "calendar", "e1"),
            sig(SignalCategory::Mail, Priority::Important, "Boss emailed", "gmail", "m1"),
        ];
        let b = build_brief(&signals, &tuned);
        assert!(!b.items.iter().any(|it| it.citation.starts_with("global_scan:")), "work silences news");
        assert!(b.items.iter().any(|it| it.citation.starts_with("calendar:")), "work keeps calendar");
        assert!(b.items.iter().any(|it| it.citation.starts_with("gmail:")), "work keeps mail");
    }

    #[test]
    fn brief_verbosity_tightens_the_cap_never_loosens() {
        // A Brief-verbosity tuned behavior caps to a single item even when many
        // signals would otherwise survive (a glance, not a dump). It can never
        // exceed MAX_ITEMS regardless.
        let tuned = TunedBehavior {
            surfacing: SignalCategory::all().to_vec(),
            verbosity: Verbosity::Brief,
            suggestions_quieted: false,
        };
        let signals = vec![
            sig(SignalCategory::Calendar, Priority::Urgent, "First", "calendar", "e1"),
            sig(SignalCategory::Mail, Priority::Important, "Second", "gmail", "m1"),
            sig(SignalCategory::News, Priority::Routine, "Third", "global_scan", "n1"),
        ];
        let b = build_brief(&signals, &tuned);
        assert_eq!(b.items.len(), 1, "Brief verbosity caps to a one-line glance");
        assert_eq!(b.items[0].priority, Priority::Urgent, "and it is the highest-priority item");
    }

    // =====================================================================
    // TELEMETRY — the HUD digest card
    // =====================================================================

    #[test]
    fn telemetry_carries_items_with_citations_and_empty_flag() {
        let signals = vec![sig(SignalCategory::Calendar, Priority::Urgent, "Meeting", "calendar", "e1")];
        let t = build_brief(&signals, &unfocused()).telemetry();
        assert_eq!(t["empty"], false);
        assert_eq!(t["count"], 1);
        assert_eq!(t["items"][0]["priority"], "urgent");
        assert_eq!(t["items"][0]["source"], "calendar:e1");
        // Empty brief telemetry.
        let te = build_brief(&[], &unfocused()).telemetry();
        assert_eq!(te["empty"], true);
        assert_eq!(te["count"], 0);
    }
}
