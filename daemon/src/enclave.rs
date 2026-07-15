//! ENCLAVE CUSTODY (`[enclave]`) — hardware-bound custody of the at-rest DB
//! master key, ADDITIVE over the existing macOS Keychain path (crypto.rs).
//!
//! ## What this adds (and what it does NOT change)
//!
//! crypto.rs already custodies the 256-bit at-rest master key in the macOS
//! Keychain (account `memory_encryption_key`, service `com.darwin.daemon`, read
//! via the argv-free `security(1)` seam). That Keychain item is OS-protected, but
//! its protecting material is, in principle, exportable by a sufficiently
//! privileged local attacker. WHERE THE SECURE ENCLAVE IS AVAILABLE, this module
//! wraps that master key with a **non-exportable, hardware-bound** Secure-Enclave
//! key (`kSecAttrTokenIDSecureEnclave`): the protecting key is minted inside the
//! SE, never leaves the chip, and cannot be exfiltrated even by root. That is a
//! STRICT SUPERSET of today's custody — protect-with-SE-when-present, else exactly
//! today's Keychain behavior.
//!
//! This module is custody-only. It NEVER changes which `SecretKey` startup
//! resolves/installs (`crypto::resolve`/`install_master_key` are untouched), so
//! the key-resolution contract and per-agent credential isolation (the
//! `integrations` allowlist) are byte-for-byte preserved. It only decides HOW the
//! already-resolved master key is protected at rest, and reports that honestly.
//!
//! ## ARMED-BUT-INERT (honest availability)
//!
//! `[enclave].enabled` SHIPS ON (armed by default). But minting an SE-bound key
//! needs specific hardware (an Apple Secure Enclave) AND a code-signed host
//! carrying the keychain-access-group / SE entitlement. An unentitled daemon
//! build CANNOT create or use an SE key, so [`probe_availability`] HONESTLY
//! reports unavailable and custody FALLS BACK to the unchanged Keychain path —
//! never a fabricated "enclave-protected" claim. This mirrors the daemon's other
//! device-gated subsystems (endpoint-security, TCC-gated capture): the switch is
//! ON, the subsystem is inert without its dependency, and it says so.
//!
//! ## Testable seam vs. device-gated runner
//!
//! The availability->custody-DECISION seam ([`custody_decision`]) and the
//! secret-free [`status_frame`] are PURE and fully unit-tested. The actual SE key
//! operation ([`enclave_protect_runner`]) is the device-gated runner: it runs only
//! where the SE is genuinely reachable, and its result is what makes
//! [`resolve_custody`] claim `EnclaveProtected` — we never claim protection the
//! runner did not actually deliver.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

/// The Keychain application-label the Secure-Enclave-bound wrapping key is minted
/// under. `com.darwin.*` namespaced, exactly like the `com.darwin.daemon` service
/// the rest of the daemon's Keychain items use.
///
/// NOTE: this is NOT a generic-password account and is deliberately NOT added to
/// `integrations::ALLOWED_ACCOUNTS`. The SE key is a `kSecClassKey` token-backed
/// item addressed by this label — a DIFFERENT Keychain class than the
/// generic-password secrets `integrations::resolve_secret` reads — so it never
/// travels the generic-password allowlist. It is a PUBLIC identifier (a label,
/// never key material) and is safe to log / put in the status frame.
pub const ENCLAVE_KEY_LABEL: &str = "com.darwin.enclave.master-wrap";

/// Whether a usable Secure Enclave (hardware + entitlement + signed host) is
/// actually reachable this run. `Unavailable` always carries an HONEST reason —
/// never a bare "off" — so the status frame and self-check can explain the SKIP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    /// A usable SE-bound key operation is reachable.
    Available,
    /// No usable SE this run; the string is the honest cause (wrong hardware, no
    /// entitlement, disabled by config, or a failed key op).
    Unavailable(String),
}

/// The custody decision for the at-rest master key: bind it to the Secure Enclave,
/// or use the existing OS-protected Keychain path unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustodyMode {
    /// The master key is wrapped by a non-exportable, hardware-bound SE key.
    EnclaveProtected,
    /// Today's behavior: the master key is custodied by the macOS Keychain alone.
    /// The fail-safe fallback whenever the SE is not genuinely usable.
    KeychainFallback,
}

