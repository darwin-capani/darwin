//! APERTURE — a private, owner-gated, ON-DEVICE activity timeline ("Recall done
//! right"). It answers "what was I working on around 3pm" / "what did I do this
//! morning" from a bounded, PII-redacted record of WHICH app was frontmost + its
//! window TITLE + for HOW LONG — never a screen pixel, never a keystroke, and
//! never anything that leaves the box.
//!
//! ## Honesty about coverage (say this, don't imply more)
//! Aperture records THREE things per activity: the frontmost **app name**, its
//! **window title** (redacted), and the **duration** it stayed frontmost. It does
//! NOT capture screen pixels, screenshots, text you typed, or content inside a
//! window — only the app + title + time. The recall copy is honest about that.
//!
//! ## Safety / privacy contract (mirrors [screen_context] + [pasteboard])
//!   * SHIPS OFF (`[aperture].enabled = false`). An activity timeline is
//!     privacy-sensitive, so NOTHING is polled or stored until the owner opts in.
//!     With it off [`global_ingest`] is a pure NO-OP (stores nothing, returns
//!     false) and the poll loop is never spawned.
//!   * PII-REDACTED at the source: every window title is passed through
//!     [`crate::optimize::redact`] BEFORE it can enter the store — the raw title
//!     never lives in an [`Activity`] (enforced by [`capture_activity`], the single
//!     construction seam, AND re-applied inside [`global_ingest`]). The app NAME is
//!     a stable, low-cardinality identifier (e.g. "Safari") and is kept verbatim
//!     (bounded in length), never a place PII lives.
//!   * BOUNDED retention: the timeline ring is capped (`[aperture].retention`);
//!     recording a new activity evicts the OLDEST past the cap, so the timeline
//!     cannot grow without bound on an always-on appliance. The ring is TRANSIENT
//!     (in-RAM only — NEVER written to memory / the optimizer corpus / disk).
//!   * ON-DEVICE only: nothing here reaches the network. Recall READS the ring and
//!     renders a summary; it never actuates, and it NEVER fabricates (an empty /
//!     un-fed ring, or a window with no recorded activity, is an honest "I have no
//!     activity recorded", never an invented session).
//!
//! ## The device-gated read is the RUNNER; the store + query are PURE seams
//! The frontmost-app + window-title read (macOS System Events / the AXTitle
//! accessibility attribute) is DEVICE-GATED (it needs runtime Accessibility TCC
//! consent, which no flag can grant) and is NOT exercised by any test. What IS
//! unit-tested is the PURE core: the bounded timeline store (record / eviction /
//! duration bucketing / merge-consecutive-same-app), redaction-before-storage,
//! the recall-query CONSTRUCTION (utterance + now -> a time window + subject), the
//! OFF-stores-nothing gate, and the conservative intent classifier.

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Local, TimeZone};
use serde_json::{json, Value};

use crate::optimize::redact;

/// Hard cap on the app-name length stored per activity (a stable identifier is
/// short; this only guards against a hostile/oversized read).
const MAX_APP_LEN: usize = 80;
/// Hard cap on the redacted-title length stored per activity.
const MAX_TITLE_LEN: usize = 200;
/// How many recent activities the `aperture.status` frame carries for the HUD.
const HUD_PREVIEW_COUNT: usize = 8;
/// Characters of a redacted title echoed as a PREVIEW to the HUD status frame.
const PREVIEW_TITLE_LEN: usize = 80;
/// Fallback settings for the pre-install OFF state (config owns the real defaults).
const FALLBACK_CAP: usize = 500;
const FALLBACK_POLL_SECS: u64 = 20;

// ===========================================================================
// Activity — one timeline entry (app + redacted title + a [start,end] span).
// ===========================================================================

/// One activity in the timeline: WHICH app was frontmost, its (already-redacted)
/// window title, and the [`start_ts`, `end_ts`] unix-second span it stayed
/// frontmost. Duration is `end_ts - start_ts`. The `title` is ALWAYS the output of
/// [`redact`] (a secret in a window title never enters the store); the `app` is a
/// stable identifier kept verbatim (bounded).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activity {
    pub app: String,
    pub title: String,
    pub start_ts: u64,
    pub end_ts: u64,
}

impl Activity {
    /// The span length in seconds (`end_ts - start_ts`, saturating so a clock
    /// glitch can never underflow). A single-sample activity is 0 seconds.
    pub fn duration_secs(&self) -> u64 {
        self.end_ts.saturating_sub(self.start_ts)
    }
}

/// Redact a raw window title and bound its length — the single seam that
/// guarantees a title is redacted BEFORE it can be stored (mirrors
/// [`crate::pasteboard::capture_clip`]). The app name is bounded but not redacted
/// (it is a stable identifier, never a place PII lives).
pub fn capture_activity(app: &str, raw_title: &str, ts: u64) -> Activity {
    Activity {
        app: bound(app.trim(), MAX_APP_LEN),
        title: bound(&redact(raw_title), MAX_TITLE_LEN),
        start_ts: ts,
        end_ts: ts,
    }
}

