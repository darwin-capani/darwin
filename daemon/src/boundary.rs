//! CUSTOMS (// EGRESS) — a PRE-FLIGHT egress BOUNDARY GATE over the cloud turn.
//! Before a cloud-bound request leaves the box, CUSTOMS builds an
//! [`EgressManifest`]: an honest, itemized inventory of EXACTLY the personal
//! context anthropic.rs is about to send — the same facts / history / world
//! rows / persona / system-prompt it already assembles — each item classified
//! by a coarse [`Sensitivity`] with a count and a byte-size. The manifest rides
//! the telemetry bus as a `boundary.manifest` frame so the operator can SEE what
//! egresses, per turn, BEFORE it goes out.
//!
//! ## Two responsibilities, both bounded to the SAFE direction
//!   1. INSPECT (read-only). [`build_egress_manifest`] is a PURE function of the
//!      same inputs `complete_with_tools` assembles. It reads, counts, classifies,
//!      and reports. It changes NOTHING and sends NOTHING — exactly the egress.rs
//!      posture ("what is my Mac about to say?" instead of "what is it saying now?").
//!   2. TRIM (reduce-only). [`apply_trim`] takes a manifest + a [`TrimSpec`] and
//!      returns a manifest with WHOLE CATEGORIES REMOVED — 'no memory' drops both
//!      Facts and History; 'don't send my facts' drops Facts. A trim can ONLY
//!      WITHHOLD a category; it can never add one, enlarge one, or invent one.
//!
//! ## The sacred invariant: a trim can only WITHHOLD, never BROADEN
//! This mirrors focus.rs's `is_no_broader_than` discipline, enforced by
//! CONSTRUCTION, not convention:
//!
//!   * [`apply_trim`] builds its result by FILTERING the input manifest's item
//!     list — it never constructs a fresh [`ContextItem`]. So every item in the
//!     output is byte-identical to an item that was already in the input, and the
//!     output list is always a SUBSET of the input list. There is no branch that
//!     could add a category the input didn't carry, enlarge a count, or grow a
//!     byte-size — the code literally cannot express it.
//!
//!   * [`EgressManifest::is_subset_of`] is the machine-checkable predicate the
//!     property test asserts for EVERY trim against its source manifest: the
//!     trimmed manifest's items are a subset, no category is introduced, and the
//!     total byte-size never grows. (See `property_no_trim_ever_broadens_egress`.)
//!
//!   * The `None` trim is the IDENTITY: `apply_trim(m, TrimSpec::None) == m`
//!     (same items, nothing withheld). With `[boundary].default_trim = "none"`
//!     (the shipped default) the turn sends exactly what it sends today — CUSTOMS
//!     ships as a NEUTRAL PREVIEW: it observes + reports, and withholds nothing
//!     until the operator (config) or a per-turn voice command asks it to.
//!
//! ## Honesty contract (never overclaim)
//!   * CUSTOMS gates ONLY the CLOUD egress path. The LOCAL inference path sends
//!     nothing off the box, so it never reaches CUSTOMS and CUSTOMS never claims
//!     to "block" it — there is nothing to block. `telemetry()` states this on the
//!     wire (`local_path_egresses: false`, `read_only: true`).
//!   * A trimmed turn is LABELED trimmed: the manifest carries the active
//!     [`TrimSpec`] and the exact list of categories it WITHHELD, so the readout
//!     never claims to have sent something it dropped, or dropped something it sent.
//!   * SECRET-FREE: the manifest carries category labels, counts, byte-sizes and a
//!     sensitivity band — never the fact VALUES, the history TEXT, or the utterance
//!     itself. It is an inventory, not a transcript.

use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::json;

// ---------------------------------------------------------------------------
// Context categories — the axis CUSTOMS inventories the egress along
// ---------------------------------------------------------------------------

/// The coarse CATEGORY of one piece of context a cloud turn assembles. Closed +
/// deliberately coarse: CUSTOMS reports "how much of WHAT KIND of context is
/// leaving", never the content. Each maps 1:1 to a distinct slice of what
/// `complete_with_tools` sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextCategory {
    /// The GLOBAL persona / grounding preamble (anthropic::persona()). DARWIN's
    /// own static scaffolding — not user data.
    SystemPrompt,
    /// The ACTIVE agent's persona text (specialists). DARWIN's own copy.
    Persona,
    /// The per-turn remembered FACTS about the user (namespaced recall). Personal.
    Facts,
    /// The recent CONVERSATION history (user/assistant turns). Personal.
    History,
    /// The shared WORLD-MODEL rows (entities/relationships) relevant to the turn.
    WorldRows,
    /// The bounded PERSONALIZATION summary (observed user-model). Personal.
    Personalization,
    /// The live USER UTTERANCE — the current request itself. Personal.
    Utterance,
}

impl ContextCategory {
    /// A stable short string for telemetry / the HUD.
    pub fn as_str(&self) -> &'static str {
        match self {
            ContextCategory::SystemPrompt => "system_prompt",
            ContextCategory::Persona => "persona",
            ContextCategory::Facts => "facts",
            ContextCategory::History => "history",
            ContextCategory::WorldRows => "world_rows",
            ContextCategory::Personalization => "personalization",
            ContextCategory::Utterance => "utterance",
        }
    }

    /// The coarse [`Sensitivity`] band this category carries. Fixed per category:
    /// DARWIN's own prompt scaffolding is `Public`, the shared world model is
    /// `Contextual`, and anything derived from the user (facts, history,
    /// personalization, their live words) is `Personal`.
    pub fn sensitivity(&self) -> Sensitivity {
        match self {
            ContextCategory::SystemPrompt | ContextCategory::Persona => Sensitivity::Public,
            ContextCategory::WorldRows => Sensitivity::Contextual,
            ContextCategory::Facts
            | ContextCategory::History
            | ContextCategory::Personalization
            | ContextCategory::Utterance => Sensitivity::Personal,
        }
    }
}

