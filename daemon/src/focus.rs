//! FOCUS PROFILES (#24) — a PERMISSION-NEUTRAL lens over DARWIN's proactive
//! surfaces. A focus profile answers ONE question: of the things DARWIN could
//! proactively show or say, which should it stay quiet about right now? It can
//! make DARWIN do LESS — surface fewer signal categories, render a terser brief,
//! hold back suggestions — and it can NEVER make DARWIN do MORE.
//!
//! ## The sacred invariant: a profile can only QUIET, never LOOSEN
//! This module is the #24 gate's enforcement, and the enforcement is at the
//! TYPE LEVEL, not by convention:
//!
//!   * [`TunedBehavior`] — what [`apply_profile`] returns — carries ONLY
//!     non-consequential knobs: a SET of signal categories that may surface, a
//!     brief verbosity, and a "suggestions quieted" bool. There is NO field for
//!     a permission, a gate, a confirm, a voice-id, a lockdown, an autonomy
//!     level, or a consequential action. The type literally cannot express
//!     "enable a side effect" or "loosen a gate" — so `apply_profile` cannot
//!     return one. (See the `tuned_behavior_has_no_permission_field` doc-level
//!     reasoning + the property tests.)
//!
//!   * Every knob a profile touches is RESTRICT-ONLY relative to the base:
//!       - the surfacing set is always a SUBSET of the base's set (a profile may
//!         REMOVE a category from surfacing, never ADD one the base suppressed);
//!       - verbosity may only step DOWN or hold (Full -> Brief -> Silent), never up;
//!       - `suggestions_quieted` may only flip false -> true (quiet more), never
//!         true -> false (un-quiet).
//!         [`TunedBehavior::is_no_broader_than`] is the machine-checkable predicate
//!         the property test asserts for EVERY profile against its base.
//!
//!   * The DEFAULT profile is the IDENTITY: `apply_profile(Default, base) == base`
//!     for every base. With `[focus].profile = "default"` (the shipped default)
//!     today's behavior is reproduced byte-for-byte — the feature ships NEUTRAL.
//!
//! ## What a profile does NOT touch (by construction, not by promise)
//! `apply_profile` takes a [`BaseBehavior`] and returns a [`TunedBehavior`].
//! Neither type references `integrations::gate`, `consequential_allowed`, the
//! confirm path, the master switch, voice-id, lockdown, or policy. The brief
//! still makes NO outward call; accepting a suggestion still routes through the
//! EXISTING gated path. A profile narrows WHICH non-consequential intel reaches
//! the user — full stop. It never enables an action, never raises autonomy,
//! never confirms anything.
//!
//! ## Wiring (live, not dead)
//! The active profile is read from `[focus].profile` (config.rs). The live
//! anticipation tick applies it to the base behavior and uses the tuned result
//! to (a) drop a surfaced brief whose category the profile silences and (b)
//! quiet the proactive-suggestion feed. The on-demand `edith_brief` path applies
//! it too. With the default profile every gate is identity, so the live paths are
//! byte-for-byte today's behavior until the operator names a quieter profile.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Signal categories — the NON-CONSEQUENTIAL axis a profile filters on
// ---------------------------------------------------------------------------

/// The coarse CATEGORY of a proactive signal, for focus filtering. This is the
/// ONLY axis a profile reasons over: which KINDS of intel are allowed to surface
/// under the active focus. Deliberately coarse + closed — a profile decides
/// "show me critical things only" or "no news right now", never anything about
/// permissions or actions.
///
/// `Critical` is the never-silenced floor: a profile may quiet everything else
/// (news, routine, calendar, mail), but DeepFocus/Sleep still let a genuinely
/// critical signal through, so DARWIN does not go silent on something urgent
/// just because the user asked for fewer interruptions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalCategory {
    /// A genuinely urgent/critical signal (an imminent calendar conflict, a
    /// critical system-health reading). The floor: NO profile silences this.
    Critical,
    /// A calendar signal that is upcoming but not urgent.
    Calendar,
    /// Unread/important mail.
    Mail,
    /// System-health intel below the critical bar (e.g. a notable but not dire
    /// reading).
    Health,
    /// A market move.
    Market,
    /// News / world-model intel (Global-Scan). The lowest-priority, first-quieted
    /// category.
    News,
    /// Routine intel that recurs (a predictive "you usually do X now" heads-up).
    Routine,
}

impl SignalCategory {
    /// A stable short string for telemetry.
    pub fn as_str(&self) -> &'static str {
        match self {
            SignalCategory::Critical => "critical",
            SignalCategory::Calendar => "calendar",
            SignalCategory::Mail => "mail",
            SignalCategory::Health => "health",
            SignalCategory::Market => "market",
            SignalCategory::News => "news",
            SignalCategory::Routine => "routine",
        }
    }

    /// Every category, in priority order (Critical first). The base behavior's
    /// "surface everything" set.
    pub fn all() -> [SignalCategory; 7] {
        [
            SignalCategory::Critical,
            SignalCategory::Calendar,
            SignalCategory::Mail,
            SignalCategory::Health,
            SignalCategory::Market,
            SignalCategory::News,
            SignalCategory::Routine,
        ]
    }
}

// ---------------------------------------------------------------------------
// Verbosity — how much a surfaced brief says (a NON-CONSEQUENTIAL knob)
// ---------------------------------------------------------------------------

/// How verbose a surfaced brief should be. Ordered from most to least: `Full`
/// (every ranked item), `Brief` (top item(s) only — a glance), `Silent` (no
/// brief at all). A profile may only step DOWN or hold (never up), so a profile
/// can make the digest terser, never chattier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verbosity {
    /// Render the full ranked digest (capped as the builder caps it).
    Full,
    /// Render only the single highest-priority item — a one-line glance.
    Brief,
    /// Render no brief at all (the surface goes dark — but a Critical category
    /// still surfaces independently via the surfacing set; verbosity governs the
    /// DIGEST, not the critical floor).
    Silent,
}

impl Verbosity {
    /// A rank where SMALLER == terser, so "no broader than" is `self >= base` in
    /// terseness (i.e. `self.rank() >= base.rank()` means self is at least as
    /// quiet). Full=0 (loudest), Brief=1, Silent=2 (quietest).
    fn rank(&self) -> u8 {
        match self {
            Verbosity::Full => 0,
            Verbosity::Brief => 1,
            Verbosity::Silent => 2,
        }
    }

    /// How many ranked items this verbosity admits (the builder caps further).
    /// `Full` => the builder's own cap; `Brief` => 1; `Silent` => 0.
    pub fn max_items(&self, full_cap: usize) -> usize {
        match self {
            Verbosity::Full => full_cap,
            Verbosity::Brief => 1,
            Verbosity::Silent => 0,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Verbosity::Full => "full",
            Verbosity::Brief => "brief",
            Verbosity::Silent => "silent",
        }
    }
}

