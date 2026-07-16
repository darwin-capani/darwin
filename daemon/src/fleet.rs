//! OVERWATCH — server-less, end-to-end-encrypted fleet POLICY across the owner's
//! OWN Macs. A signed policy BASELINE authored on one device is enforced on every
//! device as a FLOOR OF STRICTNESS: it can force a tool stricter (Never / always-
//! Ask / a feature ceiling), and it can ONLY ever HARDEN — never loosen, never
//! grant what a device's master switch forbids, never override a stricter LOCAL
//! rule.
//!
//! ## Where it sits (BENEATH the master switch, ABOVE the local policy floor)
//!
//! The fleet baseline is folded into [`crate::policy::evaluate_global`] — the ONE
//! read path the consequential chokepoints reach — as a floor of strictness:
//!
//!   effective = combine(local_policy_decision, fleet_hardening_for(tool))
//!
//! where [`combine`] returns the STRICTER of the two (Never > Ask > Always). Two
//! load-bearing consequences, each pinned by a test:
//!   * MONOTONE-TOWARD-STRICT: the fold can only move a decision toward Never/Ask,
//!     never toward Always. So the baseline can never LOOSEN a device — a device's
//!     own stricter rule still wins, and an unruled tool is untouched.
//!   * NEVER GRANTS: a fleet rule's decision is one of {Never, Ask} BY THE TYPE —
//!     it cannot even express `Always`. So an applicable fleet rule can never
//!     resolve to `Always` (auto-approve), and the [`crate::integrations`] master
//!     switch is a SEPARATE, downstream AND — the fleet layer never touches it, so
//!     it can never grant what the master switch forbids.
//!
//! ## USER-SET / OWNER-AUTHORED ONLY — the no-model-write guarantee
//!
//! A baseline enters a device ONLY as a SIGNED, owner-authored SEALED bundle: it
//! rides the EXISTING sync sealed-bundle path ([`crate::sync::seal`] / [`open`])
//! under the shared Keychain key (`sync_shared_key`, paired only between the
//! owner's own devices). AES-256-GCM authenticated encryption IS the signature:
//! only a holder of the shared key could have produced a bundle that opens, so a
//! baseline that opens was authored by the owner on one of their own Macs. The
//! daemon NEVER writes a baseline from a model — there is no baseline-write tool,
//! and [`load_and_install`] only ever OPENS + INSTALLS a pre-sealed file. An
//! injected "set the fleet baseline" reaching the model has nothing to call.
//!
//! ## Ships OFF (with sync OFF)
//!
//! `[fleet].enabled` defaults false. Off, [`load_and_install`] is a no-op (no
//! Keychain touch, no disk read), no baseline is installed, and
//! [`harden_global`] returns its input verbatim — so `evaluate_global` is
//! byte-for-byte today. This layer only ever ADDS a floor; it never loosens.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::policy::Decision;

/// The sealed baseline wire-format version — bumped if the shape changes, so an
/// old device rejects a newer baseline honestly rather than mis-parsing it.
const BASELINE_VERSION: u32 = 1;

/// Cap on rules a baseline may carry (bounded, like `policy::MAX_RULES`). A real
/// owner baseline is a handful of ceilings; the cap only stops a corrupt/hostile
/// bundle from carrying an unbounded rule list. Excess is dropped (defense).
const MAX_FLEET_RULES: usize = 256;

/// The account the shared fleet/sync key lives under in the Keychain — the SAME
/// key the owner already pairs their devices with for `sync.rs`. Never in config,
/// never on the wire.
const SHARED_KEY_ACCOUNT: &str = "sync_shared_key";

// ---------------------------------------------------------------------------
// The hardening a fleet rule may express — Never or Ask, NEVER Always
// ---------------------------------------------------------------------------