/// Trim `s` to at most `max` characters (char-safe), never a byte-split panic.
fn bound(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

// ===========================================================================
// Duration bucketing — a human-readable span label (pure).
// ===========================================================================

/// Bucket a duration in seconds into a compact, human label:
/// `< 60s` -> "under a minute"; `< 1h` -> "Nm"; else "Hh" / "Hh Mm". Pure. Used by
/// the recall render + the HUD preview so a span reads naturally.
pub fn format_duration(secs: u64) -> String {
    if secs < 60 {
        return "under a minute".to_string();
    }
    if secs < 3600 {
        return format!("{}m", secs / 60);
    }
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    if mins == 0 {
        format!("{hours}h")
    } else {
        format!("{hours}h {mins}m")
    }
}

// ===========================================================================
// ApertureTimeline — the PURE, bounded, merge-on-record ring.
// ===========================================================================

/// A BOUNDED timeline ring: oldest at the front, newest at the back. Recording an
/// activity MERGES it into the last entry when the SAME app + SAME (redacted) title
/// is still frontmost within `merge_gap_secs` (extending that entry's `end_ts`
/// rather than storing a duplicate — this is what turns a stream of identical
/// polls into one "you were in X for 40m" span). A different app/title, or a gap
/// wider than `merge_gap_secs` (so the loop was off in between — never inflate a
/// span across a hole), starts a NEW entry. Past `cap` the OLDEST entry is evicted.
/// PURE + deterministic.
#[derive(Debug, Clone)]
pub struct ApertureTimeline {
    entries: VecDeque<Activity>,
    cap: usize,
    merge_gap_secs: u64,
}

impl ApertureTimeline {
    /// A timeline bounded to `cap` entries (floored to >= 1 so a misconfigured 0
    /// never makes the ring useless), merging consecutive same-app+title samples
    /// no more than `merge_gap_secs` apart.
    pub fn new(cap: usize, merge_gap_secs: u64) -> Self {
        Self {
            entries: VecDeque::new(),
            cap: cap.max(1),
            merge_gap_secs,
        }
    }

    /// The retention cap (>= 1). Exercised by the unit tests; the live path reads
    /// the cap through the config / global settings.
    #[allow(dead_code)]
    pub fn cap(&self) -> usize {
        self.cap
    }

    /// Number of stored activities (<= `cap`).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the timeline is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Record ONE frontmost sample (`app` + already-redacted `title` at unix `ts`).
    /// MERGE-CONSECUTIVE-SAME-APP: if the newest entry has the same app + title and
    /// `ts` is within `merge_gap_secs` of its `end_ts`, that entry's `end_ts` is
    /// extended (no duplicate; its duration grows). Otherwise a new entry is pushed.
    /// EVICT-OLDEST past `cap`. An empty app is dropped (nothing frontmost => nothing
    /// recorded). A `ts` that goes backwards relative to the last entry is clamped so
    /// a span never shrinks. Pure.
    pub fn record(&mut self, app: &str, title: &str, ts: u64) {
        if app.trim().is_empty() {
            return;
        }
        if let Some(last) = self.entries.back_mut() {
            let same = last.app == app && last.title == title;
            let gap = ts.saturating_sub(last.end_ts);
            if same && gap <= self.merge_gap_secs {
                // Extend the current span (monotonic — never shrink it).
                last.end_ts = last.end_ts.max(ts);
                return;
            }
        }
        self.entries.push_back(Activity {
            app: app.to_string(),
            title: title.to_string(),
            start_ts: ts,
            end_ts: ts,
        });
        while self.entries.len() > self.cap {
            self.entries.pop_front();
        }
    }

    /// Retune the cap in place, evicting the oldest entries if it shrank. Floored to
    /// >= 1.
    pub fn set_cap(&mut self, cap: usize) {
        self.cap = cap.max(1);
        while self.entries.len() > self.cap {
            self.entries.pop_front();
        }
    }

    /// Every stored activity, NEWEST first (the window the recall reasons over).
    pub fn snapshot(&self) -> Vec<Activity> {
        self.entries.iter().rev().cloned().collect()
    }

    /// Up to `n` activities, NEWEST first (for the bounded HUD preview).
    pub fn recent(&self, n: usize) -> Vec<Activity> {
        self.entries.iter().rev().take(n).cloned().collect()
    }

    /// Drop every activity ("forget my activity timeline").
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

// ===========================================================================
// Time window + recall-query CONSTRUCTION (pure — the unit-tested heart).
// ===========================================================================

/// A bounded time window the recall is scoped to, in unix seconds, with a human
/// `label` for the reply copy ("this morning", "around 3pm").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeWindow {
    pub start: u64,
    pub end: u64,
    pub label: String,
}

/// A recall query built from an utterance: an optional time `window` ("this
/// morning" / "around 3pm") and an optional `subject` ("...about the budget"). A
/// bare recall carries neither and summarizes the whole recent timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApertureQuery {
    pub window: Option<TimeWindow>,
    pub subject: Option<String>,
}

/// The unix second at `hour:minute` on the LOCAL calendar day of `now`. Falls back
/// to `now`'s own timestamp on any (extremely rare) unrepresentable local time
/// (e.g. a DST gap), so it never panics. Pure given `now`.
fn local_unix_at(now: &DateTime<Local>, hour: u32, minute: u32) -> u64 {
    let naive = match now.date_naive().and_hms_opt(hour.min(23), minute.min(59), 0) {
        Some(n) => n,
        None => return now.timestamp().max(0) as u64,
    };
    match Local.from_local_datetime(&naive).single() {
        Some(dt) => dt.timestamp().max(0) as u64,
        None => now.timestamp().max(0) as u64,
    }
}

/// Humanize a 24h hour as a 12h clock label ("15" -> "3pm", "0" -> "12am").
fn humanize_hour(h24: u32) -> String {
    let (h12, suffix) = match h24 {
        0 => (12, "am"),
        1..=11 => (h24, "am"),
        12 => (12, "pm"),
        _ => (h24 - 12, "pm"),
    };
    format!("{h12}{suffix}")
}

/// Parse a spoken clock hour ("3pm", "3:30 pm", "11am") to a 24h hour, or None.
/// CONSERVATIVE: it requires an am/pm suffix so a bare number elsewhere in the
/// sentence never reads as a time. Pure.
fn parse_clock_hour(lower: &str) -> Option<u32> {
    let tokens: Vec<&str> = lower.split_whitespace().collect();
    for (i, tok) in tokens.iter().enumerate() {
        for suffix in ["pm", "am"] {
            if let Some(head) = tok.strip_suffix(suffix) {
                let head = head.trim();
                let numeric = if head.is_empty() {
                    // "3 pm" — the number is the previous token.
                    i.checked_sub(1).and_then(|j| tokens.get(j)).copied()
                } else {
                    Some(head)
                };
                if let Some(n) = numeric.and_then(hour_1_to_12) {
                    return Some(to_24h(n, suffix));
                }
            }
        }
    }
    None
}

/// Parse a leading 1..=12 hour from a token, tolerating a ":MM" tail ("3", "3:30").
fn hour_1_to_12(tok: &str) -> Option<u32> {
    let head = tok.split(':').next().unwrap_or(tok);
    head.parse::<u32>().ok().filter(|h| (1..=12).contains(h))
}

/// Map a 1..=12 clock hour + am/pm to a 24h hour.
fn to_24h(h: u32, suffix: &str) -> u32 {
    match suffix {
        "pm" => {
            if h == 12 {
                12
            } else {
                h + 12
            }
        }
        // "am"
        _ => {
            if h == 12 {
                0
            } else {
                h
            }
        }
    }
}

/// Extract an optional recall SUBJECT ("...about the budget" / "...on the report")
/// — the trimmed, bounded phrase or None. Pure; narrows a recall to matching
/// activities.
fn extract_subject(lower: &str) -> Option<String> {
    for lead in [
        "about the ",
        "about ",
        "regarding the ",
        "regarding ",
        "related to ",
        "on the ",
    ] {
        if let Some(idx) = lower.find(lead) {
            let tail = lower[idx + lead.len()..].trim();
            let phrase = tail.trim_end_matches(|c: char| !c.is_alphanumeric()).trim();
            // Reject a phrase that is itself only a time cue ("3pm") so "...around
            // 3pm" is a window, not a subject.
            if !phrase.is_empty() && phrase.len() <= 64 && !phrase.chars().all(|c| c.is_ascii_digit()) {
                return Some(phrase.to_string());
            }
        }
    }
    None
}