// ---------------------------------------------------------------------------
// The behavior types — NON-CONSEQUENTIAL by construction
// ---------------------------------------------------------------------------

/// The proactive behavior knobs a focus profile may tune. This is the WHOLE
/// surface a profile touches — and notice what is NOT here: no permission, no
/// gate, no confirm, no master switch, no voice-id, no lockdown, no autonomy
/// level, no consequential-action flag. Those live in `integrations`,
/// `confirm`, `lockdown`, `policy` — and `apply_profile` neither takes nor
/// returns any of them. A profile literally cannot reach them through this type.
///
/// The three knobs:
///   * `surfacing` — the set of signal CATEGORIES allowed to surface.
///   * `verbosity` — how much a surfaced brief says.
///   * `suggestions_quieted` — whether the proactive-suggestion feed is held back.
///
/// All three are NON-CONSEQUENTIAL: they govern which already-permitted,
/// outward-call-free intel the user sees, never whether an action may run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseBehavior {
    /// Categories allowed to surface. The base "show everything" is
    /// [`SignalCategory::all`].
    pub surfacing: Vec<SignalCategory>,
    /// Brief verbosity. Base is [`Verbosity::Full`].
    pub verbosity: Verbosity,
    /// Whether the proactive-suggestion feed is quieted. Base is `false`
    /// (suggestions surface as today, still behind their own `[proactive].suggest`
    /// gate — focus does not open that gate, it can only further quiet).
    pub suggestions_quieted: bool,
}

impl Default for BaseBehavior {
    /// Today's behavior: every category surfaces, full verbosity, suggestions not
    /// quieted by focus. This is the base every profile tunes DOWN from.
    fn default() -> Self {
        BaseBehavior {
            surfacing: SignalCategory::all().to_vec(),
            verbosity: Verbosity::Full,
            suggestions_quieted: false,
        }
    }
}

/// The tuned behavior `apply_profile` returns. SAME knobs as [`BaseBehavior`],
/// and — crucially — SAME absence of any permission/gate/autonomy field. There
/// is no constructor here that can add a consequential capability; `apply_profile`
/// only ever produces a value whose every knob is restrict-only relative to the
/// base it was given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunedBehavior {
    pub surfacing: Vec<SignalCategory>,
    pub verbosity: Verbosity,
    pub suggestions_quieted: bool,
}

impl TunedBehavior {
    /// Whether `category` is allowed to surface under this tuned behavior.
    pub fn surfaces(&self, category: SignalCategory) -> bool {
        self.surfacing.contains(&category)
    }

    /// View this tuned behavior AS A BASE for FURTHER restriction — the seam
    /// Auto-Focus (`select_profile`) uses to compose a second profile ON TOP
    /// through the SAME [`apply_profile`] path while preserving the restrict-only
    /// invariant. Re-applying a profile to this base can only NARROW further
    /// (never re-broaden — see the idempotent-restriction property test), and
    /// because `is_no_broader_than` is transitive, the composed result is no
    /// broader than the ORIGINAL base too. It carries only the three
    /// NON-CONSEQUENTIAL knobs across — there is no gate/permission/autonomy field
    /// to smuggle through — so composition stays permission-neutral by
    /// construction.
    pub fn as_base(&self) -> BaseBehavior {
        BaseBehavior {
            surfacing: self.surfacing.clone(),
            verbosity: self.verbosity,
            suggestions_quieted: self.suggestions_quieted,
        }
    }

    /// THE machine-checkable PERMISSION-NEUTRALITY predicate: is this tuned
    /// behavior NO BROADER than `base` on every axis? True iff:
    ///   * its surfacing set is a SUBSET of the base's (it added no category the
    ///     base suppressed);
    ///   * its verbosity is at least as terse as the base's (stepped down or held);
    ///   * its `suggestions_quieted` is at least as quiet (false->true or held,
    ///     never true->false).
    ///
    /// Because the type carries NO permission/gate/autonomy field, "no broader"
    /// on these three NON-CONSEQUENTIAL axes is the COMPLETE statement of "this
    /// profile loosened nothing" — there is no other axis on which it COULD
    /// loosen. The property test asserts this for every profile.
    ///
    /// `#[allow(dead_code)]`: this is the #24 GATE's machine-checkable predicate,
    /// exercised by the `property_no_profile_broadens_the_permission_surface`
    /// property test (a `#[cfg(test)]` consumer). It is kept as a first-class
    /// method (not test-local) so the invariant lives next to the type it guards.
    #[allow(dead_code)]
    pub fn is_no_broader_than(&self, base: &BaseBehavior) -> bool {
        let surfacing_subset = self
            .surfacing
            .iter()
            .all(|c| base.surfacing.contains(c));
        let verbosity_no_louder = self.verbosity.rank() >= base.verbosity.rank();
        // suggestions: base.quieted == true must stay true (can't un-quiet);
        // base.quieted == false may go either way (quieting more is fine).
        let suggestions_no_louder = !base.suggestions_quieted || self.suggestions_quieted;
        surfacing_subset && verbosity_no_louder && suggestions_no_louder
    }

    /// The HUD telemetry for the active focus (the `focus.active` card): which
    /// categories surface, the verbosity, whether suggestions are quieted, and
    /// the explicit permission-neutral posture. Secret-free; no permission/gate
    /// field exists to leak.
    pub fn telemetry(&self, profile: FocusProfile) -> serde_json::Value {
        let cats: Vec<&str> = self.surfacing.iter().map(|c| c.as_str()).collect();
        serde_json::json!({
            "profile": profile.as_str(),
            "surfacing": cats,
            "verbosity": self.verbosity.as_str(),
            "suggestions_quieted": self.suggestions_quieted,
            // Make the contract explicit on the wire so the HUD can state it.
            "permission_neutral": true,
            "raises_autonomy": false,
            "loosens_gate": false,
        })
    }
}

// ---------------------------------------------------------------------------
// The profiles
// ---------------------------------------------------------------------------

/// A focus profile: the named lens the operator selects. `Default` is the
/// identity (today's behavior). The others quiet progressively more:
///   * `Work` — silences News + Routine (stay heads-down on work intel: calendar,
///     mail, health still surface; market quiets).
///   * `Sleep` — silences everything EXCEPT Critical, brief verbosity, quiets
///     suggestions (the user is asleep; only a genuinely critical thing surfaces).
///   * `DeepFocus` — surfaces NOTHING but Critical, Silent digest, quiets
///     suggestions (the most restrictive: a true do-not-disturb that still lets a
///     critical signal through).
///   * `Custom(name)` — a named custom profile. It carries no extra power: a
///     custom profile is built by the SAME restrict-only construction (it can
///     only narrow the base), so an operator-named profile can never be broader
///     than the base either. The name is cosmetic (telemetry/copy).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FocusProfile {
    Default,
    Work,
    Sleep,
    DeepFocus,
    /// A named custom profile. The name is cosmetic; the behavior is the
    /// restrict-only `Custom` table below.
    Custom(String),
}