/// The direction a fleet baseline rule may force a tool. STRUCTURALLY a hardening:
/// it can force `Never` (hard-block fleet-wide) or `Ask` (strip any local auto-
/// approve — a feature ceiling that forces the per-turn park). It CANNOT express
/// `Always` — the baseline can never grant/loosen, and that guarantee is enforced
/// by the TYPE, not by a runtime check that could be forgotten.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FleetDecision {
    /// Force this tool to `Never` fleet-wide — a hard block that wins everywhere.
    Never,
    /// Force this tool to `Ask` fleet-wide — strip any local `Always`, so it
    /// ALWAYS parks for a fresh per-action confirmation (a feature ceiling).
    Ask,
}

impl FleetDecision {
    /// The `policy::Decision` this hardening maps to. Never `Always` — the variant
    /// does not exist here, so the mapping cannot produce a loosening.
    fn as_decision(self) -> Decision {
        match self {
            FleetDecision::Never => Decision::Never,
            FleetDecision::Ask => Decision::Ask,
        }
    }

    /// Stable lowercase wire token (matches the serde rename), for the status frame.
    fn as_str(self) -> &'static str {
        match self {
            FleetDecision::Never => "never",
            FleetDecision::Ask => "ask",
        }
    }
}

// ---------------------------------------------------------------------------
// The pure floor-of-strictness combinator — provably monotone toward strict
// ---------------------------------------------------------------------------

/// Strictness rank: Never (2) is strictest, then Ask (1), then Always (0, the
/// loosest / auto-approve). The single ordering `combine` is defined over, so the
/// monotonicity proof is one comparison.
fn strictness(d: Decision) -> u8 {
    match d {
        Decision::Never => 2,
        Decision::Ask => 1,
        Decision::Always => 0,
    }
}

/// Fold a device's LOCAL policy decision with a fleet baseline hardening for the
/// SAME tool, returning the STRICTER of the two. PURE + total.
///
/// This is the floor of strictness. Because a [`FleetDecision`] is only ever
/// `Never` or `Ask` (rank >= 1) and this returns the max-rank of the two:
///   * the result is ALWAYS at least as strict as `local` — the fold can never
///     loosen a device below its own decision (a stricter LOCAL rule wins);
///   * the result is ALWAYS at least as strict as the fleet rule — the baseline
///     floor is enforced;
///   * the result is NEVER `Always` when a fleet rule applies — an applicable
///     baseline always strips auto-approve, so it can never grant an action.
///
/// All three are swept exhaustively over every `Decision` x `FleetDecision` pair
/// in the tests.
pub fn combine(local: Decision, fleet: FleetDecision) -> Decision {
    let fleet = fleet.as_decision();
    if strictness(fleet) > strictness(local) {
        fleet
    } else {
        local
    }
}

// ---------------------------------------------------------------------------
// The signed baseline + its rules
// ---------------------------------------------------------------------------

/// One per-tool ceiling: force `tool` to `decision` (Never / Ask) fleet-wide.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetRule {
    /// The consequential tool this ceiling applies to (an exact tool id, e.g.
    /// "gmail_send", "x_post", or an MCP flat id "mcp__server__tool").
    pub tool: String,
    /// The hardening to force — Never or Ask. Never Always (the type forbids it).
    pub decision: FleetDecision,
}

/// A signed policy BASELINE authored on one of the owner's devices. Sealed through
/// the sync path under the shared Keychain key; a baseline that opens is proven
/// owner-authored (only a shared-key holder could seal an openable one).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetBaseline {
    /// Wire-format version — an unknown version is rejected, never mis-parsed.
    pub version: u32,
    /// The AUTHORING device's stable id (its `meta.device_id`) — surfaced so the
    /// HUD shows WHICH device set the active baseline.
    pub device_id: String,
    /// When the baseline was authored (RFC3339).
    pub created: String,
    /// The per-tool ceilings. Bounded at [`MAX_FLEET_RULES`].
    pub rules: Vec<FleetRule>,
}

