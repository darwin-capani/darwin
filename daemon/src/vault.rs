//! VAULT MODE ("go dark") — a one-word forcing switch that keeps a turn LOCAL-ONLY.
//!
//! Vault is the MAXIMAL reduce. Where CUSTOMS (`boundary.rs`) inventories + trims
//! the cloud egress, Vault removes the cloud turn ALTOGETHER: with the vault active
//! the router refuses to escalate to the Anthropic fallback (the turn stays on the
//! local MLX brain, or honestly says it can't do this offline), and CUSTOMS is
//! forced to its strongest reduce-only trim. It is a MODE, toggled by a router op /
//! a spoken "go dark" / "vault mode on|off", with a `[vault]` config default that
//! SHIPS OFF (it changes behavior, so it opts in — never silently).
//!
//! ## The sacred invariant: RESTRICT-ONLY — vault can only REMOVE cloud, never add
//! Enforced by CONSTRUCTION, not convention, exactly like `boundary.rs`'s reduce-
//! only trim:
//!
//!   * The gate is a single PURE predicate, [`deny_cloud_with`]:
//!     `deny_cloud_with(would_go_cloud, vault_active) == would_go_cloud && !vault_active`.
//!     The result is NEVER `true` where `would_go_cloud` was `false` — so a turn
//!     under vault can never egress anything the non-vault turn wouldn't. There is
//!     no branch that could grant cloud; the code literally cannot express it.
//!
//!   * With the vault INACTIVE the gate is the IDENTITY:
//!     `deny_cloud_with(x, false) == x` — the cloud decision is byte-for-byte
//!     today's. Vault only ever tightens, and only while it is on.
//!
//! ## Two seams, one predicate
//! The router folds [`deny_cloud`] into the two places it decides local vs cloud:
//! the actuating tool-loop gate (`wants_cloud` -> `to_cloud`), so a heavy /
//! low-confidence turn never reaches the cloud tool loop; and this turn's
//! `cloud_reachable`, shadowed at `route()` entry, so the conversation / roster /
//! capability paths all resolve LOCAL. Separately `boundary::gate_and_trim` reads
//! [`active`] and forces CUSTOMS to `TrimSpec::maximal()`. All three are
//! RESTRICT-ONLY — vault can only tighten.
//!
//! ## Honesty contract (never overclaim)
//! Vault forces the LOCAL-ONLY cloud routing decision and the maximal CUSTOMS trim.
//! It does NOT claim to seal every possible byte off the box — a user-originated
//! web tool still egresses if the local turn invokes one. The spoken confirm says
//! exactly what it does (keeps the turn on the local brain, no cloud escalation,
//! CUSTOMS at maximal reduction) and no more.
//!
//! ## Nothing consequential — it only tightens
//! Toggling vault removes cloud access and strengthens the egress trim; it grants
//! nothing and takes no outward action. Like the model-swap / whisper mode toggles
//! it changes NO safety gate (the consequential confirmation gate, the owner
//! voice-id gate, lockdown, and per-action policy are all untouched).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// The mode state — a live runtime toggle + a per-turn override
// ---------------------------------------------------------------------------

/// The LIVE vault mode. Unlike `boundary::BOUNDARY_GATE` (a `OnceLock` fixed at
/// startup), vault is a runtime TOGGLE ("go dark" flips it), so it is an
/// `AtomicBool`. Defaults to `false` (OFF) until [`init`] installs the config
/// default — an uninitialized vault is INERT (the cloud decision is today's).
static VAULT_ON: AtomicBool = AtomicBool::new(false);

/// The current turn's per-turn override. `Some(on)` forces the vault on/off for a
/// SINGLE turn (a per-turn "go dark for this one" or an explicit un-vault), `None`
/// = no override, so the live [`VAULT_ON`] mode applies. Cleared at turn end by
/// [`TurnVaultGuard`] so one turn's override never silently binds a later turn.
/// Mirrors `boundary::TURN_TRIM`.
static TURN_VAULT: Mutex<Option<bool>> = Mutex::new(None);

/// Install the `[vault]` config default at startup. Called once from `main()`
/// alongside `boundary::init`. Sets the live mode to the shipped default (OFF
/// unless the operator turned it on in config). Logs nothing sensitive (a bool).
pub fn init(default_on: bool) {
    VAULT_ON.store(default_on, Ordering::SeqCst);
}