impl FocusProfile {
    /// A stable short string for telemetry/copy.
    pub fn as_str(&self) -> &'static str {
        match self {
            FocusProfile::Default => "default",
            FocusProfile::Work => "work",
            FocusProfile::Sleep => "sleep",
            FocusProfile::DeepFocus => "deep_focus",
            FocusProfile::Custom(_) => "custom",
        }
    }

    /// Parse a `[focus].profile` config string into a profile. Empty/whitespace/
    /// "default" => `Default` (the identity — today's behavior). The recognized
    /// names map to their profiles. Any OTHER non-blank string is a NAMED CUSTOM
    /// profile (the name is cosmetic; the behavior is the restrict-only `Custom`
    /// table) — so even a typo can only ever QUIET, never broaden (fail SAFE by
    /// CONSTRUCTION, not by degrading to Default).
    pub fn from_config_str(s: &str) -> FocusProfile {
        match s.trim().to_lowercase().as_str() {
            "" | "default" => FocusProfile::Default,
            "work" => FocusProfile::Work,
            "sleep" => FocusProfile::Sleep,
            "deep_focus" | "deepfocus" | "deep-focus" => FocusProfile::DeepFocus,
            // A custom profile name. Preserve the original (trimmed) string for
            // telemetry copy, but the BEHAVIOR is the restrict-only Custom table.
            other => FocusProfile::Custom(other.to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// Trigger -> category (the EDITH single-card surface's focus axis)
// ---------------------------------------------------------------------------

/// Map an EDITH anticipation [`crate::anticipate::TriggerKind`] to the focus
/// [`SignalCategory`] used to decide whether the active profile silences its
/// single-card surface. A low-disk reading is CRITICAL (never silenced — a full
/// disk can break the machine); calendar/mail/mem-high map to their own
/// categories; a market move is News-adjacent low priority. PURE; no clock, no
/// state. Keeps the live-tick wiring honest: the SAME critical-floor rule the
/// snapshot->brief conversion uses governs the single-card surface too.
pub fn category_for_trigger(kind: crate::anticipate::TriggerKind) -> SignalCategory {
    use crate::anticipate::TriggerKind;
    match kind {
        // A low disk is critical — it survives even DeepFocus.
        TriggerKind::DiskLow => SignalCategory::Critical,
        TriggerKind::Calendar => SignalCategory::Calendar,
        TriggerKind::Mail => SignalCategory::Mail,
        TriggerKind::MemHigh => SignalCategory::Health,
        TriggerKind::Market => SignalCategory::Market,
    }
}

// ---------------------------------------------------------------------------
// apply_profile — PURE, restrict-only
// ---------------------------------------------------------------------------

/// Apply a focus profile to a base behavior, returning the tuned behavior.
///
/// PURE and restrict-only BY CONSTRUCTION. Every branch builds its result by
/// REMOVING categories from `base.surfacing` (never adding), stepping verbosity
/// DOWN or holding (never up), and only ever flipping `suggestions_quieted`
/// false->true (never true->false). The `Default` branch returns the base
/// unchanged (the identity). Because [`TunedBehavior`] has no permission/gate/
/// autonomy field, there is no branch that COULD return a broader posture — the
/// strongest thing any branch does is silence more.
///
/// The invariant `apply_profile(p, base).is_no_broader_than(base)` holds for
/// EVERY `p` and EVERY `base` — proven by the property test.
pub fn apply_profile(profile: &FocusProfile, base: &BaseBehavior) -> TunedBehavior {
    // Helper: keep only the base categories NOT in `silence` (subset by
    // construction — we can only drop, never add).
    let keep_except = |silence: &[SignalCategory]| -> Vec<SignalCategory> {
        base.surfacing
            .iter()
            .copied()
            .filter(|c| !silence.contains(c))
            .collect()
    };
    // Helper: the terser of (base, requested) verbosity — never louder than base.
    let step_down = |requested: Verbosity| -> Verbosity {
        if requested.rank() >= base.verbosity.rank() {
            requested
        } else {
            base.verbosity
        }
    };
    // Helper: quiet suggestions at least as much as the base (OR with base).
    let quiet = |q: bool| -> bool { q || base.suggestions_quieted };

    match profile {
        // IDENTITY: today's behavior, byte-for-byte. The shipped default.
        FocusProfile::Default => TunedBehavior {
            surfacing: base.surfacing.clone(),
            verbosity: base.verbosity,
            suggestions_quieted: base.suggestions_quieted,
        },
        // WORK: heads-down — silence News + Routine (and Market, a non-work
        // distraction); keep calendar/mail/health/critical. Full verbosity (work
        // intel is wanted in full), suggestions not additionally quieted.
        FocusProfile::Work => TunedBehavior {
            surfacing: keep_except(&[
                SignalCategory::News,
                SignalCategory::Routine,
                SignalCategory::Market,
            ]),
            verbosity: step_down(Verbosity::Full),
            suggestions_quieted: quiet(false),
        },
        // SLEEP: only Critical surfaces; everything else is silenced. Brief
        // verbosity for the rare critical thing; suggestions quieted.
        FocusProfile::Sleep => TunedBehavior {
            surfacing: keep_except(&[
                SignalCategory::Calendar,
                SignalCategory::Mail,
                SignalCategory::Health,
                SignalCategory::Market,
                SignalCategory::News,
                SignalCategory::Routine,
            ]),
            verbosity: step_down(Verbosity::Brief),
            suggestions_quieted: quiet(true),
        },
        // DEEP FOCUS: the most restrictive. Only Critical surfaces, the digest is
        // Silent (no brief at all), suggestions quieted. A true do-not-disturb —
        // but a genuinely critical signal still gets through (the floor).
        FocusProfile::DeepFocus => TunedBehavior {
            surfacing: keep_except(&[
                SignalCategory::Calendar,
                SignalCategory::Mail,
                SignalCategory::Health,
                SignalCategory::Market,
                SignalCategory::News,
                SignalCategory::Routine,
            ]),
            verbosity: step_down(Verbosity::Silent),
            suggestions_quieted: quiet(true),
        },
        // CUSTOM: a named profile. It carries no special power — it is built by
        // the SAME restrict-only construction. The shipped custom behavior quiets
        // News + Routine + Market and steps to Brief (a sensible "fewer
        // interruptions" lens). An operator who wants a different custom mix edits
        // this table; whatever they pick, `keep_except`/`step_down`/`quiet` make
        // it IMPOSSIBLE to broaden the base. The name is cosmetic.
        FocusProfile::Custom(_) => TunedBehavior {
            surfacing: keep_except(&[
                SignalCategory::News,
                SignalCategory::Routine,
                SignalCategory::Market,
            ]),
            verbosity: step_down(Verbosity::Brief),
            suggestions_quieted: quiet(true),
        },
    }
}

// ---------------------------------------------------------------------------
// AUTO-FOCUS — presence/scene-driven profile SELECTION (restrict-only)
//
// Everything below is a PURE fusion layer that PICKS one of the EXISTING
// quiet-only profiles above from sensed room state. It does NOT introduce a new
// behavior surface: the picked profile is still applied through the identical
// restrict-only [`apply_profile`] path, so an auto-selected profile rides the
// SAME `is_no_broader_than` type-level invariant — it can only ever NARROW which
// non-consequential intel surfaces, never enable an action, raise autonomy, or
// touch a gate. The selection is the only new logic; the enforcement is
// unchanged. Ships OPT-IN (`[focus].auto`, default OFF) because it changes what
// surfaces based on sensed state; OFF, focus behaves exactly as today.
// ---------------------------------------------------------------------------

/// The coarse ROOM SOUNDSCAPE the auto-focus fuser reasons over. DISTINCT from
/// [`crate::scene::SceneEvent`]'s sound-EVENT labels (doorbell/knock/…): this is
/// the ambient-scene CLASS relevant to whether DARWIN should stay quiet.
///
/// `Unknown` is the HONEST default: no room-scene classifier is bundled
/// (scene.rs ships armed-but-inert — no model, capture tap unwired), so live the
/// scene degrades to `Unknown` and the fuser falls back to presence + calendar +
/// time. The non-`Unknown` variants are exercised by the unit tests and by the
/// [`AcousticScene::from_scene_events`] seam so the fusion is fully specified for
/// when a classifier is wired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcousticScene {
    /// No usable scene reading this tick (classifier inert / not wired). Honest
    /// absence — contributes nothing to the fusion.
    Unknown,
    /// A silent room. Non-restrictive (defer to the other signals).
    Quiet,
    /// Background/ambient noise that is not a conversation. Non-restrictive.
    Ambient,
    /// People talking in the room (an in-person meeting/conversation). A
    /// do-not-disturb cue.
    Conversation,
    /// You are on/near a call (e.g. a phone ringing / call audio). A
    /// do-not-disturb cue.
    Call,
}

impl AcousticScene {
    /// Whether this scene implies you are mid-conversation/call — the "call/
    /// meeting scene" the fuser quiets for. `Unknown`/`Quiet`/`Ambient` are not
    /// conversation cues.
    fn indicates_conversation(self) -> bool {
        matches!(self, AcousticScene::Call | AcousticScene::Conversation)
    }

    /// Derive a coarse focus scene from scene.rs's sound-EVENT detections (the
    /// on-device acoustic classifier's real output type). A `phone_ring` is the
    /// one shipped event that implies you are likely on/near a call -> `Call`.
    /// Any other known event is ambient noise, not a conversation cue ->
    /// `Ambient`. No events -> `Unknown` (the honest default: scene.rs ships inert
    /// — no model, capture tap unwired — so live this stays `Unknown` and the
    /// fuser falls back to presence/calendar/time). Wired to scene.rs's actual
    /// type so the scene seam is live code, not a stub; it feeds selection once
    /// scene.rs's capture tap is built.
    ///
    /// `#[allow(dead_code)]`: this is the SCENE seam. scene.rs ships armed-but-
    /// inert (no bundled classifier, capture tap unwired), so the live tick has no
    /// scene events to feed and passes `Unknown` directly — this mapper is
    /// exercised by the `scene_from_events_*` unit test today and becomes the live
    /// feed the moment scene.rs's tap lands. Kept as a first-class method (not
    /// test-local) so the mapping lives next to the type, mirroring
    /// [`TunedBehavior::is_no_broader_than`]'s treatment.
    #[allow(dead_code)]
    pub fn from_scene_events(events: &[crate::scene::SceneEvent]) -> AcousticScene {
        if events.iter().any(|e| e.label == "phone_ring") {
            AcousticScene::Call
        } else if events.is_empty() {
            AcousticScene::Unknown
        } else {
            AcousticScene::Ambient
        }
    }
}

/// Coarse calendar busy-state for auto-focus. `InMeeting` means a meeting is in
/// progress (or imminent); `Free` means no known meeting; `Unknown` is an
/// explicit no-reading (used by tests / honest degradation — the live wire never
/// fabricates a meeting, so an unconnected calendar reads `Free`, not a made-up
/// `InMeeting`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalendarState {
    Unknown,
    Free,
    InMeeting,
}

/// Derive the [`CalendarState`] from the SAME upcoming-events the evaluator sees.
/// We have no per-event END time, so a bounded window around the start is the
/// honest "in progress" proxy: an event counts as `InMeeting` from `imminent_min`
/// minutes BEFORE it starts through `lookback_min` minutes AFTER (a
/// `minutes_until` in `[-lookback_min, imminent_min]`). Empty/absent events ->
/// `Free` (a not-connected calendar degrades to no known meeting, never a
/// fabricated one). PURE.
pub fn calendar_state(
    events: &[crate::anticipate::UpcomingEvent],
    imminent_min: i64,
    lookback_min: i64,
) -> CalendarState {
    let busy = events
        .iter()
        .any(|e| e.minutes_until <= imminent_min && e.minutes_until >= -lookback_min);
    if busy {
        CalendarState::InMeeting
    } else {
        CalendarState::Free
    }
}

/// Time-of-day bucket the fuser reasons over. `Night` (likely-asleep hours)
/// biases toward the `Sleep` profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeOfDay {
    Day,
    Night,
}

