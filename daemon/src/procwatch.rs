//! `system.processes` — PROCESS OBSERVATORY: a LIVE, STRICTLY READ-ONLY,
//! system-wide process-table feed.
//!
//! DARWIN "sees its machine working": a bounded poll ([procwatch].poll_secs)
//! that snapshots the LIVE process table via sysinfo and reduces it to one
//! SECRET-FREE `system.processes` frame for the HUD — total process count,
//! top-N by CPU and by memory, how many processes are NEW since the last poll,
//! and the load average as context.
//!
//! ## Contract: READ-ONLY, unprivileged, honest (the vitals.rs discipline)
//!
//!   * READ-ONLY — it OBSERVES and REPORTS. It NEVER kills, signals, renices,
//!     suspends, or otherwise touches any process. No such code path exists in
//!     this module at all — acting on a process would be consequential and is
//!     out of scope by construction.
//!   * SECRET-FREE — the wire carries process NAME + pid (+ ppid/uid metadata)
//!     and coarse cpu/memory numbers ONLY. NEVER argv/command line, NEVER
//!     environment, NEVER open files/paths/cwd/exe: argv and env routinely
//!     carry secrets (tokens, URLs, key material). Enforced by construction —
//!     [`ProcRecord`] simply has no such field and the collector never reads
//!     those sysinfo accessors, so no refactor of the assemble seam can leak
//!     them.
//!   * HONEST / degrades cleanly — every field is a REAL read; anything that
//!     cannot be read degrades to `None`/JSON null (ppid/uid where
//!     unavailable, `new_since_poll` on the very first poll when there is no
//!     baseline), NEVER a fabricated value — the vitals.rs `on_ac` precedent.
//!   * BOUNDED — top-N is capped at [`PROCWATCH_MAX_TOP_N`], every name at
//!     [`PROCWATCH_MAX_NAME_CHARS`] chars, and the counts/load are fixed-size
//!     scalars, so the frame has a fixed maximum size regardless of how many
//!     processes exist or how hostile their names are.
//!
//! ## PURE seam vs DEVICE-GATED runner (mirrors vitals.rs)
//!
//! The snapshot -> frame reduction is a PURE, unit-tested seam over plain
//! [`ProcRecord`] values: [`top_by_cpu`] / [`top_by_mem`] (deterministic
//! tie-break), [`count_new`] (pid+start-time keyed, so a reused pid still
//! counts as new), [`truncate_name`], and [`ProcSnapshot::to_json`]. The LIVE
//! sysinfo read ([`procwatch_task`] + `collect_procs`) is a thin DEVICE-GATED
//! runner and is NEVER exercised under test.
//!
//! ## Boundary: NOT the Persistence Sentinel
//!
//! persistence.rs ("Autoruns for the Mac") watches the AUTOSTART surfaces —
//! LaunchAgents/LaunchDaemons directories, login items, cron — for CHANGES
//! against a baseline. procwatch is about the LIVE process table only: what is
//! running right now and what it costs. The two do not overlap and neither
//! duplicates the other.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::config::Config;

/// Hard floor on the poll cadence, so a hostile/typo'd `poll_secs = 0` can
/// never busy-spin the read loop (the vitals.rs discipline). Walking the whole
/// process table is cheap but not free; a live panel needs nothing under 2s.
pub const PROCWATCH_MIN_POLL_SECS: u64 = 2;

/// Hard cap on the per-list top-N regardless of config, so a hostile/typo'd
/// `top_n` can never flood the frame / the HUD DOM. The HUD parser caps again.
pub const PROCWATCH_MAX_TOP_N: usize = 32;

/// Cap on the surfaced process-name length in chars (lossy-truncated), so a
/// hostile giant name can never balloon the frame.
pub const PROCWATCH_MAX_NAME_CHARS: usize = 64;

/// Sanity ceiling on a per-process CPU percent. Unlike a per-core reading, a
/// process percent can honestly exceed 100 on a multi-core machine (sysinfo
/// sums across cores), so this is NOT clamped to 100 — the cap only bounds the
/// frame against a garbage reading (64 cores x 100%).
const PROCWATCH_CPU_PCT_CAP: f32 = 6400.0;

// ---------------------------------------------------------------------------
// THE RECORD — a plain, SECRET-FREE-by-construction process reading.
// ---------------------------------------------------------------------------

