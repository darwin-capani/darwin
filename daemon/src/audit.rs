//! APPEND-ONLY, HASH-CHAINED, TAMPER-EVIDENT audit log of consequential
//! decisions.
//!
//! Every time a consequential action reaches a decision point — proposed,
//! parked, blocked by policy, auto-approved by policy, confirmed, denied,
//! executed — the chokepoint calls [`AuditLog::record`]. Each entry stores
//!
//!   { seq, ts, agent, tool, target_redacted, decision, outcome, prev_hash, entry_hash }
//!
//! where `entry_hash = sha256(prev_hash || canonical(entry fields))`. Because
//! each entry's hash folds in the PREVIOUS entry's hash, the records form a
//! chain: [`AuditLog::verify_chain`] recomputes the whole chain and detects ANY
//! tamper — a mutated field, an inserted row, a deleted row, or a reordering —
//! because the recomputed hash will diverge from the stored one (or the seq /
//! prev_hash linkage will break).
//!
//! ## HONESTY: tamper-EVIDENT, not tamper-PROOF
//!
//! This makes tampering DETECTABLE, not IMPOSSIBLE. The hashes live in the same
//! local SQLite file the entries do, so a root attacker with write access to that
//! file could recompute and rewrite the ENTIRE chain from any point forward and
//! it would still verify. What the chain buys: any edit that does NOT rebuild the
//! whole forward chain (a careless DELETE, an UPDATE of one field, an INSERT in
//! the middle, a row swap) is caught by `verify_chain`. It is an integrity
//! tripwire, not a vault. (A true tamper-PROOF log needs an external anchor — a
//! remote witness or notarization — which this on-device appliance does not have.)
//!
//! ## SECRET-FREE
//!
//! The log NEVER stores the raw tool input. The caller passes an ALREADY-redacted
//! `target` summary, and `record` redacts it AGAIN ([`crate::optimize::redact`])
//! as defense in depth, so a token/secret/PII in the original input can never land
//! in the log. A test (`a_token_in_the_target_never_lands_in_the_log`) pins this.
//!
//! ## BOUNDED
//!
//! Retention is capped at [`MAX_ENTRIES`]. When the cap is exceeded, the oldest
//! entries are pruned and the chain is RE-ROOTED from the new oldest surviving
//! entry: its `prev_hash` is reset to the genesis sentinel and its `entry_hash`
//! recomputed, and the chain re-links forward from there, so `verify_chain` stays
//! consistent after truncation (it verifies the surviving suffix as a fresh
//! chain). A `truncated` flag + a telemetry note record that a prune happened, so
//! the gap is explicit, not silent.
//!
//! Some of this module's public surface (the `recent`/`verify_chain`/`len` read
//! API, the `ChainStatus` indicator, the `global()` borrow, the
//! `Proposed`/`Confirmed`/`Denied` outcome variants) is consumed by the HUD
//! telemetry / command-channel layer (item #4) and the spoken-confirmation replay
//! audit hook, which land next. Until they do, the unused-item lint would flag
//! them, so `dead_code` is allowed module-wide — the same "shared contract that
//! another component reads" rationale `integrations/mod.rs` uses.
#![allow(dead_code)]

use std::future::Future;
use std::path::Path;

use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::policy::Decision;

/// Max entries retained before the oldest are pruned + the chain re-rooted.
/// Generous for an appliance's consequential-action cadence; the cap only bounds
/// disk, it does not weaken the integrity property of the surviving suffix.
pub const MAX_ENTRIES: usize = 10_000;

/// The genesis sentinel `prev_hash` for the very first entry (and for the new
/// root after a truncation). A fixed, well-known string so the chain has a
/// deterministic anchor that `verify_chain` and `record` agree on.
const GENESIS_PREV: &str = "GENESIS";

/// Fixed, secret-free markers for a FORENSIC TRIAGE evidence record (see
/// [`AuditLog::record_triage_evidence`]). A triage capture is read-only evidence,
/// not a consequential decision, so it uses its own agent/tool/decision/outcome
/// tokens rather than one of the gate's — the chain reads honestly as "an evidence
/// bundle was frozen", never a fabricated approval/execution.
const TRIAGE_AGENT: &str = "agent.aegis";
const TRIAGE_TOOL: &str = "triage_snapshot";
const TRIAGE_DECISION: &str = "ask";
const TRIAGE_OUTCOME: &str = "evidence_captured";

/// The macOS Keychain account holding the audit chain's EXTERNAL ANCHOR (the
/// witnessed `"<seq>:<head_hash>"`). Added to `integrations::ALLOWED_ACCOUNTS`
/// (a mirror test pins it) so the existing argv-free `resolve_secret` reader /
/// `keychain_write` writer reach it under the `com.darwin.daemon` service. The
/// value is a PUBLIC digest, not a credential — the Keychain is chosen because it
/// is a DISTINCT OS protection domain from the audit SQLite file, not for secrecy.
pub const AUDIT_ANCHOR_ACCOUNT: &str = "audit_chain_anchor";

/// What happened at a consequential decision point. One value per call to
/// [`AuditLog::record`], so the timeline reads as a sequence of decisions +
/// outcomes. Stable lowercase tokens (the wire/HUD contract).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The action was proposed/seen at a chokepoint (the entry point; usually
    /// paired with a following decision-specific outcome).
    Proposed,
    /// The action was PARKED for a spoken human confirmation (the Ask path).
    Parked,
    /// A policy `Never` HARD-BLOCKED the action (no park, no fire).
    BlockedByPolicy,
    /// A policy `Always` AUTO-APPROVED + executed the action directly (master ON
    /// + voice-id allowed). The controlled, logged loosening.
    AutoApprovedByPolicy,
    /// A policy `Always` matched but the MASTER SWITCH was OFF, so the action was
    /// still only previewed (DryRun) — proof that `Always` is inert under master
    /// OFF.
    AlwaysInertMasterOff,
    /// A previously-parked action was CONFIRMED (spoken yes) and replayed.
    Confirmed,
    /// A previously-parked action was DENIED / cancelled.
    Denied,
    /// The action's real side effect actually ran (Execute).
    Executed,
    /// The action returned only a DryRun preview (master OFF path, the shipped
    /// default).
    DryRun,
}

impl Outcome {
    /// Stable lowercase token for storage + the HUD.
    pub fn as_str(&self) -> &'static str {
        match self {
            Outcome::Proposed => "proposed",
            Outcome::Parked => "parked",
            Outcome::BlockedByPolicy => "blocked_by_policy",
            Outcome::AutoApprovedByPolicy => "auto_approved_by_policy",
            Outcome::AlwaysInertMasterOff => "always_inert_master_off",
            Outcome::Confirmed => "confirmed",
            Outcome::Denied => "denied",
            Outcome::Executed => "executed",
            Outcome::DryRun => "dry_run",
        }
    }
}

/// One audit record as stored + returned. All fields are secret-free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntry {
    /// Monotonic sequence number (1-based), the chain's ordering key.
    pub seq: i64,
    /// RFC3339 timestamp of the decision.
    pub ts: String,
    /// The agent namespace that proposed the action ("agent.pepper").
    pub agent: String,
    /// The consequential tool (or MCP flat id) the action targeted.
    pub tool: String,
    /// A REDACTED, secret-free target summary (recipient/channel/device/amount).
    /// Never the raw input.
    pub target_redacted: String,
    /// The policy decision rendered (always/never/ask).
    pub decision: String,
    /// What happened (see [`Outcome`]).
    pub outcome: String,
    /// The previous entry's `entry_hash` (or [`GENESIS_PREV`] for the root).
    pub prev_hash: String,
    /// `sha256(prev_hash || canonical(fields))`, the chain link.
    pub entry_hash: String,
}

/// Canonical, injective byte encoding of an entry's CONTENT fields (everything
/// except `entry_hash` itself), folded over `prev_hash`. NUL-delimited so no two
/// distinct field tuples can collide into the same byte string (a field can't
/// contain a NUL — redact + the fixed token sets guarantee it). This is the exact
/// preimage `verify_chain` recomputes, so the two MUST stay byte-identical.
// The args ARE the canonical content fields folded into the hash; a bundling
// struct would just duplicate AuditEntry's shape and risk drifting from it.
#[allow(clippy::too_many_arguments)]
fn hash_entry(
    prev_hash: &str,
    seq: i64,
    ts: &str,
    agent: &str,
    tool: &str,
    target_redacted: &str,
    decision: &str,
    outcome: &str,
) -> String {
    let mut h = Sha256::new();
    h.update(prev_hash.as_bytes());
    h.update([0u8]);
    h.update(seq.to_le_bytes());
    h.update([0u8]);
    h.update(ts.as_bytes());
    h.update([0u8]);
    h.update(agent.as_bytes());
    h.update([0u8]);
    h.update(tool.as_bytes());
    h.update([0u8]);
    h.update(target_redacted.as_bytes());
    h.update([0u8]);
    h.update(decision.as_bytes());
    h.update([0u8]);
    h.update(outcome.as_bytes());
    hex::encode(h.finalize())
}

