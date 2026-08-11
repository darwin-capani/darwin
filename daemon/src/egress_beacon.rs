//! Egress Baseline + Beacon Detector — the longitudinal follow-on named in the
//! `egress.rs` header ("longitudinal baseline + new-beacon alerting +
//! propose-only firewall suggestion"). It sits ON TOP of the read-only Egress
//! Sentinel: `egress::sample_talkers()` gives it the same lsof-based
//! outbound-connection snapshot, and this module keeps a BOUNDED longitudinal
//! store of who-talks-to-whom so it can answer two questions with PURE,
//! unit-tested classifiers:
//!
//!   1. new-host diff  — a `(process, host)` pair never seen before
//!      ([`diff_new_hosts`]).
//!   2. beacon cadence — a host contacted at a suspiciously REGULAR interval,
//!      the classic C2 callback signature ([`classify_cadence`]): low
//!      coefficient-of-variation of the inter-arrival deltas over enough samples.
//!
//! DEFENSIVE, READ-ONLY, PROPOSE-ONLY. This module CHANGES NOTHING on the host.
//! The strongest thing it does is RENDER a pf/pfctl rule as TEXT
//! ([`render_block_proposal`]) the operator reviews and applies themselves with
//! `sudo` — it never shells `pfctl`, never mutates the firewall, and has no
//! consequential surface (same discipline as `egress.rs` / `posture.rs` /
//! `tcc.rs`).
//!
//! RIDES THE EDITH GUARDS. So an alert can never spam, every finding passes
//! through [`guard_alert`], which reuses EDITH's EXACT quiet-hours band
//! ([`crate::anticipate::in_quiet_hours`], sourced from `[proactive]`) and
//! mirrors EDITH's per-key cooldown + global debounce ([`AlertLedger`], the same
//! shape as `anticipate::FiredState`). Within quiet hours, or inside a cooldown/
//! debounce window, the finding is suppressed silently.
//!
//! RISING-EDGE SAMPLING. The store records a timestamp only on a rising edge
//! (a talker ABSENT last sample, PRESENT now), not on every sample. That is what
//! separates a genuine short-lived beacon (a fresh connection each interval →
//! one edge per interval → a regular series) from a benign LONG-LIVED connection
//! (one socket held open → a single edge, no series). Without this, a persistent
//! poller sampled every N seconds would masquerade as a perfect N-second beacon.
//!
//! HONEST CAVEATS (never papered over):
//!   * Attribution is UID-scoped. Unprivileged `lsof` attributes only same-UID
//!     processes; connections owned by other users are invisible here. The
//!     [`UID_CAVEAT`] string rides every alert frame so the HUD/operator sees it.
//!   * Cadence resolution is bounded by the sample interval. A beacon that opens
//!     AND closes entirely between two samples is never observed by snapshot
//!     sampling — this detector sees only connections established at a sample
//!     instant. It is an advisory signal, not a packet-level IDS.

use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use chrono::Timelike;
use rusqlite::Connection;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

/// Rides every alert frame: the standing honesty note about UID-scoped
/// attribution. Stated, not hidden.
pub const UID_CAVEAT: &str =
    "unprivileged lsof attributes only same-UID processes; connections owned by \
     other users are not visible to this detector.";

// ---------------------------------------------------------------------------
// Config-derived knobs (pure data so the classifiers stay functions of input)
// ---------------------------------------------------------------------------

/// Beacon-cadence thresholds. A talker's rising-edge series is flagged as a
/// beacon when it has at least `min_samples` timestamps, a mean inter-arrival
/// within `[min_interval_secs, max_interval_secs]`, and a coefficient of
/// variation (stddev/mean of the deltas) at or below `max_jitter_ratio`.
///
/// HONEST FLOOR (audited, not just configured): a rising edge needs an
/// absent->present transition, so consecutive edges are at least TWO sample
/// intervals apart — at the shipped 60s cadence nothing below a ~120s period is
/// observable, whatever `min_interval_secs` says. And because edges are
/// quantized to the sample grid, an off-grid period P records deltas on the two
/// neighbouring grid multiples (the larger with fraction f = frac(P/60)), so the
/// quantization CV is 60*sqrt(f*(1-f))/P — at most 30/P. Recomputed against the
/// 0.15 ceiling (evasion needs f*(1-f) > 0.0225*(P/60)^2, cross-checked by a
/// phase-swept simulation over 6/12/64-edge windows): only mid-grid periods in
/// roughly 121–169s can evade; EVERY period from 180s — three sample intervals —
/// up fires at any phase (the m=3 worst case is P=210s, CV=0.143), as do
/// grid-aligned and near-grid periods below that. The detector's LIVE band at
/// shipped config is therefore ~180s..max_interval_secs plus aligned shorter
/// periods, with a quantization blind spot only in that ~121–169s mid-grid band.
/// (An earlier pass wrote "~240s" for this bound; the arithmetic gives 180s.)
/// Stated here because a threshold that reads "30s" and never fires below ~120s
/// is exactly the kind of dead-looking band an audit should not have to
/// rediscover.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BeaconThresholds {
    /// Minimum number of timestamps (→ `min_samples - 1` intervals) required
    /// before a cadence verdict is trustworthy.
    pub min_samples: usize,
    /// Below this mean interval the series is treated as bursty reconnection
    /// noise, not a beacon.
    pub min_interval_secs: u64,
    /// Above this mean interval the cadence is indistinguishable from ordinary
    /// slow polling at our sample resolution — an honest ceiling.
    pub max_interval_secs: u64,
    /// Coefficient-of-variation ceiling. A tight, regular cadence sits well
    /// below this; a jittery/random one blows past it.
    pub max_jitter_ratio: f64,
}

impl BeaconThresholds {
    /// Read the thresholds from the `[egress]` config section.
    pub fn from_config(cfg: &crate::config::EgressConfig) -> Self {
        Self {
            min_samples: cfg.beacon_min_samples,
            min_interval_secs: cfg.beacon_min_interval_secs,
            max_interval_secs: cfg.beacon_max_interval_secs,
            max_jitter_ratio: cfg.beacon_max_jitter,
        }
    }
}

/// Bounded-retention policy for the longitudinal store.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RetentionPolicy {
    /// Hard cap on distinct talkers held; the least-recently-seen is evicted
    /// when a new talker would exceed it.
    pub max_talkers: usize,
    /// Ring cap on rising-edge timestamps kept per talker.
    pub max_samples_per_talker: usize,
    /// A talker not seen for longer than this (seconds) is pruned.
    pub retention_secs: u64,
}

impl RetentionPolicy {
    /// Read the retention policy from the `[egress]` config section.
    pub fn from_config(cfg: &crate::config::EgressConfig) -> Self {
        Self {
            max_talkers: cfg.max_talkers,
            max_samples_per_talker: cfg.max_samples_per_talker,
            retention_secs: cfg.retention_secs,
        }
    }
}

// ---------------------------------------------------------------------------
// Observation + host:port splitting (pure)
// ---------------------------------------------------------------------------

/// One sampled outbound observation: which process talked to which host+port,
/// at which unix-second sample time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub process: String,
    pub host: String,
    pub port: u16,
    pub ts: u64,
}

/// PURE split of an lsof `NAME` remote endpoint ("host:port") into `(host,
/// port)`. Handles bracketed IPv6 (`[2001:db8::1]:443`) and plain IPv4
/// (`1.2.3.4:443`); an unparseable port degrades to 0 rather than panicking.
pub fn split_host_port(remote: &str) -> (String, u16) {
    if let Some(rest) = remote.strip_prefix('[') {
        // IPv6 in brackets: [host]:port
        if let Some((host, port)) = rest.split_once("]:") {
            return (host.to_string(), port.parse().unwrap_or(0));
        }
        return (rest.trim_end_matches(']').to_string(), 0);
    }
    // IPv4 host:port — split on the LAST ':' so a bare host still works.
    match remote.rsplit_once(':') {
        Some((host, port)) => (host.to_string(), port.parse().unwrap_or(0)),
        None => (remote.to_string(), 0),
    }
}

// ---------------------------------------------------------------------------
// Longitudinal store (bounded; rising-edge timestamps)
// ---------------------------------------------------------------------------

/// The identity of a talker: exactly the `(process, host, port)` tuple.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TalkerKey {
    process: String,
    host: String,
    port: u16,
}