/// One process-table row, as plain data. SECRET-FREE BY CONSTRUCTION: there is
/// no argv, no environment, no exe/cwd/open-file field here AT ALL — those can
/// carry secrets and are deliberately never read, so no downstream code can
/// ever surface them.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcRecord {
    pub pid: u32,
    /// Parent pid, or `None` when the parent could not be read (honest absent,
    /// never a fabricated 0/1).
    pub ppid: Option<u32>,
    /// Process name (the binary's short name — NOT its command line), capped
    /// at [`PROCWATCH_MAX_NAME_CHARS`] chars at assemble time.
    pub name: String,
    /// CPU percent. Can honestly exceed 100 on multi-core (sysinfo sums across
    /// cores); sanitized/rounded at assemble time.
    pub cpu_pct: f32,
    pub mem_bytes: u64,
    /// Unix start time (seconds). Feeds the (pid, start) identity key so a
    /// reused pid still reads as a NEW process.
    pub start_time_secs: u64,
    /// Owning uid, or `None` where unavailable (honest absent).
    pub uid: Option<u32>,
}

/// The identity key for "same process across two polls": pid ALONE is not
/// enough (macOS reuses pids), so the start time disambiguates — a reused pid
/// with a different start time is a NEW process.
pub type ProcKey = (u32, u64);

// ---------------------------------------------------------------------------
// PURE REDUCTION SEAMS — unit-tested on synthetic records, no live system.
// ---------------------------------------------------------------------------

/// Sanitize a CPU percent for ordering/serialization: non-finite -> 0 (never a
/// fabricated load), clamped into 0..=[`PROCWATCH_CPU_PCT_CAP`].
fn sane_cpu(c: f32) -> f32 {
    if c.is_finite() {
        c.clamp(0.0, PROCWATCH_CPU_PCT_CAP)
    } else {
        0.0
    }
}

/// Round a sanitized CPU percent to 1 decimal (as f64 for a clean JSON number).
fn round_pct(c: f32) -> f64 {
    ((sane_cpu(c) as f64) * 10.0).round() / 10.0
}

/// Round a load-average component to 2 decimals; a non-finite value -> 0
/// (same shape as vitals.rs).
fn round_load(x: f64) -> f64 {
    if x.is_finite() {
        (x.max(0.0) * 100.0).round() / 100.0
    } else {
        0.0
    }
}

/// Cap a process name at [`PROCWATCH_MAX_NAME_CHARS`] chars (char-boundary
/// safe, so a hostile multi-byte name can never split a code point). PURE.
pub fn truncate_name(name: &str) -> String {
    name.chars().take(PROCWATCH_MAX_NAME_CHARS).collect()
}

/// Top `n` records by CPU, descending, tie-broken by ascending pid so equal
/// readings order DETERMINISTICALLY. `n` is capped at
/// [`PROCWATCH_MAX_TOP_N`]. PURE.
pub fn top_by_cpu(procs: &[ProcRecord], n: usize) -> Vec<&ProcRecord> {
    let mut v: Vec<&ProcRecord> = procs.iter().collect();
    v.sort_by(|a, b| {
        // sane_cpu never yields NaN, so partial_cmp is always Some; the
        // Equal fallback is belt-and-braces, not a reachable arm.
        sane_cpu(b.cpu_pct)
            .partial_cmp(&sane_cpu(a.cpu_pct))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.pid.cmp(&b.pid))
    });
    v.truncate(n.min(PROCWATCH_MAX_TOP_N));
    v
}

/// Top `n` records by memory, descending, tie-broken by ascending pid.
/// `n` is capped at [`PROCWATCH_MAX_TOP_N`]. PURE.
pub fn top_by_mem(procs: &[ProcRecord], n: usize) -> Vec<&ProcRecord> {
    let mut v: Vec<&ProcRecord> = procs.iter().collect();
    v.sort_by(|a, b| b.mem_bytes.cmp(&a.mem_bytes).then(a.pid.cmp(&b.pid)));
    v.truncate(n.min(PROCWATCH_MAX_TOP_N));
    v
}

/// Count the processes in `procs` whose (pid, start-time) key is absent from
/// the previous poll's key set — i.e. processes STARTED since the last poll.
/// Keyed on pid + start time so a REUSED pid still counts as new. PURE.
pub fn count_new(procs: &[ProcRecord], prev: &HashSet<ProcKey>) -> usize {
    procs
        .iter()
        .filter(|p| !prev.contains(&(p.pid, p.start_time_secs)))
        .count()
}

// ---------------------------------------------------------------------------
// THE SNAPSHOT — a PURE value; `to_json` is the tested assemble seam.
// ---------------------------------------------------------------------------

