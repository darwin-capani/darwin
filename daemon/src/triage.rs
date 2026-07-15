//! FORENSIC TRIAGE SNAPSHOT (agent "aegis", Defense & Privacy) — a one-shot
//! "capture everything" that FREEZES a READ-ONLY, REDACTED, timestamped evidence
//! bundle so the owner can hand a professional real evidence.
//!
//! SAFETY / HONESTY CONTRACT (the same discipline as posture.rs / tcc.rs /
//! persistence.rs / secret_scan.rs):
//!   * STRICTLY READ-ONLY re: the machine. Every OS read is a FIXED-ARG bounded
//!     subprocess (an absolute program path + fixed args, NEVER a shell string),
//!     with a timeout + kill_on_drop — the SAME `run_command` shape the other
//!     sentinels use. `codesign -dv` is an ASSESSMENT read: it inspects a binary's
//!     signature, it NEVER executes it. No kills, no unloads, no config/security
//!     changes, no remediation — RESTORE is never automated (triage only captures).
//!   * REDACTED AT SOURCE. Every captured value passes through [`redact_section`]
//!     ([`crate::optimize::redact`] for secret-/PII-shaped tokens + emails +
//!     credentialed URLs + phones, then [`crate::introspect::redact_home`] to strip
//!     the home path) BEFORE it lands in the bundle. The bundle manifest carries
//!     item counts + per-file SHA-256 + honest notes — never a raw secret.
//!   * NOTHING IS TRANSMITTED. The bundle exists only on local disk; this module
//!     opens no socket and calls no network. It writes ONLY under
//!     `state/forensics/<ts>/` (a canonical starts-with confinement check), NEVER
//!     user data, and marks every written file read-only.
//!   * BOUNDED. A per-section byte cap + a whole-bundle budget
//!     (`[triage].max_bundle_bytes`) + a bounded `log show` window
//!     (`[triage].log_window_minutes`) keep a pathological host from wedging or
//!     flooding the capture.
//!   * HONEST. Unified-log `<private>` fields stay redacted by the OS without admin
//!     — the manifest says so rather than implying it captured them. A read that
//!     cannot run degrades to an explicit "unavailable" section, never a fabricated
//!     one.
//!
//! AUDIT ANCHOR: after freezing the bundle, its manifest SHA-256 is folded into the
//! append-only hash chain ([`crate::audit::AuditLog::record_triage_evidence`]) and
//! the resulting chain HEAD is written to the macOS Keychain as an EXTERNAL ANCHOR
//! ([`crate::audit::AuditLog::anchor_to`]) — a SEPARATE OS protection domain a
//! SQLite-file attacker cannot silently rewrite. So the ledger vouches for the
//! bundle, and the bundle is tied to an externally-witnessed chain head.
//!
//! DEAD-CODE: the on-demand `capture` op is invoked by the aegis tool arm / the
//! authenticated-local command channel that lands at integration (the parent wires
//! `aegis_triage` -> `triage::capture`, mirroring `aegis_report`); its pure
//! parsers/redaction/manifest cores are unit-tested here. Until that arm lands the
//! unused-item lint would flag the capture surface, so `dead_code` is allowed
//! module-wide — the same "a component that lands next reads this" rationale
//! `audit.rs` uses.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Result};
use rusqlite::{Connection, OpenFlags};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::process::Command;
use tracing::warn;

// ---------------------------------------------------------------------------
// Fixed read-only command set — each an absolute program + fixed args, never a
// shell string (posture.rs / persistence.rs discipline).
// ---------------------------------------------------------------------------

/// Process table: pid, ppid, user, cpu, mem, and the executable path (`comm`
/// prints the full path on macOS). Read-only.
const PS: &str = "/bin/ps";
/// Socket table: all sockets, numeric (no DNS), read-only.
const NETSTAT: &str = "/usr/sbin/netstat";
/// Code-signature DISPLAY — READ-ONLY, never executes the binary.
const CODESIGN: &str = "/usr/bin/codesign";
/// Unified-log reader (bounded window + predicate). Read-only.
const LOG: &str = "/usr/bin/log";

/// Per-read timeout (the same 5s discipline as the sentinels).
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// `log show` can scan a large store; give it headroom but still bound it.
const LOG_TIMEOUT: Duration = Duration::from_secs(30);

/// The unified-log predicate targeting the security subsystems (sandbox / privacy
/// / notarization / firewall daemons). Fixed — never built from external input.
const SECURITY_LOG_PREDICATE: &str =
    "process == \"sandboxd\" OR process == \"tccd\" OR process == \"syspolicyd\" OR process == \"socketfilterfw\"";