/// The resolved custody for this run: the chosen [`CustodyMode`] plus the
/// [`Availability`] that produced it (so status/self-check can explain it).
#[derive(Debug, Clone)]
pub struct Custody {
    pub mode: CustodyMode,
    pub availability: Availability,
}

// ---------------------------------------------------------------------------
// Pure seam: availability -> custody decision (unit-tested, no device)
// ---------------------------------------------------------------------------

/// PURE: map an [`Availability`] to a [`CustodyMode`]. SE present ->
/// enclave-protect; absent -> honest fallback to the Keychain. This is the whole
/// fallback-decision contract, isolated from any device op so it is exhaustively
/// unit-testable.
pub fn custody_decision(availability: &Availability) -> CustodyMode {
    match availability {
        Availability::Available => CustodyMode::EnclaveProtected,
        Availability::Unavailable(_) => CustodyMode::KeychainFallback,
    }
}

// ---------------------------------------------------------------------------
// Device-gated probe + runner
// ---------------------------------------------------------------------------

/// Probe whether SE-bound custody is usable this run. Honors `[enclave].enabled`
/// first (a disabled section is an honest `Unavailable`, not a fabricated
/// available), then defers to the device-gated [`detect_secure_enclave`].
pub fn probe_availability(enabled: bool) -> Availability {
    if !enabled {
        return Availability::Unavailable(
            "[enclave].enabled = false — the operator turned enclave custody off".to_string(),
        );
    }
    detect_secure_enclave()
}