/// The RUNTIME toggle — "go dark" / "vault mode on|off" / the `vault` router op.
/// Stores the new live mode and returns it. Idempotent. Restrict-only in spirit:
/// it never takes an outward action, it only flips which cloud decisions are made.
pub fn set(on: bool) -> bool {
    VAULT_ON.store(on, Ordering::SeqCst);
    on
}

/// Whether the live vault mode is ON (ignoring any per-turn override). Falls back
/// to `false` when [`init`] was never called (an uninitialized vault is inert).
pub fn is_on() -> bool {
    VAULT_ON.load(Ordering::SeqCst)
}

/// Whether the vault is ACTIVE for the current turn: the per-turn override if one
/// is set, else the live mode. This is the single truth every seam consults
/// ([`deny_cloud`] and `boundary::gate_and_trim`).
pub fn active() -> bool {
    current_turn_vault().unwrap_or_else(is_on)
}

// ---------------------------------------------------------------------------
// deny_cloud — THE pure restrict-only gate
// ---------------------------------------------------------------------------

/// The RESTRICT-ONLY cloud gate: cloud is permitted this turn only when the caller
/// wanted it AND the vault is inactive. Consulted at the router's cloud-decision
/// seams — wrap a would-be-cloud boolean and it can only ever turn cloud OFF.
///
/// `deny_cloud(x) == x && !active()`. See [`deny_cloud_with`] for the pure form the
/// property test drives (no global read).
pub fn deny_cloud(would_go_cloud: bool) -> bool {
    deny_cloud_with(would_go_cloud, active())
}

/// PURE restrict-only gate: `would_go_cloud && !vault_active`. This is the whole of
/// vault's cloud behavior, and it is restrict-only BY CONSTRUCTION:
///
///   * the result is `true` ONLY when `would_go_cloud` was already `true`, so vault
///     NEVER enables cloud where it was off — `deny_cloud_with(false, _) == false`;
///   * with `vault_active == false` it is the IDENTITY — `deny_cloud_with(x, false)
///     == x` — so an inactive vault changes no decision;
///   * with `vault_active == true` it is the CONSTANT `false` — cloud is refused
///     regardless of the normal cloud triggers, keeping the turn local.
///
/// The reduce-only invariant `deny_cloud_with(x, v) implies x` holds for EVERY `x`
/// and `v` — proven by `property_deny_cloud_is_restrict_only`.
pub fn deny_cloud_with(would_go_cloud: bool, vault_active: bool) -> bool {
    would_go_cloud && !vault_active
}

// ---------------------------------------------------------------------------
// Per-turn override — mirrors boundary::set_turn_trim / TurnTrimGuard
// ---------------------------------------------------------------------------

/// Record the vault state THIS turn should use (a per-turn override). Poison-
/// tolerant. `None` clears the override (the live mode applies again).
///
/// `#[allow(dead_code)]`: the PER-TURN OVERRIDE seam (mirrors
/// `boundary::set_turn_trim`). The LIVE mode toggle is the primary control and is
/// fully wired; the per-turn setter + guard install in run_pipeline is the
/// integration step. Exercised by `per_turn_override_takes_precedence_and_clears`.
#[allow(dead_code)]
pub fn set_turn_vault(on: Option<bool>) {
    *TURN_VAULT.lock().unwrap_or_else(|p| p.into_inner()) = on;
}

/// The current turn's vault override — `None` when no override is set this turn.
/// Poison-tolerant.
pub fn current_turn_vault() -> Option<bool> {
    *TURN_VAULT.lock().unwrap_or_else(|p| p.into_inner())
}

/// Clear the per-turn override at turn end. Poison-tolerant. Called by
/// [`TurnVaultGuard`]. `#[allow(dead_code)]`: part of the per-turn override seam
/// (see [`set_turn_vault`]); the run_pipeline guard install is the integration step.
#[allow(dead_code)]
pub fn clear_turn_vault() {
    set_turn_vault(None);
}

/// RAII guard that CLEARS the per-turn vault override when the turn handler returns
/// by ANY path — the analogue of `boundary::TurnTrimGuard`. Install it near the top
/// of the turn handler so a per-turn override can never leak into the next turn.
///
/// `#[allow(dead_code)]`: this is the per-turn override seam (the live mode toggle
/// is the primary control and is fully wired). The run-pipeline guard install is
/// the integration step, exactly as `boundary::TurnTrimGuard` left it; the
/// mechanism is proven by `per_turn_override_takes_precedence_and_clears`.
#[allow(dead_code)]
pub struct TurnVaultGuard;
impl Drop for TurnVaultGuard {
    fn drop(&mut self) {
        clear_turn_vault();
    }
}