/// The result of [`AuditLog::verify_chain`]: either the chain is intact, or the
/// first divergence is reported (by seq) so a HUD/operator sees WHERE the chain
/// broke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainStatus {
    /// Every entry's recomputed hash and prev-link match. `count` entries verified.
    Ok { count: usize },
    /// The chain broke at sequence `seq` (a mutated field, a bad prev-link, a
    /// reorder, a deletion, or an insertion). `reason` is a short, secret-free note.
    Broken { seq: i64, reason: String },
}

impl ChainStatus {
    /// Did the chain verify? (Convenience for the HUD "chain-OK" indicator.)
    pub fn is_ok(&self) -> bool {
        matches!(self, ChainStatus::Ok { .. })
    }
}

/// The append-only audit log. Held for the daemon's life like `Memory`, in its
/// OWN dedicated SQLite file (`state/audit.db`). rusqlite::Connection is Send but
/// not Sync, so an async Mutex serializes access (mirrors `Memory`). The schema
/// is append-only by discipline: only `record` ever INSERTs, only the bounded
/// prune ever DELETEs, and nothing UPDATEs a stored entry's content.
pub struct AuditLog {
    conn: Mutex<Connection>,
    /// Retention cap: past this many entries the oldest are pruned and the chain
    /// re-rooted. Defaults to [`MAX_ENTRIES`]; overridden from `[audit].max_entries`
    /// via [`with_max_entries`](AuditLog::with_max_entries) at construction.
    max_entries: usize,
}