impl TimeOfDay {
    /// Classify a local hour as `Night` iff it falls in the `[start, end)` band
    /// (reusing [`crate::anticipate::in_quiet_hours`] so the wrap-midnight math is
    /// shared, not re-derived). `start == end` is an empty band (never `Night`).
    /// The live tick passes the operator's `[proactive]` quiet band as the sleep
    /// window.
    pub fn from_local_hour(hour: u8, start: u8, end: u8) -> TimeOfDay {
        if crate::anticipate::in_quiet_hours(hour, start, end) {
            TimeOfDay::Night
        } else {
            TimeOfDay::Day
        }
    }
}

/// The fused, ON-DEVICE signal snapshot [`select_profile`] reasons over. Every
/// field is a coarse, non-secret bucket — no raw audio, no timestamps, no event
/// titles. PURE input: the caller assembles it at the live edge from the
/// on-device acoustic scene, the fused [`crate::presence::Presence`], the calendar
/// busy-state, and the time-of-day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FocusSignals {
    /// On-device acoustic room scene (Unknown when no classifier is wired).
    pub scene: AcousticScene,
    /// Fused presence/attention (Away / Present / Focused).
    pub presence: crate::presence::Presence,
    /// Calendar busy-state (InMeeting / Free / Unknown).
    pub calendar: CalendarState,
    /// Time-of-day bucket (Day / Night).
    pub time: TimeOfDay,
}