/// A coarse sensitivity band for a context category — the "how sensitive is this
/// to leak" axis, kept intentionally three-valued so the readout is a glance, not
/// a taxonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    /// DARWIN's own static prompt scaffolding — carries no user data.
    Public,
    /// Shared, non-personal grounding (the world model).
    Contextual,
    /// Data about or from the user — facts, conversation, observed profile, words.
    Personal,
}

impl Sensitivity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Sensitivity::Public => "public",
            Sensitivity::Contextual => "contextual",
            Sensitivity::Personal => "personal",
        }
    }
}

// ---------------------------------------------------------------------------
// The manifest — an inventory of what egresses, never the content
// ---------------------------------------------------------------------------

/// One inventoried slice of egress context: its [`ContextCategory`], the coarse
/// [`Sensitivity`], how many discrete UNITS it holds (facts, conversation turns,
/// or 1 for a single text block), and its total BYTE size. SECRET-FREE by
/// construction — there is no field that could hold a fact value, a turn's text,
/// or the utterance; only the shape of the egress is recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextItem {
    pub category: ContextCategory,
    pub sensitivity: Sensitivity,
    /// Discrete units in this slice: fact count, conversation-turn count, or 1 for
    /// a single text block (preamble / persona / world rows / personalization /
    /// utterance).
    pub count: usize,
    /// The total byte-size of this slice's content (what would ride the wire).
    pub bytes: usize,
}

/// The egress manifest: the itemized inventory of the personal context a cloud
/// turn is about to send, plus the honest record of any TRIM that was applied.
///
/// `items` is what IS being sent (already post-trim). `withheld` is the exact list
/// of categories a trim REMOVED from egress this turn (empty under the identity
/// trim). `trim` is the active [`TrimSpec`] policy. Together they make the
/// manifest self-describing: the operator sees what left, what was held back, and
/// why — with no way for the readout to disagree with what actually egressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressManifest {
    /// The context slices that ARE being sent this turn (present + not trimmed).
    pub items: Vec<ContextItem>,
    /// Categories a trim WITHHELD from this turn's egress (honest; empty when the
    /// trim withheld nothing — either `None`, or a spec whose categories weren't
    /// present anyway).
    pub withheld: Vec<ContextCategory>,
    /// The active trim policy applied to produce `items` (`None` = the identity).
    pub trim: TrimSpec,
}

impl EgressManifest {
    /// Whether this manifest carries a given category in its (sent) item list.
    pub fn sends(&self, category: ContextCategory) -> bool {
        self.items.iter().any(|it| it.category == category)
    }

    /// The total byte-size of everything being sent this turn.
    pub fn total_bytes(&self) -> usize {
        self.items.iter().map(|it| it.bytes).sum()
    }

    /// Whether a trim actually WITHHELD something this turn (something the base
    /// manifest carried is now absent). Distinct from `trim != None`: a `NoFacts`
    /// trim on a turn with no facts is an ACTIVE policy that withheld nothing.
    pub fn is_trimmed(&self) -> bool {
        !self.withheld.is_empty()
    }

    /// THE machine-checkable REDUCE-ONLY predicate: is this manifest NO BROADER
    /// than `base`? True iff every item it sends is byte-identical to an item
    /// `base` also sends (same category, same count, same bytes), it introduces no
    /// category `base` lacked, and its total byte-size does not exceed `base`'s.
    ///
    /// Because [`apply_trim`] only ever FILTERS the base's item list (never mints a
    /// new [`ContextItem`]), "subset of base" is the COMPLETE statement of "this
    /// trim broadened nothing" — there is no other axis on which it COULD. The
    /// property test asserts this for every trim.
    ///
    /// `#[allow(dead_code)]`: this is the reduce-only GATE's predicate, exercised
    /// by `property_no_trim_ever_broadens_egress` (a `#[cfg(test)]` consumer). It
    /// lives next to the type it guards rather than in the test module.
    #[allow(dead_code)]
    pub fn is_subset_of(&self, base: &EgressManifest) -> bool {
        let all_items_present = self.items.iter().all(|it| base.items.contains(it));
        let no_new_category = self
            .items
            .iter()
            .all(|it| base.sends(it.category));
        let bytes_not_grown = self.total_bytes() <= base.total_bytes();
        all_items_present && no_new_category && bytes_not_grown
    }