impl AuditLog {
    /// Open (or create) the audit DB PLAINTEXT (today's behavior, byte-for-byte).
    /// Reached when `[security].encrypt_memory` is OFF (the default). Idempotent.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::init_conn(conn)
    }

    /// Open (or create) the audit DB ENCRYPTED (transparent whole-file SQLCipher
    /// AES-256). `key` is applied via `PRAGMA key` immediately after open, before
    /// any other pragma/statement. Reached only when `[security].encrypt_memory`
    /// is ON; tests pass an explicit in-test key (no Keychain).
    pub fn open_encrypted(path: &Path, key: &crate::crypto::SecretKey) -> Result<Self> {
        let conn = Connection::open(path)?;
        crate::crypto::apply_key(&conn, key)?;
        Self::init_conn(conn)
    }

    /// Shared setup (pragmas + schema), run AFTER any `PRAGMA key`.
    fn init_conn(conn: Connection) -> Result<Self> {
        conn.busy_timeout(std::time::Duration::from_millis(250))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS audit(
                seq INTEGER PRIMARY KEY,
                ts TEXT NOT NULL,
                agent TEXT NOT NULL,
                tool TEXT NOT NULL,
                target_redacted TEXT NOT NULL,
                decision TEXT NOT NULL,
                outcome TEXT NOT NULL,
                prev_hash TEXT NOT NULL,
                entry_hash TEXT NOT NULL
            );",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
            max_entries: MAX_ENTRIES,
        })
    }

    /// Override the retention cap from `[audit].max_entries`. A value of 0 (or an
    /// absent config) keeps the [`MAX_ENTRIES`] default — a config typo can never
    /// disable retention (which would let the log grow unbounded). Chainable at
    /// construction, before the log is installed / Arc-wrapped.
    pub fn with_max_entries(mut self, n: usize) -> Self {
        if n > 0 {
            self.max_entries = n;
        }
        self
    }

    /// In-memory audit log for tests (no disk). Same schema, same chain logic.
    #[cfg(test)]
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS audit(
                seq INTEGER PRIMARY KEY,
                ts TEXT NOT NULL,
                agent TEXT NOT NULL,
                tool TEXT NOT NULL,
                target_redacted TEXT NOT NULL,
                decision TEXT NOT NULL,
                outcome TEXT NOT NULL,
                prev_hash TEXT NOT NULL,
                entry_hash TEXT NOT NULL
            );",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
            max_entries: MAX_ENTRIES,
        })
    }

    /// APPEND one decision to the chain. SECRET-FREE: `target` is redacted again
    /// here (defense in depth) before storage, so a secret in it can never land in
    /// the log. The new entry's `prev_hash` is the current tail's `entry_hash` (or
    /// [`GENESIS_PREV`] when empty), and its `entry_hash` folds that in — so the
    /// chain extends by construction. After append, if the count exceeds
    /// [`MAX_ENTRIES`], the oldest are pruned and the chain re-rooted (see
    /// [`Self::prune_and_reroot`]). Returns the stored [`AuditEntry`].
    pub async fn record(
        &self,
        agent: &str,
        tool: &str,
        target: &str,
        decision: Decision,
        outcome: Outcome,
    ) -> Result<AuditEntry> {
        // Defense in depth: the caller passes an already-redacted summary, but we
        // redact AGAIN so a secret can never enter the log even if a future call
        // site forgets. Redaction also guarantees no NUL byte (the canonical-form
        // delimiter) survives in the field.
        let target_redacted = crate::optimize::redact(target);
        self.append_raw(agent, tool, &target_redacted, decision.as_str(), outcome.as_str())
            .await
    }

    /// The SINGLE append path: link an ALREADY-FINAL record onto the chain, INSERT
    /// it, and run the bounded prune+re-root. `target_stored` is written VERBATIM —
    /// [`record`] pre-redacts it (secret-free defense in depth), while
    /// [`record_triage_evidence`] passes a public SHA-256 digest that must survive
    /// intact. Factoring this out keeps the seq/prev_hash linkage and the canonical
    /// `hash_entry` preimage identical for every caller, so no two write paths can
    /// drift on the chain bytes.
    async fn append_raw(
        &self,
        agent: &str,
        tool: &str,
        target_stored: &str,
        decision_s: &str,
        outcome_s: &str,
    ) -> Result<AuditEntry> {
        let ts = Utc::now().to_rfc3339();

        let conn = self.conn.lock().await;
        // The current tail: highest seq + its hash, or genesis when empty.
        let (next_seq, prev_hash) = conn
            .query_row(
                "SELECT seq, entry_hash FROM audit ORDER BY seq DESC LIMIT 1",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .map(|(seq, hash)| (seq + 1, hash))
            .unwrap_or((1, GENESIS_PREV.to_string()));

        let entry_hash = hash_entry(
            &prev_hash,
            next_seq,
            &ts,
            agent,
            tool,
            target_stored,
            decision_s,
            outcome_s,
        );

        conn.execute(
            "INSERT INTO audit(seq, ts, agent, tool, target_redacted, decision, outcome, prev_hash, entry_hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                next_seq,
                ts,
                agent,
                tool,
                target_stored,
                decision_s,
                outcome_s,
                prev_hash,
                entry_hash
            ],
        )?;

        let entry = AuditEntry {
            seq: next_seq,
            ts,
            agent: agent.to_string(),
            tool: tool.to_string(),
            target_redacted: target_stored.to_string(),
            decision: decision_s.to_string(),
            outcome: outcome_s.to_string(),
            prev_hash,
            entry_hash,
        };

        // Bounded retention: prune + re-root if we are over the cap.
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM audit", [], |r| r.get(0))?;
        if count as usize > self.max_entries {
            Self::prune_and_reroot(&conn, self.max_entries)?;
        }

        Ok(entry)
    }

    /// Record that a FORENSIC TRIAGE BUNDLE was frozen, folding its manifest's
    /// SHA-256 into the hash chain so the ledger itself vouches for the bundle. A
    /// triage capture is READ-ONLY evidence, not a consequential *decision*, so it
    /// enters the chain with a dedicated, non-loosening marker
    /// (`decision = "ask"`, `outcome = "evidence_captured"`) rather than one of the
    /// gate's decision/outcome tokens.
    ///
    /// The stored target is `bundle <id> sha256=<hex>`. UNLIKE [`record`], the
    /// digest is stored VERBATIM (no [`crate::optimize::redact`] pass): a SHA-256 is
    /// a one-way, non-secret integrity value and the manifest it digests is itself
    /// redacted at source, so the digest is safe in the clear — and it MUST survive
    /// intact, because redaction would collapse a 64-hex run to `[redacted]` and
    /// destroy the very anchor. The bundle id is a UTC timestamp (also non-secret).
    pub async fn record_triage_evidence(
        &self,
        bundle_id: &str,
        manifest_sha256: &str,
    ) -> Result<AuditEntry> {
        let target = format!("bundle {bundle_id} sha256={manifest_sha256}");
        self.append_raw(TRIAGE_AGENT, TRIAGE_TOOL, &target, TRIAGE_DECISION, TRIAGE_OUTCOME)
            .await
    }

    /// The current chain HEAD: `(seq, entry_hash)` of the highest-seq entry, or
    /// `None` when the chain is empty. Read-only. Pinning the head pins the WHOLE
    /// chain — the tail's `entry_hash` folds in every prior entry's hash — which is
    /// why the external anchor stores exactly this value.
    pub async fn head(&self) -> Result<Option<(i64, String)>> {
        let conn = self.conn.lock().await;
        let head = conn
            .query_row(
                "SELECT seq, entry_hash FROM audit ORDER BY seq DESC LIMIT 1",
                [],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?;
        Ok(head)
    }

    /// The `entry_hash` stored at a specific `seq`, or `None` when no entry has that
    /// seq (pruned away, or never existed). Read-only. Lets the external-anchor check
    /// tell a legitimate append-only EXTENSION (the witnessed entry is still present
    /// unchanged and the chain merely grew past it) from a genuine REWRITE (the entry
    /// at the witnessed seq changed).
    pub async fn hash_at_seq(&self, seq: i64) -> Result<Option<String>> {
        let conn = self.conn.lock().await;
        let h = conn
            .query_row(
                "SELECT entry_hash FROM audit WHERE seq = ?1",
                params![seq],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(h)
    }

    /// Read the live chain HEAD and WRITE it (as `"<seq>:<entry_hash>"`) to the
    /// EXTERNAL `anchor`, returning the witnessed `(seq, head)` — or `None` on an
    /// empty chain (nothing to witness, so no anchor is written). Call after a
    /// chain-extending event (a triage evidence record) so the anchor tracks the
    /// current head. The `anchor` is INJECTED so tests never touch the real Keychain.
    pub async fn anchor_to<A: ChainAnchor>(&self, anchor: &A) -> Result<Option<(i64, String)>> {
        let Some((seq, head)) = self.head().await? else {
            return Ok(None);
        };
        anchor.write_anchor(&format!("{seq}:{head}")).await?;
        Ok(Some((seq, head)))
    }

    /// Compare the live chain HEAD against the value stored in the EXTERNAL anchor
    /// and report honestly. This closes the gap the module header admits: the local
    /// hash chain cannot catch a root attacker who rewrites the ENTIRE forward chain
    /// consistently (it still passes [`verify_chain`]), but such a rewrite MOVES the
    /// tail's `entry_hash`, so it no longer equals the head witnessed in the
    /// Keychain — a SEPARATE OS protection domain the SQLite-file attacker cannot
    /// silently rewrite. A [`AnchorStatus::Mismatch`] is honestly ambiguous
    /// (a rewrite, OR legitimate growth/pruning since the last anchor); the caller
    /// pairs it with [`verify_chain`] + the witnessed vs live seq to interpret.
    pub async fn verify_against_anchor<A: ChainAnchor>(
        &self,
        anchor: &A,
    ) -> Result<AnchorStatus> {
        let Some(raw) = anchor.read_anchor().await else {
            return Ok(AnchorStatus::NoAnchor);
        };
        let Some((a_seq, a_head)) = parse_anchor(&raw) else {
            // Never surface the raw value unredacted (it is only a public digest,
            // but the redact pass is the module's blanket discipline).
            return Ok(AnchorStatus::Malformed { raw_redacted: crate::optimize::redact(&raw) });
        };
        let Some((l_seq, l_head)) = self.head().await? else {
            // An anchor exists but the live chain is EMPTY — the whole chain vanished
            // from under the witness. Not legitimate growth: a genuine divergence.
            return Ok(AnchorStatus::Mismatch { anchored_seq: a_seq, anchored_head: a_head, live: None });
        };
        // Exact corroboration.
        if l_seq == a_seq && l_head == a_head {
            return Ok(AnchorStatus::Match { seq: a_seq, head: a_head });
        }
        // Not exact. Distinguish a legitimate APPEND-ONLY extension from a REWRITE by
        // asking whether the WITNESSED entry is still present, unchanged, at its seq.
        // Without this the check false-alarms on EVERY boot after any new record —
        // append-only growth advances the head — training operators to ignore it and
        // nullifying the tamper detection the anchor exists to provide.
        match self.hash_at_seq(a_seq).await? {
            // Witnessed entry intact and the chain only grew past it: benign extension.
            Some(h) if h == a_head && l_seq >= a_seq => {
                Ok(AnchorStatus::Extended { anchored_seq: a_seq, anchored_head: a_head, live: (l_seq, l_head) })
            }
            // An entry EXISTS at the witnessed seq but its hash CHANGED: the chain was
            // rewritten under the witness — the exact offline full-rewrite tamper the
            // external anchor catches (and that verify_chain alone cannot). A rare
            // local prune+re-root of a still-present witnessed entry also lands here
            // (documented residual — a prune is uncommon at the generous retention cap).
            Some(_) => Ok(AnchorStatus::Mismatch {
                anchored_seq: a_seq,
                anchored_head: a_head,
                live: Some((l_seq, l_head)),
            }),
            // No entry at the witnessed seq: below the live head it was legitimately
            // pruned away (bounded retention) — benign; at/above it the chain shrank.
            None if a_seq < l_seq => {
                Ok(AnchorStatus::Extended { anchored_seq: a_seq, anchored_head: a_head, live: (l_seq, l_head) })
            }
            None => Ok(AnchorStatus::Mismatch {
                anchored_seq: a_seq,
                anchored_head: a_head,
                live: Some((l_seq, l_head)),
            }),
        }
    }

    /// Keep the newest `keep` entries; drop the rest and RE-ROOT the surviving
    /// suffix so it verifies as a fresh chain: the new oldest entry's `prev_hash`
    /// becomes [`GENESIS_PREV`] and every surviving entry's `entry_hash` is
    /// recomputed forward. The seq numbers are preserved (the gap is the visible
    /// evidence a prune happened); `verify_chain` treats the first surviving entry
    /// as the new root. Emits a secret-free telemetry note so truncation is
    /// explicit. Synchronous — runs under the held connection lock.
    fn prune_and_reroot(conn: &Connection, keep: usize) -> Result<()> {
        // The seq of the oldest entry we KEEP (the (count-keep+1)-th oldest).
        let cutoff: Option<i64> = conn
            .query_row(
                "SELECT seq FROM audit ORDER BY seq DESC LIMIT 1 OFFSET ?1",
                params![keep as i64 - 1],
                |r| r.get(0),
            )
            .ok();
        let Some(cutoff_seq) = cutoff else { return Ok(()) };

        // Drop everything older than the cutoff.
        let removed = conn.execute("DELETE FROM audit WHERE seq < ?1", params![cutoff_seq])?;

        // Recompute the chain over the survivors, re-rooting at the first one.
        let mut stmt = conn.prepare(
            "SELECT seq, ts, agent, tool, target_redacted, decision, outcome
             FROM audit ORDER BY seq ASC",
        )?;
        let rows: Vec<(i64, String, String, String, String, String, String)> = stmt
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        drop(stmt);

        let mut prev = GENESIS_PREV.to_string();
        for (seq, ts, agent, tool, target, decision, outcome) in &rows {
            let entry_hash =
                hash_entry(&prev, *seq, ts, agent, tool, target, decision, outcome);
            conn.execute(
                "UPDATE audit SET prev_hash = ?1, entry_hash = ?2 WHERE seq = ?3",
                params![prev, entry_hash, seq],
            )?;
            prev = entry_hash;
        }

        info!(removed, kept = rows.len(), "audit: pruned + re-rooted the chain (truncation)");
        crate::telemetry::emit(
            "system",
            "audit.truncated",
            serde_json::json!({"removed": removed, "kept": rows.len()}),
        );
        Ok(())
    }

    /// Recompute the ENTIRE chain and report whether it is intact. Catches:
    ///   * a MUTATED field (recomputed `entry_hash` != stored),
    ///   * a broken PREV-LINK (an entry's `prev_hash` != the prior `entry_hash`,
    ///     i.e. a reorder or a mid-chain DELETE/INSERT),
    ///   * a wrong root anchor (the first entry's `prev_hash` != [`GENESIS_PREV`]),
    ///   * a non-contiguous seq (a deletion that left a gap WITHOUT re-rooting).
    ///     Read-only.
    pub async fn verify_chain(&self) -> Result<ChainStatus> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT seq, ts, agent, tool, target_redacted, decision, outcome, prev_hash, entry_hash
             FROM audit ORDER BY seq ASC",
        )?;
        let entries: Vec<AuditEntry> = stmt
            .query_map([], |row| {
                Ok(AuditEntry {
                    seq: row.get(0)?,
                    ts: row.get(1)?,
                    agent: row.get(2)?,
                    tool: row.get(3)?,
                    target_redacted: row.get(4)?,
                    decision: row.get(5)?,
                    outcome: row.get(6)?,
                    prev_hash: row.get(7)?,
                    entry_hash: row.get(8)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        drop(stmt);

        Ok(verify_entries(&entries))
    }

    /// The most recent `n` entries, newest-first, for the HUD audit timeline.
    /// Read-only.
    pub async fn recent(&self, n: usize) -> Result<Vec<AuditEntry>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT seq, ts, agent, tool, target_redacted, decision, outcome, prev_hash, entry_hash
             FROM audit ORDER BY seq DESC LIMIT ?1",
        )?;
        let entries = stmt
            .query_map(params![n as i64], |row| {
                Ok(AuditEntry {
                    seq: row.get(0)?,
                    ts: row.get(1)?,
                    agent: row.get(2)?,
                    tool: row.get(3)?,
                    target_redacted: row.get(4)?,
                    decision: row.get(5)?,
                    outcome: row.get(6)?,
                    prev_hash: row.get(7)?,
                    entry_hash: row.get(8)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(entries)
    }

    /// Entries within the inclusive `[from, to]` RFC3339 window (both sides
    /// UTC; RFC3339 compares lexicographically), NEWEST first, capped at
    /// `limit`. Read-only — the session-rewind timeline's windowed read: a
    /// depth-only `recent(n)` would silently miss a window's entries whenever
    /// enough newer entries accrued after it, then narrate a false absence.
    pub async fn between(&self, from: &str, to: &str, limit: usize) -> Result<Vec<AuditEntry>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT seq, ts, agent, tool, target_redacted, decision, outcome, prev_hash, entry_hash
             FROM audit WHERE ts >= ?1 AND ts <= ?2 ORDER BY seq DESC LIMIT ?3",
        )?;
        let entries = stmt
            .query_map(params![from, to, limit as i64], |row| {
                Ok(AuditEntry {
                    seq: row.get(0)?,
                    ts: row.get(1)?,
                    agent: row.get(2)?,
                    tool: row.get(3)?,
                    target_redacted: row.get(4)?,
                    decision: row.get(5)?,
                    outcome: row.get(6)?,
                    prev_hash: row.get(7)?,
                    entry_hash: row.get(8)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(entries)
    }

    /// Total entries currently retained (for the HUD + tests).
    pub async fn len(&self) -> Result<usize> {
        let conn = self.conn.lock().await;
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM audit", [], |r| r.get(0))?;
        Ok(count as usize)
    }
}

// ---------------------------------------------------------------------------
// EXTERNAL ANCHOR — upgrading tamper-EVIDENT toward tamper-PROOF
//
// The hash chain lives in the same local SQLite file the entries do, so a root
// attacker with write access could rewrite the ENTIRE forward chain and it would
// still `verify_chain`. Anchoring the chain HEAD in a SEPARATE OS protection
// domain — the macOS Keychain, which a SQLite-file attacker cannot silently
// rewrite — means such a rewrite leaves the live head disagreeing with the
// witnessed head. The seam is INJECTED (like posture.rs's command runner) so
// tests exercise write / verify / mismatch with an in-memory anchor and NEVER
// touch the real Keychain.
// ---------------------------------------------------------------------------

/// The external anchor seam: read / write the witnessed chain head in a store
/// OUTSIDE the audit SQLite file. Declared with `-> impl Future + Send` (not
/// `async fn`) so the trait carries an explicit `Send` bound (avoids the
/// `async_fn_in_trait` lint) while the impls stay plain `async fn`.
pub trait ChainAnchor {
    /// The stored anchor value (`"<seq>:<head_hash>"`), or `None` when none is set.
    fn read_anchor(&self) -> impl Future<Output = Option<String>> + Send;
    /// Persist the anchor value. Errors are surfaced (a lost anchor is honest,
    /// never silently swallowed).
    fn write_anchor(&self, value: &str) -> impl Future<Output = Result<()>> + Send;
}

/// The production anchor: the macOS Keychain at [`AUDIT_ANCHOR_ACCOUNT`], reached
/// through the SAME argv-free `security(1)` seam the at-rest master key uses. The
/// write is a blocking `security(1)` child, so it runs on the blocking pool rather
/// than pinning an async worker (mirroring `resolve_encryption_key`'s key store).
pub struct KeychainAnchor;

impl ChainAnchor for KeychainAnchor {
    async fn read_anchor(&self) -> Option<String> {
        crate::integrations::resolve_secret(AUDIT_ANCHOR_ACCOUNT).await
    }

    async fn write_anchor(&self, value: &str) -> Result<()> {
        let value = value.to_string();
        tokio::task::spawn_blocking(move || {
            crate::integrations::keychain_write(AUDIT_ANCHOR_ACCOUNT, &value)
        })
        .await
        .map_err(|e| anyhow::anyhow!("keychain anchor write task failed: {e}"))?
    }
}

/// The honest verdict of [`AuditLog::verify_against_anchor`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnchorStatus {
    /// No external anchor is stored yet (never witnessed).
    NoAnchor,
    /// The live head equals the witnessed head at the same seq — the chain is
    /// externally corroborated up to that point.
    Match { seq: i64, head: String },
    /// The live head advanced PAST the witnessed head by legitimate APPEND-ONLY
    /// growth (or bounded pruning) — the witnessed entry is still corroborated at its
    /// seq, so this is NOT a tamper. Benign: the caller advances the witness to the
    /// live head.
    Extended {
        anchored_seq: i64,
        anchored_head: String,
        live: (i64, String),
    },
    /// The live head differs from the witnessed head in a way that is NOT legitimate
    /// growth: the entry at the witnessed seq CHANGED, or the chain shrank/vanished —
    /// the offline full-chain rewrite the external anchor catches that `verify_chain`
    /// cannot. Both witnessed and live are carried so the caller can interpret.
    Mismatch {
        anchored_seq: i64,
        anchored_head: String,
        live: Option<(i64, String)>,
    },
    /// The stored anchor value was not the expected `"<seq>:<hash>"` shape.
    Malformed { raw_redacted: String },
}

impl AnchorStatus {
    /// Did the live head match the external anchor? (Convenience for the HUD /
    /// operator "anchor-OK" indicator.)
    pub fn is_match(&self) -> bool {
        matches!(self, AnchorStatus::Match { .. })
    }
}

/// Parse a stored anchor value `"<seq>:<hash>"` into `(seq, hash)`. Robust to
/// surrounding whitespace; rejects a missing seq/colon or an empty hash.
fn parse_anchor(raw: &str) -> Option<(i64, String)> {
    let (seq, hash) = raw.trim().split_once(':')?;
    let seq: i64 = seq.trim().parse().ok()?;
    let hash = hash.trim();
    (!hash.is_empty()).then(|| (seq, hash.to_string()))
}

/// The SECRET-FREE `audit.anchor` wire payload: the anchor verdict plus the
/// witnessed vs live seq/hash. All values are public digests + integers — no raw
/// input, no chain-internal `prev_hash` bytes beyond the single witnessed head.
pub fn anchor_status_json(status: &AnchorStatus) -> serde_json::Value {
    match status {
        AnchorStatus::NoAnchor => serde_json::json!({ "ok": true, "state": "no_anchor" }),
        AnchorStatus::Match { seq, head } => serde_json::json!({
            "ok": true, "state": "match", "seq": seq, "head": head,
        }),
        AnchorStatus::Extended { anchored_seq, anchored_head, live } => serde_json::json!({
            "ok": true,
            "state": "extended",
            "anchored_seq": anchored_seq,
            "anchored_head": anchored_head,
            "live_seq": live.0,
            "live_head": live.1,
        }),
        AnchorStatus::Mismatch { anchored_seq, anchored_head, live } => serde_json::json!({
            "ok": false,
            "state": "mismatch",
            "anchored_seq": anchored_seq,
            "anchored_head": anchored_head,
            "live_seq": live.as_ref().map(|(s, _)| *s),
            "live_head": live.as_ref().map(|(_, h)| h.clone()),
        }),
        AnchorStatus::Malformed { raw_redacted } => serde_json::json!({
            "ok": false, "state": "malformed", "raw": raw_redacted,
        }),
    }
}

/// One-shot: verify the installed log's live head against the macOS-Keychain
/// external anchor, emit a secret-free `audit.anchor` frame, and then (re)witness
/// the CURRENT head — but ONLY when the prior state was benign (Match / NoAnchor /
/// Malformed). On a genuine MISMATCH the anchor is DELIBERATELY LEFT UNCHANGED so
/// the divergence keeps surfacing across restarts (auto-overwriting would "bless"
/// a rewritten chain). No-op when audit is not installed or disabled. Runtime-only
/// (touches the real Keychain); the pure verify/anchor logic is unit-tested with an
/// injected seam.
pub async fn verify_and_reanchor_on_start() {
    let Some((true, log)) = global() else { return };
    let anchor = KeychainAnchor;
    let status = match log.verify_against_anchor(&anchor).await {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "audit: could not read the external anchor; skipping the check");
            return;
        }
    };
    match &status {
        AnchorStatus::Mismatch { .. } => {
            warn!("audit: EXTERNAL ANCHOR MISMATCH — the live chain head diverged from the Keychain witness");
        }
        AnchorStatus::Malformed { .. } => {
            warn!("audit: the stored external anchor is malformed; re-establishing it");
        }
        _ => {}
    }
    crate::telemetry::emit("system", "audit.anchor", anchor_status_json(&status));
    // Do NOT silently overwrite a real mismatch — leave it for the operator.
    if matches!(status, AnchorStatus::Mismatch { .. }) {
        return;
    }
    if let Err(e) = log.anchor_to(&anchor).await {
        warn!(error = %e, "audit: could not write the external anchor");
    }
}

// ---------------------------------------------------------------------------
// Process-global handle + the chokepoint record path
// ---------------------------------------------------------------------------

use std::sync::{Arc, OnceLock};

/// The process-global audit log + enable flag. `None` until [`install`] runs at
/// startup; a never-installed global makes [`record_global`] a no-op (so unit
/// tests and any startup path that skips audit are unaffected) — mirroring
/// `mcp::global`'s fail-safe inert default.
static GLOBAL: OnceLock<(bool, Arc<AuditLog>)> = OnceLock::new();

/// Install the opened audit log + the `[audit].enabled` flag as the process-
/// global, once at startup. Idempotent. A disabled install is valid + inert.
pub fn install(enabled: bool, log: Arc<AuditLog>) {
    let _ = GLOBAL.set((enabled, log));
    info!(enabled, "audit: installed the audit log");
}

/// Borrow the installed (enabled, log), if any. The HUD telemetry path and the
/// `audit` command-channel verb use this to read `recent`/`verify_chain`.
pub fn global() -> Option<(bool, Arc<AuditLog>)> {
    GLOBAL.get().cloned()
}

/// The chokepoint record path: append one decision to the global log when audit
/// is installed AND enabled; otherwise a no-op. SECRET-FREE (the log redacts the
/// target). Fire-and-forget on the success path; a DB error is logged, never
/// fatal — a missed audit record must never block or fail a user action. This is
/// the single call every chokepoint uses, so adding/removing the audit dep is one
/// edit per decision point.
pub async fn record_global(
    agent: &str,
    tool: &str,
    target: &str,
    decision: Decision,
    outcome: Outcome,
) {
    let Some((true, log)) = GLOBAL.get() else { return };
    if let Err(e) = log.record(agent, tool, target, decision, outcome).await {
        warn!(error = %e, tool, "audit: failed to record a consequential decision");
    }
}

/// How many recent entries the HUD audit-timeline snapshot carries (newest-first).
/// Bounded so the read + the wire payload stay cheap; the full bounded log lives
/// on-device and the snapshot's `total` tells the HUD how many it is summarizing.
pub const SNAPSHOT_RECENT: usize = 50;

/// Build the SECRET-FREE `audit.snapshot` wire payload the HUD's AuditPanel reads.
///
/// PURE + read-only: it folds the already-stored, already-redacted fields the read
/// API returns into the exact shape `parseAuditSnapshot` (hud/src/core/events.ts)
/// consumes — NOTHING is invented here. The internal chain bytes
/// (`prev_hash`/`entry_hash`) are deliberately NOT carried: the operator reads the
/// decision/outcome timeline + the single chain verdict, not the raw hashes, so
/// even the wire shape cannot smuggle a chain byte. `enabled=false` yields the
/// honest "audit OFF" payload (no entries, chain not-verified) so the panel renders
/// the OFF state rather than a stale or fabricated one.
///
/// `entries` MUST already be newest-first (as [`AuditLog::recent`] returns them);
/// they are surfaced verbatim — an empty slice is the honest "nothing recorded yet"
/// state, NEVER backfilled.
pub fn snapshot_json(
    enabled: bool,
    total: usize,
    entries: &[AuditEntry],
    chain: &ChainStatus,
) -> serde_json::Value {
    let chain_json = match chain {
        ChainStatus::Ok { count } => serde_json::json!({ "ok": true, "count": count }),
        ChainStatus::Broken { seq, reason } => serde_json::json!({
            "ok": false,
            "broken_seq": seq,
            "reason": reason,
        }),
    };
    let entries_json: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "seq": e.seq,
                "ts": e.ts,
                "agent": e.agent,
                "tool": e.tool,
                // The ALREADY-redacted target summary (redacted twice daemon-side).
                "target_redacted": e.target_redacted,
                "decision": e.decision,
                "outcome": e.outcome,
            })
        })
        .collect();
    // `truncated` is surfaced LIVE via the separate `audit.truncated` event the
    // prune path emits; the snapshot reports the durable count, so a re-rooted
    // chain still verifies as a fresh chain (count == surviving suffix).
    serde_json::json!({
        "enabled": enabled,
        "total": total,
        "chain": chain_json,
        "entries": entries_json,
    })
}