impl FleetBaseline {
    /// The fleet hardening for `tool`, if this baseline names it. The last matching
    /// rule wins (a well-formed owner baseline holds at most one rule per tool);
    /// `None` means the baseline says nothing about this tool.
    fn hardening_for(&self, tool: &str) -> Option<FleetDecision> {
        self.rules
            .iter()
            .rev()
            .find(|r| r.tool == tool)
            .map(|r| r.decision)
    }

    /// Apply this baseline as a floor over a device's LOCAL decision for `tool`:
    /// the stricter of the local decision and any fleet ceiling for the tool. A
    /// tool the baseline doesn't name is returned unchanged.
    pub fn harden(&self, local: Decision, tool: &str) -> Decision {
        match self.hardening_for(tool) {
            Some(fleet) => combine(local, fleet),
            None => local,
        }
    }
}

/// Serialize a baseline to JSON bytes (the plaintext fed to the sealer). The
/// OWNER-AUTHORING seam: only the sealing path + the round-trip tests build a
/// baseline; the daemon itself never authors one (no-model-write), so this is
/// exercised by tests + a future owner command-channel author action — the same
/// "test seam / cross-component contract" rationale `crypto::from_bytes` carries.
#[allow(dead_code)]
pub fn serialize_baseline(baseline: &FleetBaseline) -> anyhow::Result<Vec<u8>> {
    Ok(serde_json::to_vec(baseline)?)
}

/// Parse a decrypted baseline, rejecting an unknown wire version honestly and
/// bounding the rule count (defense in depth against a corrupt/hostile bundle).
pub fn deserialize_baseline(bytes: &[u8]) -> anyhow::Result<FleetBaseline> {
    let mut baseline: FleetBaseline = serde_json::from_slice(bytes)?;
    if baseline.version != BASELINE_VERSION {
        anyhow::bail!(
            "fleet baseline version {} is not supported (this device speaks {BASELINE_VERSION})",
            baseline.version
        );
    }
    if baseline.rules.len() > MAX_FLEET_RULES {
        warn!(
            rules = baseline.rules.len(),
            cap = MAX_FLEET_RULES,
            "fleet: baseline exceeds the rule cap; dropping the excess (bounded)"
        );
        baseline.rules.truncate(MAX_FLEET_RULES);
    }
    Ok(baseline)
}

/// Seal a baseline under the shared key, riding the sync path's AES-256-GCM sealer.
/// The OWNER-AUTHORING leg (produces a bundle only a shared-key holder can open).
/// Exercised by the round-trip tests + a future owner author path; the daemon
/// never authors a baseline from a model, so it is `dead_code`-allowed like its
/// `serialize_baseline` sibling above.
#[allow(dead_code)]
pub fn seal_baseline(key: &[u8; 32], baseline: &FleetBaseline) -> anyhow::Result<Vec<u8>> {
    crate::sync::seal(key, &serialize_baseline(baseline)?)
}

/// Open + parse a sealed baseline under the shared key. FAILS (never returns a
/// half-trusted baseline) on a wrong key, a tamper, a truncation, or a bad version.
pub fn open_baseline(key: &[u8; 32], sealed: &[u8]) -> anyhow::Result<FleetBaseline> {
    deserialize_baseline(&crate::sync::open(key, sealed)?)
}

// ---------------------------------------------------------------------------
// The process-global ACTIVE baseline (owner-authored, install-once at startup)
// ---------------------------------------------------------------------------

use std::sync::OnceLock;

/// The installed active baseline. `None` until [`install`] runs at startup (and it
/// only runs when `[fleet].enabled` AND a sealed baseline actually opened). An
/// uninstalled global => no floor => [`harden_global`] is a no-op, so the shipped-
/// safe posture holds. Install-once, like `policy::install` / `lockdown` — a new
/// owner baseline takes effect on the next daemon start (deliberate, not silent).
static ACTIVE: OnceLock<FleetBaseline> = OnceLock::new();