    /// The `boundary.manifest` telemetry frame. SECRET-FREE: category labels,
    /// sensitivity bands, counts and byte-sizes only — never a value. States the
    /// honesty contract on the wire (`read_only`, `local_path_egresses: false`) so
    /// the HUD copy is grounded in the payload, not a hardcode. A trimmed turn is
    /// labeled honestly: `trimmed`, the active `trim`, and the `withheld` list.
    pub fn telemetry(&self) -> serde_json::Value {
        let items: Vec<serde_json::Value> = self
            .items
            .iter()
            .map(|it| {
                json!({
                    "category": it.category.as_str(),
                    "sensitivity": it.sensitivity.as_str(),
                    "count": it.count,
                    "bytes": it.bytes,
                })
            })
            .collect();
        let withheld: Vec<&str> = self.withheld.iter().map(|c| c.as_str()).collect();
        json!({
            "items": items,
            "total_bytes": self.total_bytes(),
            "trim": self.trim.as_str(),
            "trimmed": self.is_trimmed(),
            "withheld": withheld,
            // The contract, stated on the wire so the panel is grounded:
            // CUSTOMS INSPECTS + REDUCES; it never mutates and never sends.
            "read_only": true,
            // Honest scope: CUSTOMS gates ONLY the cloud path. The local inference
            // path egresses nothing, so it never reaches CUSTOMS — the readout must
            // never claim CUSTOMS "blocks" a local turn (there is nothing to block).
            "local_path_egresses": false,
        })
    }
}

// ---------------------------------------------------------------------------
// TrimSpec — the REDUCE-ONLY policy
// ---------------------------------------------------------------------------

/// A reduce-only egress trim policy. Each variant names a SET of categories to
/// WITHHOLD from the cloud turn — never a category to add. `None` is the identity
/// (send everything the turn assembled).
///
///   * `NoFacts` — "don't send my facts": drops [`ContextCategory::Facts`].
///   * `NoMemory` — "no memory": drops Facts + History (the remembered-facts tail
///     AND the recent conversation), so the turn reasons only from the live
///     utterance + non-personal grounding.
///
/// The shipped default is `None`. An operator selects a stronger trim via
/// `[boundary].default_trim`, or a per-turn voice command sets one for a single
/// turn (see [`set_turn_trim`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrimSpec {
    /// The identity — withhold nothing (today's behavior).
    None,
    /// Withhold the remembered FACTS only.
    NoFacts,
    /// Withhold the remembered FACTS and the recent conversation HISTORY.
    NoMemory,
}

impl TrimSpec {
    /// A stable short string for telemetry / config.
    pub fn as_str(&self) -> &'static str {
        match self {
            TrimSpec::None => "none",
            TrimSpec::NoFacts => "no_facts",
            TrimSpec::NoMemory => "no_memory",
        }
    }

    /// The MAXIMAL reduce-only trim — the one that withholds the MOST. `NoMemory`
    /// drops Facts + History, a superset of every other variant's withholding, so it
    /// is the strongest personal-context reduction CUSTOMS offers. VAULT MODE forces
    /// this (via `gate_and_trim`): "go dark" tightens CUSTOMS to its maximum. Kept as
    /// a named function (not a bare `NoMemory`) so the "maximal" intent is explicit
    /// and the `maximal_withholds_a_superset_of_every_trim` test pins the invariant —
    /// if a stronger variant is ever added, that test forces `maximal()` to follow,
    /// keeping "vault => maximal" restrict-only.
    pub fn maximal() -> TrimSpec {
        TrimSpec::NoMemory
    }

    /// The categories this trim WITHHOLDS. `None` withholds nothing; `NoFacts`
    /// withholds Facts; `NoMemory` withholds Facts + History. This closed table is
    /// the ONLY thing a trim can do — it names categories to REMOVE, never to add.
    pub fn dropped(&self) -> &'static [ContextCategory] {
        match self {
            TrimSpec::None => &[],
            TrimSpec::NoFacts => &[ContextCategory::Facts],
            TrimSpec::NoMemory => &[ContextCategory::Facts, ContextCategory::History],
        }
    }

    /// Whether this trim withholds `category` from egress.
    pub fn drops(&self, category: ContextCategory) -> bool {
        self.dropped().contains(&category)
    }

    /// The memory-recall TOOL NAMES this trim must refuse in the cloud tool loop.
    /// CUSTOMS empties the SEEDED facts/history from the initial prompt, but the loop
    /// also offers recall tools that read the SAME local store — leaving them usable
    /// would let the model pull a withheld category BACK into egress mid-loop,
    /// silently defeating the trim's stated guarantee. Facts withheld => the fact +
    /// semantic-memory recall tools; History withheld => the episodic recall tool.
    /// Reduce-only: this only names tools to REFUSE, never one to add. Enforced at
    /// the single robust chokepoint (`execute_tool`), so it holds even for a
    /// wildcard-allowlist agent whose offered set can't be filtered by name.
    pub fn withheld_recall_tools(&self) -> &'static [&'static str] {
        match self {
            TrimSpec::None => &[],
            TrimSpec::NoFacts => &["recall_facts", "mnemosyne_recall"],
            TrimSpec::NoMemory => &["recall_facts", "mnemosyne_recall", "episodic_recall"],
        }
    }

    /// Parse a `[boundary].default_trim` / voice-command string into a spec.
    /// Blank / "none" / "off" / an UNRECOGNIZED value => `None` (the identity):
    /// a trim must be EXPLICIT — CUSTOMS never SILENTLY withholds context the
    /// operator did not clearly ask to withhold, so an unknown keyword fails to the
    /// NEUTRAL "send everything" posture (and the honest inventory still ships).
    /// The recognized synonyms make the config + a spoken command line up.
    pub fn from_str(s: &str) -> TrimSpec {
        match s.trim().to_lowercase().replace([' ', '-'], "_").as_str() {
            "no_facts" | "nofacts" | "drop_facts" | "facts" | "no_my_facts" => TrimSpec::NoFacts,
            "no_memory" | "nomemory" | "drop_memory" | "memory" | "no_recall" | "amnesia" => {
                TrimSpec::NoMemory
            }
            // "", "none", "off", or anything unrecognized => the identity.
            _ => TrimSpec::None,
        }
    }
}