/// Read the installed global audit log and emit one SECRET-FREE `audit.snapshot`
/// telemetry frame for the HUD's AuditPanel. Fire-and-forget through the existing
/// telemetry hub; dropped silently when no HUD is connected.
///
/// HONESTY + SAFETY:
///   - READ-ONLY. Calls only the read API (`len`/`recent`/`verify_chain`) — it
///     never records, prunes, or mutates the log.
///   - When audit is OFF (or no log installed) it emits the honest `enabled:false`
///     payload so the panel shows the OFF state, NOT a stale or fabricated one.
///   - A read error degrades to NOT emitting that tick (warn-and-continue) rather
///     than emitting a fabricated/partial snapshot — a missed frame is recoverable
///     on the next tick; a lie is not.
pub async fn emit_snapshot() {
    let Some((enabled, log)) = GLOBAL.get() else {
        // No log installed at all — emit the honest OFF payload once so the panel
        // does not sit on a stale snapshot.
        crate::telemetry::emit(
            "system",
            "audit.snapshot",
            snapshot_json(false, 0, &[], &ChainStatus::Ok { count: 0 }),
        );
        return;
    };
    if !*enabled {
        // Audit is OFF: recording is skipped, so report the honest OFF state
        // (no entries, no verified chain) without touching the DB.
        crate::telemetry::emit(
            "system",
            "audit.snapshot",
            snapshot_json(false, 0, &[], &ChainStatus::Ok { count: 0 }),
        );
        return;
    }
    let total = match log.len().await {
        Ok(n) => n,
        Err(e) => {
            warn!(error = %e, "audit: failed to read len for snapshot; skipping this tick");
            return;
        }
    };
    let entries = match log.recent(SNAPSHOT_RECENT).await {
        Ok(es) => es,
        Err(e) => {
            warn!(error = %e, "audit: failed to read recent for snapshot; skipping this tick");
            return;
        }
    };
    let chain = match log.verify_chain().await {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "audit: failed to verify chain for snapshot; skipping this tick");
            return;
        }
    };
    crate::telemetry::emit(
        "system",
        "audit.snapshot",
        snapshot_json(true, total, &entries, &chain),
    );
}