/// The device-gated Secure-Enclave detection. HONEST by construction: it never
/// claims `Available` unless a usable SE key operation is genuinely reachable.
/// This daemon build ships without the SE entitlement / code-signing required to
/// mint a non-exportable SE key, so on every real target it reports the specific
/// reason and custody falls back to the unchanged Keychain path.
fn detect_secure_enclave() -> Availability {
    // TEST seam: a test can simulate an entitled device (or a forced-absent one)
    // to drive both branches of the decision + status pipeline without hardware.
    // Compiled out entirely in non-test builds.
    #[cfg(test)]
    {
        if let Some(force) = TEST_AVAILABILITY.with(std::cell::Cell::get) {
            return if force {
                Availability::Available
            } else {
                Availability::Unavailable("test: forced unavailable".to_string())
            };
        }
    }
    real_detect_secure_enclave()
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn real_detect_secure_enclave() -> Availability {
    // Apple Silicon HAS a Secure Enclave, but minting a NON-EXPORTABLE SE-bound key
    // (kSecAttrTokenIDSecureEnclave, kSecAccessControlPrivateKeyUsage) requires the
    // process to be code-signed with the keychain-access-group / SE entitlement. An
    // unentitled daemon build cannot create or use an SE key, so we report the exact
    // cause and fall back to the OS-protected Keychain — never a fabricated claim.
    Availability::Unavailable(
        "Secure Enclave present, but this daemon build is not code-signed with the \
         entitlement required to mint a non-exportable SE-bound key; custody falls \
         back to the OS-protected Keychain"
            .to_string(),
    )
}

#[cfg(all(target_os = "macos", not(target_arch = "aarch64")))]
fn real_detect_secure_enclave() -> Availability {
    // Intel Macs have a Secure Enclave only with a T2 security chip, and the same
    // entitlement/signing gate applies. Report unavailable honestly.
    Availability::Unavailable(
        "no guaranteed Secure Enclave on this Mac (T2-only), and the SE entitlement \
         is absent; custody falls back to the OS-protected Keychain"
            .to_string(),
    )
}

#[cfg(not(target_os = "macos"))]
fn real_detect_secure_enclave() -> Availability {
    Availability::Unavailable(
        "the Secure Enclave is an Apple-platform feature; unavailable on this OS — \
         custody falls back to the OS-protected Keychain"
            .to_string(),
    )
}

/// The DEVICE-GATED runner: actually bind the master key to the Secure Enclave.
///
/// On a genuinely-entitled device the real op would (1) look up or mint a
/// non-exportable SE-bound P-256 key at [`ENCLAVE_KEY_LABEL`]
/// (`kSecAttrTokenIDSecureEnclave` + `kSecAccessControlPrivateKeyUsage`),
/// (2) ECIES-wrap the 32 raw master-key bytes with the SE public key, and
/// (3) persist the wrapped blob — the SE private key never leaving the chip. That
/// requires the SE entitlement + a code-signed host this build does not carry, so
/// here it is honestly inert (returns `Err`), which makes [`resolve_custody`] fall
/// back rather than claim a protection it did not deliver.
fn enclave_protect_runner(key: &crate::crypto::SecretKey) -> Result<()> {
    // TEST seam: simulate the entitled-device key op (success or failure) so the
    // positive + honesty-downgrade paths are exercised without hardware.
    #[cfg(test)]
    {
        if let Some(ok) = TEST_RUNNER_OK.with(std::cell::Cell::get) {
            return if ok {
                Ok(())
            } else {
                Err(anyhow!("test: forced SE key-op failure"))
            };
        }
    }
    // The key is borrowed only so the real op can wrap its raw bytes; the inert
    // build never reads them, and they are NEVER logged.
    let _ = key;
    Err(anyhow!(
        "Secure-Enclave key operation requires the SE entitlement + a code-signed host; \
         inert in this build"
    ))
}

// ---------------------------------------------------------------------------
// Integration seam: resolve custody for this run
// ---------------------------------------------------------------------------

/// Resolve the master-key custody for this run — the seam `main.rs` calls right
/// after the at-rest master key is resolved. ADDITIVE and fail-safe:
///
///   * SE not usable (the shipped posture) -> [`CustodyMode::KeychainFallback`]:
///     today's Keychain custody, byte-for-byte, with an honest reason.
///   * `[enclave].enabled` but at-rest encryption OFF (`key` is `None`) -> there is
///     no master key to bind, so KeychainFallback with an honest reason (not a
///     failure).
///   * SE usable AND the device-gated runner actually succeeds ->
///     [`CustodyMode::EnclaveProtected`]. We claim protection ONLY on a real
///     success; a probe-says-yes-but-op-fails case downgrades honestly to fallback.
///
/// This NEVER mutates the resolved key or its Keychain custody — it only adds an
/// SE wrap over it when genuinely available.
pub fn resolve_custody(enabled: bool, key: Option<&crate::crypto::SecretKey>) -> Custody {
    let availability = probe_availability(enabled);
    match custody_decision(&availability) {
        CustodyMode::KeychainFallback => Custody {
            mode: CustodyMode::KeychainFallback,
            availability,
        },
        CustodyMode::EnclaveProtected => {
            // Nothing to bind if at-rest encryption is OFF (no master key exists).
            let Some(key) = key else {
                return Custody {
                    mode: CustodyMode::KeychainFallback,
                    availability: Availability::Unavailable(
                        "Secure Enclave available, but at-rest encryption is OFF \
                         ([security].encrypt_memory=false) — no master key to bind"
                            .to_string(),
                    ),
                };
            };
            match enclave_protect_runner(key) {
                Ok(()) => Custody {
                    mode: CustodyMode::EnclaveProtected,
                    availability: Availability::Available,
                },
                // Probe said yes but the op failed: fall back honestly — never fake
                // "protected".
                Err(e) => Custody {
                    mode: CustodyMode::KeychainFallback,
                    availability: Availability::Unavailable(format!(
                        "Secure Enclave probe succeeded but the key operation failed \
                         ({e}); using the OS-protected Keychain"
                    )),
                },
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Honest reporting: telemetry status frame + self-check leg
// ---------------------------------------------------------------------------

/// The secret-free `enclave.status` telemetry payload. `active` is GROUND TRUTH
/// (the SE wrap actually engaged this run), not merely the config intent — so a
/// config-armed-but-inert session reads honestly as NOT active. The Secure Enclave
/// key is non-exportable and NEVER leaves the chip; only its PUBLIC label and
/// booleans appear here — never the master key, never any key bytes.
pub fn status_frame(custody: &Custody) -> Value {
    let active = matches!(custody.mode, CustodyMode::EnclaveProtected);
    let available = matches!(custody.availability, Availability::Available);
    let (mode_str, reason) = match (&custody.mode, &custody.availability) {
        (CustodyMode::EnclaveProtected, _) => (
            "enclave-protected",
            "the at-rest master key is wrapped by a non-exportable, hardware-bound \
             Secure Enclave key"
                .to_string(),
        ),
        (CustodyMode::KeychainFallback, Availability::Unavailable(r)) => {
            ("keychain-fallback", r.clone())
        }
        (CustodyMode::KeychainFallback, Availability::Available) => (
            "keychain-fallback",
            "Secure Enclave reachable but custody fell back to the Keychain".to_string(),
        ),
    };
    json!({
        // GROUND TRUTH: the SE wrap actually engaged this run.
        "active": active,
        // Whether a usable Secure Enclave was detected (may be true while active is
        // false if there was no master key to bind).
        "available": available,
        "mode": mode_str,
        "reason": reason,
        // PUBLIC identifier of the SE wrapping key — a label, never key material.
        "key_label": ENCLAVE_KEY_LABEL,
        "custody": "additive: a Secure-Enclave-bound wrap OVER the existing macOS \
                    Keychain custody of the at-rest master key — it never replaces or \
                    weakens the Keychain path",
        "honesty": "ARMED by default, INERT without a Secure Enclave + the SE \
                    entitlement on a code-signed host. When inert, the existing \
                    OS-protected Keychain custody is used UNCHANGED — never a \
                    fabricated enclave claim. The SE key is non-exportable \
                    (hardware-bound), never leaves the chip, and is never in this frame."
    })
}

/// Map the resolved custody to a self-check line with honest PASS/SKIP semantics
/// (never FAIL, never a fake PASS): `EnclaveProtected` is a real PASS; a Keychain
/// fallback is a SKIP that carries the reason — honest-degraded, exactly the
/// selfcheck contract (`crate::selfcheck`).
pub fn selfcheck(custody: &Custody) -> crate::selfcheck::Check {
    match &custody.mode {
        CustodyMode::EnclaveProtected => crate::selfcheck::Check::pass(
            "enclave",
            "at-rest master key wrapped by a non-exportable Secure Enclave key",
        ),
        CustodyMode::KeychainFallback => crate::selfcheck::Check::skip(
            "enclave",
            match &custody.availability {
                Availability::Unavailable(r) => {
                    format!("Keychain custody (Secure Enclave inert): {r}")
                }
                Availability::Available => "Keychain custody".to_string(),
            },
        ),
    }
}

// ---------------------------------------------------------------------------
// #[cfg(test)] device-simulation seam (thread-local, RAII — no global mutation)
// ---------------------------------------------------------------------------

#[cfg(test)]
thread_local! {
    /// Forces the SE availability probe: `Some(true)` -> Available, `Some(false)`
    /// -> Unavailable, `None` -> the real per-platform detect. Default `None`, so
    /// tests without a guard read exactly production behavior.
    static TEST_AVAILABILITY: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    /// Forces the device-gated runner: `Some(true)` -> Ok, `Some(false)` -> Err,
    /// `None` -> the real (inert) op.
    static TEST_RUNNER_OK: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// `#[cfg(test)]`-only RAII guard that simulates a Secure-Enclave device on the
/// current thread, restoring the prior state on drop (so a simulation never leaks
/// into another test). The whole seam is `cfg(test)` — production is unchanged.
#[cfg(test)]
pub(crate) struct EnclaveSimulation {
    prev_availability: Option<bool>,
    prev_runner: Option<bool>,
}

#[cfg(test)]
impl EnclaveSimulation {
    fn install(availability: Option<bool>, runner: Option<bool>) -> Self {
        let prev_availability = TEST_AVAILABILITY.with(|c| c.replace(availability));
        let prev_runner = TEST_RUNNER_OK.with(|c| c.replace(runner));
        Self {
            prev_availability,
            prev_runner,
        }
    }

    /// A fully entitled device: the probe is Available and the SE key op succeeds.
    pub(crate) fn entitled() -> Self {
        Self::install(Some(true), Some(true))
    }

    /// The SE probe says available but the key op FAILS — proves we downgrade to an
    /// honest Keychain fallback rather than fake "protected".
    pub(crate) fn available_but_op_fails() -> Self {
        Self::install(Some(true), Some(false))
    }

    /// Force the SE unavailable — the shipped inert posture on any real build.
    pub(crate) fn unavailable() -> Self {
        Self::install(Some(false), None)
    }
}

#[cfg(test)]
impl Drop for EnclaveSimulation {
    fn drop(&mut self) {
        TEST_AVAILABILITY.with(|c| c.set(self.prev_availability));
        TEST_RUNNER_OK.with(|c| c.set(self.prev_runner));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::SecretKey;
    use crate::selfcheck::Status;

    fn a_key() -> SecretKey {
        SecretKey::from_bytes([7u8; crate::crypto::KEY_BYTES])
    }

    // --- the PURE availability -> custody-decision seam -------------------

    #[test]
    fn custody_decision_maps_present_to_protect_and_absent_to_fallback() {
        assert_eq!(
            custody_decision(&Availability::Available),
            CustodyMode::EnclaveProtected,
            "SE present must decide enclave-protect"
        );
        assert_eq!(
            custody_decision(&Availability::Unavailable("any reason".into())),
            CustodyMode::KeychainFallback,
            "SE absent must decide the honest Keychain fallback"
        );
    }

    /// Disabled config is an HONEST unavailable (never a fabricated available), and
    /// the reason names the switch.
    #[test]
    fn probe_honors_the_config_switch() {
        match probe_availability(false) {
            Availability::Unavailable(r) => assert!(
                r.contains("enclave"),
                "disabled reason must name the switch: {r}"
            ),
            Availability::Available => panic!("disabled config must never be Available"),
        }
    }

    /// On this real (unentitled) build the probe is HONESTLY unavailable — never a
    /// fabricated enclave claim — so custody is the Keychain fallback.
    #[test]
    fn real_build_probe_is_honestly_unavailable() {
        match probe_availability(true) {
            Availability::Unavailable(r) => assert!(!r.is_empty(), "must carry a reason"),
            Availability::Available => {
                panic!("an unentitled build must not claim a usable Secure Enclave")
            }
        }
    }

    // --- fallback preserves the existing resolution contract --------------

    /// SE inert (the shipped posture) -> KeychainFallback, even with a real master
    /// key present: the key is untouched and today's Keychain custody stands.
    #[test]
    fn fallback_preserves_keychain_custody_when_se_inert() {
        let _sim = EnclaveSimulation::unavailable();
        let key = a_key();
        let custody = resolve_custody(true, Some(&key));
        assert_eq!(custody.mode, CustodyMode::KeychainFallback);
        assert!(matches!(custody.availability, Availability::Unavailable(_)));
    }

    /// `[enclave]` disabled -> KeychainFallback -> byte-for-byte today's behavior.
    #[test]
    fn disabled_enclave_is_keychain_fallback() {
        let key = a_key();
        let custody = resolve_custody(false, Some(&key));
        assert_eq!(custody.mode, CustodyMode::KeychainFallback);
    }

    /// SE available but at-rest encryption OFF (no master key) -> honest fallback,
    /// not a failure and not a fake claim.
    #[test]
    fn available_but_no_master_key_falls_back_honestly() {
        let _sim = EnclaveSimulation::entitled();
        let custody = resolve_custody(true, None);
        assert_eq!(custody.mode, CustodyMode::KeychainFallback);
        match custody.availability {
            Availability::Unavailable(r) => {
                assert!(r.contains("no master key"), "reason must explain the fallback: {r}")
            }
            Availability::Available => panic!("no key to bind must not read as protected"),
        }
    }

    // --- SE present -> enclave-protect (positive path, simulated device) --

    #[test]
    fn entitled_device_enclave_protects_the_key() {
        let _sim = EnclaveSimulation::entitled();
        let key = a_key();
        let custody = resolve_custody(true, Some(&key));
        assert_eq!(
            custody.mode,
            CustodyMode::EnclaveProtected,
            "an entitled device with a master key must enclave-protect it"
        );
        assert_eq!(custody.availability, Availability::Available);
    }

    /// Probe says available but the SE key op FAILS -> we NEVER claim protected; we
    /// downgrade to an honest Keychain fallback carrying the failure reason.
    #[test]
    fn probe_ok_but_op_fails_never_fakes_protected() {
        let _sim = EnclaveSimulation::available_but_op_fails();
        let key = a_key();
        let custody = resolve_custody(true, Some(&key));
        assert_eq!(custody.mode, CustodyMode::KeychainFallback);
        match custody.availability {
            Availability::Unavailable(r) => assert!(
                r.contains("key operation failed"),
                "must record the op failure honestly: {r}"
            ),
            Availability::Available => panic!("a failed op must not read as protected"),
        }
    }

    // --- status frame: shape + secret-free ------------------------------

    #[test]
    fn status_frame_is_active_only_when_enclave_protected() {
        let protected = status_frame(&Custody {
            mode: CustodyMode::EnclaveProtected,
            availability: Availability::Available,
        });
        assert_eq!(protected["active"], true);
        assert_eq!(protected["mode"], "enclave-protected");
        assert_eq!(protected["available"], true);

        let fallback = status_frame(&Custody {
            mode: CustodyMode::KeychainFallback,
            availability: Availability::Unavailable("no entitlement".into()),
        });
        assert_eq!(fallback["active"], false);
        assert_eq!(fallback["mode"], "keychain-fallback");
        assert_eq!(fallback["available"], false);
        assert_eq!(fallback["reason"], "no entitlement");
    }

    /// The frame carries the PUBLIC key label and NEVER any key material. We build
    /// it from a real key's custody and assert the key's hex never appears.
    #[test]
    fn status_frame_never_leaks_key_material() {
        let key = SecretKey::from_bytes([0xABu8; crate::crypto::KEY_BYTES]);
        // Whatever custody results, the frame must never contain key bytes.
        let custody = resolve_custody(true, Some(&key));
        let frame = status_frame(&custody);
        let s = frame.to_string();
        assert!(
            s.contains(ENCLAVE_KEY_LABEL),
            "frame must carry the public key label"
        );
        assert!(
            !s.contains("abababab"),
            "frame must never contain key hex: {s}"
        );
        // Also true for a simulated protected frame.
        let protected = status_frame(&Custody {
            mode: CustodyMode::EnclaveProtected,
            availability: Availability::Available,
        });
        assert!(!protected.to_string().contains("abababab"));
    }

    #[test]
    fn key_label_is_com_darwin_namespaced() {
        // starts_with("com.darwin.") already guarantees the correct namespace and,
        // transitively, that no legacy brand token can appear anywhere in the label.
        assert!(
            ENCLAVE_KEY_LABEL.starts_with("com.darwin."),
            "the SE key label must use the com.darwin.* namespace, got {ENCLAVE_KEY_LABEL}"
        );
    }

    // --- self-check leg: honest PASS/SKIP -------------------------------

    #[test]
    fn selfcheck_is_pass_when_protected_and_skip_when_fallback() {
        let pass = selfcheck(&Custody {
            mode: CustodyMode::EnclaveProtected,
            availability: Availability::Available,
        });
        assert_eq!(pass.status, Status::Pass);
        assert_eq!(pass.name, "enclave");

        let skip = selfcheck(&Custody {
            mode: CustodyMode::KeychainFallback,
            availability: Availability::Unavailable("no SE entitlement".into()),
        });
        assert_eq!(skip.status, Status::Skip, "an inert enclave is SKIP, never FAIL");
        assert!(
            skip.detail.contains("no SE entitlement"),
            "the SKIP must carry the honest reason: {}",
            skip.detail
        );
    }
}