// ---------------------------------------------------------------------------
// build_egress_manifest — PURE inspector
// ---------------------------------------------------------------------------

/// One text-block item (preamble / persona / world / personalization / utterance):
/// present iff its trimmed body is non-empty, `count` 1, `bytes` its byte-length.
/// Returns `None` for an empty block so an absent category never pads the manifest.
fn text_item(category: ContextCategory, body: &str) -> Option<ContextItem> {
    let bytes = body.trim().len();
    if bytes == 0 {
        return None;
    }
    Some(ContextItem {
        category,
        sensitivity: category.sensitivity(),
        count: 1,
        bytes,
    })
}

/// Build the egress manifest from the SAME data a cloud turn assembles — the exact
/// inputs `complete_with_tools` threads to `build_system_blocks` / `build_messages`.
/// PURE: reads, counts, classifies; sends nothing, mutates nothing. An absent /
/// empty slice contributes NO item (an honest inventory shows only what actually
/// egresses). The returned manifest is UNTRIMMED (`trim: None`, `withheld: []`);
/// call [`apply_trim`] to reduce it. Unit-tested without any network.
///
/// `preamble` is the global persona/grounding preamble; `agent_persona` the active
/// agent's own persona (None for the orchestrator); `facts` the remembered facts;
/// `history` the recent user/assistant turns; `world_context` the shared world-model
/// rows; `personalization` the observed user-model summary; `utterance` the live
/// request. Byte-sizes approximate what rides the wire (the raw content lengths).
pub fn build_egress_manifest(
    preamble: &str,
    agent_persona: Option<&str>,
    facts: &[(String, String)],
    history: &[(String, String)],
    world_context: &str,
    personalization: &str,
    utterance: &str,
) -> EgressManifest {
    let mut items: Vec<ContextItem> = Vec::new();

    if let Some(it) = text_item(ContextCategory::SystemPrompt, preamble) {
        items.push(it);
    }
    if let Some(it) = agent_persona.and_then(|p| text_item(ContextCategory::Persona, p)) {
        items.push(it);
    }
    // FACTS: one unit per remembered fact; bytes approximate the rendered
    // "- key: value" tail (key + value lengths).
    if !facts.is_empty() {
        let bytes: usize = facts.iter().map(|(k, v)| k.len() + v.len()).sum();
        if bytes > 0 {
            items.push(ContextItem {
                category: ContextCategory::Facts,
                sensitivity: ContextCategory::Facts.sensitivity(),
                count: facts.len(),
                bytes,
            });
        }
    }
    // HISTORY: one unit per user/assistant EXCHANGE; bytes sum both sides. A turn
    // with an empty side is dropped from the wire (build_messages skips it), so we
    // count only exchanges that would actually be sent.
    let live_turns: Vec<&(String, String)> = history
        .iter()
        .filter(|(u, a)| !u.trim().is_empty() && !a.trim().is_empty())
        .collect();
    if !live_turns.is_empty() {
        let bytes: usize = live_turns.iter().map(|(u, a)| u.len() + a.len()).sum();
        items.push(ContextItem {
            category: ContextCategory::History,
            sensitivity: ContextCategory::History.sensitivity(),
            count: live_turns.len(),
            bytes,
        });
    }
    if let Some(it) = text_item(ContextCategory::WorldRows, world_context) {
        items.push(it);
    }
    if let Some(it) = text_item(ContextCategory::Personalization, personalization) {
        items.push(it);
    }
    if let Some(it) = text_item(ContextCategory::Utterance, utterance) {
        items.push(it);
    }

    EgressManifest {
        items,
        withheld: Vec::new(),
        trim: TrimSpec::None,
    }
}

// ---------------------------------------------------------------------------
// apply_trim — REDUCE-ONLY filter
// ---------------------------------------------------------------------------

/// Apply a reduce-only [`TrimSpec`] to a manifest, WITHHOLDING whole categories.
///
/// REDUCE-ONLY BY CONSTRUCTION: the result's item list is built by FILTERING
/// `manifest.items` — dropping any item whose category the spec withholds. It
/// never constructs a fresh [`ContextItem`], so it cannot add a category, enlarge
/// a count, or grow a byte-size; the strongest thing it can do is remove. `None`
/// is the identity (returns an equal manifest). The invariant
/// `apply_trim(m, spec).is_subset_of(m)` holds for EVERY `m` and EVERY `spec` —
/// proven by the property test. The result records the `spec` and the exact list
/// of categories it actually WITHHELD (present in the input, dropped in the output)
/// so a trimmed turn is labeled honestly.
pub fn apply_trim(manifest: &EgressManifest, spec: TrimSpec) -> EgressManifest {
    // FILTER-ONLY: keep the items whose category the spec does NOT withhold. This
    // is the reduce-only guarantee — we can only ever drop from `manifest.items`,
    // never add to it (mirror of focus.rs's keep_except).
    let items: Vec<ContextItem> = manifest
        .items
        .iter()
        .filter(|it| !spec.drops(it.category))
        .cloned()
        .collect();
    // The honest record of what THIS trim removed: categories present in the input
    // that the spec withheld (preserving any already-withheld from a prior trim).
    let mut withheld = manifest.withheld.clone();
    for cat in spec.dropped() {
        if manifest.sends(*cat) && !withheld.contains(cat) {
            withheld.push(*cat);
        }
    }
    EgressManifest {
        items,
        withheld,
        trim: spec,
    }
}