/// PURE chain verifier over an ordered (by seq ASC) slice of entries. Factored
/// out of [`AuditLog::verify_chain`] so the chain logic is unit-testable directly
/// (and so the DB method and the test exercise the exact same code). An empty
/// chain is trivially OK.
fn verify_entries(entries: &[AuditEntry]) -> ChainStatus {
    let mut expected_prev = GENESIS_PREV.to_string();
    let mut expected_seq = 1i64; // re-rooted chains are verified as a fresh chain
    for (i, e) in entries.iter().enumerate() {
        // The first entry anchors to GENESIS; each subsequent prev_hash must equal
        // the prior entry_hash. A reorder / mid-chain delete / insert breaks this.
        if e.prev_hash != expected_prev {
            return ChainStatus::Broken {
                seq: e.seq,
                reason: "prev_hash does not link to the prior entry (reorder/insert/delete)".into(),
            };
        }
        // Seq must be strictly increasing by 1 from the first entry's own seq.
        // (We seed expected_seq from the first entry so a re-rooted suffix that
        // legitimately starts above 1 still verifies, while a GAP within the
        // retained chain is caught.)
        if i == 0 {
            expected_seq = e.seq;
        }
        if e.seq != expected_seq {
            return ChainStatus::Broken {
                seq: e.seq,
                reason: "sequence gap (a deletion that did not re-root)".into(),
            };
        }
        // The recomputed content hash must equal the stored one — catches any
        // mutated field.
        let recomputed = hash_entry(
            &e.prev_hash,
            e.seq,
            &e.ts,
            &e.agent,
            &e.tool,
            &e.target_redacted,
            &e.decision,
            &e.outcome,
        );
        if recomputed != e.entry_hash {
            return ChainStatus::Broken {
                seq: e.seq,
                reason: "entry_hash mismatch (a field was altered)".into(),
            };
        }
        expected_prev = e.entry_hash.clone();
        expected_seq += 1;
    }
    ChainStatus::Ok {
        count: entries.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn log_some(log: &AuditLog) {
        log.record("agent.pepper", "gmail_send", "a@b.com", Decision::Ask, Outcome::Parked)
            .await
            .unwrap();
        log.record("agent.veronica", "x_post", "tweet", Decision::Always, Outcome::AutoApprovedByPolicy)
            .await
            .unwrap();
        log.record("agent.pepper", "slack_post_message", "#ops", Decision::Never, Outcome::BlockedByPolicy)
            .await
            .unwrap();
    }

    /// The CONFIGURED retention cap ([audit].max_entries) is honored, not the
    /// compile-time `MAX_ENTRIES` — the knob was documented + parsed but never read.
    #[tokio::test]
    async fn retention_honors_a_custom_max_entries() {
        // A tiny cap (3), far below the 10_000 default.
        let log = AuditLog::in_memory().unwrap().with_max_entries(3);
        for i in 0..6 {
            log.record("agent.darwin", "open_url", &format!("t{i}"), Decision::Ask, Outcome::Parked)
                .await
                .unwrap();
        }
        assert_eq!(
            log.len().await.unwrap(),
            3,
            "retention pruned to the CONFIGURED cap, not MAX_ENTRIES"
        );
        // The chain stays intact across the re-root.
        assert!(log.verify_chain().await.unwrap().is_ok(), "chain re-roots cleanly at the custom cap");

        // A 0 / absent config keeps the default cap — a typo can never DISABLE
        // retention (which would let the log grow unbounded). A handful of records
        // stays unpruned (5 < MAX_ENTRIES).
        let dflt = AuditLog::in_memory().unwrap().with_max_entries(0);
        for i in 0..5 {
            dflt.record("a", "t", &format!("{i}"), Decision::Ask, Outcome::Parked).await.unwrap();
        }
        assert_eq!(dflt.len().await.unwrap(), 5, "with_max_entries(0) keeps the default (no premature prune)");
    }

    // -- at-rest encryption (#11) ---------------------------------------------

    #[tokio::test]
    async fn open_encrypted_round_trips_and_is_ciphertext_at_rest() {
        // Hermetic temp file + an EXPLICIT in-test key (no Keychain, no network).
        let path = std::env::temp_dir().join(format!("darwin-audit-enc-{}.db", std::process::id()));
        for s in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{s}", path.display()));
        }
        let key = crate::crypto::SecretKey::from_bytes([8u8; crate::crypto::KEY_BYTES]);
        {
            let log = AuditLog::open_encrypted(&path, &key).unwrap();
            log.record("agent.pepper", "gmail_send", "audit-canary-target", Decision::Ask, Outcome::Parked)
                .await
                .unwrap();
        }
        // On-disk is ciphertext: no SQLite magic header (it's SQLCipher).
        let raw = std::fs::read(&path).unwrap();
        assert!(!raw.starts_with(b"SQLite format 3\0"), "audit DB must be encrypted");
        // Reopen WITH the key reads back; the chain verifies.
        {
            let log = AuditLog::open_encrypted(&path, &key).unwrap();
            assert_eq!(log.len().await.unwrap(), 1);
            assert!(log.verify_chain().await.unwrap().is_ok());
        }
        for s in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{s}", path.display()));
        }
    }

    // -- chain integrity + verify_chain ---------------------------------------

    #[tokio::test]
    async fn a_fresh_chain_verifies() {
        let log = AuditLog::in_memory().unwrap();
        log_some(&log).await;
        assert_eq!(log.len().await.unwrap(), 3);
        let status = log.verify_chain().await.unwrap();
        assert!(status.is_ok(), "a fresh chain must verify: {status:?}");
        if let ChainStatus::Ok { count } = status {
            assert_eq!(count, 3);
        }
    }

    #[tokio::test]
    async fn an_empty_chain_verifies() {
        let log = AuditLog::in_memory().unwrap();
        assert!(log.verify_chain().await.unwrap().is_ok());
    }

    /// TAMPER: mutate a stored field. The recomputed entry_hash diverges -> Broken.
    #[tokio::test]
    async fn mutating_a_field_breaks_the_chain() {
        let log = AuditLog::in_memory().unwrap();
        log_some(&log).await;
        {
            let conn = log.conn.lock().await;
            // Change the recipient on entry 2 WITHOUT recomputing the hash.
            conn.execute("UPDATE audit SET target_redacted = 'evil@x.com' WHERE seq = 2", [])
                .unwrap();
        }
        match log.verify_chain().await.unwrap() {
            ChainStatus::Broken { seq, .. } => assert_eq!(seq, 2, "tamper detected at the mutated entry"),
            ChainStatus::Ok { .. } => panic!("a mutated field must break the chain"),
        }
    }

    /// TAMPER: delete a middle entry. The next entry's prev_hash no longer links
    /// (and a seq gap appears) -> Broken.
    #[tokio::test]
    async fn deleting_a_middle_entry_breaks_the_chain() {
        let log = AuditLog::in_memory().unwrap();
        log_some(&log).await;
        {
            let conn = log.conn.lock().await;
            conn.execute("DELETE FROM audit WHERE seq = 2", []).unwrap();
        }
        match log.verify_chain().await.unwrap() {
            ChainStatus::Broken { seq, .. } => assert_eq!(seq, 3, "the deletion is caught at the orphaned next entry"),
            ChainStatus::Ok { .. } => panic!("a mid-chain delete must break the chain"),
        }
    }

    /// TAMPER: insert a forged entry. Its prev_hash cannot match the real prior
    /// hash AND keep the following link intact -> Broken.
    #[tokio::test]
    async fn inserting_a_forged_entry_breaks_the_chain() {
        let log = AuditLog::in_memory().unwrap();
        log_some(&log).await; // seqs 1,2,3
        {
            let conn = log.conn.lock().await;
            // Shift seq 3 to 4 to make room, then insert a forged seq 3 with a
            // plausible-but-wrong hash. The forged entry cannot satisfy both its
            // own prev-link and entry-4's prev-link.
            conn.execute("UPDATE audit SET seq = 4 WHERE seq = 3", []).unwrap();
            conn.execute(
                "INSERT INTO audit(seq, ts, agent, tool, target_redacted, decision, outcome, prev_hash, entry_hash)
                 VALUES (3, '2026-01-01T00:00:00+00:00', 'agent.evil', 'gmail_send', 'attacker@x.com', 'always', 'executed', 'forged', 'forgedhash')",
                [],
            ).unwrap();
        }
        match log.verify_chain().await.unwrap() {
            ChainStatus::Broken { .. } => {}
            ChainStatus::Ok { .. } => panic!("a forged insert must break the chain"),
        }
    }

    /// TAMPER: reorder two entries (swap their seq). The prev-link chain breaks.
    #[tokio::test]
    async fn reordering_entries_breaks_the_chain() {
        let log = AuditLog::in_memory().unwrap();
        log_some(&log).await;
        {
            let conn = log.conn.lock().await;
            // Swap seq 1 and 2 via a temporary id.
            conn.execute("UPDATE audit SET seq = 99 WHERE seq = 1", []).unwrap();
            conn.execute("UPDATE audit SET seq = 1 WHERE seq = 2", []).unwrap();
            conn.execute("UPDATE audit SET seq = 2 WHERE seq = 99", []).unwrap();
        }
        match log.verify_chain().await.unwrap() {
            ChainStatus::Broken { .. } => {}
            ChainStatus::Ok { .. } => panic!("a reorder must break the chain"),
        }
    }

    // -- SECRET-FREE ----------------------------------------------------------

    /// A token in the target NEVER lands in the log: record redacts it, and no
    /// stored field contains the raw secret. (`record`'s own re-redaction is the
    /// backstop even if a caller forgets.)
    #[tokio::test]
    async fn a_token_in_the_target_never_lands_in_the_log() {
        let log = AuditLog::in_memory().unwrap();
        let secret = "sk-ABCdef0123456789LIVEKEY";
        log.record(
            "agent.pepper",
            "gmail_send",
            &format!("send to bob with key {secret}"),
            Decision::Ask,
            Outcome::Parked,
        )
        .await
        .unwrap();
        let entries = log.recent(10).await.unwrap();
        assert_eq!(entries.len(), 1);
        for e in &entries {
            assert!(
                !e.target_redacted.contains(secret),
                "the raw token must never be stored: {}",
                e.target_redacted
            );
            assert!(e.target_redacted.contains("[redacted]"), "the secret was redacted");
        }
        // The chain still verifies with the redacted content.
        assert!(log.verify_chain().await.unwrap().is_ok());
    }

    // -- recent() ordering ----------------------------------------------------

    #[tokio::test]
    async fn recent_returns_newest_first() {
        let log = AuditLog::in_memory().unwrap();
        log_some(&log).await;
        let recent = log.recent(2).await.unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].seq, 3, "newest first");
        assert_eq!(recent[1].seq, 2);
    }

    // -- bounded retention + re-root ------------------------------------------

    /// After pruning past the cap, the surviving suffix re-roots and STILL
    /// verifies as a fresh chain (truncation keeps integrity).
    #[test]
    fn truncation_re_roots_and_still_verifies() {
        // Drive the pure prune logic over a hand-built chain to keep it fast +
        // deterministic, then verify the re-rooted result.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE audit(seq INTEGER PRIMARY KEY, ts TEXT, agent TEXT, tool TEXT,
             target_redacted TEXT, decision TEXT, outcome TEXT, prev_hash TEXT, entry_hash TEXT);",
        )
        .unwrap();
        // Build a valid 5-entry chain by hand.
        let mut prev = GENESIS_PREV.to_string();
        for seq in 1..=5i64 {
            let ts = format!("2026-01-0{seq}T00:00:00+00:00");
            let h = hash_entry(&prev, seq, &ts, "agent.pepper", "gmail_send", "a@b.com", "ask", "parked");
            conn.execute(
                "INSERT INTO audit VALUES (?1,?2,'agent.pepper','gmail_send','a@b.com','ask','parked',?3,?4)",
                params![seq, ts, prev, h],
            )
            .unwrap();
            prev = h;
        }
        // Keep only the newest 2 -> prune seqs 1,2,3, re-root at seq 4.
        AuditLog::prune_and_reroot(&conn, 2).unwrap();

        // Read back + verify the re-rooted suffix.
        let mut stmt = conn
            .prepare("SELECT seq, ts, agent, tool, target_redacted, decision, outcome, prev_hash, entry_hash FROM audit ORDER BY seq ASC")
            .unwrap();
        let entries: Vec<AuditEntry> = stmt
            .query_map([], |row| {
                Ok(AuditEntry {
                    seq: row.get(0)?,
                    ts: row.get(1)?,
                    agent: row.get(2)?,
                    tool: row.get(3)?,
                    target_redacted: row.get(4)?,
                    decision: row.get(5)?,
                    outcome: row.get(6)?,
                    prev_hash: row.get(7)?,
                    entry_hash: row.get(8)?,
                })
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(entries.len(), 2, "only the newest 2 survive");
        assert_eq!(entries[0].seq, 4, "seq is preserved (the gap is the prune evidence)");
        assert_eq!(entries[0].prev_hash, GENESIS_PREV, "the new oldest is re-rooted to genesis");
        assert!(
            verify_entries(&entries).is_ok(),
            "the re-rooted suffix must verify as a fresh chain"
        );
    }

    // -- HUD snapshot wire shape (audit.snapshot) -----------------------------

    /// The snapshot folds the REAL stored, redacted fields into the wire shape the
    /// HUD parses — newest-first — and carries NO chain bytes (prev_hash/entry_hash)
    /// nor any raw input. Pins the secret-free contract at the wire boundary.
    #[tokio::test]
    async fn snapshot_json_surfaces_only_the_secret_free_subset_newest_first() {
        let log = AuditLog::in_memory().unwrap();
        log_some(&log).await; // seqs 1,2,3
        let total = log.len().await.unwrap();
        let entries = log.recent(SNAPSHOT_RECENT).await.unwrap();
        let chain = log.verify_chain().await.unwrap();
        let snap = snapshot_json(true, total, &entries, &chain);

        assert_eq!(snap["enabled"], serde_json::json!(true));
        assert_eq!(snap["total"], serde_json::json!(3));
        assert_eq!(snap["chain"]["ok"], serde_json::json!(true));
        assert_eq!(snap["chain"]["count"], serde_json::json!(3));

        let arr = snap["entries"].as_array().expect("entries array");
        assert_eq!(arr.len(), 3, "all three recorded decisions are surfaced");
        // Newest-first: the last recorded (slack_post_message, BLOCKED) is row 0.
        assert_eq!(arr[0]["seq"], serde_json::json!(3));
        assert_eq!(arr[0]["tool"], serde_json::json!("slack_post_message"));
        assert_eq!(arr[0]["decision"], serde_json::json!("never"));
        assert_eq!(arr[0]["outcome"], serde_json::json!("blocked_by_policy"));

        // The chain bytes are NEVER on the wire — only the verdict is.
        let whole = snap.to_string();
        assert!(!whole.contains("prev_hash"), "no chain bytes on the wire: {whole}");
        assert!(!whole.contains("entry_hash"), "no chain bytes on the wire: {whole}");
        for e in arr {
            let obj = e.as_object().unwrap();
            assert!(obj.contains_key("target_redacted"), "the redacted target is the only target field");
            assert!(!obj.contains_key("prev_hash"));
            assert!(!obj.contains_key("entry_hash"));
        }
    }

    /// A secret in the target is redacted BEFORE it reaches the wire snapshot too —
    /// the raw token never appears in the emitted JSON.
    #[tokio::test]
    async fn snapshot_json_never_carries_a_raw_secret() {
        let log = AuditLog::in_memory().unwrap();
        let secret = "sk-LIVE-DEADBEEF-0123456789";
        log.record("agent.pepper", "gmail_send", &format!("key {secret}"), Decision::Ask, Outcome::Parked)
            .await
            .unwrap();
        let entries = log.recent(SNAPSHOT_RECENT).await.unwrap();
        let chain = log.verify_chain().await.unwrap();
        let snap = snapshot_json(true, 1, &entries, &chain);
        assert!(!snap.to_string().contains(secret), "the raw secret must never reach the wire");
    }

    /// HONEST EMPTY/OFF: a disabled (or empty) snapshot carries enabled:false, zero
    /// total, and NO fabricated entries — the panel renders the OFF/empty state,
    /// never an invented decision.
    #[test]
    fn snapshot_json_off_state_is_honest_and_empty() {
        let snap = snapshot_json(false, 0, &[], &ChainStatus::Ok { count: 0 });
        assert_eq!(snap["enabled"], serde_json::json!(false));
        assert_eq!(snap["total"], serde_json::json!(0));
        assert_eq!(
            snap["entries"].as_array().expect("entries array").len(),
            0,
            "an OFF/empty snapshot fabricates no entries"
        );
    }

    /// A BROKEN chain is reported honestly on the wire (ok:false + where/why),
    /// never silently downgraded to a green verdict.
    #[test]
    fn snapshot_json_reports_a_broken_chain_honestly() {
        let snap = snapshot_json(
            true,
            5,
            &[],
            &ChainStatus::Broken { seq: 2, reason: "hash mismatch".into() },
        );
        assert_eq!(snap["chain"]["ok"], serde_json::json!(false));
        assert_eq!(snap["chain"]["broken_seq"], serde_json::json!(2));
        assert_eq!(snap["chain"]["reason"], serde_json::json!("hash mismatch"));
    }

    // -- triage evidence records ----------------------------------------------

    /// A forensic-triage evidence record folds the bundle's SHA-256 into the chain
    /// VERBATIM (a 64-hex digest is non-secret and must survive — `record`'s
    /// redactor would collapse it to `[redacted]`), the chain still verifies, and
    /// the entry reads honestly as evidence (its own agent/tool/decision/outcome).
    #[tokio::test]
    async fn triage_evidence_stores_the_digest_verbatim_and_verifies() {
        let log = AuditLog::in_memory().unwrap();
        // A realistic manifest SHA-256 (64 lowercase hex chars) — exactly the shape
        // optimize::redact would otherwise treat as a secret-looking token.
        let sha = "9f2c4e1a7b3d5f6081a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f7081";
        let entry = log.record_triage_evidence("2026-07-15T12-00-00Z", sha).await.unwrap();
        assert_eq!(entry.agent, "agent.aegis");
        assert_eq!(entry.tool, "triage_snapshot");
        assert_eq!(entry.outcome, "evidence_captured");
        assert!(
            entry.target_redacted.contains(sha),
            "the digest must be stored verbatim to anchor the bundle: {}",
            entry.target_redacted
        );
        assert!(!entry.target_redacted.contains("[redacted]"), "the digest was NOT redacted away");
        assert!(log.verify_chain().await.unwrap().is_ok(), "the chain verifies with the evidence entry");
    }

    // -- external anchor: an in-memory injected seam --------------------------

    /// A hermetic in-memory anchor — the injected seam. Tests exercise
    /// write/verify/mismatch WITHOUT ever touching the real Keychain.
    #[derive(Default)]
    struct MemAnchor {
        value: Mutex<Option<String>>,
    }
    impl ChainAnchor for MemAnchor {
        async fn read_anchor(&self) -> Option<String> {
            self.value.lock().await.clone()
        }
        async fn write_anchor(&self, value: &str) -> Result<()> {
            *self.value.lock().await = Some(value.to_string());
            Ok(())
        }
    }

    #[test]
    fn parse_anchor_reads_seq_and_hash_and_rejects_junk() {
        assert_eq!(parse_anchor(" 3 : abc123 ").unwrap(), (3, "abc123".to_string()));
        assert!(parse_anchor("no-colon").is_none());
        assert!(parse_anchor("x:abc").is_none(), "non-numeric seq");
        assert!(parse_anchor("3:").is_none(), "empty hash");
    }

    #[tokio::test]
    async fn anchoring_an_empty_chain_writes_nothing_and_reads_no_anchor() {
        let log = AuditLog::in_memory().unwrap();
        let anchor = MemAnchor::default();
        assert_eq!(log.anchor_to(&anchor).await.unwrap(), None, "nothing to witness on an empty chain");
        assert_eq!(log.verify_against_anchor(&anchor).await.unwrap(), AnchorStatus::NoAnchor);
    }

    #[tokio::test]
    async fn anchor_then_verify_matches_the_witnessed_head() {
        let log = AuditLog::in_memory().unwrap();
        log_some(&log).await; // seqs 1,2,3
        let anchor = MemAnchor::default();
        let (seq, head) = log.anchor_to(&anchor).await.unwrap().unwrap();
        assert_eq!(seq, 3, "the head is the highest seq");
        let status = log.verify_against_anchor(&anchor).await.unwrap();
        assert!(status.is_match(), "a freshly-anchored head must match: {status:?}");
        assert_eq!(status, AnchorStatus::Match { seq, head });
    }

    /// THE money property: a root attacker who rewrites the ENTIRE chain
    /// CONSISTENTLY still passes `verify_chain` (the local chain cannot tell), but
    /// the rewrite MOVES the tail hash, so it no longer equals the externally
    /// witnessed head — `verify_against_anchor` reports the tamper.
    #[tokio::test]
    async fn a_full_consistent_rewrite_is_caught_by_the_external_anchor() {
        let log = AuditLog::in_memory().unwrap();
        log_some(&log).await; // seqs 1,2,3
        let anchor = MemAnchor::default();
        log.anchor_to(&anchor).await.unwrap();

        // Simulate a root attacker: delete every row and re-insert a fully
        // self-consistent chain with a CHANGED field (so `verify_chain` passes but
        // the tail hash differs from the witnessed one).
        {
            let conn = log.conn.lock().await;
            conn.execute("DELETE FROM audit", []).unwrap();
            let mut prev = GENESIS_PREV.to_string();
            for seq in 1..=3i64 {
                let ts = format!("2026-02-0{seq}T00:00:00+00:00");
                // The forged recipient — the edit the attacker wants to hide.
                let h = hash_entry(&prev, seq, &ts, "agent.evil", "gmail_send", "attacker@x.com", "always", "executed");
                conn.execute(
                    "INSERT INTO audit VALUES (?1,?2,'agent.evil','gmail_send','attacker@x.com','always','executed',?3,?4)",
                    params![seq, ts, prev, h],
                ).unwrap();
                prev = h;
            }
        }
        // The internal chain STILL verifies — the local tripwire is defeated.
        assert!(log.verify_chain().await.unwrap().is_ok(), "a consistent rewrite passes the local chain");
        // ...but the external anchor catches it.
        match log.verify_against_anchor(&anchor).await.unwrap() {
            AnchorStatus::Mismatch { anchored_seq, live, .. } => {
                assert_eq!(anchored_seq, 3);
                assert!(live.is_some(), "the live head is reported alongside the witnessed one");
            }
            other => panic!("a full rewrite must mismatch the anchor, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn append_only_growth_is_extended_not_a_mismatch() {
        // THE bug the review caught: after anchoring, ordinary new records advance the
        // head every session, and the check must NOT cry tamper on legitimate
        // append-only growth (else it false-alarms every boot → alarm fatigue).
        let log = AuditLog::in_memory().unwrap();
        log_some(&log).await; // seqs 1,2,3
        let anchor = MemAnchor::default();
        let (a_seq, a_head) = log.anchor_to(&anchor).await.unwrap().unwrap();
        log_some(&log).await; // a new session grows the chain: seqs 4,5,6
        match log.verify_against_anchor(&anchor).await.unwrap() {
            AnchorStatus::Extended { anchored_seq, anchored_head, live } => {
                assert_eq!(anchored_seq, a_seq);
                assert_eq!(anchored_head, a_head, "witnessed entry still corroborated");
                assert!(live.0 > a_seq, "the chain grew past the witness");
            }
            other => panic!("append-only growth must be Extended (benign), got {other:?}"),
        }
        // reanchor-on-start advances the witness forward on a benign Extended, so it
        // does NOT stick — the next check is an exact match.
        let (new_seq, _) = log.anchor_to(&anchor).await.unwrap().unwrap();
        assert!(new_seq > a_seq);
        assert!(log.verify_against_anchor(&anchor).await.unwrap().is_match());
    }

    #[tokio::test]
    async fn a_changed_witnessed_entry_is_a_mismatch_even_when_the_chain_grew() {
        // Growth is no cover for tampering: if the entry at the witnessed seq changed,
        // it is a Mismatch even though the chain also grew past it.
        let log = AuditLog::in_memory().unwrap();
        log_some(&log).await; // 1,2,3
        let anchor = MemAnchor::default();
        let (a_seq, _) = log.anchor_to(&anchor).await.unwrap().unwrap();
        {
            let conn = log.conn.lock().await;
            conn.execute("UPDATE audit SET entry_hash = 'deadbeef' WHERE seq = ?1", params![a_seq])
                .unwrap();
        }
        log_some(&log).await; // still grows the head
        match log.verify_against_anchor(&anchor).await.unwrap() {
            AnchorStatus::Mismatch { anchored_seq, .. } => assert_eq!(anchored_seq, a_seq),
            other => panic!("a changed witnessed entry must Mismatch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_malformed_anchor_is_reported_honestly() {
        let log = AuditLog::in_memory().unwrap();
        log_some(&log).await;
        let anchor = MemAnchor::default();
        anchor.write_anchor("this is not seq:hash shaped enough").await.unwrap();
        // "not:hash..." actually splits on ':' — force a truly malformed value.
        *anchor.value.lock().await = Some("garbage-no-colon".to_string());
        assert!(matches!(
            log.verify_against_anchor(&anchor).await.unwrap(),
            AnchorStatus::Malformed { .. }
        ));
    }

    /// The `audit.anchor` wire payload reports the verdict honestly and carries no
    /// raw input — only public digests + integers.
    #[test]
    fn anchor_status_json_is_honest_and_secret_free() {
        let ok = anchor_status_json(&AnchorStatus::Match { seq: 7, head: "abc".into() });
        assert_eq!(ok["ok"], serde_json::json!(true));
        assert_eq!(ok["state"], serde_json::json!("match"));
        assert_eq!(ok["seq"], serde_json::json!(7));

        let bad = anchor_status_json(&AnchorStatus::Mismatch {
            anchored_seq: 3,
            anchored_head: "witnessed".into(),
            live: Some((5, "livehash".into())),
        });
        assert_eq!(bad["ok"], serde_json::json!(false));
        assert_eq!(bad["state"], serde_json::json!("mismatch"));
        assert_eq!(bad["anchored_seq"], serde_json::json!(3));
        assert_eq!(bad["live_seq"], serde_json::json!(5));

        assert_eq!(
            anchor_status_json(&AnchorStatus::NoAnchor)["state"],
            serde_json::json!("no_anchor")
        );
    }
}