/// Per-section byte cap — one runaway read can never dominate the bundle.
const PER_SECTION_CAP: usize = 256 * 1024;
/// How many DISTINCT resident executables get a (bounded) signing assessment.
const MAX_SIGN_ASSESS: usize = 48;
/// How many recent quarantine events to carry.
const MAX_QUARANTINE_ROWS: usize = 100;

/// The subdirectory (under `state/`) every bundle is confined to. NOTHING is ever
/// written outside `state/forensics/`.
const FORENSICS_SUBDIR: &str = "forensics";

// ---------------------------------------------------------------------------
// Read outcome + injected runner (persistence.rs shape)
// ---------------------------------------------------------------------------

/// The captured outcome of one read: combined stdout+stderr text, or a note that
/// the read itself could not run (missing binary, timed out).
enum ReadOutput {
    Text(String),
    Unavailable(String),
}

/// Spawn one read-only command with explicit args (never a shell string), capture
/// combined stdout+stderr, bound it with `timeout` + kill_on_drop. Mirrors
/// persistence::run_real. Runtime-only.
async fn run_real(program: &'static str, args: Vec<String>, timeout: Duration) -> ReadOutput {
    let mut cmd = Command::new(program);
    cmd.args(&args).kill_on_drop(true);
    match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(out)) => {
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            let err = String::from_utf8_lossy(&out.stderr);
            if !err.trim().is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&err);
            }
            ReadOutput::Text(text)
        }
        Ok(Err(e)) => {
            warn!(program, error = %e, "triage: command could not run");
            ReadOutput::Unavailable("not available on this machine".to_string())
        }
        Err(_) => {
            warn!(program, secs = timeout.as_secs(), "triage: command timed out");
            ReadOutput::Unavailable("the read timed out".to_string())
        }
    }
}

// ---------------------------------------------------------------------------
// Redaction at source (PURE) — every captured byte passes through this.
// ---------------------------------------------------------------------------

/// Redact one captured section: strip the home path, then collapse every
/// secret-/PII-shaped token, LINE BY LINE, and truncate to `cap` bytes on a char
/// boundary. The single choke every section flows through, so no raw secret / PII
/// / home path can reach the bundle.
fn redact_section(text: &str, cap: usize) -> String {
    let mut out = String::with_capacity(text.len().min(cap) + 16);
    for line in text.lines() {
        // redact_home first (path -> ~), then the token redactor over the result.
        let dehomed = crate::introspect::redact_home(line);
        out.push_str(&crate::optimize::redact(&dehomed));
        out.push('\n');
    }
    truncate_on_char_boundary(out, cap)
}

/// Truncate `s` to at most `cap` bytes without splitting a UTF-8 code point,
/// appending an honest marker when it actually cut.
fn truncate_on_char_boundary(mut s: String, cap: usize) -> String {
    if s.len() <= cap {
        return s;
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s.push_str("\n… [truncated at the section cap]\n");
    s
}

// ---------------------------------------------------------------------------
// Pure parsers — one per surface, unit-tested on canned output. No I/O.
// ---------------------------------------------------------------------------

/// One process-table row (already the fields `ps -Ao pid,ppid,user,%cpu,%mem,comm`
/// yields). `comm` is the executable path on macOS.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcRow {
    pid: String,
    ppid: String,
    exec: String,
}

/// PURE: parse `ps -Ao pid,ppid,user,%cpu,%mem,comm` output into rows. The header
/// line and short/malformed rows are skipped. The executable path is everything
/// from the 6th field onward (a path may contain spaces).
fn parse_ps(text: &str) -> Vec<ProcRow> {
    let mut out = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 6 {
            continue;
        }
        if fields[0].eq_ignore_ascii_case("pid") {
            continue; // header
        }
        // pid + ppid must be numeric; otherwise it is not a data row.
        if fields[0].parse::<u64>().is_err() || fields[1].parse::<u64>().is_err() {
            continue;
        }
        // Rejoin the executable path (fields 6..), which may contain spaces.
        let exec = join_from(line, &fields, 5);
        out.push(ProcRow {
            pid: fields[0].to_string(),
            ppid: fields[1].to_string(),
            exec,
        });
    }
    out
}

/// Rejoin the original line's tail from the `n`-th whitespace field onward (so a
/// path with embedded spaces survives). Falls back to the field when the tail
/// cannot be located.
fn join_from(line: &str, fields: &[&str], n: usize) -> String {
    match fields.get(n) {
        Some(f) => match line.find(f) {
            Some(idx) => line[idx..].trim().to_string(),
            None => (*f).to_string(),
        },
        None => String::new(),
    }
}