/// One talker's longitudinal record: its rising-edge timestamps (bounded ring)
/// and when it was last OBSERVED (for pruning + LRU eviction).
struct TalkerRecord {
    key: TalkerKey,
    /// When this talker was last seen in a sample — refreshed on EVERY sample it
    /// appears in, not just on a rising edge. (It used to be written only by
    /// `record_edge`, i.e. only on a rising edge, which made `prune` drop
    /// connections that had never gone away. See `ingest_sample`.)
    last_seen: u64,
    /// Rising-edge sample times (absent→present transitions), oldest-first,
    /// bounded to `max_samples_per_talker`.
    edges: VecDeque<u64>,
}

/// The bounded longitudinal store. Holds the per-talker rising-edge series plus
/// the set of talkers PRESENT in the last ingested sample (so the next sample
/// can compute rising edges). Everything is bounded: distinct talkers by
/// `max_talkers`, per-talker edges by `max_samples_per_talker`, and stale
/// talkers pruned past `retention_secs`.
pub struct BaselineStore {
    talkers: Vec<TalkerRecord>,
    present: HashSet<TalkerKey>,
    retention: RetentionPolicy,
    /// Latched TRUE the first time a NON-EMPTY sample is ingested (or when the
    /// store is loaded from a non-empty persisted baseline). While false the
    /// store is COLD: it has never observed anything, so a diff against it is
    /// an inventory of everything currently talking, not a finding set — see
    /// [`fold_and_diff`]'s silent seed. A latch (not `talkers.is_empty()`) so a
    /// mid-run full prune does not quietly re-arm seeding semantics.
    seeded: bool,
}

impl BaselineStore {
    pub fn new(retention: RetentionPolicy) -> Self {
        Self {
            talkers: Vec::new(),
            present: HashSet::new(),
            retention,
            seeded: false,
        }
    }

    /// COLD = this store's lineage has never observed a non-empty sample: a
    /// first-ever run, or the in-memory fallback when the durable baseline
    /// could not be opened. Its first sample seeds silently.
    pub fn is_cold(&self) -> bool {
        !self.seeded
    }

    /// The set of `(process, host)` pairs the store has EVER recorded — the
    /// baseline the new-host diff is taken against.
    pub fn known_host_pairs(&self) -> HashSet<(String, String)> {
        self.talkers
            .iter()
            .map(|r| (r.key.process.clone(), r.key.host.clone()))
            .collect()
    }

    /// Every rising-edge timestamp for a `(process, host)` pair, merged across
    /// ports and sorted — the series [`classify_cadence`] reasons over.
    pub fn edge_timestamps(&self, process: &str, host: &str) -> Vec<u64> {
        let mut out: Vec<u64> = self
            .talkers
            .iter()
            .filter(|r| r.key.process == process && r.key.host == host)
            .flat_map(|r| r.edges.iter().copied())
            .collect();
        out.sort_unstable();
        out
    }

    /// Ingest one sample. A talker present now but ABSENT in the previous sample
    /// is a rising edge and stamps `now` onto its (possibly new) record. Then
    /// stale talkers are pruned and the distinct-talker cap enforced.
    pub fn ingest_sample(&mut self, obs: &[Observation], now: u64) {
        if !obs.is_empty() {
            self.seeded = true;
        }
        let current: HashSet<TalkerKey> = obs
            .iter()
            .map(|o| TalkerKey {
                process: o.process.clone(),
                host: o.host.clone(),
                port: o.port,
            })
            .collect();
        for key in &current {
            if !self.present.contains(key) {
                self.record_edge(key.clone(), now);
            }
        }
        // REFRESH `last_seen` for EVERY talker present in this sample, not only for
        // the ones that just rose.
        //
        // WHAT WENT WRONG WITHOUT THIS: `record_edge` was the sole writer of
        // `last_seen`, and it only runs on an absent->present transition — so
        // `last_seen` was the time of the last RISING EDGE, while `prune` reads it
        // as "last observed". One outbound socket held continuously open (a VPN
        // tunnel, an IMAP IDLE connection, a chat/websocket keepalive) therefore got
        // pruned at t = retention_secs (24h by default) even though it was
        // ESTABLISHED in every single sample. Its `(process, host)` pair then left
        // the baseline, so the very next sample reported it as a FIRST-SEEN outbound
        // talker — a false `egress.newhost` security alert complete with a rendered
        // pfctl block-rule proposal — and kept doing so once per cooldown window for
        // as long as the connection lived. This is precisely the long-lived case the
        // module header claims to handle ("one socket held open -> a single edge, no
        // series").
        for rec in self.talkers.iter_mut() {
            if current.contains(&rec.key) {
                rec.last_seen = now;
            }
        }
        self.present = current;
        self.prune(now);
    }

    /// Stamp a rising edge for `key` at `now`, creating the record if new and
    /// keeping the per-talker edge ring + distinct-talker cap bounded.
    fn record_edge(&mut self, key: TalkerKey, now: u64) {
        if let Some(rec) = self.talkers.iter_mut().find(|r| r.key == key) {
            rec.last_seen = now;
            rec.edges.push_back(now);
            while rec.edges.len() > self.retention.max_samples_per_talker.max(1) {
                rec.edges.pop_front();
            }
            return;
        }
        // New talker: evict the least-recently-seen if we are at the cap.
        if self.talkers.len() >= self.retention.max_talkers.max(1) {
            if let Some((idx, _)) = self
                .talkers
                .iter()
                .enumerate()
                .min_by_key(|(_, r)| r.last_seen)
            {
                self.talkers.swap_remove(idx);
            }
        }
        let mut edges = VecDeque::new();
        edges.push_back(now);
        self.talkers.push(TalkerRecord {
            key,
            last_seen: now,
            edges,
        });
    }

    /// Drop talkers unseen for longer than `retention_secs`, and forget them from
    /// `present` too so the pair can be re-recorded if it ever comes back. Without
    /// the `present` cleanup a pruned-but-still-open socket could NEVER be
    /// re-recorded (no absent->present transition would ever occur again), leaving
    /// the cadence classifier permanently blind to it.
    fn prune(&mut self, now: u64) {
        let cutoff = self.retention.retention_secs;
        let before: HashSet<TalkerKey> = self.talkers.iter().map(|r| r.key.clone()).collect();
        self.talkers
            .retain(|r| now.saturating_sub(r.last_seen) <= cutoff);
        let after: HashSet<TalkerKey> = self.talkers.iter().map(|r| r.key.clone()).collect();
        for key in before.difference(&after) {
            self.present.remove(key);
        }
    }

    /// Rebuild a store from persisted rows — the boot path of a restart. The
    /// retention BOUNDS are re-imposed on load (the config may have changed,
    /// and a hand-edited/oversized file must not blow memory): per-talker edge
    /// rings keep their NEWEST `max_samples_per_talker` timestamps, and if the
    /// row count exceeds `max_talkers` the most-recently-seen rows win. A store
    /// loaded from a NON-EMPTY baseline is not cold — that is the whole point:
    /// a restart diffs against what the previous run knew instead of re-calling
    /// the owner's ordinary traffic first-seen.
    pub fn from_persisted(retention: RetentionPolicy, mut rows: Vec<PersistedTalker>) -> Self {
        if rows.len() > retention.max_talkers.max(1) {
            rows.sort_by_key(|r| std::cmp::Reverse(r.last_seen));
            rows.truncate(retention.max_talkers.max(1));
        }
        let seeded = !rows.is_empty();
        let mut present = HashSet::new();
        let mut talkers = Vec::with_capacity(rows.len());
        for row in rows {
            let key = TalkerKey {
                process: row.process,
                host: row.host,
                port: row.port,
            };
            let mut edges: Vec<u64> = row.edges;
            edges.sort_unstable();
            let cap = retention.max_samples_per_talker.max(1);
            if edges.len() > cap {
                edges.drain(..edges.len() - cap);
            }
            // `present` survives the restart so a connection that was OPEN when
            // the previous run stopped, and is still open now, does NOT get a
            // fabricated rising edge on the first post-restart sample. (A
            // connection that really dropped and re-established during the
            // downtime loses that one edge — a miss toward FEWER alerts.)
            if row.present {
                present.insert(key.clone());
            }
            talkers.push(TalkerRecord {
                key,
                last_seen: row.last_seen,
                edges: edges.into(),
            });
        }
        Self {
            talkers,
            present,
            retention,
            seeded,
        }
    }