/// One assembled reading of the process table. A PURE value — the live task
/// fills it from the sysinfo read, and [`to_json`](Self::to_json) is the
/// tested reduce/assemble seam.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcSnapshot {
    pub procs: Vec<ProcRecord>,
    /// Load average (1 / 5 / 15 minute) — the frame's load context.
    pub load_avg: (f64, f64, f64),
}

/// Serialize one top-list entry. SECRET-FREE: exactly name/pid/ppid/uid/
/// cpu_pct/mem_bytes — the input record has no argv/env/path field to leak.
fn entry_json(p: &ProcRecord) -> Value {
    json!({
        "name": truncate_name(&p.name),
        "pid": p.pid,
        "ppid": p.ppid,
        "uid": p.uid,
        "cpu_pct": round_pct(p.cpu_pct),
        "mem_bytes": p.mem_bytes,
    })
}

impl ProcSnapshot {
    /// The (pid, start-time) identity keys of this snapshot — the baseline the
    /// NEXT poll diffs against for `new_since_poll`. PURE.
    pub fn keys(&self) -> HashSet<ProcKey> {
        self.procs.iter().map(|p| (p.pid, p.start_time_secs)).collect()
    }

    /// Assemble the SECRET-FREE `system.processes` wire JSON. PURE — bounded
    /// top-N by CPU and by memory, the total count, the new-since-last-poll
    /// count (`prev` is the previous poll's key set; `None` on the FIRST poll
    /// serializes to an honest null — we genuinely have no baseline yet, and a
    /// fabricated 0 would claim "nothing new" we never measured), and the load
    /// average. Unit-tested on synthetic records.
    pub fn to_json(&self, prev: Option<&HashSet<ProcKey>>, top_n: usize) -> Value {
        let top_cpu: Vec<Value> = top_by_cpu(&self.procs, top_n).into_iter().map(entry_json).collect();
        let top_mem: Vec<Value> = top_by_mem(&self.procs, top_n).into_iter().map(entry_json).collect();
        let new_since_poll: Option<usize> = prev.map(|set| count_new(&self.procs, set));
        json!({
            "total": self.procs.len(),
            "new_since_poll": new_since_poll,
            "top_cpu": top_cpu,
            "top_mem": top_mem,
            "load_avg": [
                round_load(self.load_avg.0),
                round_load(self.load_avg.1),
                round_load(self.load_avg.2),
            ],
        })
    }
}

// ---------------------------------------------------------------------------
// DEVICE-GATED RUNNER — the live sysinfo read (NEVER exercised under test).
// ---------------------------------------------------------------------------

/// Read the live process table into plain [`ProcRecord`]s. SECRET-FREE BY
/// CONSTRUCTION: only `pid`/`parent`/`name`/`cpu_usage`/`memory`/`start_time`/
/// `user_id` are read — never `cmd()` (argv), never `environ()`, never
/// `exe()`/`cwd()`/`root()`/open files. DEVICE-GATED runner.
fn collect_procs(sys: &sysinfo::System) -> Vec<ProcRecord> {
    sys.processes()
        .values()
        .map(|p| ProcRecord {
            pid: p.pid().as_u32(),
            ppid: p.parent().map(|pp| pp.as_u32()),
            name: p.name().to_string_lossy().into_owned(),
            cpu_pct: p.cpu_usage(),
            mem_bytes: p.memory(),
            start_time_secs: p.start_time(),
            uid: p.user_id().map(|u| **u),
        })
        .collect()
}