/// Install the opened, owner-authored baseline as the process-global floor. Called
/// once from startup after [`load_and_install`] opens it. Idempotent.
pub fn install(baseline: FleetBaseline) {
    info!(
        author = %baseline.device_id,
        rules = baseline.rules.len(),
        "fleet: installed the owner-authored policy baseline (floor of strictness)"
    );
    let _ = ACTIVE.set(baseline);
}

// `#[cfg(test)]` override seam (mirrors `policy::POLICY_OVERRIDE` /
// `lockdown::LOCKDOWN_TL`): a test forces a baseline on its OWN thread so the
// evaluate-fold is exercisable WITHOUT poisoning the set-once `ACTIVE` global that
// other tests rely on being empty. Production compiles this out and reads `ACTIVE`.
#[cfg(test)]
thread_local! {
    static FLEET_OVERRIDE: std::cell::RefCell<Option<FleetBaseline>> =
        const { std::cell::RefCell::new(None) };
}

/// Apply the active fleet floor to a device's local decision for `tool`. This is
/// the ONE seam `policy::evaluate_global` folds in. With no active baseline (OFF,
/// the shipped default, or an unopened/absent bundle) it returns `local` verbatim
/// — so the fold is byte-for-byte a no-op when the layer is off. READ-ONLY.
pub fn harden_global(local: Decision, tool: &str) -> Decision {
    #[cfg(test)]
    {
        if let Some(d) =
            FLEET_OVERRIDE.with(|c| c.borrow().as_ref().map(|b| b.harden(local, tool)))
        {
            return d;
        }
    }
    match ACTIVE.get() {
        Some(baseline) => baseline.harden(local, tool),
        None => local,
    }
}

/// A read-only snapshot of the installed baseline (clone), honoring the test seam.
/// `None` when no baseline is active. Used by the status emit; never mutates.
fn active_snapshot() -> Option<FleetBaseline> {
    #[cfg(test)]
    {
        if let Some(b) = FLEET_OVERRIDE.with(|c| c.borrow().clone()) {
            return Some(b);
        }
    }
    ACTIVE.get().cloned()
}

/// `#[cfg(test)]`-only RAII guard that forces the active baseline on this thread
/// until drop, restoring the prior state so the override never leaks into a
/// parallel test. Mirrors `policy::PolicyOverride`.
#[cfg(test)]
pub(crate) struct FleetOverride {
    prev: Option<FleetBaseline>,
}

#[cfg(test)]
impl FleetOverride {
    /// Force the active baseline to `baseline` on this thread until the guard drops.
    pub(crate) fn force(baseline: FleetBaseline) -> Self {
        let prev = FLEET_OVERRIDE.with(|c| c.borrow_mut().replace(baseline));
        Self { prev }
    }
}

#[cfg(test)]
impl Drop for FleetOverride {
    fn drop(&mut self) {
        FLEET_OVERRIDE.with(|c| *c.borrow_mut() = self.prev.take());
    }
}

// ---------------------------------------------------------------------------
// Startup load — OPEN + INSTALL an owner-authored sealed baseline (no model write)
// ---------------------------------------------------------------------------

/// The fleet state tree under the daemon-owned, gitignored `state/`.
fn fleet_root(root: &std::path::Path) -> std::path::PathBuf {
    root.join("state").join("fleet")
}

/// The sealed baseline path. The owner drops/syncs a sealed baseline here (via the
/// sync transport or by hand); the daemon only ever READS it.
fn baseline_path(root: &std::path::Path) -> std::path::PathBuf {
    fleet_root(root).join("baseline.sealed")
}