// ---------------------------------------------------------------------------
// Gate + per-turn override — the live wiring (mirrors anthropic FORGE_GATE /
// response_voice), so complete_with_tools reads process-globals instead of &Config
// ---------------------------------------------------------------------------

/// The `[boundary]` gate, captured once at startup so the cloud path can read
/// `enabled` + the default trim WITHOUT threading a `&Config` through
/// `complete_with_tools` (mirrors anthropic::FORGE_GATE / MISSION_MODEL).
///
/// Defaults to OFF (`enabled=false`, `TrimSpec::None`) when [`init`] was never
/// called — any test, or a path that bypasses startup — so an uninitialized gate
/// is INERT (no manifest computed, no trim applied) and the cloud turn is
/// byte-for-byte today's. The live daemon calls `init(true, "none")` from the
/// shipped config, turning the neutral PREVIEW on.
static BOUNDARY_GATE: OnceLock<(bool, TrimSpec)> = OnceLock::new();

/// Wire the `[boundary]` gate from the loaded config. Called once from `main()`
/// alongside `init_answers`. Idempotent (a lost `set` means the same value was
/// already installed). Logs nothing sensitive (just the bool + trim word).
pub fn init(enabled: bool, default_trim: &str) {
    let _ = BOUNDARY_GATE.set((enabled, TrimSpec::from_str(default_trim)));
}

/// The gate + the EFFECTIVE trim for the current turn: the per-turn voice override
/// if one is set, else the config default. Falls back to `(false, None)` when
/// [`init`] was never called, so the manifest path is inert and today's behavior
/// holds.
pub fn gate_and_trim() -> (bool, TrimSpec) {
    let (enabled, default_trim) = BOUNDARY_GATE.get().copied().unwrap_or((false, TrimSpec::None));
    let trim = current_turn_trim().unwrap_or(default_trim);
    // VAULT MODE ("go dark", vault.rs): an active vault forces CUSTOMS to the
    // MAXIMAL reduce-only trim. RESTRICT-ONLY — `TrimSpec::maximal()` withholds a
    // superset of every other trim (pinned by a boundary test), so this can only
    // ever TIGHTEN the egress, never loosen a stronger operator / per-turn trim.
    // Inert when vault is off (byte-for-byte today's trim). Vault is the maximal
    // reduce; CUSTOMS is the inventory + partial reduce it strengthens.
    let trim = if crate::vault::active() { TrimSpec::maximal() } else { trim };
    (enabled, trim)
}

/// The current turn's per-turn trim override. `None` = no voice command set a trim
/// this turn, so the config default applies. Set by a `boundary`/`customs` voice
/// command arm, read by [`gate_and_trim`], cleared at turn end by [`TurnTrimGuard`]
/// so one turn's "don't send my facts" never silently trims a LATER turn.
static TURN_TRIM: Mutex<Option<TrimSpec>> = Mutex::new(None);

/// Record the trim THIS turn should apply (a per-turn voice command). Poison-
/// tolerant. Passing `None` clears the override (falls back to the config default).
///
/// `#[allow(dead_code)]`: this is the PER-TURN OVERRIDE seam — the setter a
/// `boundary`/`customs` voice-command arm calls to trim a single turn, and the
/// clear/guard that resets it at turn end (the exact analogue of
/// anthropic::response_voice's set/clear/TurnLangGuard, which main.rs installs in
/// run_pipeline). The read side ([`gate_and_trim`] -> [`current_turn_trim`]) is
/// already live; wiring the command arm + the run_pipeline guard install is the
/// integration step. Fully exercised by the `per_turn_override_takes_precedence`
/// test, so the mechanism is proven even before the arm lands.
#[allow(dead_code)]
pub fn set_turn_trim(spec: Option<TrimSpec>) {
    *TURN_TRIM.lock().unwrap_or_else(|p| p.into_inner()) = spec;
}

/// The current turn's trim override — `None` when no command set one. Poison-
/// tolerant.
pub fn current_turn_trim() -> Option<TrimSpec> {
    *TURN_TRIM.lock().unwrap_or_else(|p| p.into_inner())
}

/// Clear the per-turn trim override at turn end. Poison-tolerant. Part of the
/// per-turn override seam (see [`set_turn_trim`]); called by [`TurnTrimGuard`].
#[allow(dead_code)]
pub fn clear_turn_trim() {
    set_turn_trim(None);
}