/// The live `system.processes` poll. STRICTLY READ-ONLY: every tick it walks
/// the process table (cpu + memory refresh only — argv/env are never read) and
/// emits one bounded, SECRET-FREE frame for the HUD. It acts on NOTHING — no
/// kill, no signal, no renice exists here. Gated by [procwatch].enabled — OFF,
/// it returns immediately and never spawns a read. The poll cadence is clamped
/// to [`PROCWATCH_MIN_POLL_SECS`]; top-N to [`PROCWATCH_MAX_TOP_N`].
pub async fn procwatch_task(cfg: Arc<Config>) {
    if !cfg.procwatch.enabled {
        return;
    }
    let poll = cfg.procwatch.poll_secs.max(PROCWATCH_MIN_POLL_SECS);
    let top_n = cfg.procwatch.top_n.min(PROCWATCH_MAX_TOP_N as u64) as usize;
    let mut sys = sysinfo::System::new();
    let mut prev: Option<HashSet<ProcKey>> = None;
    let mut interval = tokio::time::interval(Duration::from_secs(poll));
    loop {
        interval.tick().await;
        // Per-process CPU deltas need two refreshes; the inter-tick gap
        // (>= 2s) supplies the delta, so each tick refreshes against the
        // prior one (the vitals.rs pattern). Dead processes are dropped so
        // the (pid, start) baseline stays honest.
        sys.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::All,
            true,
            sysinfo::ProcessRefreshKind::new().with_cpu().with_memory(),
        );
        let la = sysinfo::System::load_average();
        let snapshot = ProcSnapshot {
            procs: collect_procs(&sys),
            load_avg: (la.one, la.five, la.fifteen),
        };
        let frame = snapshot.to_json(prev.as_ref(), top_n);
        prev = Some(snapshot.keys());
        crate::telemetry::emit("system", "system.processes", frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic record — the pure seams never see a live process.
    fn rec(pid: u32, name: &str, cpu: f32, mem: u64, start: u64) -> ProcRecord {
        ProcRecord {
            pid,
            ppid: Some(1),
            name: name.into(),
            cpu_pct: cpu,
            mem_bytes: mem,
            start_time_secs: start,
            uid: Some(501),
        }
    }

    fn snap(procs: Vec<ProcRecord>) -> ProcSnapshot {
        ProcSnapshot { procs, load_avg: (1.234, 0.5, 0.0) }
    }

    // --- top-N selection (PURE) ----------------------------------------------

    #[test]
    fn top_selection_orders_desc_and_breaks_ties_by_pid() {
        let procs = vec![
            rec(30, "c", 50.0, 10, 0),
            rec(10, "a", 50.0, 30, 0), // CPU tie with pid 30 -> lower pid first
            rec(20, "b", 90.0, 20, 0),
        ];
        let cpu: Vec<u32> = top_by_cpu(&procs, 3).iter().map(|p| p.pid).collect();
        assert_eq!(cpu, vec![20, 10, 30], "cpu desc, ties by ascending pid");
        let mem: Vec<u32> = top_by_mem(&procs, 2).iter().map(|p| p.pid).collect();
        assert_eq!(mem, vec![10, 20], "mem desc, truncated to n");
    }

    #[test]
    fn top_n_is_capped_at_32_even_for_a_hostile_n() {
        let procs: Vec<ProcRecord> =
            (0..100).map(|i| rec(i, "p", i as f32, u64::from(i), 0)).collect();
        assert_eq!(top_by_cpu(&procs, usize::MAX).len(), PROCWATCH_MAX_TOP_N);
        assert_eq!(top_by_mem(&procs, 10_000).len(), PROCWATCH_MAX_TOP_N);
        let v = snap(procs).to_json(None, usize::MAX);
        assert_eq!(v["top_cpu"].as_array().unwrap().len(), PROCWATCH_MAX_TOP_N);
        assert_eq!(v["top_mem"].as_array().unwrap().len(), PROCWATCH_MAX_TOP_N);
    }

    #[test]
    fn nan_cpu_sorts_as_zero_never_poisons_the_order() {
        let procs = vec![rec(1, "nan", f32::NAN, 0, 0), rec(2, "busy", 10.0, 0, 0)];
        let top: Vec<u32> = top_by_cpu(&procs, 2).iter().map(|p| p.pid).collect();
        assert_eq!(top, vec![2, 1], "NaN reads as 0, not as greatest");
    }

    // --- name truncation (PURE) ----------------------------------------------

    #[test]
    fn hostile_giant_name_is_truncated_lossy_and_char_safe() {
        // 10k chars of multi-byte content: the cap must count CHARS (never
        // split a code point) and hold the frame bounded.
        let giant = "é".repeat(10_000);
        let t = truncate_name(&giant);
        assert_eq!(t.chars().count(), PROCWATCH_MAX_NAME_CHARS);
        let v = snap(vec![rec(7, &giant, 1.0, 1, 0)]).to_json(None, 1);
        let name = v["top_cpu"][0]["name"].as_str().unwrap();
        assert_eq!(name.chars().count(), PROCWATCH_MAX_NAME_CHARS);
        // A short name passes through untouched.
        assert_eq!(truncate_name("kernel_task"), "kernel_task");
    }

    // --- new-process counting (PURE, across two synthetic snapshots) ---------

    #[test]
    fn new_process_counting_across_two_snapshots_keys_on_pid_plus_start() {
        let first = snap(vec![
            rec(100, "survivor", 1.0, 1, 1000),
            rec(200, "dies", 1.0, 1, 1000),
            rec(300, "reused-pid", 1.0, 1, 1000),
        ]);
        let baseline = first.keys();
        let second = vec![
            rec(100, "survivor", 1.0, 1, 1000), // same pid + start -> not new
            rec(300, "reused-pid", 1.0, 1, 2000), // SAME pid, new start -> NEW
            rec(400, "fresh", 1.0, 1, 2000),    // new pid -> NEW
        ];
        assert_eq!(count_new(&second, &baseline), 2);
        // And the frame carries it as a number once a baseline exists.
        let v = snap(second).to_json(Some(&baseline), 8);
        assert_eq!(v["new_since_poll"], json!(2));
    }

    #[test]
    fn first_poll_has_no_baseline_so_new_since_poll_is_honest_null() {
        // No previous snapshot => we genuinely don't know what is "new". The
        // frame must say null, never a fabricated 0 ("nothing new") we never
        // measured.
        let v = snap(vec![rec(1, "launchd", 0.1, 1, 0)]).to_json(None, 8);
        assert!(v["new_since_poll"].is_null(), "no baseline => null, never a fabricated 0");
    }

    // --- to_json ASSEMBLE seam (PURE) ----------------------------------------

    #[test]
    fn to_json_empty_table_is_honest_empty() {
        let v = snap(vec![]).to_json(None, 12);
        assert_eq!(v["total"], json!(0));
        assert_eq!(v["top_cpu"], json!([]));
        assert_eq!(v["top_mem"], json!([]));
        assert!(v["new_since_poll"].is_null());
        assert_eq!(v["load_avg"], json!([1.23, 0.5, 0.0]));
    }

    #[test]
    fn to_json_entry_carries_exactly_the_secret_free_keys() {
        // The entry object exposes EXACTLY the six secret-free keys (serde_json
        // sorts object keys, so compare as a set) — no argv/cmd/env/exe/cwd/
        // open-file key can exist because ProcRecord has no such field.
        let v = snap(vec![rec(42, "darwind", 12.34, 1024, 99)]).to_json(None, 4);
        let entry = &v["top_cpu"][0];
        let mut keys: Vec<&str> =
            entry.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["cpu_pct", "mem_bytes", "name", "pid", "ppid", "uid"]);
        assert_eq!(entry["name"], json!("darwind"));
        assert_eq!(entry["pid"], json!(42));
        assert_eq!(entry["ppid"], json!(1));
        assert_eq!(entry["uid"], json!(501));
        assert_eq!(entry["cpu_pct"], json!(12.3));
        assert_eq!(entry["mem_bytes"], json!(1024));
    }

    #[test]
    fn to_json_unreadable_ppid_and_uid_degrade_to_null_not_fabricated() {
        let mut p = rec(9, "orphanish", 1.0, 1, 0);
        p.ppid = None;
        p.uid = None;
        let v = snap(vec![p]).to_json(None, 1);
        assert!(v["top_cpu"][0]["ppid"].is_null(), "unreadable ppid => null, never a fake 1");
        assert!(v["top_cpu"][0]["uid"].is_null(), "unreadable uid => null, never a fake 501");
    }

    #[test]
    fn to_json_sanitizes_cpu_and_load() {
        let procs = vec![
            rec(1, "nan", f32::NAN, 1, 0),
            rec(2, "neg", -5.0, 1, 0),
            rec(3, "round", 10.04, 1, 0),
            rec(4, "multi-core", 340.0, 1, 0), // >100 is HONEST on multi-core
            rec(5, "garbage", 1e9, 1, 0),      // absurd reading hits the cap
        ];
        let v = snap(procs).to_json(None, 8);
        let by_pid = |pid: u64| -> f64 {
            v["top_mem"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["pid"] == json!(pid))
                .unwrap()["cpu_pct"]
                .as_f64()
                .unwrap()
        };
        assert_eq!(by_pid(1), 0.0, "NaN -> 0, never a fabricated load");
        assert_eq!(by_pid(2), 0.0, "negative clamps to 0");
        assert_eq!(by_pid(3), 10.0, "rounded to 1dp");
        assert_eq!(by_pid(4), 340.0, "multi-core >100% is honest and preserved");
        assert_eq!(by_pid(5), f64::from(PROCWATCH_CPU_PCT_CAP), "garbage hits the sanity cap");
        // Load: rounded to 2dp, non-finite -> 0.
        let s = ProcSnapshot { procs: vec![], load_avg: (f64::NAN, -1.0, 2.345) };
        assert_eq!(s.to_json(None, 1)["load_avg"], json!([0.0, 0.0, 2.35]));
    }
}