/// PURE: the DISTINCT absolute executable paths across the process rows, in
/// first-seen order, capped at `max` — the (bounded) set that gets a signing
/// assessment. Non-absolute `comm` values (kernel threads, bracketed names) are
/// skipped (nothing to codesign).
fn distinct_exec_paths(rows: &[ProcRow], max: usize) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for r in rows {
        if !r.exec.starts_with('/') {
            continue;
        }
        if seen.insert(r.exec.clone()) {
            out.push(r.exec.clone());
            if out.len() >= max {
                break;
            }
        }
    }
    out
}

/// A binary's signing verdict (mirrors the persistence sentinel's loud set, kept
/// local so triage carries no dependency on that module's private types).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Signing {
    Unsigned,
    Adhoc,
    Signed,
    Unknown,
}

impl Signing {
    fn label(self) -> &'static str {
        match self {
            Signing::Unsigned => "UNSIGNED",
            Signing::Adhoc => "adhoc",
            Signing::Signed => "signed",
            Signing::Unknown => "unknown",
        }
    }
}

/// PURE: classify `codesign -dv --verbose=2` output (which prints to stderr).
fn classify_codesign(text: &str) -> Signing {
    let low = text.to_lowercase();
    if low.contains("not signed at all") {
        Signing::Unsigned
    } else if low.contains("signature=adhoc") {
        Signing::Adhoc
    } else if text.contains("Authority=") {
        Signing::Signed
    } else {
        Signing::Unknown
    }
}

/// PURE: count the sockets in `netstat -an` output — `(listening, established)`.
/// A generic tally (secret-free) rather than the raw endpoints; the redacted raw
/// table still rides its own section for the professional to read.
fn parse_netstat(text: &str) -> (usize, usize) {
    let mut listening = 0;
    let mut established = 0;
    for line in text.lines() {
        let up = line.to_uppercase();
        if up.contains("LISTEN") {
            listening += 1;
        } else if up.contains("ESTABLISHED") {
            established += 1;
        }
    }
    (listening, established)
}

/// PURE: redact one quarantine event into a single secret-free line. The origin
/// URL / referrer can carry tokens, so the whole formatted line runs through the
/// token redactor; only the host + a short fingerprint survive.
fn format_quarantine_row(ts: &str, agent: &str, url: &str) -> String {
    let line = format!("{ts} | {agent} | {url}");
    crate::optimize::redact(&line)
}

// ---------------------------------------------------------------------------
// Sections + manifest (PURE assembly)
// ---------------------------------------------------------------------------

/// One captured, already-REDACTED evidence section destined for its own file.
#[derive(Debug, Clone)]
struct Section {
    /// Short stable name (also the telemetry key): "processes", "sockets", …
    name: &'static str,
    /// The file it is written to inside the bundle.
    filename: &'static str,
    /// The redacted body (already truncated to the section cap).
    body: String,
    /// A secret-free count of the items the section summarizes.
    item_count: usize,
}

impl Section {
    fn sha256(&self) -> String {
        sha256_hex(self.body.as_bytes())
    }
}

/// Lowercase-hex SHA-256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

/// PURE: fold the captured sections + honest notes + the prior-anchor state into
/// the bundle manifest JSON. SECRET-FREE by construction: only section names,
/// filenames, item counts, per-file SHA-256 (public digests), and fixed notes
/// reach it — never a byte of captured body. The manifest deliberately does NOT
/// contain its own hash (the bundle SHA-256 is `sha256(manifest bytes)`, computed
/// after this serializes).
fn assemble_manifest(
    bundle_id: &str,
    created_ts: &str,
    sections: &[Section],
    notes: &[String],
    prior_anchor: &serde_json::Value,
) -> serde_json::Value {
    let section_json: Vec<serde_json::Value> = sections
        .iter()
        .map(|s| {
            json!({
                "name": s.name,
                "file": s.filename,
                "items": s.item_count,
                "bytes": s.body.len(),
                "sha256": s.sha256(),
            })
        })
        .collect();
    json!({
        "schema": "darwin.triage/1",
        "bundle_id": bundle_id,
        "created_ts": created_ts,
        "read_only": true,
        "transmitted": false,
        "sections": section_json,
        "notes": notes,
        // The audit external-anchor state OBSERVED before this bundle was frozen —
        // forensic context (was the chain already diverged from its witness?).
        "prior_anchor": prior_anchor,
    })
}

/// PURE: is `dir` confined under `<state_dir>/forensics`? The single guard that
/// keeps a capture from ever writing outside its own tree.
fn is_confined(dir: &Path, state_dir: &Path) -> bool {
    dir.starts_with(state_dir.join(FORENSICS_SUBDIR))
}