/// The result of an auto-focus selection: the chosen [`FocusProfile`] plus a
/// stable, non-secret `reason` token for telemetry/copy. Carries NO extra power —
/// it names one of the EXISTING restrict-only profiles; applying it goes through
/// [`apply_profile`], so it is bound by the same invariant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileChoice {
    pub profile: FocusProfile,
    /// Why this profile was picked (a stable token: "away", "in_meeting",
    /// "night", "focused_flow", "clear"). Secret-free.
    pub reason: &'static str,
}

impl ProfileChoice {
    fn new(profile: FocusProfile, reason: &'static str) -> Self {
        ProfileChoice { profile, reason }
    }

    /// The `focus.active` telemetry frame for an AUTO selection: the same
    /// permission-neutral posture the manual card carries (via
    /// [`TunedBehavior::telemetry`]), plus `source: "auto"` and the `reason`
    /// token so the HUD can show WHY the lens changed. Secret-free — it reuses the
    /// tuned telemetry (which has no gate/permission field to leak) and adds two
    /// non-secret strings.
    pub fn telemetry(&self, tuned: &TunedBehavior) -> serde_json::Value {
        let mut v = tuned.telemetry(self.profile.clone());
        if let Some(obj) = v.as_object_mut() {
            obj.insert("source".to_string(), serde_json::json!("auto"));
            obj.insert("reason".to_string(), serde_json::json!(self.reason));
        }
        v
    }
}