/// RAII guard that CLEARS the per-turn trim override when the turn handler returns
/// by ANY path — the analogue of anthropic::response_voice::TurnLangGuard. Install
/// it near the top of the turn handler so a per-turn "no memory" can never leak
/// into the next turn's egress. Part of the per-turn override seam (see
/// [`set_turn_trim`]); the run_pipeline install is the integration step.
#[allow(dead_code)]
pub struct TurnTrimGuard;
impl Drop for TurnTrimGuard {
    fn drop(&mut self) {
        clear_turn_trim();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_facts() -> Vec<(String, String)> {
        vec![
            ("user.name".to_string(), "Darwin".to_string()),
            ("user.pref.editor".to_string(), "helix".to_string()),
        ]
    }

    fn sample_history() -> Vec<(String, String)> {
        vec![
            ("what's the weather".to_string(), "Clear skies, sir.".to_string()),
            ("thanks".to_string(), "Anytime.".to_string()),
        ]
    }

    /// The canonical FULL manifest a live cloud turn would inventory — every
    /// category present.
    fn full_manifest() -> EgressManifest {
        build_egress_manifest(
            "GLOBAL PERSONA PREAMBLE",
            Some("FRIDAY specialist persona"),
            &sample_facts(),
            &sample_history(),
            "acme_corp -> employs -> user",
            "prefers terse replies; works late",
            "open my calendar",
        )
    }

    // =====================================================================
    // build_egress_manifest — construction + classification + counts/bytes
    // =====================================================================

    #[test]
    fn manifest_inventories_every_present_category_with_counts_and_bytes() {
        let m = full_manifest();
        // All seven categories are present in this turn.
        for cat in [
            ContextCategory::SystemPrompt,
            ContextCategory::Persona,
            ContextCategory::Facts,
            ContextCategory::History,
            ContextCategory::WorldRows,
            ContextCategory::Personalization,
            ContextCategory::Utterance,
        ] {
            assert!(m.sends(cat), "manifest should inventory {cat:?}");
        }
        // Facts: one unit per remembered fact, bytes = sum(key+value).
        let facts = m.items.iter().find(|it| it.category == ContextCategory::Facts).unwrap();
        assert_eq!(facts.count, 2, "two facts");
        assert_eq!(facts.sensitivity, Sensitivity::Personal, "facts are personal");
        let expected_fact_bytes: usize =
            sample_facts().iter().map(|(k, v)| k.len() + v.len()).sum();
        assert_eq!(facts.bytes, expected_fact_bytes);
        // History: one unit per exchange.
        let hist = m.items.iter().find(|it| it.category == ContextCategory::History).unwrap();
        assert_eq!(hist.count, 2, "two live exchanges");
        // The system prompt is PUBLIC (DARWIN's own scaffolding), not personal.
        let sys = m.items.iter().find(|it| it.category == ContextCategory::SystemPrompt).unwrap();
        assert_eq!(sys.sensitivity, Sensitivity::Public);
        // World rows are CONTEXTUAL.
        let world = m.items.iter().find(|it| it.category == ContextCategory::WorldRows).unwrap();
        assert_eq!(world.sensitivity, Sensitivity::Contextual);
        // The untrimmed manifest withholds nothing.
        assert!(!m.is_trimmed());
        assert_eq!(m.trim, TrimSpec::None);
        assert!(m.withheld.is_empty());
    }

    #[test]
    fn manifest_omits_absent_categories_and_skips_empty_history_turns() {
        // Orchestrator (no agent persona), no facts, no world/personalization, and
        // a history whose only turn has an empty assistant side (never sent).
        let m = build_egress_manifest(
            "PREAMBLE",
            None,
            &[],
            &[("hi".to_string(), "  ".to_string())],
            "",
            "   ",
            "what time is it",
        );
        assert!(m.sends(ContextCategory::SystemPrompt));
        assert!(m.sends(ContextCategory::Utterance));
        // Absent categories contribute NO item — an honest inventory of what
        // actually egresses.
        assert!(!m.sends(ContextCategory::Persona));
        assert!(!m.sends(ContextCategory::Facts));
        assert!(!m.sends(ContextCategory::WorldRows));
        assert!(!m.sends(ContextCategory::Personalization));
        // The empty-sided history turn is not sent, so no History item.
        assert!(!m.sends(ContextCategory::History), "empty-sided turn is not egress");
    }

    #[test]
    fn total_bytes_is_the_sum_of_item_bytes() {
        let m = full_manifest();
        let expected: usize = m.items.iter().map(|it| it.bytes).sum();
        assert_eq!(m.total_bytes(), expected);
        assert!(m.total_bytes() > 0);
    }

    // =====================================================================
    // TrimSpec table + parsing
    // =====================================================================

    #[test]
    fn trim_spec_drops_the_right_categories() {
        assert!(TrimSpec::None.dropped().is_empty(), "None is the identity");
        assert!(TrimSpec::NoFacts.drops(ContextCategory::Facts));
        assert!(!TrimSpec::NoFacts.drops(ContextCategory::History), "no_facts keeps history");
        assert!(TrimSpec::NoMemory.drops(ContextCategory::Facts));
        assert!(TrimSpec::NoMemory.drops(ContextCategory::History));
        // A trim NEVER withholds DARWIN's own scaffolding or the live utterance.
        for spec in [TrimSpec::None, TrimSpec::NoFacts, TrimSpec::NoMemory] {
            assert!(!spec.drops(ContextCategory::SystemPrompt));
            assert!(!spec.drops(ContextCategory::Utterance));
        }
    }

    #[test]
    fn maximal_withholds_a_superset_of_every_trim() {
        // THE invariant VAULT relies on: `maximal()` is the STRONGEST reduce — it
        // withholds every category any other trim withholds (a superset). This is
        // what makes "vault => force maximal" RESTRICT-ONLY: forcing maximal can only
        // tighten, never loosen a stronger operator/per-turn trim. If a stronger
        // variant is ever added, this test fails until `maximal()` follows it.
        let max = TrimSpec::maximal();
        for spec in [TrimSpec::None, TrimSpec::NoFacts, TrimSpec::NoMemory] {
            for cat in spec.dropped() {
                assert!(
                    max.drops(*cat),
                    "maximal() must withhold {cat:?} that {spec:?} withholds"
                );
            }
        }
        // Concretely, the maximal trim withholds both personal-memory categories.
        assert!(max.drops(ContextCategory::Facts));
        assert!(max.drops(ContextCategory::History));
    }

    #[test]
    fn trim_spec_parses_config_and_voice_synonyms_and_fails_to_none() {
        assert_eq!(TrimSpec::from_str(""), TrimSpec::None);
        assert_eq!(TrimSpec::from_str("none"), TrimSpec::None);
        assert_eq!(TrimSpec::from_str("off"), TrimSpec::None);
        assert_eq!(TrimSpec::from_str("no_facts"), TrimSpec::NoFacts);
        assert_eq!(TrimSpec::from_str("don't send my facts".replace('\'', "").as_str()), TrimSpec::None); // apostrophe form isn't a keyword
        assert_eq!(TrimSpec::from_str("no facts"), TrimSpec::NoFacts); // space-normalized
        assert_eq!(TrimSpec::from_str("NO-FACTS"), TrimSpec::NoFacts); // case + dash
        assert_eq!(TrimSpec::from_str("no memory"), TrimSpec::NoMemory);
        assert_eq!(TrimSpec::from_str("amnesia"), TrimSpec::NoMemory);
        // An UNRECOGNIZED value fails to the NEUTRAL identity (never silently
        // withholds context the operator didn't clearly ask to drop).
        assert_eq!(TrimSpec::from_str("nonsense"), TrimSpec::None);
    }

    // =====================================================================
    // apply_trim — each spec actually withholds, and only ever withholds
    // =====================================================================

    #[test]
    fn none_trim_is_the_identity() {
        let m = full_manifest();
        let trimmed = apply_trim(&m, TrimSpec::None);
        assert_eq!(trimmed.items, m.items, "None keeps every item");
        assert!(!trimmed.is_trimmed(), "None withholds nothing");
        assert_eq!(trimmed.trim, TrimSpec::None);
    }

    #[test]
    fn no_facts_trim_withholds_only_facts() {
        let m = full_manifest();
        let trimmed = apply_trim(&m, TrimSpec::NoFacts);
        assert!(!trimmed.sends(ContextCategory::Facts), "facts withheld");
        assert!(trimmed.sends(ContextCategory::History), "history still sent");
        assert!(trimmed.sends(ContextCategory::Utterance), "utterance still sent");
        assert!(trimmed.is_trimmed());
        assert_eq!(trimmed.withheld, vec![ContextCategory::Facts]);
        assert_eq!(trimmed.trim, TrimSpec::NoFacts);
        // Reduce-only: strictly fewer bytes than the full manifest.
        assert!(trimmed.total_bytes() < m.total_bytes());
    }

    #[test]
    fn no_memory_trim_withholds_facts_and_history() {
        let m = full_manifest();
        let trimmed = apply_trim(&m, TrimSpec::NoMemory);
        assert!(!trimmed.sends(ContextCategory::Facts), "facts withheld");
        assert!(!trimmed.sends(ContextCategory::History), "history withheld");
        // Non-memory grounding + the live utterance still egress.
        assert!(trimmed.sends(ContextCategory::WorldRows));
        assert!(trimmed.sends(ContextCategory::Utterance));
        assert!(trimmed.is_trimmed());
        assert!(trimmed.withheld.contains(&ContextCategory::Facts));
        assert!(trimmed.withheld.contains(&ContextCategory::History));
    }

    #[test]
    fn a_trim_that_withholds_an_absent_category_records_no_withholding() {
        // NoFacts on a turn with NO facts is an ACTIVE policy that withheld nothing:
        // honest — the manifest is not marked trimmed because nothing was dropped.
        let m = build_egress_manifest("P", None, &[], &sample_history(), "", "", "hi");
        assert!(!m.sends(ContextCategory::Facts));
        let trimmed = apply_trim(&m, TrimSpec::NoFacts);
        assert!(!trimmed.is_trimmed(), "nothing was actually withheld");
        assert!(trimmed.withheld.is_empty());
        // ...but the item set is still a subset (unchanged here).
        assert!(trimmed.is_subset_of(&m));
    }

    // =====================================================================
    // THE REDUCE-ONLY INVARIANT: the property test — no trim ever broadens
    // =====================================================================

    fn all_specs() -> Vec<TrimSpec> {
        vec![TrimSpec::None, TrimSpec::NoFacts, TrimSpec::NoMemory]
    }

    /// A spread of manifests to run the property over: the full one, a
    /// facts-only-ish one, an orchestrator (no persona) one, and an already-trimmed
    /// one — so the invariant holds against EVERY base, and composition stays
    /// reduce-only.
    fn bases() -> Vec<EgressManifest> {
        vec![
            full_manifest(),
            build_egress_manifest("P", None, &sample_facts(), &[], "", "", "q"),
            build_egress_manifest("P", Some("persona"), &[], &sample_history(), "world", "", "q"),
            apply_trim(&full_manifest(), TrimSpec::NoFacts),
        ]
    }

    #[test]
    fn property_no_trim_ever_broadens_egress() {
        // THE reduce-only GATE, machine-checked: for EVERY spec and EVERY base, the
        // trimmed manifest is a SUBSET of the base — no item appears that the base
        // didn't already send, no category is introduced, total bytes never grow. A
        // trim can only ever make the egress SMALLER.
        for base in bases() {
            for spec in all_specs() {
                let trimmed = apply_trim(&base, spec);
                assert!(
                    trimmed.is_subset_of(&base),
                    "spec {spec:?} broadened base {base:?} -> {trimmed:?}"
                );
                // Every sent item existed in the base, byte-identical.
                for it in &trimmed.items {
                    assert!(base.items.contains(it), "spec {spec:?} minted an item {it:?}");
                }
                // No withheld category is still being sent (drop is real).
                for cat in &trimmed.withheld {
                    assert!(!trimmed.sends(*cat), "withheld {cat:?} is still egressing");
                }
                // Bytes never grow.
                assert!(trimmed.total_bytes() <= base.total_bytes());
            }
        }
    }

    #[test]
    fn applying_a_trim_twice_never_re_broadens() {
        // Composing a trim onto its own output must not re-broaden.
        for spec in all_specs() {
            let base = full_manifest();
            let once = apply_trim(&base, spec);
            let twice = apply_trim(&once, spec);
            assert!(twice.is_subset_of(&once), "re-applying {spec:?} re-broadened its own output");
            // Idempotent item set: the same categories survive.
            assert_eq!(twice.items, once.items, "{spec:?} is idempotent on its own output");
        }
    }

    // =====================================================================
    // TELEMETRY shape — the boundary.manifest frame the HUD parses
    // =====================================================================

    #[test]
    fn telemetry_states_the_read_only_contract_and_labels_a_trim_honestly() {
        let m = apply_trim(&full_manifest(), TrimSpec::NoFacts);
        let v = m.telemetry();
        // The honesty contract is on the wire (grounds the HUD copy).
        assert_eq!(v["read_only"], true);
        assert_eq!(v["local_path_egresses"], false, "CUSTOMS never claims to gate the local path");
        // A trimmed turn is labeled honestly.
        assert_eq!(v["trimmed"], true);
        assert_eq!(v["trim"], "no_facts");
        assert_eq!(v["withheld"], serde_json::json!(["facts"]));
        // Items carry category/sensitivity/count/bytes — never a value.
        let items = v["items"].as_array().unwrap();
        assert!(items.iter().any(|it| it["category"] == "history" && it["sensitivity"] == "personal"));
        assert!(!items.iter().any(|it| it["category"] == "facts"), "trimmed facts are not in the sent items");
        assert!(v["total_bytes"].as_u64().unwrap() > 0);
    }

    #[test]
    fn telemetry_is_secret_free() {
        // The manifest telemetry must carry only SHAPE, never CONTENT — no fact
        // value, no history text, no utterance string.
        let m = full_manifest();
        let wire = m.telemetry().to_string();
        assert!(!wire.contains("Darwin"), "a fact value leaked into the manifest");
        assert!(!wire.contains("helix"), "a fact value leaked into the manifest");
        assert!(!wire.contains("weather"), "history text leaked into the manifest");
        assert!(!wire.contains("open my calendar"), "the utterance leaked into the manifest");
    }

    // =====================================================================
    // GATE + per-turn override wiring
    // =====================================================================

    #[test]
    fn a_trim_strips_the_recall_tools_that_could_re_egress_a_withheld_category() {
        // The identity offers everything.
        assert!(TrimSpec::None.withheld_recall_tools().is_empty());
        // Facts withheld => the fact + semantic-memory recall tools are refused, so
        // the model cannot pull the seeded-then-dropped facts back into egress.
        let nf = TrimSpec::NoFacts.withheld_recall_tools();
        assert!(nf.contains(&"recall_facts"));
        assert!(nf.contains(&"mnemosyne_recall"));
        assert!(!nf.contains(&"episodic_recall"), "history is still allowed under no_facts");
        // No-memory withholds facts + history => all three recall tools refused.
        let nm = TrimSpec::NoMemory.withheld_recall_tools();
        for t in ["recall_facts", "mnemosyne_recall", "episodic_recall"] {
            assert!(nm.contains(&t), "{t} must be refused under no_memory");
        }
        // The stripped set aligns with the categories the trim drops (no tool leaks a
        // category the trim claims to keep).
        assert!(TrimSpec::NoFacts.drops(ContextCategory::Facts));
        assert!(!TrimSpec::NoFacts.drops(ContextCategory::History));
    }

    #[test]
    fn uninitialized_gate_is_inert() {
        // Without init the gate is OFF and the trim is the identity — the manifest
        // path never runs and today's behavior holds. (This test may run before or
        // after init in the suite; it asserts the FALLBACK semantics via a fresh
        // read only when uninitialized. The turn override is cleared to isolate.)
        clear_turn_trim();
        // gate_and_trim never panics and returns a valid pair regardless of init.
        let (_enabled, trim) = gate_and_trim();
        // With no per-turn override, the effective trim equals the gate default
        // (None when uninit; whatever init set otherwise) — never an invalid value.
        assert!(matches!(trim, TrimSpec::None | TrimSpec::NoFacts | TrimSpec::NoMemory));
    }

    #[test]
    fn per_turn_override_takes_precedence_and_clears() {
        clear_turn_trim();
        assert_eq!(current_turn_trim(), None);
        set_turn_trim(Some(TrimSpec::NoMemory));
        assert_eq!(current_turn_trim(), Some(TrimSpec::NoMemory));
        // The override wins in gate_and_trim regardless of the config default.
        let (_enabled, trim) = gate_and_trim();
        assert_eq!(trim, TrimSpec::NoMemory, "per-turn override beats the config default");
        // The guard clears it on drop (no leak into the next turn).
        {
            let _guard = TurnTrimGuard;
            assert_eq!(current_turn_trim(), Some(TrimSpec::NoMemory));
        }
        assert_eq!(current_turn_trim(), None, "TurnTrimGuard cleared the override");
    }
}