// ---------------------------------------------------------------------------
// Runtime gatherers (real subprocess / fs / rusqlite reads) — bounded + redacted.
// ---------------------------------------------------------------------------

/// Capture the process table + a bounded signing assessment of the resident
/// executables. Returns the two sections. Runtime-only.
async fn gather_processes() -> (Section, Section) {
    let ps_text = match run_real(
        PS,
        vec!["-Ao".into(), "pid,ppid,user,%cpu,%mem,comm".into()],
        READ_TIMEOUT,
    )
    .await
    {
        ReadOutput::Text(t) => t,
        ReadOutput::Unavailable(why) => format!("(process table unavailable: {why})"),
    };
    let rows = parse_ps(&ps_text);
    let processes = Section {
        name: "processes",
        filename: "processes.txt",
        body: redact_section(&ps_text, PER_SECTION_CAP),
        item_count: rows.len(),
    };

    // Bounded signing assessment (READ-ONLY: codesign -dv never executes).
    let mut sign_body = String::from(
        "path | signing (codesign -dv, READ-ONLY assessment — the binary is never executed)\n",
    );
    let mut assessed = 0usize;
    for path in distinct_exec_paths(&rows, MAX_SIGN_ASSESS) {
        let verdict = match run_real(
            CODESIGN,
            vec!["-dv".into(), "--verbose=2".into(), path.clone()],
            READ_TIMEOUT,
        )
        .await
        {
            ReadOutput::Text(t) => classify_codesign(&t),
            ReadOutput::Unavailable(_) => Signing::Unknown,
        };
        let line = format!("{path} | {}\n", verdict.label());
        // Route through the SAME redaction as every other section — home-strip THEN
        // token/PII redaction. signing.txt was previously home-stripped only, so a
        // secret-shaped path component (or a non-$HOME user path) landed verbatim
        // while the identical path in processes.txt was collapsed to [redacted] (the
        // review caught the inconsistency + the module-header invariant it violated).
        sign_body.push_str(&crate::optimize::redact(&crate::introspect::redact_home(&line)));
        assessed += 1;
    }
    let signing = Section {
        name: "signing",
        filename: "signing.txt",
        body: truncate_on_char_boundary(sign_body, PER_SECTION_CAP),
        item_count: assessed,
    };
    (processes, signing)
}

/// Capture the socket table (`netstat -an`), redacted, with a secret-free count.
async fn gather_sockets() -> Section {
    let text = match run_real(NETSTAT, vec!["-an".into()], READ_TIMEOUT).await {
        ReadOutput::Text(t) => t,
        ReadOutput::Unavailable(why) => format!("(socket table unavailable: {why})"),
    };
    let (listening, established) = parse_netstat(&text);
    Section {
        name: "sockets",
        filename: "sockets.txt",
        body: redact_section(&text, PER_SECTION_CAP),
        item_count: listening + established,
    }
}

/// Capture the machine posture + the TCC and persistence sentinel summaries (the
/// baselines + their current read-only state). These are ALREADY secret-free /
/// redacted read-only reports; re-redacting is defense in depth.
async fn gather_posture_baselines() -> Section {
    let mut body = String::new();
    let mut items = 0usize;

    let posture = crate::posture::local_posture()
        .await
        .unwrap_or_else(|e| format!("machine posture unavailable ({e})"));
    body.push_str("== machine posture ==\n");
    body.push_str(&posture);
    body.push_str("\n\n");
    items += 1;

    let tcc = crate::tcc::snapshot()
        .await
        .unwrap_or_else(|e| format!("TCC inventory unavailable ({e})"));
    body.push_str("== TCC app-privacy grants (baseline surface) ==\n");
    body.push_str(&tcc);
    body.push_str("\n\n");
    items += 1;

    body.push_str("== persistence sentinel (autostart baseline + current diff) ==\n");
    match crate::persistence::posture_line() {
        Some(line) => {
            body.push_str(&line);
            items += 1;
        }
        None => body.push_str("(the persistence sentinel has not completed a scan yet)"),
    }
    body.push('\n');

    Section {
        name: "posture_baselines",
        filename: "posture_baselines.txt",
        body: redact_section(&body, PER_SECTION_CAP),
        item_count: items,
    }
}

/// Capture a bounded, redacted excerpt of the security subsystems' unified log.
async fn gather_security_logs(window_minutes: u64) -> Section {
    let last = format!("{window_minutes}m");
    let text = match run_real(
        LOG,
        vec![
            "show".into(),
            "--predicate".into(),
            SECURITY_LOG_PREDICATE.into(),
            "--last".into(),
            last,
            "--style".into(),
            "syslog".into(),
            "--info".into(),
        ],
        LOG_TIMEOUT,
    )
    .await
    {
        ReadOutput::Text(t) => t,
        ReadOutput::Unavailable(why) => format!("(security log excerpt unavailable: {why})"),
    };
    let line_count = text.lines().count();
    Section {
        name: "security_logs",
        filename: "security_logs.txt",
        body: redact_section(&text, PER_SECTION_CAP),
        item_count: line_count,
    }
}