/// Load the owner-authored sealed baseline and install it as the active floor.
/// Runs ONCE at startup. NO-MODEL-WRITE: this only OPENS + INSTALLS a pre-sealed
/// file — it never authors a baseline. Fail-SAFE and honest:
///   * `[fleet].enabled` false => no-op (no Keychain touch, no disk read);
///   * no shared key => no baseline (unpaired; the floor stays empty);
///   * no sealed file => no baseline (armed, awaiting the owner's bundle);
///   * a bundle that won't open => SKIPPED (wrong key / tampered / bad version),
///     never a half-trusted install.
///
/// Returns whether a baseline was installed (for the startup log/telemetry).
pub async fn load_and_install(cfg: &crate::config::Config, root: &std::path::Path) -> bool {
    if !cfg.fleet.enabled {
        return false;
    }
    let path = baseline_path(root);
    let Ok(sealed) = std::fs::read(&path) else {
        info!("fleet: enabled, no sealed baseline present yet — armed, awaiting the owner's bundle");
        return false;
    };
    let Some(key) = crate::integrations::resolve_secret(SHARED_KEY_ACCOUNT)
        .await
        .and_then(|hex| crate::crypto::SecretKey::from_hex(hex.trim()).ok())
    else {
        warn!("fleet: a sealed baseline is present but no shared key — not paired; the floor stays empty");
        return false;
    };
    match open_baseline(key.raw_bytes(), &sealed) {
        Ok(baseline) => {
            install(baseline);
            true
        }
        Err(e) => {
            // Wrong key / tampered / bad version: refuse, never install a
            // half-trusted baseline (mirrors sync's inbox skip).
            warn!(error = %e, "fleet: sealed baseline failed to open — NOT installed (the floor stays empty)");
            false
        }
    }
}

// ---------------------------------------------------------------------------
// The honest status surface
// ---------------------------------------------------------------------------

/// The `fleet.status` wire payload. PURE + total. SECRET-FREE: the enable bool,
/// whether a baseline is active, the AUTHORING device id (a device name, not a
/// secret — surfaced so the HUD shows who set it), when it was authored, and the
/// per-tool ceilings (tool ids + Never/Ask). It NEVER carries a fact value or the
/// shared key. `transport_inert` mirrors sync's honesty: moving a baseline between
/// devices rides the same armed-but-inert transport.
pub fn status_payload(enabled: bool, baseline: Option<&FleetBaseline>) -> Value {
    let rules: Vec<Value> = baseline
        .map(|b| {
            b.rules
                .iter()
                .map(|r| json!({ "tool": r.tool, "decision": r.decision.as_str() }))
                .collect()
        })
        .unwrap_or_default();
    json!({
        "enabled": enabled,
        "baseline_active": baseline.is_some(),
        "authored_by": baseline.map(|b| b.device_id.as_str()).unwrap_or(""),
        "created": baseline.map(|b| b.created.as_str()).unwrap_or(""),
        "rule_count": rules.len(),
        "rules": rules,
        // The baseline can only HARDEN — the HUD copy leans on this honest pin.
        "hardens_only": true,
        "transport_inert": true,
    })
}

/// Emit `fleet.status` for the HUD on the audit-snapshot cadence. READ-ONLY and
/// CHEAP: it reports the ALREADY-INSTALLED baseline (opened once at startup), so it
/// never touches the Keychain or opens a file on the tick. OFF (the shipped
/// default) emits the honest off payload. Fail-open.
pub async fn emit_status(cfg: &crate::config::Config, _root: &std::path::Path) {
    let baseline = if cfg.fleet.enabled { active_snapshot() } else { None };
    crate::telemetry::emit(
        "system",
        "fleet.status",
        status_payload(cfg.fleet.enabled, baseline.as_ref()),
    );
}