/// PURE. Fuse the on-device [`FocusSignals`] into ONE of the existing quiet-only
/// profiles. This is the whole of Auto-Focus's new decision logic; enforcement of
/// the restrict-only invariant is unchanged (the caller applies the choice via
/// [`apply_profile`]).
///
/// The table, most-restrictive cue first (first match wins):
///   1. **Away** -> `DeepFocus`. Nobody is at the machine; go fully quiet. (The
///      single card is already presence-gated, but the multi-item digest and the
///      suggestion feed are NOT — quieting those is the real effect.)
///   2. **Call/meeting** (a conversation/call scene OR the calendar says
///      InMeeting) -> `DeepFocus`. Do not disturb while you are on a call / in a
///      meeting.
///   3. **Night** -> `Sleep`. Likely asleep: only Critical surfaces, brief,
///      suggestions quieted.
///   4. **Focused** (at the machine, in silent flow) -> `Work`. Heads-down:
///      silence news/routine/market, keep work intel.
///   5. Otherwise (Present, quiet room, daytime, calendar free) -> `Default` (the
///      IDENTITY — today's behavior). "Quiet room + present -> normal."
///
/// EVERY branch returns a profile that `apply_profile` guarantees is no broader
/// than its base, so no [`FocusSignals`] can produce a broadening choice.
pub fn select_profile(signals: FocusSignals) -> ProfileChoice {
    if signals.presence == crate::presence::Presence::Away {
        return ProfileChoice::new(FocusProfile::DeepFocus, "away");
    }
    if signals.scene.indicates_conversation() || signals.calendar == CalendarState::InMeeting {
        return ProfileChoice::new(FocusProfile::DeepFocus, "in_meeting");
    }
    if signals.time == TimeOfDay::Night {
        return ProfileChoice::new(FocusProfile::Sleep, "night");
    }
    if signals.presence == crate::presence::Presence::Focused {
        return ProfileChoice::new(FocusProfile::Work, "focused_flow");
    }
    ProfileChoice::new(FocusProfile::Default, "clear")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every profile we ship, including a named custom, for the table + property
    /// tests. Exhaustive over the profile space (Custom stands in for the named
    /// family — every Custom shares one restrict-only table).
    fn all_profiles() -> Vec<FocusProfile> {
        vec![
            FocusProfile::Default,
            FocusProfile::Work,
            FocusProfile::Sleep,
            FocusProfile::DeepFocus,
            FocusProfile::Custom("study".to_string()),
        ]
    }

    // =====================================================================
    // DEFAULT == TODAY: the identity (the shipped neutral default)
    // =====================================================================

    #[test]
    fn default_profile_is_the_identity_todays_behavior() {
        let base = BaseBehavior::default();
        let tuned = apply_profile(&FocusProfile::Default, &base);
        assert_eq!(tuned.surfacing, base.surfacing, "default surfaces everything the base does");
        assert_eq!(tuned.verbosity, base.verbosity, "default keeps base verbosity");
        assert_eq!(
            tuned.suggestions_quieted, base.suggestions_quieted,
            "default does not quiet suggestions"
        );
        // The identity over ANY base, not just the canonical one.
        let custom_base = BaseBehavior {
            surfacing: vec![SignalCategory::Critical, SignalCategory::Mail],
            verbosity: Verbosity::Brief,
            suggestions_quieted: true,
        };
        let tuned = apply_profile(&FocusProfile::Default, &custom_base);
        assert_eq!(
            TunedBehavior {
                surfacing: custom_base.surfacing.clone(),
                verbosity: custom_base.verbosity,
                suggestions_quieted: custom_base.suggestions_quieted,
            },
            tuned,
            "Default is the identity over any base"
        );
    }

    #[test]
    fn shipped_default_config_profile_is_default() {
        // The ships-NEUTRAL contract: an empty/"default" config string parses to
        // the identity profile, so the shipped config reproduces today's behavior.
        assert_eq!(FocusProfile::from_config_str(""), FocusProfile::Default);
        assert_eq!(FocusProfile::from_config_str("default"), FocusProfile::Default);
        // A blank-but-whitespace value also degrades to the identity.
        assert_eq!(FocusProfile::from_config_str("   "), FocusProfile::Default);
        // An unrecognized non-blank value is a NAMED CUSTOM profile — which is
        // ITSELF restrict-only (it can only quiet, never broaden), so an operator
        // typo can never accidentally LOOSEN anything. Safety here is the
        // restrict-only construction, not a degrade-to-default.
        let unknown = FocusProfile::from_config_str("nonsense");
        assert_eq!(unknown, FocusProfile::Custom("nonsense".to_string()));
        assert!(
            apply_profile(&unknown, &BaseBehavior::default()).is_no_broader_than(&BaseBehavior::default()),
            "an unknown profile name is a restrict-only custom profile, never broader"
        );
        // The config default (FocusConfig::default) is "default".
        let cfg = crate::config::FocusConfig::default();
        assert_eq!(cfg.profile, "default", "[focus].profile ships \"default\"");
        assert_eq!(
            FocusProfile::from_config_str(&cfg.profile),
            FocusProfile::Default,
            "the shipped config profile is the identity"
        );
    }

    // =====================================================================
    // THE PROFILE TABLE: each non-default profile only RESTRICTS/QUIETS
    // =====================================================================

    #[test]
    fn work_silences_news_and_routine_keeps_work_intel() {
        let base = BaseBehavior::default();
        let t = apply_profile(&FocusProfile::Work, &base);
        assert!(!t.surfaces(SignalCategory::News), "work silences news");
        assert!(!t.surfaces(SignalCategory::Routine), "work silences routine");
        assert!(!t.surfaces(SignalCategory::Market), "work silences market");
        // Work intel still surfaces.
        assert!(t.surfaces(SignalCategory::Calendar));
        assert!(t.surfaces(SignalCategory::Mail));
        assert!(t.surfaces(SignalCategory::Critical), "critical never silenced");
    }

    #[test]
    fn sleep_surfaces_only_critical() {
        let base = BaseBehavior::default();
        let t = apply_profile(&FocusProfile::Sleep, &base);
        assert!(t.surfaces(SignalCategory::Critical), "critical still gets through asleep");
        for c in [
            SignalCategory::Calendar,
            SignalCategory::Mail,
            SignalCategory::Health,
            SignalCategory::Market,
            SignalCategory::News,
            SignalCategory::Routine,
        ] {
            assert!(!t.surfaces(c), "sleep silences {c:?}");
        }
        assert!(t.suggestions_quieted, "sleep quiets suggestions");
        assert_eq!(t.verbosity, Verbosity::Brief);
    }

    #[test]
    fn deep_focus_is_the_most_restrictive_but_still_lets_critical_through() {
        let base = BaseBehavior::default();
        let t = apply_profile(&FocusProfile::DeepFocus, &base);
        // Only critical surfaces.
        assert_eq!(t.surfacing, vec![SignalCategory::Critical]);
        assert!(t.surfaces(SignalCategory::Critical), "even deep focus passes critical");
        assert_eq!(t.verbosity, Verbosity::Silent, "deep focus renders no digest");
        assert!(t.suggestions_quieted, "deep focus quiets suggestions");
    }

    #[test]
    fn custom_profile_is_restrict_only_and_named() {
        let base = BaseBehavior::default();
        let p = FocusProfile::from_config_str("study");
        assert_eq!(p, FocusProfile::Custom("study".to_string()), "an unknown name is a custom profile");
        let t = apply_profile(&p, &base);
        // Restrict-only: it dropped categories, never added; it cannot exceed base.
        assert!(t.is_no_broader_than(&base), "a custom profile can never broaden the base");
        assert!(t.surfaces(SignalCategory::Critical), "critical floor holds for custom too");
    }

    // =====================================================================
    // PERMISSION-NEUTRALITY: the property test — NO profile broadens
    // =====================================================================

    /// A small spread of bases to run the property over: the full base, an
    /// already-narrowed base, an already-quieted base, and a critical-only base.
    /// The invariant must hold against EVERY base, not just the canonical one —
    /// applying a profile to an already-restricted behavior must still only
    /// restrict further (composition stays restrict-only).
    fn bases() -> Vec<BaseBehavior> {
        vec![
            BaseBehavior::default(),
            BaseBehavior {
                surfacing: vec![SignalCategory::Critical, SignalCategory::Calendar, SignalCategory::Mail],
                verbosity: Verbosity::Brief,
                suggestions_quieted: false,
            },
            BaseBehavior {
                surfacing: vec![SignalCategory::Critical, SignalCategory::News],
                verbosity: Verbosity::Full,
                suggestions_quieted: true,
            },
            BaseBehavior {
                surfacing: vec![SignalCategory::Critical],
                verbosity: Verbosity::Silent,
                suggestions_quieted: true,
            },
        ]
    }

    #[test]
    fn property_no_profile_broadens_the_permission_surface() {
        // THE #24 GATE, machine-checked: for EVERY profile and EVERY base, the
        // tuned behavior is NO BROADER than the base on every axis. A profile can
        // only ever make DARWIN quieter — never surface a category the base
        // suppressed, never get louder, never un-quiet suggestions.
        for base in bases() {
            for profile in all_profiles() {
                let tuned = apply_profile(&profile, &base);
                assert!(
                    tuned.is_no_broader_than(&base),
                    "profile {:?} broadened base {:?} -> {:?}",
                    profile,
                    base,
                    tuned
                );
                // Surfacing is strictly a SUBSET (no category appears that the
                // base didn't already allow).
                for c in &tuned.surfacing {
                    assert!(
                        base.surfacing.contains(c),
                        "profile {:?} surfaced {:?} which base {:?} suppressed",
                        profile,
                        c,
                        base
                    );
                }
                // Verbosity never louder than base.
                assert!(
                    tuned.verbosity.rank() >= base.verbosity.rank(),
                    "profile {:?} made the digest louder than base {:?}",
                    profile,
                    base
                );
                // Suggestions: a base that quieted them stays quieted.
                if base.suggestions_quieted {
                    assert!(
                        tuned.suggestions_quieted,
                        "profile {:?} un-quieted suggestions the base had quieted",
                        profile
                    );
                }
            }
        }
    }

    #[test]
    fn applying_a_profile_twice_never_re_broadens_idempotent_restriction() {
        // Composing a profile onto its own output must not re-broaden: feed the
        // tuned result back as a base and re-apply — the second pass is still no
        // broader than the first. (Restriction composes monotonically.)
        for profile in all_profiles() {
            let base = BaseBehavior::default();
            let once = apply_profile(&profile, &base);
            let twice_base = BaseBehavior {
                surfacing: once.surfacing.clone(),
                verbosity: once.verbosity,
                suggestions_quieted: once.suggestions_quieted,
            };
            let twice = apply_profile(&profile, &twice_base);
            assert!(
                twice.is_no_broader_than(&twice_base),
                "re-applying {profile:?} re-broadened its own output"
            );
        }
    }

    // =====================================================================
    // TYPE-LEVEL ARGUMENT: TunedBehavior carries no permission/gate field
    // =====================================================================

    #[test]
    fn tuned_behavior_has_only_non_consequential_knobs() {
        // This test is a STANDING ASSERTION (read with the struct def): the only
        // way to read anything off a TunedBehavior is the three NON-CONSEQUENTIAL
        // knobs below. There is no `.gate`, `.confirm`, `.allow_consequential`,
        // `.autonomy`, `.voice_id`, `.lockdown`, `.permission` — the type does not
        // have them, so `apply_profile` provably cannot return one. If a future
        // edit added a permission field to TunedBehavior, this test's exhaustive
        // destructuring would FAIL TO COMPILE, forcing a re-review of the #24 gate.
        let t = apply_profile(&FocusProfile::DeepFocus, &BaseBehavior::default());
        let TunedBehavior {
            surfacing: _,
            verbosity: _,
            suggestions_quieted: _,
        } = t;
        // (No further assertions needed — the exhaustive pattern IS the proof.)
    }

    // =====================================================================
    // TELEMETRY shape — the HUD focus.active card
    // =====================================================================

    #[test]
    fn telemetry_states_the_permission_neutral_posture() {
        let t = apply_profile(&FocusProfile::Sleep, &BaseBehavior::default());
        let v = t.telemetry(FocusProfile::Sleep);
        assert_eq!(v["profile"], "sleep");
        assert_eq!(v["verbosity"], "brief");
        assert_eq!(v["suggestions_quieted"], true);
        // The contract is on the wire so the HUD copy is grounded, not hardcoded.
        assert_eq!(v["permission_neutral"], true);
        assert_eq!(v["raises_autonomy"], false);
        assert_eq!(v["loosens_gate"], false);
        // Surfacing carries only the critical floor under sleep.
        assert_eq!(v["surfacing"], serde_json::json!(["critical"]));
    }

    // =====================================================================
    // AUTO-FOCUS — select_profile fusion + the restrict-only invariant
    // =====================================================================

    use crate::presence::Presence;

    /// A neutral snapshot: present, quiet room, no meeting, daytime — the case
    /// that must resolve to the IDENTITY (Default). Tests override single fields.
    fn clear_signals() -> FocusSignals {
        FocusSignals {
            scene: AcousticScene::Quiet,
            presence: Presence::Present,
            calendar: CalendarState::Free,
            time: TimeOfDay::Day,
        }
    }

    #[test]
    fn quiet_room_present_daytime_selects_default_normal() {
        // "Quiet room + present -> normal": the fuser picks the IDENTITY profile,
        // so with auto ON in a clear room DARWIN behaves exactly as today.
        let choice = select_profile(clear_signals());
        assert_eq!(choice.profile, FocusProfile::Default);
        assert_eq!(choice.reason, "clear");
        // And applying it is the identity over the base.
        let base = BaseBehavior::default();
        let tuned = apply_profile(&choice.profile, &base);
        assert_eq!(tuned, apply_profile(&FocusProfile::Default, &base));
    }

    #[test]
    fn away_selects_deep_focus_quiet() {
        // "Away -> quiet": nobody at the machine -> the most restrictive profile,
        // regardless of scene/calendar/time.
        let choice = select_profile(FocusSignals { presence: Presence::Away, ..clear_signals() });
        assert_eq!(choice.profile, FocusProfile::DeepFocus);
        assert_eq!(choice.reason, "away");
    }

    #[test]
    fn call_scene_selects_deep_focus_quiet() {
        // "Call/meeting scene -> quiet": a call scene silences everything but
        // Critical while present in a quiet-hours-free daytime.
        let choice = select_profile(FocusSignals { scene: AcousticScene::Call, ..clear_signals() });
        assert_eq!(choice.profile, FocusProfile::DeepFocus);
        assert_eq!(choice.reason, "in_meeting");
        // A conversation scene is the same do-not-disturb cue.
        let conv = select_profile(FocusSignals { scene: AcousticScene::Conversation, ..clear_signals() });
        assert_eq!(conv.profile, FocusProfile::DeepFocus);
    }

    #[test]
    fn calendar_in_meeting_selects_deep_focus_even_without_a_scene() {
        // The calendar alone (scene Unknown — no classifier wired) still drives
        // the do-not-disturb selection.
        let choice = select_profile(FocusSignals {
            scene: AcousticScene::Unknown,
            calendar: CalendarState::InMeeting,
            ..clear_signals()
        });
        assert_eq!(choice.profile, FocusProfile::DeepFocus);
        assert_eq!(choice.reason, "in_meeting");
    }

    #[test]
    fn night_selects_sleep() {
        let choice = select_profile(FocusSignals { time: TimeOfDay::Night, ..clear_signals() });
        assert_eq!(choice.profile, FocusProfile::Sleep);
        assert_eq!(choice.reason, "night");
    }

    #[test]
    fn focused_silent_flow_selects_work() {
        // Present and in silent flow (no meeting/night/away) -> heads-down Work.
        let choice = select_profile(FocusSignals { presence: Presence::Focused, ..clear_signals() });
        assert_eq!(choice.profile, FocusProfile::Work);
        assert_eq!(choice.reason, "focused_flow");
    }

    #[test]
    fn away_outranks_meeting_night_and_flow() {
        // Priority: Away wins even when a meeting/night/flow cue is also set.
        let choice = select_profile(FocusSignals {
            scene: AcousticScene::Call,
            presence: Presence::Away,
            calendar: CalendarState::InMeeting,
            time: TimeOfDay::Night,
        });
        assert_eq!(choice.profile, FocusProfile::DeepFocus);
        assert_eq!(choice.reason, "away", "away is the first-matched cue");
    }

    /// The representative cross-product of signal combinations, for the invariant
    /// sweep below.
    fn representative_signals() -> Vec<FocusSignals> {
        let scenes = [
            AcousticScene::Unknown,
            AcousticScene::Quiet,
            AcousticScene::Ambient,
            AcousticScene::Conversation,
            AcousticScene::Call,
        ];
        let presences = [Presence::Away, Presence::Present, Presence::Focused];
        let calendars = [CalendarState::Unknown, CalendarState::Free, CalendarState::InMeeting];
        let times = [TimeOfDay::Day, TimeOfDay::Night];
        let mut out = Vec::new();
        for &scene in &scenes {
            for &presence in &presences {
                for &calendar in &calendars {
                    for &time in &times {
                        out.push(FocusSignals { scene, presence, calendar, time });
                    }
                }
            }
        }
        out
    }

    #[test]
    fn every_auto_choice_is_a_quiet_only_profile() {
        // select_profile may ONLY ever return one of the existing quiet-only
        // profiles — never a broadening or novel one. (Default is the identity;
        // the rest narrow.)
        for s in representative_signals() {
            let p = select_profile(s).profile;
            assert!(
                matches!(
                    p,
                    FocusProfile::Default
                        | FocusProfile::Work
                        | FocusProfile::Sleep
                        | FocusProfile::DeepFocus
                ),
                "auto picked an unexpected profile {p:?} for {s:?}"
            );
        }
    }

    #[test]
    fn property_no_auto_choice_broadens_the_base() {
        // THE #24 GATE for auto-selection, machine-checked: for EVERY representative
        // signal snapshot, applying the auto-selected profile is NO BROADER than the
        // base — auto-selection rides the SAME `is_no_broader_than` invariant as a
        // manually named profile. It can only ever quiet more.
        for base in [
            BaseBehavior::default(),
            // Compose over an already-restricted base too (the live tick composes
            // auto ON TOP of the configured profile via `as_base`): the result must
            // STILL be no broader than that base — restriction composes monotonically.
            apply_profile(&FocusProfile::Work, &BaseBehavior::default()).as_base(),
            apply_profile(&FocusProfile::DeepFocus, &BaseBehavior::default()).as_base(),
        ] {
            for s in representative_signals() {
                let choice = select_profile(s);
                let tuned = apply_profile(&choice.profile, &base);
                assert!(
                    tuned.is_no_broader_than(&base),
                    "auto choice {:?} for {:?} broadened base {:?} -> {:?}",
                    choice.profile,
                    s,
                    base,
                    tuned
                );
            }
        }
    }

    #[test]
    fn composing_auto_over_a_restrictive_configured_profile_never_re_broadens() {
        // The live tick applies the auto choice ON TOP of the configured profile's
        // tuned behavior (via `as_base`). Even when auto picks a LESS restrictive
        // profile (e.g. Work) than the configured one (DeepFocus), composition can
        // only narrow: the effective result is no broader than the configured
        // DeepFocus, so a cautious operator's static floor is never loosened.
        let configured = apply_profile(&FocusProfile::DeepFocus, &BaseBehavior::default());
        // Force auto to pick Work (present, silent flow, quiet, daytime, free).
        let choice = select_profile(FocusSignals { presence: Presence::Focused, ..clear_signals() });
        assert_eq!(choice.profile, FocusProfile::Work, "precondition: auto picks Work here");
        let composed = apply_profile(&choice.profile, &configured.as_base());
        assert!(
            composed.is_no_broader_than(&configured.as_base()),
            "auto composed over DeepFocus re-broadened it: {composed:?}"
        );
        // Concretely: DeepFocus surfaced only Critical; composing Work cannot add
        // any category back.
        assert_eq!(composed.surfacing, vec![SignalCategory::Critical]);
    }

    #[test]
    fn as_base_round_trips_the_three_knobs() {
        // The composition seam carries exactly the three non-consequential knobs.
        let tuned = apply_profile(&FocusProfile::Sleep, &BaseBehavior::default());
        let base = tuned.as_base();
        assert_eq!(base.surfacing, tuned.surfacing);
        assert_eq!(base.verbosity, tuned.verbosity);
        assert_eq!(base.suggestions_quieted, tuned.suggestions_quieted);
        // Applying Default (identity) to it reproduces the same tuned behavior.
        let again = apply_profile(&FocusProfile::Default, &base);
        assert_eq!(again, tuned);
    }

    #[test]
    fn scene_from_events_maps_phone_ring_to_call_and_degrades_honestly() {
        use crate::scene::SceneEvent;
        let ring = vec![SceneEvent { label: "phone_ring".into(), confidence: 0.9, ts: "2026-07-15T10:00:00Z".into() }];
        assert_eq!(AcousticScene::from_scene_events(&ring), AcousticScene::Call);
        let bark = vec![SceneEvent { label: "dog_bark".into(), confidence: 0.9, ts: "2026-07-15T10:00:00Z".into() }];
        assert_eq!(AcousticScene::from_scene_events(&bark), AcousticScene::Ambient, "non-call event is ambient");
        // No events (the live reality: scene.rs is inert) -> Unknown, the honest
        // default that makes the fuser fall back to presence/calendar/time.
        assert_eq!(AcousticScene::from_scene_events(&[]), AcousticScene::Unknown);
    }

    #[test]
    fn calendar_state_uses_a_bounded_window_and_never_fabricates() {
        use crate::anticipate::UpcomingEvent;
        let ev = |m: i64| UpcomingEvent { summary: "Sync".into(), minutes_until: m };
        // Imminent (starts in 3 min, window 5) -> InMeeting.
        assert_eq!(calendar_state(&[ev(3)], 5, 60), CalendarState::InMeeting);
        // In progress (started 20 min ago, lookback 60) -> InMeeting.
        assert_eq!(calendar_state(&[ev(-20)], 5, 60), CalendarState::InMeeting);
        // Far future (starts in 30 min, window 5) -> Free (not in it yet).
        assert_eq!(calendar_state(&[ev(30)], 5, 60), CalendarState::Free);
        // Long past (ended, older than lookback) -> Free.
        assert_eq!(calendar_state(&[ev(-120)], 5, 60), CalendarState::Free);
        // No events (unconnected calendar) -> Free, never a fabricated InMeeting.
        assert_eq!(calendar_state(&[], 5, 60), CalendarState::Free);
    }

    #[test]
    fn time_of_day_reuses_the_quiet_band_wrap_math() {
        // Night = inside the (wrap-midnight) band; shares in_quiet_hours' logic.
        assert_eq!(TimeOfDay::from_local_hour(23, 22, 7), TimeOfDay::Night);
        assert_eq!(TimeOfDay::from_local_hour(3, 22, 7), TimeOfDay::Night);
        assert_eq!(TimeOfDay::from_local_hour(12, 22, 7), TimeOfDay::Day);
        // An empty band (start == end) is never Night.
        assert_eq!(TimeOfDay::from_local_hour(3, 0, 0), TimeOfDay::Day);
    }

    #[test]
    fn auto_telemetry_states_source_reason_and_the_permission_neutral_posture() {
        let base = BaseBehavior::default();
        let choice = select_profile(FocusSignals { presence: Presence::Away, ..clear_signals() });
        let tuned = apply_profile(&choice.profile, &base);
        let v = choice.telemetry(&tuned);
        // The auto framing.
        assert_eq!(v["source"], "auto");
        assert_eq!(v["reason"], "away");
        assert_eq!(v["profile"], "deep_focus");
        // The SAME permission-neutral contract the manual card carries — auto adds
        // no capability, only two non-secret strings.
        assert_eq!(v["permission_neutral"], true);
        assert_eq!(v["raises_autonomy"], false);
        assert_eq!(v["loosens_gate"], false);
        assert_eq!(v["surfacing"], serde_json::json!(["critical"]));
    }
}