/// The user's LSQuarantine store (recent "downloaded from the internet" events),
/// relative to `$HOME`.
const QUARANTINE_REL: &str = "Library/Preferences/com.apple.LaunchServices.QuarantineEventsV2";

/// Capture recent quarantine events by opening the LSQuarantine store READ-ONLY
/// (rusqlite `SQLITE_OPEN_READ_ONLY`, the same discipline as tcc.rs). Degrades
/// honestly when the store is absent/unreadable — never a fabricated list.
async fn gather_quarantine() -> Section {
    let body = tokio::task::spawn_blocking(read_quarantine_rows)
        .await
        .unwrap_or_else(|e| Err(anyhow!("quarantine read task failed: {e}")));
    match body {
        Ok((rendered, count)) => Section {
            name: "quarantine",
            filename: "quarantine.txt",
            body: truncate_on_char_boundary(rendered, PER_SECTION_CAP),
            item_count: count,
        },
        Err(e) => Section {
            name: "quarantine",
            filename: "quarantine.txt",
            body: format!("(recent quarantine events unavailable: {e})\n"),
            item_count: 0,
        },
    }
}

/// Sync (rusqlite) read of the recent quarantine events, already redacted. Driven
/// under `spawn_blocking`.
fn read_quarantine_rows() -> Result<(String, usize)> {
    let home = std::env::var("HOME").map_err(|_| anyhow!("$HOME is unset"))?;
    let path = Path::new(&home).join(QUARANTINE_REL);
    if !path.exists() {
        return Err(anyhow!("no quarantine store present"));
    }
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| anyhow!("read-only open failed ({e})"))?;
    let mut stmt = conn.prepare(
        "SELECT LSQuarantineTimeStamp, LSQuarantineAgentName, LSQuarantineDataURLString
         FROM LSQuarantineEvent ORDER BY LSQuarantineTimeStamp DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map([MAX_QUARANTINE_ROWS as i64], |r| {
        let ts: f64 = r.get(0).unwrap_or_default();
        let agent: String = r.get(1).unwrap_or_default();
        let url: String = r.get(2).unwrap_or_default();
        Ok((ts, agent, url))
    })?;
    let mut body = String::from("when(CFAbsoluteTime) | agent | origin (REDACTED)\n");
    let mut count = 0usize;
    for row in rows {
        let (ts, agent, url) = row?;
        body.push_str(&format_quarantine_row(&format!("{ts}"), &agent, &url));
        body.push('\n');
        count += 1;
    }
    Ok((body, count))
}

// ---------------------------------------------------------------------------
// Bundle write (confined + read-only) + the public capture entry
// ---------------------------------------------------------------------------

/// The result of a triage capture (secret-free): where the bundle landed, its
/// manifest digest, per-section item counts, whether the head was anchored, and
/// the anchor state observed BEFORE the capture.
#[derive(Debug, Clone)]
pub struct TriageSummary {
    pub bundle_dir: PathBuf,
    pub manifest_sha256: String,
    pub section_items: BTreeMap<String, usize>,
    pub anchored: bool,
    pub prior_anchor: serde_json::Value,
}