/// Parse an optional TIME WINDOW from a (lowercased) utterance relative to `now`.
/// Recognizes the named periods (this morning / afternoon / evening / tonight),
/// "today" / "earlier today" (midnight -> now), "the last/past hour", "the past N
/// hours", and "around/at N (am|pm)" (a +-45min window). None when no time cue is
/// present (the recall then summarizes the whole recent timeline). Pure given `now`.
fn parse_time_window(lower: &str, now: &DateTime<Local>) -> Option<TimeWindow> {
    let now_ts = now.timestamp().max(0) as u64;

    if lower.contains("this morning") || lower.contains("in the morning") {
        return Some(TimeWindow {
            start: local_unix_at(now, 5, 0),
            end: local_unix_at(now, 12, 0),
            label: "this morning".to_string(),
        });
    }
    if lower.contains("this afternoon") || lower.contains("in the afternoon") {
        return Some(TimeWindow {
            start: local_unix_at(now, 12, 0),
            end: local_unix_at(now, 17, 0),
            label: "this afternoon".to_string(),
        });
    }
    if lower.contains("this evening") || lower.contains("tonight") || lower.contains("in the evening")
    {
        return Some(TimeWindow {
            start: local_unix_at(now, 17, 0),
            end: local_unix_at(now, 23, 0),
            label: "this evening".to_string(),
        });
    }
    // "the past 2 hours" / "last 3 hours".
    if let Some(n) = parse_past_n_hours(lower) {
        return Some(TimeWindow {
            start: now_ts.saturating_sub(u64::from(n) * 3600),
            end: now_ts,
            label: format!("the past {n} hours"),
        });
    }
    if lower.contains("the last hour") || lower.contains("past hour") || lower.contains("last hour") {
        return Some(TimeWindow {
            start: now_ts.saturating_sub(3600),
            end: now_ts,
            label: "the last hour".to_string(),
        });
    }
    if lower.contains("earlier today") || lower.contains("so far today") || lower.contains("today") {
        return Some(TimeWindow {
            start: local_unix_at(now, 0, 0),
            end: now_ts,
            label: "today".to_string(),
        });
    }
    // "around 3pm" / "at 9am" — a +-45min window around the named hour.
    if let Some(h24) = parse_clock_hour(lower) {
        let center = local_unix_at(now, h24, 0);
        return Some(TimeWindow {
            start: center.saturating_sub(45 * 60),
            end: center + 45 * 60,
            label: format!("around {}", humanize_hour(h24)),
        });
    }
    None
}

/// Parse "past/last N hours" -> N (bounded 1..=24), or None. Pure.
fn parse_past_n_hours(lower: &str) -> Option<u32> {
    for lead in ["past ", "last "] {
        if let Some(idx) = lower.find(lead) {
            let tail = &lower[idx + lead.len()..];
            let mut parts = tail.split_whitespace();
            if let (Some(num), Some(unit)) = (parts.next(), parts.next()) {
                if unit.starts_with("hour") {
                    if let Ok(n) = num.parse::<u32>() {
                        if (1..=24).contains(&n) {
                            return Some(n);
                        }
                    }
                }
            }
        }
    }
    None
}

/// Build an [`ApertureQuery`] from a free-text recall utterance + `now` — the
/// RECALL-QUERY-CONSTRUCTION seam. Pure and deterministic given `now`.
pub fn build_query(utterance: &str, now: &DateTime<Local>) -> ApertureQuery {
    let lower = utterance.to_lowercase();
    ApertureQuery {
        window: parse_time_window(&lower, now),
        subject: extract_subject(&lower),
    }
}

// ===========================================================================
// Summarize + render (pure — group by app, sum bucketed durations, honest-empty).
// ===========================================================================

/// One app's aggregated presence within a recall: the app, the summed (window-
/// clamped) seconds, and the title of that app's LONGEST single span (the
/// representative "what you were doing there").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppSpan {
    pub app: String,
    pub secs: u64,
    pub title: String,
}

/// The seconds `e` overlaps `window` (its full duration when unwindowed). Pure.
fn overlap_secs(e: &Activity, window: Option<&TimeWindow>) -> u64 {
    match window {
        None => e.duration_secs(),
        Some(w) => {
            let start = e.start_ts.max(w.start);
            let end = e.end_ts.min(w.end);
            end.saturating_sub(start)
        }
    }
}

/// Whether `e` overlaps `window` at all (always true when unwindowed). Pure.
fn in_window(e: &Activity, window: Option<&TimeWindow>) -> bool {
    match window {
        None => true,
        Some(w) => e.start_ts <= w.end && e.end_ts >= w.start,
    }
}

/// Summarize `entries` into per-app spans within an optional `window`: group by
/// app, SUM each app's window-clamped seconds, keep the LONGEST single span's title
/// as the representative, and sort by total seconds DESC (ties by app name, so the
/// order is deterministic — never hashmap iteration). Entries outside the window
/// are dropped. Pure.
pub fn summarize(entries: &[Activity], window: Option<&TimeWindow>) -> Vec<AppSpan> {
    let mut spans: Vec<AppSpan> = Vec::new();
    // Parallel to `spans`: the seconds of the single longest span behind each app's
    // representative title, so a later, longer span replaces the title.
    let mut best_title_secs: Vec<u64> = Vec::new();
    for e in entries {
        if !in_window(e, window) {
            continue;
        }
        let secs = overlap_secs(e, window);
        if let Some(idx) = spans.iter().position(|s| s.app == e.app) {
            spans[idx].secs = spans[idx].secs.saturating_add(secs);
            if secs > best_title_secs[idx] {
                spans[idx].title = e.title.clone();
                best_title_secs[idx] = secs;
            }
        } else {
            spans.push(AppSpan {
                app: e.app.clone(),
                secs,
                title: e.title.clone(),
            });
            best_title_secs.push(secs);
        }
    }
    spans.sort_by(|a, b| b.secs.cmp(&a.secs).then_with(|| a.app.cmp(&b.app)));
    spans
}

/// Render a bounded, honest recall over `entries` for `query`, top-`k` apps. Filters
/// by the optional subject (title/app contains it), then summarizes by app within
/// the optional window. HONEST-EMPTY: an empty timeline, or a window/subject with no
/// matching activity, yields an "I have no activity recorded…" line (never a
/// fabricated session). Names its COVERAGE (app + window title + time, not pixels).
/// Pure.
pub fn render_recall(entries: &[Activity], query: &ApertureQuery, k: usize) -> String {
    let subject_lc = query.subject.as_ref().map(|s| s.to_lowercase());
    let filtered: Vec<Activity> = entries
        .iter()
        .filter(|e| match &subject_lc {
            Some(s) => e.title.to_lowercase().contains(s) || e.app.to_lowercase().contains(s),
            None => true,
        })
        .cloned()
        .collect();

    let spans = summarize(&filtered, query.window.as_ref());
    if spans.is_empty() {
        return empty_recall_line(query);
    }

    let header = match (&query.window, &query.subject) {
        (Some(w), Some(s)) => format!("{}, you were working on this about \"{}\", sir:", cap_first(&w.label), s),
        (Some(w), None) => format!("{}, you were working on, sir:", cap_first(&w.label)),
        (None, Some(s)) => format!("Here's your recent activity about \"{s}\", sir:"),
        (None, None) => "Here's your recent activity, sir:".to_string(),
    };
    let mut out = header;
    for span in spans.iter().take(k.max(1)) {
        out.push('\n');
        if span.title.trim().is_empty() {
            out.push_str(&format!("- {} — {}", span.app, format_duration(span.secs)));
        } else {
            out.push_str(&format!(
                "- {} — {} ({})",
                span.app,
                format_duration(span.secs),
                span.title.trim()
            ));
        }
    }
    out
}