// ---------------------------------------------------------------------------
// Telemetry + spoken confirm
// ---------------------------------------------------------------------------

/// The `vault.status` telemetry payload — SECRET-FREE, booleans only. `active` is
/// the ground truth the HUD's VAULT indicator renders. `read_only` + `restrict_only`
/// state the contract on the wire so the HUD copy is grounded in the payload:
/// toggling vault takes no outward action and can only ever tighten egress.
pub fn status_frame(active: bool) -> Value {
    json!({
        "active": active,
        // The contract, on the wire: vault only ever REMOVES cloud + tightens the
        // egress trim. It grants nothing and takes no consequential action.
        "read_only": true,
        "restrict_only": true,
    })
}

/// The HONEST spoken confirmation for a vault toggle — says exactly what vault does
/// and no more (keeps the turn on the local brain, no cloud escalation, CUSTOMS at
/// maximal reduction). It does NOT claim to seal every byte off the box.
pub fn ack(on: bool) -> &'static str {
    if on {
        "Going dark, sir. I'll keep this on the local brain — no cloud escalation — \
         and CUSTOMS is at maximal reduction. Say \"vault mode off\" to lift it."
    } else {
        "Vault lifted, sir. Cloud routing is available again, and CUSTOMS returns to \
         its configured trim."
    }
}

// ---------------------------------------------------------------------------
// Voice command — "go dark" / "vault mode on|off" (conservatively anchored)
// ---------------------------------------------------------------------------

/// A parsed vault toggle command from a spoken utterance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VaultCommand {
    /// Engage the vault ("go dark" / "vault mode on" / "vault on" / "enter vault").
    On,
    /// Lift the vault ("vault mode off" / "vault off" / "exit vault" / "come back online").
    Off,
}