/// FREEZE a forensic triage bundle under `state/forensics/<ts>/`, fold its
/// manifest SHA-256 into the audit chain, and write the resulting chain head to
/// the Keychain external anchor. READ-ONLY re: the machine; NOTHING is
/// transmitted. Returns a secret-free [`TriageSummary`].
pub async fn capture(
    state_dir: &Path,
    cfg: &crate::config::TriageConfig,
) -> Result<TriageSummary> {
    let created_ts = chrono::Utc::now().to_rfc3339();
    // A filesystem-safe bundle id (no ':' — that is the anchor's own delimiter and
    // an illegal path char on some tools).
    let bundle_id = chrono::Utc::now().format("%Y-%m-%dT%H-%M-%SZ").to_string();
    let forensics_root = state_dir.join(FORENSICS_SUBDIR);
    let bundle_dir = forensics_root.join(&bundle_id);

    // CONFINEMENT: refuse to write anywhere but under state/forensics/.
    if !is_confined(&bundle_dir, state_dir) {
        return Err(anyhow!("triage: refusing to write outside state/forensics"));
    }
    std::fs::create_dir_all(&bundle_dir)
        .map_err(|e| anyhow!("triage: cannot create the bundle dir: {e}"))?;

    // Observe the audit external-anchor state BEFORE freezing (forensic context).
    let (audit_on, prior_anchor) = match crate::audit::global() {
        Some((true, log)) => {
            let status = log
                .verify_against_anchor(&crate::audit::KeychainAnchor)
                .await
                .unwrap_or(crate::audit::AnchorStatus::NoAnchor);
            (true, crate::audit::anchor_status_json(&status))
        }
        _ => (false, json!({ "ok": true, "state": "audit_off" })),
    };

    // Gather every section (each redacted at source + bounded).
    let (processes, signing) = gather_processes().await;
    let sockets = gather_sockets().await;
    let posture = gather_posture_baselines().await;
    let logs = gather_security_logs(cfg.log_window_minutes).await;
    let quarantine = gather_quarantine().await;
    let mut sections = vec![processes, signing, sockets, posture, logs, quarantine];

    // Enforce the whole-bundle byte budget: keep sections while the running total
    // fits; a section that would overflow is truncated to the remaining budget, and
    // nothing past it is dropped SILENTLY — a note records the cut.
    let notes = enforce_budget(&mut sections, cfg.max_bundle_bytes);

    // Assemble + serialize the manifest, then digest the manifest bytes: because
    // the manifest carries every section's SHA-256, this one digest transitively
    // anchors every file in the bundle.
    let manifest = assemble_manifest(&bundle_id, &created_ts, &sections, &notes, &prior_anchor);
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|e| anyhow!("triage: manifest serialization failed: {e}"))?;
    let manifest_sha = sha256_hex(&manifest_bytes);

    // Write every section file + the manifest, each read-only.
    for s in &sections {
        write_readonly(&bundle_dir.join(s.filename), s.body.as_bytes())?;
    }
    write_readonly(&bundle_dir.join("manifest.json"), &manifest_bytes)?;

    // AUDIT ANCHOR: record the bundle's digest in the chain, then witness the new
    // head in the Keychain. Non-fatal on failure — the frozen bundle still stands.
    let mut anchored = false;
    if audit_on {
        if let Some((_, log)) = crate::audit::global() {
            if let Err(e) = log.record_triage_evidence(&bundle_id, &manifest_sha).await {
                warn!(error = %e, "triage: failed to record the bundle digest in the audit chain");
            }
            match log.anchor_to(&crate::audit::KeychainAnchor).await {
                Ok(Some(_)) => anchored = true,
                Ok(None) => {}
                Err(e) => warn!(error = %e, "triage: failed to write the external anchor"),
            }
        }
    }

    // Secret-free telemetry: the bundle path (home-stripped), per-section counts,
    // the manifest digest, and whether the head was anchored. No captured content.
    let section_items: BTreeMap<String, usize> =
        sections.iter().map(|s| (s.name.to_string(), s.item_count)).collect();
    crate::telemetry::emit(
        "system",
        "security.triage",
        json!({
            "bundle": crate::introspect::redact_home(&bundle_dir.display().to_string()),
            "manifest_sha256": manifest_sha,
            "items": section_items,
            "anchored": anchored,
            "audit": audit_on,
        }),
    );

    Ok(TriageSummary {
        bundle_dir,
        manifest_sha256: manifest_sha,
        section_items,
        anchored,
        prior_anchor,
    })
}

/// Keep sections while their running byte total fits `budget`; truncate the first
/// section that would overflow to the remaining room and record every cut as an
/// honest note. Returns the manifest notes (always incl. the standing honesty
/// notes about `<private>` log fields + read-only scope).
fn enforce_budget(sections: &mut [Section], budget: usize) -> Vec<String> {
    let mut notes = vec![
        "READ-ONLY capture: the machine was only read, never changed; RESTORE is never automated."
            .to_string(),
        "Unified-log <private> fields stay REDACTED by macOS without admin — this bundle never \
         de-anonymizes them."
            .to_string(),
        "Every value was redacted at source (secrets / PII / credentialed URLs / the home path)."
            .to_string(),
    ];
    let mut used = 0usize;
    for s in sections.iter_mut() {
        if used >= budget {
            if !s.body.is_empty() {
                notes.push(format!("section '{}' dropped: whole-bundle byte budget reached", s.name));
            }
            s.body = format!("(section '{}' omitted — bundle byte budget reached)\n", s.name);
            continue;
        }
        let remaining = budget - used;
        if s.body.len() > remaining {
            s.body = truncate_on_char_boundary(std::mem::take(&mut s.body), remaining);
            notes.push(format!("section '{}' truncated to the remaining bundle budget", s.name));
        }
        used += s.body.len();
    }
    notes
}