/// The honest empty line for a recall that matched nothing — scoped to the query so
/// it never implies data that was not recorded. Each variant LEADS with "I have no"
/// so the citation layer treats it as an empty retrieval.
fn empty_recall_line(query: &ApertureQuery) -> String {
    match (&query.window, &query.subject) {
        (Some(w), _) => format!(
            "I have no record of what you were working on {}, sir.",
            w.label
        ),
        (None, Some(s)) => format!("I have no activity recorded about \"{s}\", sir."),
        (None, None) => "I have no activity recorded yet, sir.".to_string(),
    }
}

/// Uppercase the first character of a label ("this morning" -> "This morning").
fn cap_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

// ===========================================================================
// Process-global ring + settings (mirrors pasteboard's poison-tolerant slots).
// ===========================================================================

/// The live `[aperture]` settings, installed ONCE at startup. Poison-tolerant
/// `Mutex` global (mirrors screen_context / pasteboard process-global state).
#[derive(Debug, Clone, Copy)]
struct ApertureSettings {
    enabled: bool,
    cap: usize,
    poll_interval_secs: u64,
    merge_gap_secs: u64,
}

impl ApertureSettings {
    /// The shipped OFF default: nothing polled or stored.
    const fn off() -> Self {
        Self {
            enabled: false,
            cap: FALLBACK_CAP,
            poll_interval_secs: FALLBACK_POLL_SECS,
            merge_gap_secs: FALLBACK_POLL_SECS * 3,
        }
    }
}

static RING: Mutex<Option<ApertureTimeline>> = Mutex::new(None);
static SETTINGS: Mutex<ApertureSettings> = Mutex::new(ApertureSettings::off());

fn ring_lock() -> MutexGuard<'static, Option<ApertureTimeline>> {
    RING.lock().unwrap_or_else(|p| p.into_inner())
}

fn settings_lock() -> MutexGuard<'static, ApertureSettings> {
    SETTINGS.lock().unwrap_or_else(|p| p.into_inner())
}

/// The merge gap derived from the poll interval: three missed polls (bounded below
/// so a tiny interval still tolerates a hiccup). A gap wider than this starts a new
/// span rather than inflating one across a hole.
fn derive_merge_gap(poll_interval_secs: u64) -> u64 {
    poll_interval_secs.saturating_mul(3).max(60)
}

/// Install the `[aperture]` settings ONCE from config at startup. When ENABLED the
/// bounded timeline is created (or retuned to the new cap). When OFF (the shipped
/// default) the ring is DROPPED entirely — nothing is retained, and the poll loop
/// (which checks [`is_enabled`]) records nothing.
pub fn install_settings(enabled: bool, cap: usize, poll_interval_secs: u64) {
    let cap = cap.max(1);
    let poll_interval_secs = poll_interval_secs.max(1);
    let merge_gap_secs = derive_merge_gap(poll_interval_secs);
    *settings_lock() = ApertureSettings {
        enabled,
        cap,
        poll_interval_secs,
        merge_gap_secs,
    };
    let mut ring = ring_lock();
    if enabled {
        match ring.as_mut() {
            Some(store) => store.set_cap(cap),
            None => *ring = Some(ApertureTimeline::new(cap, merge_gap_secs)),
        }
    } else {
        // OFF: retain nothing (a runtime disable also wipes the transient ring).
        *ring = None;
    }
}

/// Whether the activity timeline is currently on (the poll loop checks this each
/// tick, so a runtime disable stops recording immediately).
pub fn is_enabled() -> bool {
    settings_lock().enabled
}

/// Ingest ONE frontmost sample IFF enabled: redact the title, then record it
/// (merging into the current span when the same app + title stays frontmost).
/// Returns whether it was recorded. When OFF this is a pure NO-OP — it stores
/// NOTHING and returns `false` (the privacy guarantee). An empty app is skipped.
/// The title is REDACTED here (via [`capture_activity`]) before it can enter the
/// ring.
pub fn global_ingest(app: &str, raw_title: &str, ts: u64) -> bool {
    let settings = *settings_lock();
    if !settings.enabled {
        return false; // shipped-OFF default: record NOTHING.
    }
    if app.trim().is_empty() {
        return false;
    }
    let activity = capture_activity(app, raw_title, ts);
    let mut ring = ring_lock();
    let store = ring.get_or_insert_with(|| ApertureTimeline::new(settings.cap, settings.merge_gap_secs));
    store.record(&activity.app, &activity.title, ts);
    true
}

/// The full stored timeline, newest first (what the recall reasons over).
fn global_snapshot() -> Vec<Activity> {
    ring_lock().as_ref().map(|s| s.snapshot()).unwrap_or_default()
}

/// Up to `n` stored activities, newest first (empty when off / un-fed).
fn global_recent(n: usize) -> Vec<Activity> {
    ring_lock().as_ref().map(|s| s.recent(n)).unwrap_or_default()
}

/// Number of stored activities (0 when off / un-fed).
pub fn global_len() -> usize {
    ring_lock().as_ref().map(|s| s.len()).unwrap_or(0)
}

/// Wipe the activity timeline. Returns whether anything was cleared.
pub fn global_clear() -> bool {
    let mut ring = ring_lock();
    match ring.as_mut() {
        Some(store) => {
            let had = !store.is_empty();
            store.clear();
            had
        }
        None => false,
    }
}

/// Render a recall over the live timeline for a pre-built [`ApertureQuery`] — the
/// router-op recall surface (the router already classified the intent).
pub fn global_render_recall(query: &ApertureQuery, k: usize) -> String {
    render_recall(&global_snapshot(), query, k)
}

/// Render a recall over the live timeline from a FREE-TEXT utterance + `now` — the
/// `aperture_recall` TOOL surface (it builds the query itself). Honest-empty when
/// off / un-fed / no match.
pub fn global_render_recall_text(utterance: &str, now: &DateTime<Local>, k: usize) -> String {
    let query = build_query(utterance, now);
    global_render_recall(&query, k)
}