    /// Snapshot the store for persistence — the inverse of [`from_persisted`].
    pub fn to_persisted(&self) -> Vec<PersistedTalker> {
        self.talkers
            .iter()
            .map(|r| PersistedTalker {
                process: r.key.process.clone(),
                host: r.key.host.clone(),
                port: r.key.port,
                last_seen: r.last_seen,
                present: self.present.contains(&r.key),
                edges: r.edges.iter().copied().collect(),
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Durable baseline store — mirrors tcc.rs::TccBaseline / persistence.rs::
// PersistenceBaseline (its OWN dedicated SQLite file under state/, plaintext or
// SQLCipher via the same open/open_encrypted seam, async-Mutex serialized).
// This is what makes `egress.newhost` a statement about the NETWORK instead of
// a statement about daemon uptime: before it existed, run_task's baseline was
// `BaselineStore::new(..)` — in-memory, empty at every boot — so the first tick
// after every restart called the owner's entirely ordinary traffic first-seen.
// ---------------------------------------------------------------------------

/// One persisted talker row: the full longitudinal record (identity, last-seen,
/// presence at last save, rising-edge ring), so a restart resumes the store
/// where the previous run stopped. Secret-free: process names + bare host IPs +
/// unix seconds, the same data the module already held in memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedTalker {
    pub process: String,
    pub host: String,
    pub port: u16,
    pub last_seen: u64,
    pub present: bool,
    pub edges: Vec<u64>,
}

/// The durable egress baseline (`state/egress_baseline.db`). BOUNDED by
/// construction: it only ever holds `BaselineStore::to_persisted()`, which is
/// capped at `max_talkers` rows of `max_samples_per_talker` edges each (the
/// shipped config bounds the file to 2048 rows x 64 edges).
pub struct EgressBaselineDb {
    conn: Mutex<Connection>,
}

impl EgressBaselineDb {
    /// Open (or create) the baseline DB PLAINTEXT (the default).
    pub fn open(path: &Path) -> Result<Self> {
        Self::init_conn(Connection::open(path)?)
    }

    /// Open (or create) the baseline DB ENCRYPTED (SQLCipher). `key` is applied
    /// via `PRAGMA key` before any other statement — the same seam as AuditLog /
    /// TccBaseline / PersistenceBaseline.
    pub fn open_encrypted(path: &Path, key: &crate::crypto::SecretKey) -> Result<Self> {
        let conn = Connection::open(path)?;
        crate::crypto::apply_key(&conn, key)?;
        Self::init_conn(conn)
    }

    /// Shared pragmas + schema, run AFTER any `PRAGMA key`.
    fn init_conn(conn: Connection) -> Result<Self> {
        conn.busy_timeout(Duration::from_millis(250))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS egress_baseline(
                process TEXT NOT NULL,
                host TEXT NOT NULL,
                port INTEGER NOT NULL,
                last_seen INTEGER NOT NULL,
                present INTEGER NOT NULL,
                edges TEXT NOT NULL,
                PRIMARY KEY(process, host, port)
            );",
        )?;
        crate::schema::ensure(&conn, "egress_baseline.db")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// In-memory baseline for tests (no disk). Same schema.
    #[cfg(test)]
    fn in_memory() -> Result<Self> {
        Self::init_conn(Connection::open_in_memory()?)
    }

    /// Load every persisted talker row. A row whose `edges` cell does not parse
    /// (hand-edited, torn, or from a future schema) degrades to an EDGELESS
    /// known talker rather than an error: the `(process, host)` pair still
    /// counts toward the baseline — corruption fails toward FEWER alerts, never
    /// toward re-alarming on a known talker.
    pub async fn load(&self) -> Result<Vec<PersistedTalker>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT process, host, port, last_seen, present, edges FROM egress_baseline",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(PersistedTalker {
                process: r.get::<_, String>(0)?,
                host: r.get::<_, String>(1)?,
                port: r.get::<_, i64>(2)?.clamp(0, u16::MAX as i64) as u16,
                last_seen: r.get::<_, i64>(3)?.max(0) as u64,
                present: r.get::<_, i64>(4)? != 0,
                edges: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or_default(),
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Replace the persisted baseline with EXACTLY `rows`, in ONE transaction
    /// (the same atomic set-replacement as persistence.rs::replace_with — a
    /// crash mid-save must leave the old baseline or the new one, never a
    /// half-set the next boot would diff against).
    pub async fn save(&self, rows: &[PersistedTalker]) -> Result<()> {
        let conn = self.conn.lock().await;
        let tx = conn.unchecked_transaction()?;
        tx.execute("DELETE FROM egress_baseline", [])?;
        for row in rows {
            tx.execute(
                "INSERT INTO egress_baseline(process, host, port, last_seen, present, edges)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    row.process,
                    row.host,
                    row.port as i64,
                    row.last_seen as i64,
                    row.present as i64,
                    serde_json::to_string(&row.edges)?,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Classifier 1 — new-host baseline diff (PURE)
// ---------------------------------------------------------------------------

/// PURE: which current observations are talkers whose `(process, host)` pair is
/// NOT in the baseline `known` set — first-seen talkers. At most one
/// observation per new pair is returned (the first), so several ports to one new
/// host raise a single finding.
/// DARWIN's own process names AS `lsof` REPORTS THEM — never alarm on the daemon's
/// own outbound (esp. its api.anthropic.com cloud lifeline).
///
/// SCOPE, HONESTLY — THE DAEMON ONLY. The inference sidecar is NOT covered and
/// cannot be by name: `boot/run_inference.sh` ends in
/// `exec "$PYTHON" "$DARWIN_ROOT/inference/server.py"`, so lsof's COMMAND for it is
/// `Python`, and excluding that would silence every python process on the machine.
/// A `"darwin-inference"` entry used to sit in this list; the string existed in no
/// other file in the repository — not a binary, not a plist Label, not a script —
/// and macOS lsof truncates COMMAND to 9 characters anyway, so even a binary with
/// that name would arrive as `darwin-in`. The comment claimed this "mirrors
/// persistence.rs's SELF_LABELS", but those are launchd LABELS
/// (`com.darwin.inference`), a different namespace; the mirror only ever held for
/// `darwind`. Consequence, stated plainly: the sidecar's own egress (model fetches,
/// the ElevenLabs calls in server.py) IS flagged as a third-party talker. Fixing
/// that needs a PID-based exclusion resolved from the launchd label/pidfile, not a
/// name that cannot appear.
const SELF_PROCESSES: &[&str] = &["darwind"];

/// Whether a (process, host) is DARWIN itself or a loopback peer, and so must never
/// be alarmed on. PURE — unit-tested. Loopback covers the HUD/telemetry socket
/// (127.0.0.1 / ::1); the self-process check covers the DAEMON's cloud lifeline
/// only (see [`SELF_PROCESSES`] — the inference sidecar is not name-matchable).
pub fn is_self_or_loopback(process: &str, host: &str) -> bool {
    SELF_PROCESSES.contains(&process)
        || host == "::1"
        || host == "localhost"
        || host.starts_with("127.")
}

pub fn diff_new_hosts(current: &[Observation], known: &HashSet<(String, String)>) -> Vec<Observation> {
    let mut seen: HashSet<(String, String)> = HashSet::new();
    current
        .iter()
        .filter(|o| {
            let pair = (o.process.clone(), o.host.clone());
            !known.contains(&pair) && seen.insert(pair)
        })
        .cloned()
        .collect()
}

/// PURE per-tick new-host pass, in exactly the live loop's order: read the
/// baseline, diff the sample against it, ingest the sample, and return the
/// first-seen findings — EMPTY when the store was COLD. A cold store's first
/// sample is an inventory of everything currently talking, not a finding set;
/// alerting on it is what made `egress.newhost` a false positive by
/// construction once per boot (the in-memory baseline era) and would still be
/// one per FRESH INSTALL without this seed. Mirrors the silent cold-start seed
/// in tcc.rs::sentinel_tick / persistence.rs.
pub fn fold_and_diff(
    store: &mut BaselineStore,
    obs: &[Observation],
    now: u64,
) -> Vec<Observation> {
    let cold = store.is_cold();
    let known = store.known_host_pairs();
    let new_hosts = diff_new_hosts(obs, &known);
    store.ingest_sample(obs, now);
    if cold {
        Vec::new()
    } else {
        new_hosts
    }
}

// ---------------------------------------------------------------------------
// Classifier 2 — beacon cadence (PURE)
// ---------------------------------------------------------------------------

/// The verdict of the cadence classifier for one talker's timestamp series.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BeaconVerdict {
    pub is_beacon: bool,
    /// Mean inter-arrival delta (seconds); 0.0 when undecidable.
    pub period_secs: f64,
    /// Coefficient of variation of the deltas (stddev/mean); lower = more
    /// regular. 0.0 when undecidable.
    pub jitter_ratio: f64,
    /// Number of timestamps considered.
    pub samples: usize,
}

impl BeaconVerdict {
    fn not_beacon(samples: usize) -> Self {
        Self {
            is_beacon: false,
            period_secs: 0.0,
            jitter_ratio: 0.0,
            samples,
        }
    }
}

/// PURE beacon-cadence classifier. Given a timestamp series (any order) and the
/// thresholds, decide whether the inter-arrival deltas are regular enough — and
/// on a plausible interval — to look like a C2 callback. A perfectly periodic
/// series has coefficient of variation 0; bursty or random traffic drives it
/// high. Undecidable inputs (too few samples, a non-positive mean) return a
/// non-beacon verdict, never a panic.
pub fn classify_cadence(timestamps: &[u64], t: &BeaconThresholds) -> BeaconVerdict {
    let samples = timestamps.len();
    if samples < t.min_samples.max(2) {
        return BeaconVerdict::not_beacon(samples);
    }
    let mut ts = timestamps.to_vec();
    ts.sort_unstable();
    let deltas: Vec<f64> = ts.windows(2).map(|w| (w[1] - w[0]) as f64).collect();
    let n = deltas.len() as f64;
    let mean = deltas.iter().sum::<f64>() / n;
    if mean <= 0.0 {
        return BeaconVerdict::not_beacon(samples);
    }
    let variance = deltas.iter().map(|d| (d - mean).powi(2)).sum::<f64>() / n;
    let jitter_ratio = variance.sqrt() / mean;
    let is_beacon = mean >= t.min_interval_secs as f64
        && mean <= t.max_interval_secs as f64
        && jitter_ratio <= t.max_jitter_ratio;
    BeaconVerdict {
        is_beacon,
        period_secs: mean,
        jitter_ratio,
        samples,
    }
}

// ---------------------------------------------------------------------------
// Guard — rides EDITH's quiet-hours + cooldown + debounce
// ---------------------------------------------------------------------------

/// The suppression policy for egress alerts. `quiet_start`/`quiet_end` are the
/// SAME `[proactive]` band EDITH uses (fed in at the live edge); the cooldown /
/// min-gap come from `[egress]`. Pure data so [`guard_alert`] is testable.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AlertGuardPolicy {
    pub quiet_start: u8,
    pub quiet_end: u8,
    /// Don't repeat the SAME alert key until this many seconds pass.
    pub cooldown_secs: u64,
    /// Never two egress alerts (any key) closer than this — the debounce.
    pub min_gap_secs: u64,
}

/// The gate decision for one candidate alert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlertGate {
    /// Emit the alert.
    Allow,
    /// Suppressed by a guard; the `&str` names which one (for logging).
    Suppressed(&'static str),
}

/// Hard cap on distinct alert keys the ledger retains, bounding memory over the
/// daemon's lifetime (every other structure here is capped too: max_talkers,
/// max_samples_per_talker, retention_secs).
const MAX_LEDGER_KEYS: usize = 4096;

/// Per-key cooldown + global debounce ledger — the same shape as EDITH's
/// `anticipate::FiredState`, carried by the live loop across ticks.
#[derive(Debug, Clone, Default)]
pub struct AlertLedger {
    /// (alert key, unix secs it last fired) — the per-key cooldown ledger. Bounded
    /// by `MAX_LEDGER_KEYS` (see [`AlertLedger::record`]).
    last_fired: Vec<(String, u64)>,
    /// The most recent alert time, for the global min-gap debounce.
    most_recent: Option<u64>,
}

impl AlertLedger {
    fn last_fired_at(&self, key: &str) -> Option<u64> {
        self.last_fired
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, t)| *t)
    }

    /// Record that `key` fired at `now`: stamp the cooldown ledger and advance
    /// the global debounce clock. Called by the live loop only when it ACTS on
    /// an `Allow`. BOUNDED: like every other structure in this module, the per-key
    /// ledger is capped — when it would exceed `MAX_LEDGER_KEYS` the oldest-fired
    /// key is evicted. Re-alerting an evicted key later is acceptable (worst case
    /// one extra proposal), and it prevents unbounded growth on a host that reaches
    /// many first-seen destinations over the daemon's lifetime.
    pub fn record(&mut self, key: &str, now: u64) {
        match self.last_fired.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = now,
            None => self.last_fired.push((key.to_string(), now)),
        }
        if self.last_fired.len() > MAX_LEDGER_KEYS {
            // Evict the least-recently-fired key (smallest ts) to stay bounded.
            if let Some(oldest) = self
                .last_fired
                .iter()
                .enumerate()
                .min_by_key(|(_, (_, t))| *t)
                .map(|(i, _)| i)
            {
                self.last_fired.swap_remove(oldest);
            }
        }
        self.most_recent = Some(now);
    }
}

/// THE guard. Deterministic and pure: an egress finding survives only if it is
/// (1) outside EDITH's quiet-hours band, (2) past this key's cooldown, and
/// (3) past the global debounce gap. The order mirrors `anticipate::evaluate`.
pub fn guard_alert(
    key: &str,
    local_hour: u8,
    now: u64,
    ledger: &AlertLedger,
    policy: &AlertGuardPolicy,
) -> AlertGate {
    // 1. Quiet hours — reuse EDITH's exact band predicate.
    if crate::anticipate::in_quiet_hours(local_hour, policy.quiet_start, policy.quiet_end) {
        return AlertGate::Suppressed("quiet_hours");
    }
    // 2. Per-key cooldown — don't renag on the same talker.
    if let Some(last) = ledger.last_fired_at(key) {
        if now.saturating_sub(last) < policy.cooldown_secs {
            return AlertGate::Suppressed("cooldown");
        }
    }
    // 3. Global debounce — never two egress alerts closer than the min gap.
    if let Some(last) = ledger.most_recent {
        if now.saturating_sub(last) < policy.min_gap_secs {
            return AlertGate::Suppressed("debounce");
        }
    }
    AlertGate::Allow
}

// ---------------------------------------------------------------------------
// Propose-only firewall rule rendering (PURE — never applied)
// ---------------------------------------------------------------------------

/// PURE: render a pf/pfctl block rule as TEXT for the operator to review and
/// apply THEMSELVES with `sudo`. This module NEVER runs `pfctl` and NEVER
/// mutates the firewall — the returned string is advisory only, carrying the
/// exact command and its undo so the human stays in control.
pub fn render_block_proposal(process: &str, host: &str, port: u16, reason: &str) -> String {
    format!(
        "# DARWIN egress proposal — PROPOSE-ONLY. DARWIN never applies this; you do.\n\
         # Reason: {reason}\n\
         # Talker: process '{process}' -> {host}:{port}\n\
         #\n\
         # Review, then apply yourself (requires sudo; pf must be enabled):\n\
         #   echo \"block drop out quick proto tcp from any to {host} port {port}\" | sudo pfctl -a darwin_egress -f -\n\
         #   sudo pfctl -e   # only if pf is not already enabled\n\
         # Undo:\n\
         #   sudo pfctl -a darwin_egress -F rules"
    )
}

// ---------------------------------------------------------------------------
// Telemetry frame builders (PURE)
// ---------------------------------------------------------------------------

/// The `egress.newhost` telemetry payload for a first-seen talker.
pub fn newhost_frame(o: &Observation, proposal: &str) -> Value {
    json!({
        "process": o.process,
        "host": o.host,
        "port": o.port,
        "first_seen_ts": o.ts,
        "proposal": proposal,
        "caveat": UID_CAVEAT,
    })
}

/// The `egress.beacon` telemetry payload for a suspected beacon talker.
pub fn beacon_frame(o: &Observation, v: &BeaconVerdict, proposal: &str) -> Value {
    json!({
        "process": o.process,
        "host": o.host,
        "port": o.port,
        // Rounded so the HUD renders a clean cadence, not float noise.
        "period_secs": (v.period_secs * 10.0).round() / 10.0,
        "jitter_ratio": (v.jitter_ratio * 1000.0).round() / 1000.0,
        "samples": v.samples,
        "proposal": proposal,
        "caveat": UID_CAVEAT,
    })
}

// ---------------------------------------------------------------------------
// Live loop (runtime-only — the pure pieces above are what the tests cover)
// ---------------------------------------------------------------------------

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The live baseline+beacon loop. Runtime-only (never run in tests): every
/// `sample_interval_secs` it samples the read-only egress snapshot, folds it into
/// the longitudinal store, runs the two PURE classifiers, and emits a guarded,
/// propose-only `egress.newhost` / `egress.beacon` frame for any survivor. It
/// changes nothing on the host.
///
/// `baseline_db` is the durable baseline (main.rs opens it beside the TCC /
/// persistence sentinel baselines). `None` — the store failed to open — falls
/// back to exactly the old in-memory behaviour: a per-boot baseline whose first
/// sample seeds silently.
pub async fn run_task(
    cfg: std::sync::Arc<crate::config::Config>,
    baseline_db: Option<std::sync::Arc<EgressBaselineDb>>,
) {
    let ec = &cfg.egress;
    tokio::time::sleep(Duration::from_secs(ec.startup_delay_secs)).await;
    let interval = Duration::from_secs(ec.sample_interval_secs.max(1));
    let thresholds = BeaconThresholds::from_config(ec);
    let guard_policy = AlertGuardPolicy {
        // Ride EDITH's configured quiet-hours band verbatim.
        quiet_start: cfg.proactive.quiet_start,
        quiet_end: cfg.proactive.quiet_end,
        cooldown_secs: ec.alert_cooldown_secs,
        min_gap_secs: ec.alert_min_gap_secs,
    };
    let retention = RetentionPolicy::from_config(ec);
    // Resume the previous run's baseline so `egress.newhost` is a statement
    // about the network, not about daemon uptime. An unreadable store degrades
    // to the old in-memory behaviour (warn once, alert on nothing this boot's
    // first sample) — toward FEWER alerts, never a wedge and never a flood.
    let mut store = match &baseline_db {
        Some(db) => match db.load().await {
            Ok(rows) => BaselineStore::from_persisted(retention, rows),
            Err(e) => {
                warn!(error = %e, "egress: baseline load failed; using an empty in-memory baseline this run");
                BaselineStore::new(retention)
            }
        },
        None => BaselineStore::new(retention),
    };
    let mut ledger = AlertLedger::default();

    loop {
        tokio::time::sleep(interval).await;
        let now = now_secs();
        let local_hour = chrono::Local::now().hour() as u8;

        let obs: Vec<Observation> = crate::egress::sample_talkers()
            .await
            .into_iter()
            .filter_map(|(process, remote)| {
                let (host, port) = split_host_port(&remote);
                // SELF-EXCLUSION (mirrors persistence.rs's SELF_LABELS discipline):
                // never alarm on DARWIN's OWN egress — its api.anthropic.com cloud
                // lifeline is owned by `darwind`, and the HUD/telemetry socket is a
                // loopback connection. Both are the daemon working as designed, not a
                // suspicious talker; flagging them (or proposing a pf rule to sever the
                // cloud lifeline) would be a self-inflicted false positive.
                if is_self_or_loopback(&process, &host) {
                    return None;
                }
                Some(Observation {
                    process,
                    host,
                    port,
                    ts: now,
                })
            })
            .collect();

        // New-host pass, diff BEFORE ingest (fold_and_diff), so a brand-new pair
        // is flagged exactly on the tick it first appears (and never again — it
        // is in the baseline after). A COLD store's first sample seeds silently.
        let new_hosts = fold_and_diff(&mut store, &obs, now);
        // Persist the fold so the NEXT boot diffs against what this run knew. A
        // save failure is a warning, not an alert: the loop keeps its in-memory
        // baseline and merely risks a stale file on restart.
        if let Some(db) = &baseline_db {
            if let Err(e) = db.save(&store.to_persisted()).await {
                warn!(error = %e, "egress: baseline save failed; the on-disk baseline is stale");
            }
        }

        for o in &new_hosts {
            let key = format!("newhost:{}:{}", o.process, o.host);
            match guard_alert(&key, local_hour, now, &ledger, &guard_policy) {
                AlertGate::Allow => {
                    let proposal =
                        render_block_proposal(&o.process, &o.host, o.port, "first-seen outbound talker");
                    info!(process = %o.process, host = %o.host, "egress: new outbound talker");
                    // The per-boot false positive is FIXED (the baseline survives
                    // restarts via EgressBaselineDb, and a cold store's first
                    // sample seeds silently — test: a_restart_does_not_re_alert_
                    // on_a_known_talker). The finding is real now, and it still
                    // stays off the HUD, for a reason that is about the OWNER,
                    // not a defect: the host is a bare IP (`lsof -nP`), and
                    // ordinary browsing mints new (process, IP) pairs continuously
                    // — measured on a desktop: 3 new pairs in one 45s window, all
                    // browser-owned — so a rendered row would be dominated by
                    // "browser -> fresh CDN IP" at the 5-minute debounce floor
                    // (up to 288 rows/day). Nobody acts on that row; drawing it
                    // trains dismissal. The regular-cadence beacon alert below IS
                    // rendered. PIXEL-FREE(diagnostic): operator stream only; the
                    // rendered pf proposal still rides this frame.
                    crate::telemetry::emit("egress", "egress.newhost", newhost_frame(o, &proposal));
                    ledger.record(&key, now);
                }
                AlertGate::Suppressed(reason) => {
                    debug!(process = %o.process, host = %o.host, reason, "egress: new-host alert suppressed");
                }
            }
        }

        // Beacon cadence over each distinct (process, host) seen this tick.
        let mut checked: HashSet<(String, String)> = HashSet::new();
        for o in &obs {
            if !checked.insert((o.process.clone(), o.host.clone())) {
                continue;
            }
            let series = store.edge_timestamps(&o.process, &o.host);
            let verdict = classify_cadence(&series, &thresholds);
            if !verdict.is_beacon {
                continue;
            }
            let key = format!("beacon:{}:{}", o.process, o.host);
            match guard_alert(&key, local_hour, now, &ledger, &guard_policy) {
                AlertGate::Allow => {
                    let reason = format!("regular ~{:.0}s callback cadence", verdict.period_secs);
                    let proposal = render_block_proposal(&o.process, &o.host, o.port, &reason);
                    info!(process = %o.process, host = %o.host, period = verdict.period_secs, "egress: suspected beacon");
                    crate::telemetry::emit("egress", "egress.beacon", beacon_frame(o, &verdict, &proposal));
                    ledger.record(&key, now);
                }
                AlertGate::Suppressed(reason) => {
                    debug!(process = %o.process, host = %o.host, reason, "egress: beacon alert suppressed");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thresholds() -> BeaconThresholds {
        BeaconThresholds {
            min_samples: 6,
            min_interval_secs: 30,
            max_interval_secs: 3600,
            max_jitter_ratio: 0.15,
        }
    }

    fn retention() -> RetentionPolicy {
        RetentionPolicy {
            max_talkers: 4,
            max_samples_per_talker: 8,
            retention_secs: 86_400,
        }
    }

    fn obs(process: &str, host: &str, port: u16, ts: u64) -> Observation {
        Observation {
            process: process.to_string(),
            host: host.to_string(),
            port,
            ts,
        }
    }

    // ---- split_host_port ----

    #[test]
    fn split_host_port_handles_ipv4_ipv6_and_bare() {
        assert_eq!(split_host_port("93.184.216.34:443"), ("93.184.216.34".into(), 443));
        assert_eq!(split_host_port("[2001:db8::1]:8443"), ("2001:db8::1".into(), 8443));
        assert_eq!(split_host_port("example.com:80"), ("example.com".into(), 80));
        // Unparseable / missing port degrades to 0, never panics.
        assert_eq!(split_host_port("host-only"), ("host-only".into(), 0));
        assert_eq!(split_host_port("1.2.3.4:notaport"), ("1.2.3.4".into(), 0));
    }

    // ---- Classifier 1: new-host baseline diff ----

    /// A perfectly ordinary desktop sample: nothing here is suspicious.
    fn ordinary_sample(ts: u64) -> Vec<Observation> {
        vec![
            obs("Google Chrome", "142.250.72.14", 443, ts),
            obs("Mail", "17.42.251.7", 993, ts),
            obs("softwareupdated", "17.253.55.202", 443, ts),
        ]
    }

    /// THE COLD-START PROPERTY, FIXED AND RE-PINNED. The test that used to live
    /// here measured the defect: run_task's baseline was in-memory-only, so the
    /// first tick of EVERY boot called the owner's ordinary traffic first-seen
    /// and the global debounce let one arbitrary talker through. Now a COLD
    /// store's first sample is a silent seed (mirroring tcc.rs::sentinel_tick's
    /// cold start): [`fold_and_diff`] returns NO findings on the seeding tick,
    /// the same traffic stays silent afterwards, and only a pair genuinely new
    /// AFTER the seed is a finding.
    #[test]
    fn a_cold_store_seeds_silently_then_flags_only_genuinely_new_talkers() {
        let mut store = BaselineStore::new(retention());
        assert!(store.is_cold(), "a fresh store must be cold — the seed depends on it");

        let ordinary = ordinary_sample(100);
        let first = fold_and_diff(&mut store, &ordinary, 100);
        assert!(
            first.is_empty(),
            "the seeding tick is an inventory, not a finding set: {first:?}"
        );
        assert!(!store.is_cold(), "a non-empty sample must latch the seed");

        // The same traffic on the next tick is still silent…
        let again = fold_and_diff(&mut store, &ordinary_sample(160), 160);
        assert!(again.is_empty(), "identical traffic must be silent once baselined");

        // …and a pair that genuinely appears AFTER the seed IS a finding.
        let mut with_new = ordinary_sample(220);
        with_new.push(obs("implant", "203.0.113.7", 443, 220));
        let found = fold_and_diff(&mut store, &with_new, 220);
        assert_eq!(found.len(), 1, "exactly the new pair: {found:?}");
        assert_eq!(found[0].host, "203.0.113.7");
    }

    /// An all-empty first sample (network down at boot) must NOT latch the seed:
    /// the store learns nothing from nothing, and the first REAL sample still
    /// seeds silently instead of flagging the whole machine.
    #[test]
    fn an_empty_first_sample_does_not_consume_the_silent_seed() {
        let mut store = BaselineStore::new(retention());
        assert!(fold_and_diff(&mut store, &[], 40).is_empty());
        assert!(store.is_cold(), "an empty sample must not latch the seed");
        let first_real = fold_and_diff(&mut store, &ordinary_sample(100), 100);
        assert!(first_real.is_empty(), "the first real sample still seeds silently");
    }

    /// THE PROPERTY THE PERSISTENCE EXISTS FOR: a restart does not re-alert on a
    /// known talker. Round-trip the store through EgressBaselineDb exactly as
    /// run_task does (save after ingest; load at boot) and diff the same
    /// ordinary sample — silence. And the diff did not go blind: a talker first
    /// seen only AFTER the reload is still a finding.
    #[tokio::test]
    async fn a_restart_does_not_re_alert_on_a_known_talker() {
        let db = EgressBaselineDb::in_memory().unwrap();
        let mut store = BaselineStore::new(retention());
        fold_and_diff(&mut store, &ordinary_sample(100), 100); // silent seed
        db.save(&store.to_persisted()).await.unwrap();

        // "Restart": a fresh store loaded from the DB — run_task's boot path.
        let mut reloaded = BaselineStore::from_persisted(retention(), db.load().await.unwrap());
        assert!(
            !reloaded.is_cold(),
            "a store loaded from a non-empty baseline must not look cold, or every boot re-seeds"
        );
        let after_restart = fold_and_diff(&mut reloaded, &ordinary_sample(200), 200);
        assert!(
            after_restart.is_empty(),
            "a restart must not re-alert on known talkers: {after_restart:?}"
        );

        let mut with_new = ordinary_sample(260);
        with_new.push(obs("implant", "203.0.113.7", 443, 260));
        let found = fold_and_diff(&mut reloaded, &with_new, 260);
        assert_eq!(found.len(), 1, "a genuinely new pair after the reload still fires");
        assert_eq!(found[0].host, "203.0.113.7");
    }

    /// Round-trip fidelity: edges, last_seen and PRESENCE survive the DB. The
    /// presence bit is load-bearing — a connection that was open when the
    /// previous run stopped, and is still open at the first post-restart
    /// sample, must NOT gain a fabricated rising edge (six regular restarts
    /// would otherwise dress a long-lived tunnel up as a beacon).
    #[tokio::test]
    async fn the_round_trip_keeps_edges_and_does_not_fabricate_an_edge_for_a_still_open_socket() {
        let db = EgressBaselineDb::in_memory().unwrap();
        let mut store = BaselineStore::new(retention());
        // A reappearing beacon: edges at 0, 120, 240 — and present at the end.
        for tick in 0..5u64 {
            let now = tick * 60;
            let sample = if tick % 2 == 0 {
                vec![obs("implant", "203.0.113.7", 443, now)]
            } else {
                vec![]
            };
            store.ingest_sample(&sample, now);
        }
        // A persistent talker, present in every sample including the last.
        store.ingest_sample(&[obs("vpn", "10.0.0.1", 443, 300)], 300);
        db.save(&store.to_persisted()).await.unwrap();

        let mut reloaded = BaselineStore::from_persisted(retention(), db.load().await.unwrap());
        assert_eq!(
            reloaded.edge_timestamps("implant", "203.0.113.7"),
            vec![0, 120, 240],
            "the rising-edge series must survive the restart — cadence reads it"
        );
        assert_eq!(reloaded.edge_timestamps("vpn", "10.0.0.1"), vec![300]);

        // Still open across the restart: the first post-restart sample must not
        // stamp a new edge for it.
        reloaded.ingest_sample(&[obs("vpn", "10.0.0.1", 443, 360)], 360);
        assert_eq!(
            reloaded.edge_timestamps("vpn", "10.0.0.1"),
            vec![300],
            "a still-open socket across a restart gains NO fabricated rising edge"
        );
    }

    /// A corrupt `edges` cell degrades to an EDGELESS known talker: the pair
    /// still counts toward the baseline (corruption fails toward FEWER alerts,
    /// never toward re-alarming on a known talker), and nothing panics.
    #[tokio::test]
    async fn a_corrupt_edges_cell_degrades_to_an_edgeless_known_talker() {
        let db = EgressBaselineDb::in_memory().unwrap();
        {
            let conn = db.conn.lock().await;
            conn.execute(
                "INSERT INTO egress_baseline(process, host, port, last_seen, present, edges)
                 VALUES('Mail', '17.42.251.7', 993, 50, 1, 'not-json')",
                [],
            )
            .unwrap();
        }
        let rows = db.load().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].edges.is_empty(), "corrupt edges degrade to empty, never a panic");
        let store = BaselineStore::from_persisted(retention(), rows);
        assert!(
            store
                .known_host_pairs()
                .contains(&("Mail".to_string(), "17.42.251.7".to_string())),
            "the pair still counts as KNOWN"
        );
        assert!(!store.is_cold(), "a degraded row still seeds the store");
    }

    /// The retention BOUNDS are re-imposed on load: an oversized file (config
    /// shrank, or hand-edited) is clamped to `max_talkers` most-recent rows and
    /// `max_samples_per_talker` newest edges — the daemon's memory stays bounded
    /// no matter what is on disk.
    #[tokio::test]
    async fn load_reimposes_the_retention_bounds() {
        let db = EgressBaselineDb::in_memory().unwrap();
        let rows: Vec<PersistedTalker> = (0..10u64)
            .map(|i| PersistedTalker {
                process: "p".into(),
                host: format!("h{i}"),
                port: 1,
                last_seen: i * 10,
                present: false,
                edges: (0..20).map(|e| e * 7).collect(), // 20 edges, ring cap is 8
            })
            .collect();
        db.save(&rows).await.unwrap();
        let store = BaselineStore::from_persisted(retention(), db.load().await.unwrap()); // max_talkers 4
        let known = store.known_host_pairs();
        assert_eq!(known.len(), 4, "distinct-talker cap re-imposed on load");
        assert!(known.contains(&("p".to_string(), "h9".to_string())), "most-recent rows win");
        assert!(!known.contains(&("p".to_string(), "h0".to_string())));
        assert_eq!(
            store.edge_timestamps("p", "h9").len(),
            8,
            "per-talker edge ring re-imposed on load (newest kept)"
        );
        assert_eq!(
            store.edge_timestamps("p", "h9"),
            vec![84, 91, 98, 105, 112, 119, 126, 133],
            "the NEWEST edges are the ones kept"
        );
    }

    #[test]
    fn new_host_diff_flags_only_unknown_pairs_once() {
        let known: HashSet<(String, String)> =
            [("curl".to_string(), "1.1.1.1".to_string())].into_iter().collect();
        let current = vec![
            obs("curl", "1.1.1.1", 443, 100),   // known -> not flagged
            obs("evil", "9.9.9.9", 443, 100),   // new
            obs("evil", "9.9.9.9", 8443, 100),  // same new pair, different port -> deduped
            obs("evil", "8.8.8.8", 53, 100),    // another new pair
        ];
        let new = diff_new_hosts(&current, &known);
        assert_eq!(new.len(), 2, "one finding per new (process,host) pair");
        assert!(new.iter().any(|o| o.host == "9.9.9.9" && o.port == 443));
        assert!(new.iter().any(|o| o.host == "8.8.8.8"));
        assert!(!new.iter().any(|o| o.host == "1.1.1.1"), "known pair never flagged");
    }

    #[test]
    fn new_host_diff_empty_when_all_known() {
        let known: HashSet<(String, String)> =
            [("a".to_string(), "h".to_string())].into_iter().collect();
        assert!(diff_new_hosts(&[obs("a", "h", 1, 0)], &known).is_empty());
    }

    // ---- Classifier 2: beacon cadence ----

    #[test]
    fn cadence_flags_a_regular_beacon() {
        let t = thresholds();
        // Perfectly periodic 60s callbacks.
        let series = [0, 60, 120, 180, 240, 300];
        let v = classify_cadence(&series, &t);
        assert!(v.is_beacon, "regular cadence must be flagged: {v:?}");
        assert!((v.period_secs - 60.0).abs() < 1e-9);
        assert!(v.jitter_ratio < 1e-9, "zero jitter for a perfect beacon");
    }

    #[test]
    fn cadence_flags_a_slightly_jittered_beacon() {
        let t = thresholds();
        // ~60s with small real-world jitter -> still under the CV ceiling.
        let series = [0, 58, 121, 179, 241, 300];
        let v = classify_cadence(&series, &t);
        assert!(v.is_beacon, "small jitter still a beacon: {v:?}");
        assert!(v.jitter_ratio <= t.max_jitter_ratio);
    }

    #[test]
    fn cadence_rejects_bursty_traffic() {
        let t = thresholds();
        // A burst then a long gap then a burst -> high variance -> not a beacon.
        let series = [0, 1, 2, 3, 600, 601, 602];
        let v = classify_cadence(&series, &t);
        assert!(!v.is_beacon, "bursty traffic must not be flagged: {v:?}");
        assert!(v.jitter_ratio > t.max_jitter_ratio);
    }

    #[test]
    fn cadence_rejects_random_traffic() {
        let t = thresholds();
        let series = [0, 50, 300, 340, 900, 1500];
        let v = classify_cadence(&series, &t);
        assert!(!v.is_beacon, "irregular traffic must not be flagged: {v:?}");
    }

    #[test]
    fn cadence_needs_enough_samples() {
        let t = thresholds();
        // Perfectly regular but too few samples to trust.
        let v = classify_cadence(&[0, 60, 120], &t);
        assert!(!v.is_beacon, "too few samples is undecidable, not a beacon");
        assert_eq!(v.samples, 3);
    }

    #[test]
    fn cadence_is_panic_free_on_degenerate_input() {
        let t = thresholds();
        assert!(!classify_cadence(&[], &t).is_beacon);
        // All-identical timestamps -> mean delta 0 -> undecidable, no NaN panic.
        assert!(!classify_cadence(&[5, 5, 5, 5, 5, 5], &t).is_beacon);
    }

    #[test]
    fn cadence_rejects_a_period_outside_the_band() {
        let t = thresholds();
        // Regular but sub-min-interval (every 5s) -> bursty reconnection noise.
        let fast = [0, 5, 10, 15, 20, 25];
        assert!(!classify_cadence(&fast, &t).is_beacon, "sub-min-interval is not a beacon");
    }

    // ---- Longitudinal store: rising edges + retention ----

    #[test]
    fn store_records_one_edge_for_a_persistent_talker() {
        // A LONG-LIVED connection present in every sample must NOT masquerade as
        // a sample-interval beacon: it produces a single rising edge.
        let mut store = BaselineStore::new(retention());
        for tick in 0..6u64 {
            store.ingest_sample(&[obs("vpn", "10.0.0.1", 443, tick * 60)], tick * 60);
        }
        let series = store.edge_timestamps("vpn", "10.0.0.1");
        assert_eq!(series, vec![0], "persistent connection = a single rising edge");
        assert!(!classify_cadence(&series, &thresholds()).is_beacon);
    }

    /// REGRESSION: a connection that is ESTABLISHED in every sample past
    /// `retention_secs` is never re-flagged as a first-seen talker.
    ///
    /// `last_seen` was written ONLY by `record_edge`, i.e. only on an
    /// absent->present transition, while `prune` read it as "last observed". So a
    /// socket held continuously open (VPN tunnel, IMAP IDLE, websocket keepalive)
    /// was pruned at t = retention_secs even though it never went away — and the
    /// next sample then reported it as a brand-new outbound talker, raising a false
    /// `egress.newhost` alert with a pfctl block-rule proposal, once per cooldown
    /// window, forever. Its rising-edge series was wiped at the same moment and
    /// could never be rebuilt while the socket stayed up. The pre-existing
    /// persistent-talker test only ran 6 ticks (300s), far under retention.
    #[test]
    fn a_continuously_present_talker_is_never_re_flagged_after_retention() {
        let policy = retention(); // retention_secs = 86_400, i.e. the shipped 24h
        let mut store = BaselineStore::new(policy);
        let mut new_host_ticks: Vec<u64> = Vec::new();
        // 30h of 60s samples, the connection ESTABLISHED in every single one.
        for tick in 0..1800u64 {
            let now = tick * 60;
            let sample = vec![obs("vpn", "10.0.0.1", 443, now)];
            // Exactly run_task's order: baseline read -> diff -> ingest.
            let known = store.known_host_pairs();
            if !diff_new_hosts(&sample, &known).is_empty() {
                new_host_ticks.push(now);
            }
            store.ingest_sample(&sample, now);
        }
        assert_eq!(
            new_host_ticks,
            vec![0],
            "the ONLY first-seen report may be the very first sample; a still-open \
             connection must never be re-alerted: {new_host_ticks:?}"
        );
        assert_eq!(
            store.edge_timestamps("vpn", "10.0.0.1"),
            vec![0],
            "the rising-edge series must survive too — the cadence classifier reads it"
        );
        assert!(
            store.known_host_pairs().contains(&("vpn".to_string(), "10.0.0.1".to_string())),
            "the talker must still be in the baseline after 30h of being connected"
        );
    }

    /// A talker that GENUINELY goes away for longer than retention is still pruned
    /// (the fix must not turn the retention bound into a leak), and it can be
    /// re-recorded afterwards — `present` is cleaned up alongside `talkers`.
    #[test]
    fn a_talker_that_really_disappears_is_still_pruned_and_can_return() {
        let mut store = BaselineStore::new(retention());
        store.ingest_sample(&[obs("curl", "203.0.113.7", 443, 0)], 0);
        assert!(store.known_host_pairs().contains(&("curl".to_string(), "203.0.113.7".to_string())));
        // Gone, and time passes beyond retention_secs.
        store.ingest_sample(&[], 60);
        store.ingest_sample(&[], 86_400 + 120);
        assert!(
            !store.known_host_pairs().contains(&("curl".to_string(), "203.0.113.7".to_string())),
            "a genuinely absent talker must still be pruned at the retention bound"
        );
        // It comes back: a rising edge is recorded again.
        let now = 86_400 + 180;
        store.ingest_sample(&[obs("curl", "203.0.113.7", 443, now)], now);
        assert_eq!(store.edge_timestamps("curl", "203.0.113.7"), vec![now]);
    }

    #[test]
    fn store_accumulates_a_regular_series_for_a_reappearing_beacon() {
        // A short-lived beacon: present on even ticks, gone on odd ticks. Each
        // reappearance is a rising edge -> a regular 120s series.
        let mut store = BaselineStore::new(retention());
        for tick in 0..12u64 {
            let now = tick * 60;
            let sample = if tick % 2 == 0 {
                vec![obs("implant", "203.0.113.7", 443, now)]
            } else {
                vec![]
            };
            store.ingest_sample(&sample, now);
        }
        let series = store.edge_timestamps("implant", "203.0.113.7");
        assert_eq!(series, vec![0, 120, 240, 360, 480, 600], "one edge per reappearance");
        assert!(classify_cadence(&series, &thresholds()).is_beacon);
    }

    #[test]
    fn store_bounds_edges_per_talker() {
        let mut store = BaselineStore::new(retention()); // ring cap 8
        for tick in 0..40u64 {
            let now = tick * 100;
            // present only on even ticks so every reappearance is an edge
            let sample = if tick % 2 == 0 {
                vec![obs("p", "h", 1, now)]
            } else {
                vec![]
            };
            store.ingest_sample(&sample, now);
        }
        assert!(
            store.edge_timestamps("p", "h").len() <= 8,
            "per-talker edge ring stays bounded"
        );
    }

    #[test]
    fn store_bounds_distinct_talkers_by_lru() {
        let mut store = BaselineStore::new(retention()); // max_talkers 4
        // Six distinct hosts, each a one-shot edge at increasing times.
        for i in 0..6u64 {
            let host = format!("h{i}");
            store.ingest_sample(&[obs("p", &host, 1, i * 10)], i * 10);
        }
        assert!(store.talkers.len() <= 4, "distinct-talker cap enforced");
        // The most recent hosts survive; the oldest were evicted.
        let known = store.known_host_pairs();
        assert!(known.contains(&("p".to_string(), "h5".to_string())));
        assert!(!known.contains(&("p".to_string(), "h0".to_string())));
    }

    #[test]
    fn store_prunes_stale_talkers() {
        let policy = RetentionPolicy {
            max_talkers: 8,
            max_samples_per_talker: 8,
            retention_secs: 100,
        };
        let mut store = BaselineStore::new(policy);
        store.ingest_sample(&[obs("p", "old", 1, 0)], 0);
        // A much later sample with a different talker prunes the stale one.
        store.ingest_sample(&[obs("p", "new", 1, 1000)], 1000);
        let known = store.known_host_pairs();
        assert!(!known.contains(&("p".to_string(), "old".to_string())), "stale talker pruned");
        assert!(known.contains(&("p".to_string(), "new".to_string())));
    }

    // ---- Guard: rides EDITH quiet-hours + cooldown + debounce ----

    fn guard_policy() -> AlertGuardPolicy {
        AlertGuardPolicy {
            quiet_start: 22,
            quiet_end: 7,
            cooldown_secs: 3600,
            min_gap_secs: 300,
        }
    }

    #[test]
    fn guard_allows_a_fresh_alert_outside_quiet_hours() {
        let ledger = AlertLedger::default();
        assert_eq!(
            guard_alert("beacon:p:h", 12, 1000, &ledger, &guard_policy()),
            AlertGate::Allow
        );
    }

    #[test]
    fn guard_suppresses_inside_quiet_hours() {
        let ledger = AlertLedger::default();
        // 02:00 local is inside the 22..7 band EDITH configured.
        assert_eq!(
            guard_alert("beacon:p:h", 2, 1000, &ledger, &guard_policy()),
            AlertGate::Suppressed("quiet_hours")
        );
    }

    #[test]
    fn guard_enforces_per_key_cooldown() {
        let mut ledger = AlertLedger::default();
        ledger.record("beacon:p:h", 1000);
        // Same key, 10 min later, cooldown is 60 min -> suppressed.
        assert_eq!(
            guard_alert("beacon:p:h", 12, 1600, &ledger, &guard_policy()),
            AlertGate::Suppressed("cooldown")
        );
        // A DIFFERENT key is NOT gagged by the first key's cooldown: at 1600 the
        // 300s debounce has also elapsed (600s > 300s), so it is allowed — proving
        // the cooldown is per-key, not global.
        assert_eq!(
            guard_alert("beacon:p:other", 12, 1600, &ledger, &guard_policy()),
            AlertGate::Allow
        );
    }

    #[test]
    fn guard_enforces_global_debounce_then_allows_after_the_gap() {
        let mut ledger = AlertLedger::default();
        ledger.record("newhost:p:a", 1000);
        // A different key 100s later -> within the 300s min-gap -> debounced.
        assert_eq!(
            guard_alert("newhost:p:b", 12, 1100, &ledger, &guard_policy()),
            AlertGate::Suppressed("debounce")
        );
        // Past the gap AND past that key's (nonexistent) cooldown -> allowed.
        assert_eq!(
            guard_alert("newhost:p:b", 12, 1400, &ledger, &guard_policy()),
            AlertGate::Allow
        );
    }

    // ---- Propose-only rule rendering ----

    #[test]
    fn proposal_is_propose_only_text_carrying_host_and_port() {
        let text = render_block_proposal("evil", "198.51.100.9", 443, "regular ~60s callback cadence");
        assert!(text.contains("198.51.100.9"), "carries the host");
        assert!(text.contains("port 443"), "carries the port");
        assert!(text.contains("PROPOSE-ONLY"), "labelled propose-only");
        assert!(text.contains("DARWIN never applies this"), "states DARWIN never applies it");
        assert!(text.contains("block drop out quick proto tcp"), "renders a pf block rule");
        assert!(text.contains("sudo pfctl"), "the user applies it themselves with sudo");
        assert!(text.contains("Undo:"), "carries the undo command");
        assert!(text.contains("regular ~60s callback cadence"), "carries the reason");
    }

    // ---- Telemetry frames ----

    #[test]
    fn frames_carry_the_fields_and_the_uid_caveat() {
        let o = obs("implant", "203.0.113.7", 443, 4242);
        let nf = newhost_frame(&o, "PROPOSAL");
        assert_eq!(nf["process"], "implant");
        assert_eq!(nf["host"], "203.0.113.7");
        assert_eq!(nf["port"], 443);
        assert_eq!(nf["first_seen_ts"], 4242);
        assert_eq!(nf["proposal"], "PROPOSAL");
        assert_eq!(nf["caveat"], UID_CAVEAT);

        let v = BeaconVerdict {
            is_beacon: true,
            period_secs: 60.04,
            jitter_ratio: 0.0123,
            samples: 6,
        };
        let bf = beacon_frame(&o, &v, "PROPOSAL");
        assert_eq!(bf["samples"], 6);
        assert_eq!(bf["period_secs"], 60.0, "period rounded to one decimal");
        assert_eq!(bf["jitter_ratio"], 0.012, "jitter rounded to three decimals");
        assert_eq!(bf["caveat"], UID_CAVEAT);
    }

    #[test]
    fn self_and_loopback_are_never_alarmed() {
        // DARWIN's own cloud lifeline (darwind -> api.anthropic.com) must never be
        // flagged or proposed for a block; the HUD/telemetry loopback socket likewise.
        assert!(is_self_or_loopback("darwind", "160.79.104.10"), "own cloud egress excluded");
        // HONEST SCOPE: the inference SIDECAR is NOT name-excluded, and this test
        // used to assert it was — with the input `"darwin-inference"`, a COMMAND
        // string `egress::sample_talkers()` can never produce (the sidecar is
        // `exec python inference/server.py`, so lsof reports `Python`, and lsof
        // truncates COMMAND to 9 chars regardless). That gave false confidence in a
        // gate that was always false. See SELF_PROCESSES.
        assert!(
            !is_self_or_loopback("Python", "1.2.3.4"),
            "the sidecar is NOT name-excluded — do not silence every python process"
        );
        assert!(
            !is_self_or_loopback("darwin-inference", "1.2.3.4"),
            "a name lsof can never report must not be in SELF_PROCESSES"
        );
        assert!(is_self_or_loopback("anything", "127.0.0.1"), "loopback excluded");
        assert!(is_self_or_loopback("anything", "127.5.5.5"), "127/8 loopback excluded");
        assert!(is_self_or_loopback("anything", "::1"), "ipv6 loopback excluded");
        assert!(is_self_or_loopback("anything", "localhost"), "localhost excluded");
        // A genuine third-party talker to a public host is NOT excluded.
        assert!(!is_self_or_loopback("implant", "203.0.113.7"), "third-party talker kept");
        assert!(!is_self_or_loopback("curl", "8.8.8.8"), "ordinary process kept");
    }

    #[test]
    fn ledger_is_bounded_and_evicts_the_oldest() {
        let mut led = AlertLedger::default();
        // Fill past the cap; each distinct key fires at an increasing ts.
        for i in 0..(MAX_LEDGER_KEYS + 50) {
            led.record(&format!("newhost:p:{i}"), i as u64);
        }
        assert!(led.last_fired.len() <= MAX_LEDGER_KEYS, "ledger stays bounded");
        // The oldest keys (smallest ts) were evicted; the most-recent key survives.
        let newest = format!("newhost:p:{}", MAX_LEDGER_KEYS + 49);
        assert!(led.last_fired_at(&newest).is_some(), "most-recent key retained");
        assert!(led.last_fired_at("newhost:p:0").is_none(), "oldest key evicted");
    }
}