/// Write `bytes` to `path` and mark the file READ-ONLY (0o444) — a frozen evidence
/// file is not meant to be edited in place.
fn write_readonly(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, bytes)
        .map_err(|e| anyhow!("triage: cannot write {}: {e}", path.display()))?;
    let perms = std::fs::Permissions::from_mode(0o444);
    if let Err(e) = std::fs::set_permissions(path, perms) {
        // Non-fatal: the content is written; read-only is a hardening nicety.
        warn!(path = %path.display(), error = %e, "triage: could not set the evidence file read-only");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- redaction at source -------------------------------------------------

    #[test]
    fn redact_section_scrubs_secrets_pii_and_home_and_bounds_size() {
        let raw = "user AKIAIOSFODNN7EXAMPLE1 logged in from bob@example.com\n\
                   token ghp_0123456789abcdefghijklmnopqrstuvwxyz used\n\
                   path /Users/darwincapani/Downloads/evidence";
        // Fake a $HOME so redact_home has something to strip deterministically.
        std::env::set_var("HOME", "/Users/darwincapani");
        let out = redact_section(raw, 4096);
        assert!(!out.contains("AKIAIOSFODNN7EXAMPLE1"), "AWS key must be redacted: {out}");
        assert!(!out.contains("ghp_0123456789abcdefghijklmnopqrstuvwxyz"), "GH token redacted");
        assert!(!out.contains("bob@example.com"), "email redacted");
        assert!(!out.contains("/Users/darwincapani"), "home path stripped: {out}");
    }

    #[test]
    fn truncate_on_char_boundary_never_splits_a_codepoint_and_marks_the_cut() {
        let s = "héllo wörld ".repeat(50); // multibyte chars
        let out = truncate_on_char_boundary(s.clone(), 20);
        assert!(out.len() <= 20 + 64, "truncated near the cap (plus the marker)");
        assert!(out.contains("[truncated"), "the cut is marked honestly");
        // A no-op when already under cap.
        assert_eq!(truncate_on_char_boundary("short".into(), 4096), "short");
    }

    // -- process table + signing ---------------------------------------------

    #[test]
    fn parse_ps_reads_rows_skips_header_and_junk_and_keeps_pathspaces() {
        let out = "  PID  PPID USER  %CPU %MEM COMM\n\
                     1     0 root   0.0  0.1 /sbin/launchd\n\
                   501   1 me     1.2  0.5 /Applications/Some App.app/Contents/MacOS/Some App\n\
                   junk row here\n\
                     0     0 root   0.0  0.0 kernel_task";
        let rows = parse_ps(out);
        assert_eq!(rows.len(), 3, "header + non-numeric junk row skipped: {rows:?}");
        assert_eq!(rows[0].pid, "1");
        assert_eq!(rows[0].exec, "/sbin/launchd");
        assert_eq!(rows[1].exec, "/Applications/Some App.app/Contents/MacOS/Some App", "path spaces kept");
        assert_eq!(rows[2].exec, "kernel_task", "non-absolute comm kept in the row");
    }

    #[test]
    fn distinct_exec_paths_dedupes_absolute_only_and_bounds() {
        let rows = vec![
            ProcRow { pid: "1".into(), ppid: "0".into(), exec: "/sbin/launchd".into() },
            ProcRow { pid: "2".into(), ppid: "1".into(), exec: "/sbin/launchd".into() },
            ProcRow { pid: "3".into(), ppid: "1".into(), exec: "kernel_task".into() },
            ProcRow { pid: "4".into(), ppid: "1".into(), exec: "/usr/bin/foo".into() },
        ];
        let paths = distinct_exec_paths(&rows, 48);
        assert_eq!(paths, vec!["/sbin/launchd", "/usr/bin/foo"], "abs-only, deduped");
        assert_eq!(distinct_exec_paths(&rows, 1).len(), 1, "bounded by max");
    }

    #[test]
    fn classify_codesign_reads_the_states() {
        assert_eq!(classify_codesign("test: code object is not signed at all"), Signing::Unsigned);
        assert_eq!(classify_codesign("Executable=/x\nSignature=adhoc"), Signing::Adhoc);
        assert_eq!(classify_codesign("Authority=Developer ID Application: Acme"), Signing::Signed);
        assert_eq!(classify_codesign("weird"), Signing::Unknown);
    }

    // -- sockets + quarantine ------------------------------------------------

    #[test]
    fn parse_netstat_counts_listen_and_established() {
        let out = "Proto ...\n\
                   tcp4  0 0  127.0.0.1.7177  *.*  LISTEN\n\
                   tcp4  0 0  10.0.0.2.51000  93.1.2.3.443  ESTABLISHED\n\
                   tcp4  0 0  10.0.0.2.51001  93.1.2.4.443  ESTABLISHED\n\
                   udp4  0 0  *.*  *.*";
        assert_eq!(parse_netstat(out), (1, 2));
    }

    #[test]
    fn quarantine_row_redacts_a_token_bearing_origin() {
        let line = format_quarantine_row(
            "700000000.0",
            "Safari",
            "https://dl.example.com/setup?token=ghp_0123456789abcdefghijklmnopqrstuvwxyz",
        );
        assert!(!line.contains("ghp_0123456789abcdefghijklmnopqrstuvwxyz"), "token redacted: {line}");
        assert!(line.contains("Safari"), "the agent survives");
    }

    // -- manifest assembly ---------------------------------------------------

    fn sample_sections() -> Vec<Section> {
        vec![
            Section {
                name: "processes",
                filename: "processes.txt",
                body: "1 0 /sbin/launchd\n".to_string(),
                item_count: 1,
            },
            Section {
                name: "sockets",
                filename: "sockets.txt",
                body: "LISTEN 127.0.0.1.7177\n".to_string(),
                item_count: 1,
            },
        ]
    }

    #[test]
    fn assemble_manifest_folds_counts_and_per_file_digests_secret_free() {
        let sections = sample_sections();
        let notes = vec!["read-only".to_string()];
        let anchor = json!({ "ok": true, "state": "match", "seq": 3 });
        let m = assemble_manifest("2026-07-15T12-00-00Z", "2026-07-15T12:00:00+00:00", &sections, &notes, &anchor);

        assert_eq!(m["schema"], json!("darwin.triage/1"));
        assert_eq!(m["read_only"], json!(true));
        assert_eq!(m["transmitted"], json!(false));
        let arr = m["sections"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["name"], json!("processes"));
        assert_eq!(arr[0]["items"], json!(1));
        // The per-file digest matches an independent SHA-256 of the body.
        assert_eq!(arr[0]["sha256"], json!(sha256_hex(sections[0].body.as_bytes())));
        assert_eq!(m["prior_anchor"]["state"], json!("match"));
    }

    #[test]
    fn manifest_and_bodies_never_carry_a_raw_secret() {
        // A section whose RAW capture held a secret, but redaction ran at source.
        let secret = "ghp_0123456789abcdefghijklmnopqrstuvwxyz";
        let redacted = redact_section(&format!("token {secret} here"), 4096);
        let sections = vec![Section {
            name: "processes",
            filename: "processes.txt",
            body: redacted,
            item_count: 1,
        }];
        let m = assemble_manifest("id", "ts", &sections, &[], &json!({}));
        let whole = m.to_string();
        assert!(!whole.contains(secret), "the manifest must never carry a raw secret: {whole}");
        assert!(!sections[0].body.contains(secret), "nor the section body");
    }

    // -- confinement + budget ------------------------------------------------

    #[test]
    fn is_confined_admits_only_the_forensics_subtree() {
        let state = Path::new("/darwin/state");
        assert!(is_confined(&state.join("forensics").join("2026"), state));
        assert!(!is_confined(&state.join("darwin.db"), state), "state root, not forensics");
        assert!(!is_confined(Path::new("/etc/passwd"), state));
        assert!(!is_confined(&state.join("forensics-evil"), state), "prefix look-alike rejected");
    }

    #[test]
    fn enforce_budget_truncates_then_drops_and_notes_every_cut() {
        let mut sections = vec![
            Section { name: "a", filename: "a.txt", body: "x".repeat(30), item_count: 1 },
            Section { name: "b", filename: "b.txt", body: "y".repeat(30), item_count: 1 },
        ];
        // Budget only fits part of 'a'; 'b' must be dropped.
        let notes = enforce_budget(&mut sections, 20);
        assert!(sections[0].body.len() <= 20 + 64, "'a' truncated to the budget");
        assert!(sections[1].body.contains("omitted"), "'b' dropped once the budget is spent");
        assert!(notes.iter().any(|n| n.contains("truncated")), "the truncation is noted");
        assert!(notes.iter().any(|n| n.contains("dropped") || n.contains("omitted")), "the drop is noted");
        // The standing honesty notes are always present.
        assert!(notes.iter().any(|n| n.contains("READ-ONLY")));
        assert!(notes.iter().any(|n| n.to_lowercase().contains("<private>")));
    }

    #[test]
    fn sha256_hex_is_deterministic_and_hex() {
        let a = sha256_hex(b"darwin");
        assert_eq!(a, sha256_hex(b"darwin"));
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