// ===========================================================================
// Telemetry — the secret-free `aperture.status` frame for the HUD panel.
// ===========================================================================

/// A short, redaction-safe PREVIEW of an ALREADY-redacted title: trimmed to
/// [`PREVIEW_TITLE_LEN`] with a trailing ellipsis when clipped. Pure.
fn title_preview(redacted: &str) -> String {
    let trimmed = redacted.trim();
    if trimmed.chars().count() <= PREVIEW_TITLE_LEN {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(PREVIEW_TITLE_LEN).collect();
    format!("{}…", head.trim_end())
}

/// Build the `aperture.status` payload: the enabled gate, the activity COUNT, the
/// cap, the poll cadence, and up to [`HUD_PREVIEW_COUNT`] recent activities (each an
/// app name, an ALREADY-redacted + truncated title, and the span's `duration_secs`)
/// — newest first. When off, `recent` is empty and the count is 0. SECRET-FREE
/// (titles are redacted at capture; app names are stable identifiers).
pub fn status_frame() -> Value {
    let settings = *settings_lock();
    let recent: Vec<Value> = if settings.enabled {
        global_recent(HUD_PREVIEW_COUNT)
            .iter()
            .map(|a| {
                json!({
                    "app": a.app,
                    "title": title_preview(&a.title),
                    "duration_secs": a.duration_secs(),
                })
            })
            .collect()
    } else {
        Vec::new()
    };
    json!({
        "enabled": settings.enabled,
        "count": global_len(),
        "cap": settings.cap,
        "poll_interval_secs": settings.poll_interval_secs,
        "recent": recent,
    })
}

/// Emit the `aperture.status` telemetry frame for the HUD.
pub fn emit_status() {
    crate::telemetry::emit("aperture", "aperture.status", status_frame());
}

// ===========================================================================
// Intent classification — RECALL / FORGET (pure, conservative).
// ===========================================================================

/// A recognized activity-timeline voice command. Both are OWNER-gated at the
/// router (behind the voice-id all-scope gate) and READ-ONLY except FORGET (which
/// only wipes the in-RAM ring).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApertureIntent {
    /// "what did I do this morning" / "what was I working on around 3pm" / "show my
    /// activity timeline" -> render the bounded activity summary for the query.
    Recall(ApertureQuery),
    /// "forget my activity timeline" / "wipe my timeline" -> clear the ring.
    Forget,
}

/// Map an utterance to an aperture intent, or None when it is not one (the turn
/// falls through — crucially to the RECENT-screen-context recall for a bare "what
/// was I working on"). CONSERVATIVE and deliberately distinct from
/// `screen_context::classify_screen_context_intent`: a RECALL requires a recall cue
/// AND either an explicit "activity"/"timeline" word OR a resolvable TIME WINDOW
/// ("this morning", "around 3pm", "the last hour", "today"). So:
///   * "what was I working on around 3pm" / "what did I do this morning" -> aperture
///     (a time window is present) — this is why the router checks aperture BEFORE
///     screen_context;
///   * a bare "what was I working on" (no window, no timeline word) -> None here, so
///     it falls through to the recent screen-context recall (a different feature).
///
/// FORGET requires the "activity"/"timeline" word so a generic "clear that" never
/// wipes the ring. Pure and deterministic given `now`.
pub fn classify_aperture_intent(utterance: &str, now: &DateTime<Local>) -> Option<ApertureIntent> {
    let lower = utterance.trim().to_lowercase();
    if lower.is_empty() {
        return None;
    }
    let mentions_timeline = lower.contains("activity timeline")
        || lower.contains("my timeline")
        || lower.contains("my activity")
        || lower.contains("activity log")
        || lower.contains("activity history");

    // FORGET takes precedence so "forget my activity timeline" never reads as a
    // recall. Requires the timeline word + a wipe verb.
    let is_forget = lower.contains("forget")
        || lower.contains("wipe")
        || lower.contains("clear")
        || lower.contains("delete");
    if mentions_timeline && is_forget {
        return Some(ApertureIntent::Forget);
    }

    let recall_cue = lower.contains("what did i do")
        || lower.contains("what have i been doing")
        || lower.contains("what did i work on")
        || lower.contains("what was i working on")
        || lower.contains("what was i doing")
        || lower.contains("what were i doing")
        || lower.contains("what was i up to")
        || lower.contains("recall my activity")
        || lower.contains("show my activity")
        || lower.contains("show me my activity")
        || mentions_timeline;
    if !recall_cue {
        return None;
    }
    let query = build_query(&lower, now);
    // Require a time window OR the explicit timeline word — this is the guard that
    // keeps a bare "what was I working on" out of aperture (it belongs to the
    // recent screen-context recall) while catching the time-scoped questions.
    if query.window.is_some() || mentions_timeline {
        return Some(ApertureIntent::Recall(query));
    }
    None
}

// ===========================================================================
// The DEVICE-GATED frontmost read + poll loop (the RUNNER — untested by contract).
// ===========================================================================

/// Read the current frontmost app NAME + its window TITLE. DEVICE-GATED: the window
/// title comes from the macOS Accessibility attribute `AXTitle`, which needs runtime
/// Accessibility TCC consent (no flag can grant it) — so this only ever runs inside
/// the enabled poll loop and is never exercised by a test. Returns `None` on any
/// error or when nothing is frontmost. Uses the dependency-free `/usr/bin/osascript`
/// (mirrors pasteboard's `pbpaste` subprocess seam). The window title may be empty
/// when the app exposes none or consent is absent — the app name alone is still a
/// useful timeline entry.
#[cfg(target_os = "macos")]
async fn read_frontmost() -> Option<(String, String)> {
    // One osascript call yields "appName\nwindowTitle". The AXTitle read is wrapped
    // in a try so a missing title (or absent Accessibility consent) degrades to an
    // empty title rather than an error.
    const SCRIPT: &str = r#"tell application "System Events"
    set frontApp to first application process whose frontmost is true
    set appName to name of frontApp
    set winTitle to ""
    try
        set winTitle to value of attribute "AXTitle" of window 1 of frontApp
    end try
end tell
return appName & "\n" & winTitle"#;
    let out = tokio::process::Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(SCRIPT)
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.splitn(2, '\n');
    let app = lines.next().unwrap_or("").trim().to_string();
    let title = lines.next().unwrap_or("").trim().to_string();
    if app.is_empty() {
        None
    } else {
        Some((app, title))
    }
}

#[cfg(not(target_os = "macos"))]
async fn read_frontmost() -> Option<(String, String)> {
    None
}