/// Normalize an utterance for anchored matching: lowercase, strip surrounding
/// whitespace + trailing sentence punctuation, and collapse internal runs of
/// whitespace to single spaces. Pure.
fn normalize(text: &str) -> String {
    let lowered = text.trim().trim_end_matches(['.', '!', '?', ',']).to_lowercase();
    lowered.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The OFF anchor phrases — checked FIRST so a "vault mode off" utterance (which
/// contains "vault mode") never reads as ON. Mirrors the whisper parser's
/// off-precedence.
const OFF_PHRASES: &[&str] = &[
    "vault mode off",
    "vault off",
    "turn off vault",
    "turn off vault mode",
    "disable vault",
    "disable vault mode",
    "exit vault",
    "exit vault mode",
    "leave vault",
    "leave vault mode",
    "end vault mode",
    "come back online",
    "go back online",
    "come out of the dark",
];

/// The ON anchor phrases.
const ON_PHRASES: &[&str] = &[
    "go dark",
    "vault mode on",
    // NOTE: the BARE "vault mode" is deliberately NOT here — matches_phrase treats a
    // phrase as a leading imperative, so "vault mode, what does it do?" would strip
    // "vault mode" and engage the vault when the user is merely ASKING about it. An
    // intentional toggle uses an explicit verb form below.
    "vault on",
    "turn on vault",
    "turn on vault mode",
    "enable vault",
    "enable vault mode",
    "enter vault",
    "enter vault mode",
    "engage vault",
    "engage vault mode",
];

/// Whether the normalized utterance IS one of `phrases` — either the whole thing or
/// its leading imperative (so "go dark now" / "vault mode on please" match, but a
/// sentence that merely mentions vault does not). Conservative by construction:
/// the phrase must be the utterance's opening imperative, anchored at the start.
fn matches_phrase(norm: &str, phrases: &[&str]) -> bool {
    phrases.iter().any(|p| {
        norm == *p
            || norm
                .strip_prefix(p)
                .is_some_and(|rest| rest.starts_with(' '))
    })
}

/// CONSERVATIVELY classify a spoken vault toggle. Anchored on the imperative phrase
/// set (an ordinary sentence that merely mentions "vault" never triggers), with OFF
/// taking precedence over ON. Returns `None` for anything that is not a clear vault
/// command. PURE — the boundary is unit-tested. Mirrors the model-swap / whisper
/// command classifiers handled BEFORE normal routing.
pub fn classify_vault_command(text: &str) -> Option<VaultCommand> {
    let norm = normalize(text);
    if norm.is_empty() {
        return None;
    }
    if matches_phrase(&norm, OFF_PHRASES) {
        return Some(VaultCommand::Off);
    }
    if matches_phrase(&norm, ON_PHRASES) {
        return Some(VaultCommand::On);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SERIALIZE the tests that mutate the process-global mode (`VAULT_ON` /
    /// `TURN_VAULT`). Rust runs tests in parallel threads that share these globals,
    /// so without a lock two global-touching tests race and stomp each other. Every
    /// such test takes this lock FIRST (before `ModeReset`), so only one runs at a
    /// time; the PURE tests (deny_cloud_with / status_frame / ack / classify) touch
    /// no global and need no lock.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Restore the global mode to the shipped-OFF baseline when a test's scope ends,
    /// so a transient toggle never leaks into the next serialized test. Declared
    /// AFTER the `ENV_LOCK` guard in each test so it drops (resets) BEFORE the lock
    /// is released.
    struct ModeReset;
    impl Drop for ModeReset {
        fn drop(&mut self) {
            set(false);
            clear_turn_vault();
        }
    }

    // =====================================================================
    // deny_cloud_with — THE restrict-only gate (PURE, no global)
    // =====================================================================

    #[test]
    fn vault_on_forces_the_cloud_decision_local_regardless_of_triggers() {
        // The normal cloud triggers — a heavy turn and a low-confidence turn — both
        // want cloud (base == true). With the vault ACTIVE the gate forces LOCAL
        // (false) regardless of which trigger fired.
        for base in [true /* heavy */, true /* low-confidence */] {
            assert!(
                !deny_cloud_with(base, true),
                "vault-on must force the cloud decision LOCAL"
            );
        }
    }

    #[test]
    fn vault_off_is_the_identity() {
        // With the vault INACTIVE the gate changes nothing — the cloud decision is
        // byte-for-byte today's, for both a cloud-bound and a local-bound base.
        assert!(deny_cloud_with(true, false), "off: a cloud turn stays cloud");
        assert!(!deny_cloud_with(false, false), "off: a local turn stays local");
    }

    #[test]
    fn property_deny_cloud_is_restrict_only() {
        // THE restrict-only invariant, machine-checked over EVERY (base, vault)
        // combination: the gate can only ever REMOVE cloud, never add it.
        for base in [false, true] {
            for vault in [false, true] {
                let out = deny_cloud_with(base, vault);
                // (1) never enables where the base was off — vault never adds cloud.
                if !base {
                    assert!(!out, "vault added cloud where the base was local");
                }
                // (2) the output implies the base: no turn under vault egresses
                //     anything the non-vault turn wouldn't.
                if out {
                    assert!(base, "gate produced cloud the base did not want");
                }
                // (3) an active vault is the constant-false (always local).
                if vault {
                    assert!(!out, "vault-on must never permit cloud");
                }
                // (4) an inactive vault is the identity.
                if !vault {
                    assert_eq!(out, base, "inactive vault must not change the decision");
                }
            }
        }
    }

    #[test]
    fn deny_cloud_never_enables_cloud_where_it_was_off() {
        // The restrict-only property, stated directly: for BOTH vault states, a
        // local base stays local. Vault can only subtract.
        assert!(!deny_cloud_with(false, false));
        assert!(!deny_cloud_with(false, true));
    }

    // =====================================================================
    // Live mode toggle + per-turn override (touch the global; reset after)
    // =====================================================================

    #[test]
    fn init_installs_the_config_default_and_ships_off() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _reset = ModeReset;
        init(false);
        assert!(!is_on(), "ships OFF");
        assert!(!active(), "inactive with no override");
        // deny_cloud is the identity while off.
        assert!(deny_cloud(true), "off: cloud stays cloud");
    }

    #[test]
    fn set_toggles_the_live_mode_and_gates_cloud() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _reset = ModeReset;
        set(true);
        assert!(is_on());
        assert!(active());
        // Live mode active => deny_cloud forces local.
        assert!(!deny_cloud(true), "vault-on forces local through the live gate");
        set(false);
        assert!(!is_on());
        assert!(deny_cloud(true), "vault-off restores the identity");
    }

    #[test]
    fn per_turn_override_takes_precedence_and_clears() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _reset = ModeReset;
        set(false); // live mode OFF
        clear_turn_vault();
        assert_eq!(current_turn_vault(), None);
        assert!(!active(), "no override + live off => inactive");
        // A per-turn override forces the vault ON for this turn, beating the live mode.
        set_turn_vault(Some(true));
        assert_eq!(current_turn_vault(), Some(true));
        assert!(active(), "per-turn override wins over the live mode");
        assert!(!deny_cloud(true), "override-on forces local");
        // The guard clears the override on drop (no leak into the next turn).
        {
            let _guard = TurnVaultGuard;
            assert!(active());
        }
        assert_eq!(current_turn_vault(), None, "TurnVaultGuard cleared the override");
        assert!(!active(), "back to the live mode (off) after the guard");
    }

    #[test]
    fn per_turn_override_can_force_off_over_a_live_on_mode() {
        // Symmetry: an override is restrict-only for the CLOUD decision (deny_cloud),
        // but the override itself can point either way — a per-turn Some(false) lifts
        // the vault for one turn even while the live mode is on.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _reset = ModeReset;
        set(true);
        assert!(active());
        set_turn_vault(Some(false));
        assert!(!active(), "override(false) lifts the vault for this turn");
        assert!(deny_cloud(true), "with the override lifting vault, cloud is permitted again");
        clear_turn_vault();
        assert!(active(), "after clearing, the live ON mode applies again");
    }

    // =====================================================================
    // Telemetry frame + spoken confirm (secret-free, honest)
    // =====================================================================

    #[test]
    fn status_frame_is_secret_free_and_states_the_contract() {
        let on = status_frame(true);
        assert_eq!(on["active"], true);
        assert_eq!(on["read_only"], true, "toggling vault takes no outward action");
        assert_eq!(on["restrict_only"], true, "vault can only tighten");
        let off = status_frame(false);
        assert_eq!(off["active"], false);
        // Booleans only — no field could carry a secret.
        assert!(on.as_object().unwrap().values().all(|v| v.is_boolean()));
    }

    #[test]
    fn ack_is_honest_about_what_vault_does_and_does_not_do() {
        let on = ack(true);
        assert!(on.contains("local"), "the ON ack names the local brain");
        assert!(on.to_lowercase().contains("no cloud"), "the ON ack names no cloud escalation");
        assert!(on.to_lowercase().contains("maximal"), "the ON ack names the maximal CUSTOMS trim");
        let off = ack(false);
        assert!(off.to_lowercase().contains("available again"), "the OFF ack says cloud is back");
    }

    // =====================================================================
    // classify_vault_command — conservatively anchored, off-precedence
    // =====================================================================

    #[test]
    fn classifies_on_phrases() {
        for p in ["go dark", "vault mode on", "vault on", "enter vault", "engage vault mode"] {
            assert_eq!(classify_vault_command(p), Some(VaultCommand::On), "{p:?} should engage");
        }
        // Case + trailing punctuation + a trailing filler word are tolerated.
        assert_eq!(classify_vault_command("Go Dark."), Some(VaultCommand::On));
        assert_eq!(classify_vault_command("go dark now"), Some(VaultCommand::On));
        assert_eq!(classify_vault_command("vault mode on please"), Some(VaultCommand::On));
    }

    #[test]
    fn classifies_off_phrases_with_precedence() {
        for p in ["vault mode off", "vault off", "exit vault", "disable vault", "come back online"] {
            assert_eq!(classify_vault_command(p), Some(VaultCommand::Off), "{p:?} should lift");
        }
        // OFF wins even though "vault mode off" contains the ON anchor "vault mode".
        assert_eq!(classify_vault_command("vault mode off"), Some(VaultCommand::Off));
    }

    #[test]
    fn does_not_trigger_on_ordinary_sentences_that_merely_mention_vault() {
        // Conservative: a sentence ABOUT vault (not the imperative command) never
        // toggles it — the phrase must be the leading imperative.
        for s in [
            "what is vault mode",
            "tell me about vault mode and how it works",
            // REGRESSION: a leading "vault mode ..." QUESTION must not engage the vault
            // (the bare "vault mode" imperative phrase was removed for exactly this).
            "vault mode what does it do",
            "vault mode is that a good idea",
            "the room went dark last night",
            "i left my documents in the vault",
            "should i go dark or stay online",
            "",
        ] {
            assert_eq!(classify_vault_command(s), None, "{s:?} must NOT toggle vault");
        }
    }
}