// ---------------------------------------------------------------------------
// Tests — combine floor exhaustively (can force stricter, can NEVER loosen); the
// seal/open round-trip via the sync path (injected key); a fleet rule cannot
// override a device master-switch-off; OFF is a no-op; the evaluate_global fold.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> crate::crypto::SecretKey {
        crate::crypto::SecretKey::from_bytes([7u8; 32])
    }

    fn rule(tool: &str, decision: FleetDecision) -> FleetRule {
        FleetRule { tool: tool.into(), decision }
    }

    fn baseline(device: &str, rules: Vec<FleetRule>) -> FleetBaseline {
        FleetBaseline {
            version: BASELINE_VERSION,
            device_id: device.into(),
            created: "2026-07-15T10:00:00Z".into(),
            rules,
        }
    }

    const ALL_DECISIONS: [Decision; 3] = [Decision::Never, Decision::Ask, Decision::Always];
    const ALL_FLEET: [FleetDecision; 2] = [FleetDecision::Never, FleetDecision::Ask];

    // -- combine: the floor of strictness (sweep every pair) -------------------

    #[test]
    fn combine_returns_the_stricter_and_can_force_stricter() {
        // Never floor forces Never over anything.
        assert_eq!(combine(Decision::Always, FleetDecision::Never), Decision::Never);
        assert_eq!(combine(Decision::Ask, FleetDecision::Never), Decision::Never);
        assert_eq!(combine(Decision::Never, FleetDecision::Never), Decision::Never);
        // Ask floor strips a local Always (a feature ceiling) but keeps a stricter local.
        assert_eq!(combine(Decision::Always, FleetDecision::Ask), Decision::Ask, "Ask ceiling strips auto-approve");
        assert_eq!(combine(Decision::Ask, FleetDecision::Ask), Decision::Ask);
        assert_eq!(combine(Decision::Never, FleetDecision::Ask), Decision::Never, "a stricter LOCAL Never wins");
    }

    #[test]
    fn combine_is_monotone_toward_strict_and_can_never_loosen() {
        // The load-bearing invariant, swept over EVERY Decision x FleetDecision:
        for local in ALL_DECISIONS {
            for fleet in ALL_FLEET {
                let out = combine(local, fleet);
                // (1) never looser than the LOCAL decision — the fold can never
                //     loosen a device below its own rule.
                assert!(
                    strictness(out) >= strictness(local),
                    "combine loosened below local: local={local:?} fleet={fleet:?} -> {out:?}"
                );
                // (2) at least as strict as the FLEET floor — the ceiling is enforced.
                assert!(
                    strictness(out) >= strictness(fleet.as_decision()),
                    "combine fell below the fleet floor: local={local:?} fleet={fleet:?} -> {out:?}"
                );
                // (3) it is exactly the stricter of the two.
                let want = if strictness(fleet.as_decision()) > strictness(local) {
                    fleet.as_decision()
                } else {
                    local
                };
                assert_eq!(out, want, "combine must be the stricter of local and fleet");
            }
        }
    }

    #[test]
    fn an_applicable_fleet_rule_can_never_resolve_to_always() {
        // A FleetDecision cannot even express Always (no such variant), and every
        // fleet rank is >= Ask, so an applicable rule NEVER yields Always
        // (auto-approve). This is why a fleet rule can never GRANT an action.
        for local in ALL_DECISIONS {
            for fleet in ALL_FLEET {
                assert_ne!(
                    combine(local, fleet),
                    Decision::Always,
                    "an applicable fleet rule must never resolve to Always: local={local:?} fleet={fleet:?}"
                );
            }
        }
    }

    // -- a fleet rule cannot override a device master-switch-off ---------------

    #[test]
    fn a_fleet_rule_cannot_override_a_device_master_switch_off() {
        // The fleet floor lives ENTIRELY on the harden side (evaluate_global); it
        // never touches the [integrations] master switch, which is a separate
        // downstream AND. Prove both halves:
        //   (a) no fleet rule resolves to Always (grant) — swept above; and
        //   (b) with the master switch forced OFF, the gate is DryRun even on a
        //       fresh confirm, regardless of any baseline.
        let b = baseline("mac-studio", vec![rule("gmail_send", FleetDecision::Ask)]);
        // Even hardening a locally-Always tool only ever parks it — never grants.
        assert_eq!(b.harden(Decision::Always, "gmail_send"), Decision::Ask);
        let _master_off = crate::integrations::ConsequentialOverride::force(false);
        assert_eq!(
            crate::integrations::gate(true),
            crate::integrations::ActionMode::DryRun,
            "master off => DryRun regardless of the fleet baseline or a fresh confirm"
        );
    }

    // -- harden: matched tightens, unmatched untouched -------------------------

    #[test]
    fn harden_tightens_a_named_tool_and_leaves_others_untouched() {
        let b = baseline(
            "mac-studio",
            vec![rule("gmail_send", FleetDecision::Ask), rule("x_post", FleetDecision::Never)],
        );
        // Named tools are hardened to the stricter.
        assert_eq!(b.harden(Decision::Always, "gmail_send"), Decision::Ask);
        assert_eq!(b.harden(Decision::Always, "x_post"), Decision::Never);
        assert_eq!(b.harden(Decision::Ask, "gmail_send"), Decision::Ask);
        // A locally-stricter rule for a named tool still wins.
        assert_eq!(b.harden(Decision::Never, "gmail_send"), Decision::Never);
        // A tool the baseline doesn't name is returned verbatim.
        assert_eq!(b.harden(Decision::Always, "slack_post_message"), Decision::Always);
        assert_eq!(b.harden(Decision::Ask, "slack_post_message"), Decision::Ask);
    }

    // -- seal/open round-trip via the sync path (injected key) -----------------

    #[test]
    fn seal_open_round_trips_via_the_sync_path_and_rejects_tamper_and_wrong_key() {
        let key = *test_key().raw_bytes();
        let b = baseline("mac-studio", vec![rule("gmail_send", FleetDecision::Never)]);
        let sealed = seal_baseline(&key, &b).unwrap();
        // The plaintext (tool name) never appears on the wire — it's sealed.
        assert!(!sealed.windows(10).any(|w| w == b"gmail_send"), "baseline is sealed, no plaintext");
        // Round-trips under the right key.
        assert_eq!(open_baseline(&key, &sealed).unwrap(), b);

        // A single flipped byte fails authentication (the GCM tag = the signature).
        let mut tampered = sealed.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        assert!(open_baseline(&key, &tampered).is_err(), "tamper is caught");
        // The wrong key fails — a non-owner cannot forge an openable baseline.
        assert!(open_baseline(&[9u8; 32], &sealed).is_err(), "wrong key fails");
    }

    #[test]
    fn baseline_serialize_round_trips_bounds_rules_and_rejects_a_wrong_version() {
        let b = baseline("mac-studio", vec![rule("gmail_send", FleetDecision::Ask)]);
        let bytes = serialize_baseline(&b).unwrap();
        assert_eq!(deserialize_baseline(&bytes).unwrap(), b);

        // A future/old version is refused, not mis-parsed.
        let mut bad = b.clone();
        bad.version = 999;
        assert!(deserialize_baseline(&serialize_baseline(&bad).unwrap()).is_err());

        // The rule count is bounded on the way in (defense against a hostile bundle).
        let flood = baseline(
            "mac-studio",
            (0..(MAX_FLEET_RULES + 10)).map(|i| rule(&format!("t{i}"), FleetDecision::Never)).collect(),
        );
        let parsed = deserialize_baseline(&serialize_baseline(&flood).unwrap()).unwrap();
        assert_eq!(parsed.rules.len(), MAX_FLEET_RULES, "the rule list is capped");
    }

    // -- status: honest off + the active baseline ------------------------------

    #[test]
    fn status_is_honest_off_and_reports_the_active_baseline() {
        // OFF: no baseline, no rules, secret-free.
        let off = status_payload(false, None);
        assert_eq!(off["enabled"], false);
        assert_eq!(off["baseline_active"], false);
        assert_eq!(off["authored_by"], "");
        assert_eq!(off["rule_count"], 0);
        assert_eq!(off["rules"], json!([]));
        assert_eq!(off["hardens_only"], true);

        // ACTIVE: the authoring device + the ceilings are surfaced (tool + never/ask).
        let b = baseline("mac-studio", vec![rule("gmail_send", FleetDecision::Ask), rule("x_post", FleetDecision::Never)]);
        let on = status_payload(true, Some(&b));
        assert_eq!(on["enabled"], true);
        assert_eq!(on["baseline_active"], true);
        assert_eq!(on["authored_by"], "mac-studio");
        assert_eq!(on["created"], "2026-07-15T10:00:00Z");
        assert_eq!(on["rule_count"], 2);
        assert_eq!(on["rules"][0]["tool"], "gmail_send");
        assert_eq!(on["rules"][0]["decision"], "ask");
        assert_eq!(on["rules"][1]["tool"], "x_post");
        assert_eq!(on["rules"][1]["decision"], "never");
        // The status carries no key material.
        assert!(!on.to_string().contains("0707"), "no key bytes on the wire");
    }

    // -- OFF is a no-op (no Keychain touch, no disk read, no install) ----------

    #[tokio::test]
    async fn load_is_a_no_op_when_off() {
        struct TempDir(std::path::PathBuf);
        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let dir = std::env::temp_dir().join(format!("darwin-fleet-off-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _guard = TempDir(dir.clone());

        // OFF (the shipped default): load_and_install is a no-op and never touches
        // the state/fleet tree (no Keychain probe, no file read/write).
        let cfg = crate::config::Config::default();
        assert!(!cfg.fleet.enabled, "ships OFF");
        let installed = load_and_install(&cfg, &dir).await;
        assert!(!installed, "off installs nothing");
        assert!(!fleet_root(&dir).exists(), "off never touches the fleet state tree");

        // And with no baseline active, harden_global is byte-for-byte a no-op.
        for local in ALL_DECISIONS {
            assert_eq!(harden_global(local, "gmail_send"), local, "off: harden is a no-op");
        }
    }

    // -- the fold into evaluate_global: only tightens, only for named tools ----

    #[test]
    fn the_fleet_floor_folds_into_evaluate_global_and_only_tightens() {
        // A LOCAL policy that AUTO-APPROVES gmail_send (and x_post).
        let mut store = crate::policy::PolicyStore::empty();
        store.set(crate::policy::PolicyScope::tool("gmail_send"), Decision::Always);
        store.set(crate::policy::PolicyScope::tool("x_post"), Decision::Always);
        let _p = crate::policy::PolicyOverride::force(true, store);

        // Without a fleet baseline the local Always shows through.
        assert_eq!(crate::policy::evaluate_global("gmail_send", "agent.pepper", ""), Decision::Always);

        // Install a baseline that tightens gmail_send to Ask (a feature ceiling).
        let b = baseline("mac-studio", vec![rule("gmail_send", FleetDecision::Ask)]);
        let _f = FleetOverride::force(b);

        // The fleet FLOOR strips the auto-approve for the named tool — it parks again.
        assert_eq!(
            crate::policy::evaluate_global("gmail_send", "agent.pepper", ""),
            Decision::Ask,
            "the fleet floor tightens the named tool"
        );
        // A tool the baseline does NOT name is untouched — still the local Always.
        assert_eq!(
            crate::policy::evaluate_global("x_post", "agent.veronica", ""),
            Decision::Always,
            "an unnamed tool is untouched — the fleet floor never loosens or broadens"
        );
    }

    #[test]
    fn a_never_ceiling_folds_to_a_hard_block_through_evaluate_global() {
        // Even a local Always is hard-blocked when the fleet baseline forces Never.
        let mut store = crate::policy::PolicyStore::empty();
        store.set(crate::policy::PolicyScope::tool("gmail_send"), Decision::Always);
        let _p = crate::policy::PolicyOverride::force(true, store);
        let b = baseline("mac-studio", vec![rule("gmail_send", FleetDecision::Never)]);
        let _f = FleetOverride::force(b);
        assert_eq!(
            crate::policy::evaluate_global("gmail_send", "agent.pepper", ""),
            Decision::Never,
            "a fleet Never ceiling hard-blocks fleet-wide"
        );
    }
}