/// Unix seconds now (the capture timestamp).
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The OPT-IN activity poll loop — spawned by main ONLY when `[aperture].enabled`.
/// Each tick it reads the frontmost app + window title (device-gated), redacts +
/// records it (merging the current span), and emits `aperture.status`. It re-checks
/// [`is_enabled`] every tick so a runtime disable stops recording at once. The read
/// itself is device-gated; this loop is never exercised by a test.
pub async fn poll_loop(interval_secs: u64) {
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs.max(1)));
    // Announce the initial (empty) status so the HUD panel renders immediately.
    emit_status();
    loop {
        ticker.tick().await;
        if !is_enabled() {
            continue;
        }
        let Some((app, title)) = read_frontmost().await else {
            continue;
        };
        if global_ingest(&app, &title, now_unix()) {
            emit_status();
        }
    }
}

// ===========================================================================
// Test-only reset for the process-global ring/settings.
// ===========================================================================

#[cfg(test)]
pub fn reset_for_test() {
    *settings_lock() = ApertureSettings::off();
    *ring_lock() = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    // The globals are process-global; serialize every global-touching test and
    // reset on entry so one case never leaks state into the next.
    fn serial() -> MutexGuard<'static, ()> {
        static SERIAL: Mutex<()> = Mutex::new(());
        let g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        reset_for_test();
        g
    }

    // A fixed local "now" so the window math is deterministic regardless of the
    // machine clock: 2026-07-15 at 15:30 local.
    fn fixed_now() -> DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 7, 15, 15, 30, 0)
            .single()
            .expect("a representable local time")
    }

    // The unix second at a local hour on the fixed-now day — lets a test place an
    // activity "at 3pm today" and assert the "around 3pm" query catches it.
    fn at(hour: u32, minute: u32) -> u64 {
        local_unix_at(&fixed_now(), hour, minute)
    }

    // -- the bounded store: record + eviction --------------------------------

    #[test]
    fn record_pushes_newest_last_and_snapshot_is_newest_first() {
        let mut t = ApertureTimeline::new(10, 60);
        t.record("Safari", "GitHub", 100);
        t.record("Xcode", "aperture.rs", 200);
        t.record("Mail", "Inbox", 300);
        assert_eq!(t.len(), 3);
        let snap = t.snapshot();
        let apps: Vec<&str> = snap.iter().map(|a| a.app.as_str()).collect();
        assert_eq!(apps, vec!["Mail", "Xcode", "Safari"], "newest first");
    }

    #[test]
    fn eviction_drops_the_oldest_past_the_cap() {
        let mut t = ApertureTimeline::new(2, 5);
        // Distinct apps so none merge.
        t.record("A", "a", 100);
        t.record("B", "b", 200);
        t.record("C", "c", 300);
        assert_eq!(t.len(), 2, "cap holds the ring to 2");
        let apps: Vec<String> = t.snapshot().into_iter().map(|a| a.app).collect();
        assert_eq!(apps, vec!["C".to_string(), "B".to_string()], "oldest (A) evicted");
    }

    #[test]
    fn cap_is_floored_to_one() {
        let t = ApertureTimeline::new(0, 60);
        assert_eq!(t.cap(), 1, "a 0 cap is floored so the ring is never useless");
    }

    #[test]
    fn set_cap_shrinks_and_evicts_oldest() {
        let mut t = ApertureTimeline::new(5, 5);
        for i in 0..5u64 {
            t.record(&format!("app{i}"), "x", i * 100);
        }
        t.set_cap(2);
        assert_eq!(t.len(), 2);
        let apps: Vec<String> = t.snapshot().into_iter().map(|a| a.app).collect();
        assert_eq!(apps, vec!["app4".to_string(), "app3".to_string()]);
    }

    #[test]
    fn empty_app_is_never_recorded() {
        let mut t = ApertureTimeline::new(10, 60);
        t.record("   ", "something", 100);
        assert!(t.is_empty(), "nothing frontmost => nothing recorded");
    }

    // -- MERGE-CONSECUTIVE-SAME-APP + duration -------------------------------

    #[test]
    fn consecutive_same_app_and_title_merge_into_one_growing_span() {
        let mut t = ApertureTimeline::new(10, 60);
        // Same app+title sampled every 20s within the merge gap -> ONE entry whose
        // duration grows.
        t.record("Xcode", "aperture.rs", 1000);
        t.record("Xcode", "aperture.rs", 1020);
        t.record("Xcode", "aperture.rs", 1040);
        assert_eq!(t.len(), 1, "consecutive same-app samples merge to one span");
        let e = &t.snapshot()[0];
        assert_eq!(e.start_ts, 1000);
        assert_eq!(e.end_ts, 1040);
        assert_eq!(e.duration_secs(), 40, "the span grew across the merged samples");
    }

    #[test]
    fn a_different_app_or_title_starts_a_new_span() {
        let mut t = ApertureTimeline::new(10, 60);
        t.record("Safari", "GitHub", 1000);
        t.record("Safari", "GitHub", 1020); // merges
        t.record("Safari", "Google", 1040); // title changed -> new span
        t.record("Xcode", "main.rs", 1060); // app changed -> new span
        assert_eq!(t.len(), 3, "a changed app OR title starts a new span");
    }

    #[test]
    fn a_gap_wider_than_the_merge_window_starts_a_new_span_not_an_inflated_one() {
        let mut t = ApertureTimeline::new(10, 60);
        t.record("Xcode", "aperture.rs", 1000);
        // Same app+title but 5 minutes later (loop was off in between): must NOT
        // merge into a 5-minute span — that would fabricate presence.
        t.record("Xcode", "aperture.rs", 1300);
        assert_eq!(t.len(), 2, "a wide gap starts a fresh span");
        let durations: Vec<u64> = t.snapshot().iter().map(|a| a.duration_secs()).collect();
        assert!(durations.iter().all(|&d| d == 0), "neither span was inflated across the hole");
    }

    #[test]
    fn a_backwards_timestamp_never_shrinks_a_span() {
        let mut t = ApertureTimeline::new(10, 600);
        t.record("Xcode", "x", 1000);
        t.record("Xcode", "x", 1050);
        t.record("Xcode", "x", 1020); // out-of-order/backwards tick
        let e = &t.snapshot()[0];
        assert_eq!(e.end_ts, 1050, "the span end is monotonic, never shrunk");
    }

    // -- DURATION BUCKETING --------------------------------------------------

    #[test]
    fn format_duration_buckets_are_human_and_honest() {
        assert_eq!(format_duration(0), "under a minute");
        assert_eq!(format_duration(59), "under a minute");
        assert_eq!(format_duration(60), "1m");
        assert_eq!(format_duration(150), "2m");
        assert_eq!(format_duration(3600), "1h");
        assert_eq!(format_duration(3660), "1h 1m");
        assert_eq!(format_duration(2 * 3600 + 5 * 60), "2h 5m");
    }

    // -- REDACTION BEFORE STORAGE --------------------------------------------

    #[test]
    fn capture_activity_redacts_the_title_before_storage() {
        // A secret-shaped token + an email in a window title must be stripped at
        // capture — the raw secret NEVER reaches an Activity.
        let a = capture_activity(
            "Mail",
            "Re: token sk-LIVE-abc123def456ghi789 for alice@example.com",
            7,
        );
        assert!(a.title.contains("[redacted]"), "the title was redacted: {}", a.title);
        assert!(!a.title.contains("sk-LIVE-abc123def456ghi789"), "raw key survived: {}", a.title);
        assert!(!a.title.contains("alice@example.com"), "raw email survived: {}", a.title);
        // The app name is a stable identifier, kept verbatim.
        assert_eq!(a.app, "Mail");
    }

    #[test]
    fn global_ingest_redacts_before_it_reaches_the_ring() {
        let _g = serial();
        install_settings(true, 10, 20);
        assert!(global_ingest("Notes", "card 4111111111111111 pin 123456", 1));
        let e = &global_recent(1)[0];
        assert!(e.title.contains("[redacted]"), "long digit runs redacted: {}", e.title);
        assert!(!e.title.contains("4111111111111111"), "raw PAN survived: {}", e.title);
        reset_for_test();
    }

    // -- OFF STORES NOTHING (the privacy gate) -------------------------------

    #[test]
    fn off_stores_nothing() {
        let _g = serial();
        install_settings(false, 10, 20); // shipped default posture
        assert!(!is_enabled());
        assert!(!global_ingest("Safari", "a private page", 1), "an ingest while OFF stores nothing");
        assert_eq!(global_len(), 0, "the ring stays empty when off");
        assert!(global_recent(5).is_empty());
        // The status frame is honest: off, empty, no previews.
        let frame = status_frame();
        assert_eq!(frame["enabled"], json!(false));
        assert_eq!(frame["count"], json!(0));
        assert_eq!(frame["recent"], json!([]));
        reset_for_test();
    }

    #[test]
    fn a_runtime_disable_wipes_the_transient_ring() {
        let _g = serial();
        install_settings(true, 10, 20);
        assert!(global_ingest("Xcode", "aperture.rs", 1));
        assert_eq!(global_len(), 1);
        install_settings(false, 10, 20);
        assert_eq!(global_len(), 0, "disabling wipes the in-RAM ring");
        reset_for_test();
    }

    // -- RECALL QUERY CONSTRUCTION -------------------------------------------

    #[test]
    fn build_query_parses_this_morning_window() {
        let q = build_query("what did I do this morning", &fixed_now());
        let w = q.window.expect("a morning window");
        assert_eq!(w.label, "this morning");
        assert_eq!(w.start, at(5, 0));
        assert_eq!(w.end, at(12, 0));
        assert!(q.subject.is_none());
    }

    #[test]
    fn build_query_parses_this_afternoon_and_evening() {
        let a = build_query("what was I doing this afternoon", &fixed_now())
            .window
            .expect("afternoon");
        assert_eq!((a.start, a.end), (at(12, 0), at(17, 0)));
        let e = build_query("what was I working on this evening", &fixed_now())
            .window
            .expect("evening");
        assert_eq!((e.start, e.end), (at(17, 0), at(23, 0)));
    }

    #[test]
    fn build_query_parses_around_a_clock_hour_as_a_bounded_window() {
        let q = build_query("what was I working on around 3pm", &fixed_now());
        let w = q.window.expect("a 3pm window");
        assert_eq!(w.label, "around 3pm");
        // Centered on 15:00 local, +-45 minutes.
        assert_eq!(w.start, at(15, 0) - 45 * 60);
        assert_eq!(w.end, at(15, 0) + 45 * 60);
        // "9 am" (spaced) resolves too.
        let m = build_query("what did I do at 9 am", &fixed_now())
            .window
            .expect("9am");
        assert_eq!(m.label, "around 9am");
        assert_eq!(m.start, at(9, 0) - 45 * 60);
    }

    #[test]
    fn build_query_parses_the_last_hour_and_past_n_hours() {
        let now = fixed_now();
        let now_ts = now.timestamp() as u64;
        let h = build_query("what was I working on in the last hour", &now)
            .window
            .expect("last hour");
        assert_eq!(h.start, now_ts - 3600);
        assert_eq!(h.end, now_ts);
        let n = build_query("what did I do the past 3 hours", &now)
            .window
            .expect("past N hours");
        assert_eq!(n.start, now_ts - 3 * 3600);
        assert_eq!(n.label, "the past 3 hours");
    }

    #[test]
    fn build_query_extracts_a_subject() {
        let q = build_query("what was I working on this morning about the budget", &fixed_now());
        assert_eq!(q.subject.as_deref(), Some("budget"));
        assert!(q.window.is_some(), "a subject and a window can coexist");
        // A bare recall carries neither window nor subject.
        let bare = build_query("what was I working on", &fixed_now());
        assert!(bare.window.is_none() && bare.subject.is_none());
    }

    // -- SUMMARIZE + RENDER (group by app, honest-empty) ---------------------

    fn timeline_for_summary() -> Vec<Activity> {
        // A morning of work, newest-first as snapshot() would yield.
        vec![
            Activity { app: "Mail".into(), title: "Inbox".into(), start_ts: at(11, 30), end_ts: at(11, 45) },
            Activity { app: "Safari".into(), title: "GitHub PR".into(), start_ts: at(10, 30), end_ts: at(11, 0) },
            Activity { app: "Xcode".into(), title: "aperture.rs".into(), start_ts: at(9, 0), end_ts: at(10, 0) },
            Activity { app: "Safari".into(), title: "docs".into(), start_ts: at(8, 30), end_ts: at(8, 45) },
        ]
    }

    #[test]
    fn summarize_groups_by_app_and_sorts_by_total_time() {
        let spans = summarize(&timeline_for_summary(), None);
        // Safari appears twice (30m + 15m = 45m); Xcode 60m; Mail 15m.
        assert_eq!(spans[0].app, "Xcode", "the longest total ranks first");
        assert_eq!(spans[0].secs, 3600);
        let safari = spans.iter().find(|s| s.app == "Safari").unwrap();
        assert_eq!(safari.secs, 45 * 60, "an app's spans are summed");
        // The representative title is the LONGEST span's ("GitHub PR", 30m > docs 15m).
        assert_eq!(safari.title, "GitHub PR");
    }

    #[test]
    fn summarize_clamps_durations_to_the_window() {
        // A window of 9:00-10:00 clamps Xcode's 9-10 span fully (60m) but excludes
        // the later apps entirely.
        let w = TimeWindow { start: at(9, 0), end: at(10, 0), label: "test".into() };
        let spans = summarize(&timeline_for_summary(), Some(&w));
        assert_eq!(spans.len(), 1, "only the in-window app is summarized");
        assert_eq!(spans[0].app, "Xcode");
        assert_eq!(spans[0].secs, 3600);
    }

    #[test]
    fn render_recall_names_apps_durations_and_titles() {
        let q = ApertureQuery { window: None, subject: None };
        let out = render_recall(&timeline_for_summary(), &q, 5);
        assert!(out.contains("Xcode"), "{out}");
        assert!(out.contains("1h"), "durations are bucketed: {out}");
        assert!(out.contains("aperture.rs"), "the representative title is named: {out}");
    }

    #[test]
    fn render_recall_windowed_header_names_the_period() {
        let q = build_query("what was I working on this morning", &fixed_now());
        let out = render_recall(&timeline_for_summary(), &q, 5);
        assert!(out.to_lowercase().contains("this morning"), "the window is named: {out}");
        assert!(out.contains("Xcode"));
    }

    #[test]
    fn render_recall_subject_filters_and_is_honest_on_no_match() {
        // Subject "github" matches only the Safari GitHub span.
        let q = ApertureQuery { window: None, subject: Some("github".into()) };
        let out = render_recall(&timeline_for_summary(), &q, 5);
        assert!(out.contains("Safari"), "{out}");
        assert!(!out.contains("Xcode"), "a non-matching app is excluded: {out}");
        // A subject with no match is honest, never fabricated.
        let miss = ApertureQuery { window: None, subject: Some("quarterly taxes".into()) };
        let out = render_recall(&timeline_for_summary(), &miss, 5);
        assert!(out.to_lowercase().starts_with("i have no activity recorded about"), "{out}");
    }

    #[test]
    fn render_recall_empty_timeline_is_honest_never_fabricated() {
        let q = build_query("what did I do this morning", &fixed_now());
        let out = render_recall(&[], &q, 5);
        assert!(out.to_lowercase().starts_with("i have no record of"), "{out}");
        // A bare empty recall.
        let bare = render_recall(&[], &ApertureQuery { window: None, subject: None }, 5);
        assert!(bare.to_lowercase().starts_with("i have no activity recorded yet"), "{bare}");
    }

    #[test]
    fn render_recall_window_with_no_activity_is_honest() {
        // Activity exists, but not in the requested (evening) window.
        let q = build_query("what was I working on this evening", &fixed_now());
        let out = render_recall(&timeline_for_summary(), &q, 5);
        assert!(out.to_lowercase().starts_with("i have no record of"), "{out}");
        assert!(out.contains("this evening"), "the empty line names the window: {out}");
    }

    // -- INTENT CLASSIFICATION (conservative; coexists with screen_context) --

    #[test]
    fn classifies_time_scoped_recalls() {
        for u in [
            "what did I do this morning",
            "what was I working on around 3pm",
            "what was I doing this afternoon",
            "what have I been doing in the last hour",
            "what did I do today",
        ] {
            assert!(
                matches!(
                    classify_aperture_intent(u, &fixed_now()),
                    Some(ApertureIntent::Recall(_))
                ),
                "{u:?} should be an aperture recall"
            );
        }
    }

    #[test]
    fn classifies_explicit_timeline_recalls_even_without_a_window() {
        for u in ["show my activity timeline", "recall my activity", "show me my activity log"] {
            assert!(
                matches!(
                    classify_aperture_intent(u, &fixed_now()),
                    Some(ApertureIntent::Recall(_))
                ),
                "{u:?} should be an aperture recall"
            );
        }
    }

    #[test]
    fn a_bare_working_on_question_is_not_aperture_it_falls_through_to_screen_context() {
        // No window, no timeline word -> None here, so router falls through to the
        // recent screen-context recall (a different feature). This is the crucial
        // coexistence guarantee.
        assert_eq!(classify_aperture_intent("what was I working on", &fixed_now()), None);
        assert_eq!(classify_aperture_intent("recall my screen context", &fixed_now()), None);
    }

    #[test]
    fn classifies_forget_intents() {
        for u in [
            "forget my activity timeline",
            "wipe my activity timeline",
            "clear my activity log",
            "delete my activity history",
        ] {
            assert_eq!(
                classify_aperture_intent(u, &fixed_now()),
                Some(ApertureIntent::Forget),
                "{u:?} should be a forget"
            );
        }
    }

    #[test]
    fn forget_takes_precedence_over_recall() {
        assert_eq!(
            classify_aperture_intent("forget my activity timeline", &fixed_now()),
            Some(ApertureIntent::Forget)
        );
    }

    #[test]
    fn ordinary_utterances_do_not_trigger() {
        for u in [
            "what's the weather",
            "clear the screen",
            "what am I working on", // present tense, live question
            "forget it",
            "",
        ] {
            assert_eq!(
                classify_aperture_intent(u, &fixed_now()),
                None,
                "{u:?} must NOT trigger an aperture intent"
            );
        }
    }

    // -- STATUS FRAME (secret-free: app + redacted-truncated title + duration) -

    #[test]
    fn status_frame_carries_counts_and_redacted_previews() {
        let _g = serial();
        install_settings(true, 50, 20);
        global_ingest("Mail", "Re: alice@example.com about the lease", 1000);
        global_ingest("Mail", "Re: alice@example.com about the lease", 1020); // merges
        let frame = status_frame();
        assert_eq!(frame["enabled"], json!(true));
        assert_eq!(frame["count"], json!(1), "merged into one activity");
        assert_eq!(frame["cap"], json!(50));
        assert_eq!(frame["poll_interval_secs"], json!(20));
        let recent = frame["recent"].as_array().expect("recent array");
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0]["app"], json!("Mail"));
        let title = recent[0]["title"].as_str().unwrap();
        assert!(title.contains("[redacted]"), "preview is post-redaction: {title}");
        assert!(!title.contains("alice@example.com"), "preview never leaks a raw email: {title}");
        assert_eq!(recent[0]["duration_secs"], json!(20), "the merged span's duration");
        reset_for_test();
    }

    // -- global recall over the live ring ------------------------------------

    #[test]
    fn global_render_recall_summarizes_the_live_ring() {
        let _g = serial();
        install_settings(true, 50, 600);
        global_ingest("Xcode", "aperture.rs", at(9, 0));
        global_ingest("Xcode", "aperture.rs", at(9, 30)); // merges -> 30m
        let out = global_render_recall_text("what was I working on this morning", &fixed_now(), 5);
        assert!(out.contains("Xcode"), "the live ring is summarized: {out}");
        assert!(out.to_lowercase().contains("this morning"));
        reset_for_test();
    }

    #[test]
    fn global_clear_wipes_and_is_honest_when_empty() {
        let _g = serial();
        install_settings(true, 50, 20);
        assert!(!global_clear(), "an un-fed ring has nothing to forget");
        global_ingest("Safari", "GitHub", 1);
        assert!(global_clear(), "a fed ring is forgettable");
        assert_eq!(global_len(), 0);
        reset_for_test();
    }
}
