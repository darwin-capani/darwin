//! SCHEMA EVOLUTION — the one mechanism that makes an UPGRADE safe.
//!
//! THE DEFECT THIS CLOSES. Every table in DARWIN is created with `CREATE TABLE IF
//! NOT EXISTS`, and `install.sh --yes` deliberately PRESERVES `state/` (see
//! `scripts/test_install_config_preserved.sh`). So on an upgrade where a release
//! ADDS A COLUMN, the table already exists, `IF NOT EXISTS` is a silent no-op, and
//! the new column is simply absent. MEASURED against 15 constructed old-shape DBs:
//! the opener succeeds without a word in 13 of 15 cases, and the failure then lands
//! on the first real statement — `table events has no column named payload`,
//! `no such column: edges` — inside whatever subsystem got there first. A fresh
//! install is fine; CI is fine; only the machine that already held the owner's data
//! breaks.
//!
//! THE MECHANISM. `PRAGMA user_version` carries [`REVISION`]. On every open, after
//! the `CREATE TABLE IF NOT EXISTS` batch, [`ensure`] compares the on-disk columns
//! (`PRAGMA table_info`) against this module's INVENTORY and adds any that are
//! missing with `ALTER TABLE ... ADD COLUMN`. It is:
//!   * ADDITIVE ONLY — there is no `DROP TABLE`, no `DROP COLUMN`, and no table
//!     rebuild anywhere in this file. A migration that "repaired" the schema by
//!     deleting the owner's facts would be far worse than the bug.
//!   * IDEMPOTENT — a column already present is skipped; a DB already at
//!     [`REVISION`] short-circuits before it reads a single pragma.
//!   * LOUD ON WHAT IT CANNOT DO — a declared table that is absent, or a missing
//!     column whose [`Col::repair`] is empty, returns an Err NAMING the db, the
//!     table and the column instead of limping on.
//!
//! WHICH COLUMNS MAY BE BACKFILLED, AND WHY THAT IS A DATA-CLASS DECISION.
//! `ALTER TABLE ADD COLUMN` cannot add a `NOT NULL` column without a DEFAULT, so a
//! repairable [`Col::repair`] supplies one — and old rows then read back that
//! default. That is honest for a column that simply was not recorded yet
//! (`episodes.summary`, `egress_baseline.edges` — whose own loader already documents
//! an empty edge list as the degrade), and DISHONEST for a column that carries
//! integrity: backfilling `audit.entry_hash` or `audit.prev_hash` with `''` would
//! silently forge a hash chain, and backfilling `owner.profile_json` would fabricate
//! a voice enrollment. Those carry `repair: ""` — a missing one is a named hard
//! error, never an invented value.
//!
//! THE STANDING GUARD (`the_create_table_statements_match_the_inventory`) parses
//! every `CREATE TABLE IF NOT EXISTS` in `daemon/src` and fails when its column list
//! differs from the INVENTORY here. A new column therefore cannot ship without an
//! inventory entry — which is the migration. That guard is the durable part of this
//! module; the migrations it forces are the cheap part.

use anyhow::{bail, Context, Result};
use rusqlite::Connection;

/// The inventory revision stamped into `PRAGMA user_version`. BUMP THIS whenever
/// the INVENTORY below changes, so existing DBs re-run [`ensure`] once instead of
/// short-circuiting on the fast path.
pub const REVISION: i32 = 1;

/// One column of a production table.
#[derive(Clone, Copy)]
pub struct Col {
    /// The column name, exactly as the `CREATE TABLE` spells it.
    pub name: &'static str,
    /// The type + constraints used to ADD this column to a table that predates it,
    /// or `""` when it must never be added by backfill (an identity/PRIMARY KEY
    /// column, which SQLite cannot add at all, or an integrity column whose
    /// invented value would be a lie). Must be `ALTER TABLE ... ADD COLUMN`-legal:
    /// `NOT NULL` requires a DEFAULT, and PRIMARY KEY/UNIQUE are impossible.
    pub repair: &'static str,
}

/// One production table and its declared column list, in `CREATE TABLE` order.
#[derive(Clone, Copy)]
pub struct Table {
    pub name: &'static str,
    pub cols: &'static [Col],
}

const fn c(name: &'static str, repair: &'static str) -> Col {
    Col { name, repair }
}
const fn t(name: &'static str, cols: &'static [Col]) -> Table {
    Table { name, cols }
}

// ---------------------------------------------------------------------------
// INVENTORY. One entry per production table, grouped by the DB FILE that holds
// it. The standing guard pins each list to its `CREATE TABLE` statement.
// ---------------------------------------------------------------------------

/// `state/darwin.db` (memory.rs) — IRREPLACEABLE owner data: remembered facts,
/// episodes, transcripts and research notebooks. Never rebuildable.
pub const MEMORY: &[Table] = &[
    t(
        "events",
        &[
            c("id", ""),
            c("ts", "TEXT NOT NULL DEFAULT ''"),
            c("source", "TEXT NOT NULL DEFAULT ''"),
            c("kind", "TEXT NOT NULL DEFAULT ''"),
            c("payload", "TEXT"),
        ],
    ),
    t(
        "facts",
        &[
            c("id", ""),
            c("ts", "TEXT NOT NULL DEFAULT ''"),
            c("key", "TEXT NOT NULL DEFAULT ''"),
            c("value", "TEXT NOT NULL DEFAULT ''"),
            c("confidence", "REAL DEFAULT 1.0"),
        ],
    ),
    t(
        "transcripts",
        &[
            c("id", ""),
            c("ts", "TEXT NOT NULL DEFAULT ''"),
            c("wav_path", "TEXT"),
            c("text", "TEXT NOT NULL DEFAULT ''"),
            c("intent", "TEXT"),
            c("routed_to", "TEXT"),
            // The ONE column this codebase has ever added to a shipped table. It
            // used to carry a hand-written `ALTER TABLE ... ADD COLUMN` in
            // memory.rs::init_conn; the generic repair subsumes it exactly.
            c("response", "TEXT"),
        ],
    ),
    t(
        "episodes",
        &[
            c("id", ""),
            c("ts", "TEXT NOT NULL DEFAULT ''"),
            c("agent_namespace", "TEXT NOT NULL DEFAULT ''"),
            c("utterance_redacted", "TEXT NOT NULL DEFAULT ''"),
            c("topic", "TEXT NOT NULL DEFAULT ''"),
            c("salient_entities", "TEXT NOT NULL DEFAULT ''"),
            c("outcome", "TEXT NOT NULL DEFAULT ''"),
            c("summary", "TEXT NOT NULL DEFAULT ''"),
        ],
    ),
    t(
        "notebook_entries",
        &[
            c("id", ""),
            c("ts", "TEXT NOT NULL DEFAULT ''"),
            c("agent_namespace", "TEXT NOT NULL DEFAULT ''"),
            c("topic_key", "TEXT NOT NULL DEFAULT ''"),
            c("topic", "TEXT NOT NULL DEFAULT ''"),
            c("synthesized", "TEXT NOT NULL DEFAULT ''"),
        ],
    ),
    t(
        "notebook_citations",
        &[
            c("id", ""),
            c("entry_id", "INTEGER NOT NULL DEFAULT 0"),
            c("source_id", "INTEGER NOT NULL DEFAULT 0"),
            c("title", "TEXT NOT NULL DEFAULT ''"),
            c("url", "TEXT NOT NULL DEFAULT ''"),
        ],
    ),
];

/// `state/audit.db` (audit.rs) — IRREPLACEABLE and INTEGRITY-BEARING: a
/// hash-linked decision chain. Every chain field is `repair: ""` on purpose: a
/// backfilled `prev_hash`/`entry_hash` would render a forged chain verifiable.
pub const AUDIT: &[Table] = &[t(
    "audit",
    &[
        c("seq", ""),
        c("ts", ""),
        c("agent", ""),
        c("tool", ""),
        c("target_redacted", ""),
        c("decision", ""),
        c("outcome", ""),
        c("prev_hash", ""),
        c("entry_hash", ""),
    ],
)];

/// `state/docsearch.db` (docsearch.rs) — a REBUILDABLE CACHE. Every row is derived
/// from files that still exist on disk, and `reindex` regenerates it, so losing this
/// file costs a reindex and nothing else. Repairs are still additive (there is no
/// reason to drop what a reindex would only recompute).
pub const DOCSEARCH: &[Table] = &[
    t(
        "doc_chunks",
        &[
            c("id", ""),
            c("root", "TEXT NOT NULL DEFAULT ''"),
            c("file_path", "TEXT NOT NULL DEFAULT ''"),
            c("byte_offset", "INTEGER NOT NULL DEFAULT 0"),
            c("chunk_text", "TEXT NOT NULL DEFAULT ''"),
            c("vector", "TEXT"),
        ],
    ),
    t("doc_meta", &[c("key", ""), c("value", "TEXT NOT NULL DEFAULT ''")]),
];

/// `state/optimize/optimize.db` (optimize.rs) — the routing-learning corpus.
/// Bounded and self-evicting, and NOT re-derivable (these are records of past
/// turns), so it is treated as owner data: additive repairs only.
pub const OPTIMIZE: &[Table] = &[t(
    "traces",
    &[
        c("id", ""),
        c("ts", "INTEGER NOT NULL DEFAULT 0"),
        c("utterance_redacted", "TEXT NOT NULL DEFAULT ''"),
        c("intent", "TEXT NOT NULL DEFAULT ''"),
        c("agent", "TEXT NOT NULL DEFAULT ''"),
        c("mode", "TEXT NOT NULL DEFAULT ''"),
        c("tool_or_skill", "TEXT NOT NULL DEFAULT ''"),
        c("outcome", "TEXT NOT NULL DEFAULT ''"),
        c("latency_ms", "INTEGER NOT NULL DEFAULT 0"),
    ],
)];

/// `state/obol/obol.db` (obol.rs) — the cloud-spend ledger. Money the owner already
/// spent; not re-derivable. Additive repairs only.
pub const OBOL: &[Table] = &[t(
    "spend",
    &[
        c("id", ""),
        c("ts", "INTEGER NOT NULL DEFAULT 0"),
        c("model", "TEXT NOT NULL DEFAULT ''"),
        c("input_tokens", "INTEGER NOT NULL DEFAULT 0"),
        c("output_tokens", "INTEGER NOT NULL DEFAULT 0"),
        c("cache_read_tokens", "INTEGER NOT NULL DEFAULT 0"),
        c("cost_usd", "REAL NOT NULL DEFAULT 0"),
        c("agent", "TEXT NOT NULL DEFAULT ''"),
    ],
)];

/// `state/tcc_baseline.db` (tcc.rs) — a sentinel baseline. REBUILDABLE by design:
/// each sentinel re-seeds silently from the live system when its store is empty
/// (`is_empty` cold start), which fails toward FEWER alerts, never toward a flood.
/// `client`/`service` form the composite PRIMARY KEY and cannot be added.
pub const TCC: &[Table] = &[t(
    "tcc_baseline",
    &[
        c("client", ""),
        c("service", ""),
        c("decision", "TEXT NOT NULL DEFAULT ''"),
        c("first_seen", "INTEGER NOT NULL DEFAULT 0"),
        c("last_seen", "INTEGER NOT NULL DEFAULT 0"),
    ],
)];

/// `state/persistence_baseline.db` (persistence.rs) — sentinel baseline, same class
/// as [`TCC`]. `surface`/`key` are the composite PRIMARY KEY.
pub const PERSISTENCE: &[Table] = &[t(
    "persistence_baseline",
    &[
        c("surface", ""),
        c("key", ""),
        c("signed", "TEXT NOT NULL DEFAULT ''"),
        c("first_seen", "INTEGER NOT NULL DEFAULT 0"),
        c("last_seen", "INTEGER NOT NULL DEFAULT 0"),
    ],
)];

/// `state/egress_baseline.db` (egress_beacon.rs) — sentinel baseline, same class as
/// [`TCC`]. `process`/`host`/`port` are the composite PRIMARY KEY. `edges` defaults
/// to `'[]'`, which is exactly the EDGELESS degrade its own loader documents.
pub const EGRESS: &[Table] = &[t(
    "egress_baseline",
    &[
        c("process", ""),
        c("host", ""),
        c("port", ""),
        c("last_seen", "INTEGER NOT NULL DEFAULT 0"),
        c("present", "INTEGER NOT NULL DEFAULT 0"),
        c("edges", "TEXT NOT NULL DEFAULT '[]'"),
    ],
)];

/// `state/voiceid/owner.enc.db` (voiceid.rs) — the owner's voice enrollment.
/// IRREPLACEABLE (re-enrolling needs the owner's voice) and security-bearing:
/// `profile_json` is `repair: ""` so the vault is never left holding a fabricated
/// `''` profile, and `save_profile_encrypted` surfaces the refusal. THE LIMIT OF
/// WHAT THAT BUYS, measured rather than assumed: the BOOT READ
/// (`load_profile_encrypted`) swallows every error with `.ok()?`, and main.rs treats
/// `None` as "unenrolled = voice-id gates nothing" — so a shape problem here still
/// degrades the voice gate SILENTLY, whatever this decl says. Closing that needs a
/// change in `voiceid.rs` (distinguish "no vault file" from "vault unreadable"), and
/// is reported to the owner rather than made here.
pub const VOICEID: &[Table] = &[t("owner", &[c("id", ""), c("profile_json", "")])];

/// Every DB file and the tables it holds — the roster the standing guard checks
/// the sources against.
pub const ALL: &[(&str, &[Table])] = &[
    ("darwin.db", MEMORY),
    ("audit.db", AUDIT),
    ("docsearch.db", DOCSEARCH),
    ("optimize.db", OPTIMIZE),
    ("obol.db", OBOL),
    ("tcc_baseline.db", TCC),
    ("persistence_baseline.db", PERSISTENCE),
    ("egress_baseline.db", EGRESS),
    ("owner.enc.db", VOICEID),
];

/// Bring `conn`'s schema up to [`REVISION`]. Call IMMEDIATELY after the
/// `CREATE TABLE IF NOT EXISTS` batch and BEFORE any statement that names a column
/// (docsearch's embedder stamp is the one open path that reads during init, so it
/// must sit after this call).
///
/// `db` is the file's BASENAME and the lookup key into [`ALL`] — an unknown one is
/// an error, so a new DB file cannot be opened without a roster entry. Additive,
/// idempotent, and never destructive — see the module docs for why some columns
/// refuse repair.
///
/// NOT AN INJECTION SEAM: the table and column names interpolated into the
/// `ALTER TABLE` / `PRAGMA table_info` statements below come ONLY from [`ALL`],
/// which is `&'static str` written in this file. No caller-, config- or
/// model-supplied string ever reaches them — `db` itself is compared, never
/// interpolated.
pub fn ensure(conn: &Connection, db: &str) -> Result<()> {
    let Some((_, tables)) = ALL.iter().find(|(name, _)| *name == db) else {
        bail!(
            "DARWIN schema: `{db}` has no entry in the schema::ALL roster, so nothing \
             would ever migrate it. Add its tables to daemon/src/schema.rs."
        );
    };
    let found: i32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .with_context(|| format!("reading PRAGMA user_version on {db}"))?;
    if found == REVISION {
        // Fast path: this file was written by a build with the same inventory.
        return Ok(());
    }
    for tb in *tables {
        let have = column_names(conn, tb.name)
            .with_context(|| format!("reading the shape of {db}:{}", tb.name))?;
        if have.is_empty() {
            // `ensure` runs right after the CREATE TABLE batch, so an absent table
            // means the batch and this inventory disagree (a rename, or a table
            // moved to another DB file) — name it instead of failing later on a
            // "no such table" from whatever subsystem gets there first.
            bail!(
                "DARWIN schema: {db} has no table `{}`, which this build requires. \
                 NOTHING WAS CHANGED on disk. If the table was renamed, the rename \
                 needs a migration step in daemon/src/schema.rs.",
                tb.name
            );
        }
        for col in tb.cols {
            if have.iter().any(|h| h == col.name) {
                continue;
            }
            if col.repair.is_empty() {
                bail!(
                    "DARWIN schema: {db}:{}.{} is missing and MUST NOT be backfilled \
                     (it is an identity or integrity column — an invented value would \
                     be a lie). NOTHING WAS CHANGED on disk; the file is intact. This \
                     store is unusable by this build until the column is restored.",
                    tb.name,
                    col.name
                );
            }
            conn.execute(
                &format!("ALTER TABLE {} ADD COLUMN {} {}", tb.name, col.name, col.repair),
                [],
            )
            .with_context(|| {
                format!(
                    "migrating {db}: adding the missing column {}.{} ({}). No existing \
                     row or column was touched.",
                    tb.name, col.name, col.repair
                )
            })?;
        }
    }
    // Reaching here means every declared column is now present: the loop above
    // either found it, added it, or returned an Err. A second verifying pass was
    // tried here and removed — no mutation could make it fire, so it was unproven
    // code pretending to be a safety net.
    //
    // A file written by a NEWER build (found > REVISION) keeps its higher stamp:
    // lowering it would make that build re-migrate on the next rollback-forward.
    // Extra columns this build does not know are left alone — every read and write
    // in DARWIN names its columns explicitly, so an unknown nullable column is
    // inert. (An unknown NOT NULL column WITHOUT a default would break INSERTs;
    // that is a downgrade hazard, reported by `an_extra_nullable_column_is_inert`,
    // and not enforced here because refusing to open on a rollback would wedge the
    // daemon under launchd.)
    if found < REVISION {
        conn.pragma_update(None, "user_version", REVISION)
            .with_context(|| format!("stamping PRAGMA user_version={REVISION} on {db}"))?;
    }
    Ok(())
}

/// The column names of `table`, in declared order. Empty when the table is absent
/// (`PRAGMA table_info` on an unknown table returns no rows, not an error).
fn column_names(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse every `CREATE TABLE IF NOT EXISTS <name>( ... )` in `src`, returning
    /// (table, columns-in-order). Paren-depth matched, so `PRIMARY KEY(a, b)` and
    /// `CHECK(id=1)` do not confuse the column split.
    fn scrape(src: &str) -> Vec<(String, Vec<String>)> {
        const NEEDLE: &str = "CREATE TABLE IF NOT EXISTS ";
        let mut out = Vec::new();
        for (idx, _) in src.match_indices(NEEDLE) {
            let rest = &src[idx + NEEDLE.len()..];
            let Some(open) = rest.find('(') else { continue };
            let name = rest[..open].trim();
            // Prose ("re-runs the CREATE TABLE IF NOT EXISTS against the DB") never
            // has an identifier immediately followed by `(`.
            if name.is_empty() || !name.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
                continue;
            }
            let body_start = open + 1;
            let mut depth = 1usize;
            let mut end = None;
            for (i, ch) in rest[body_start..].char_indices() {
                match ch {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            end = Some(body_start + i);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let Some(end) = end else { continue };
            let body = &rest[body_start..end];
            let mut cols = Vec::new();
            let mut depth = 0usize;
            let mut part = String::new();
            for ch in body.chars() {
                match ch {
                    '(' => {
                        depth += 1;
                        part.push(ch);
                    }
                    ')' => {
                        depth -= 1;
                        part.push(ch);
                    }
                    ',' if depth == 0 => {
                        cols.push(std::mem::take(&mut part));
                    }
                    _ => part.push(ch),
                }
            }
            cols.push(part);
            let cols: Vec<String> = cols
                .iter()
                .filter_map(|p| {
                    let cleaned: String = p
                        .lines()
                        .map(|l| l.split("--").next().unwrap_or("").trim())
                        .filter(|l| !l.is_empty())
                        .collect::<Vec<_>>()
                        .join(" ");
                    let first = cleaned.split_whitespace().next()?.to_string();
                    let up = first.to_ascii_uppercase();
                    if ["PRIMARY", "UNIQUE", "CHECK", "FOREIGN", "CONSTRAINT"].contains(&up.as_str())
                    {
                        return None;
                    }
                    Some(first)
                })
                .collect();
            out.push((name.to_string(), cols));
        }
        out
    }

    fn rust_sources() -> Vec<(String, String)> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut out = Vec::new();
        let mut stack = vec![root.clone()];
        while let Some(d) = stack.pop() {
            let rd = std::fs::read_dir(&d)
                .unwrap_or_else(|e| panic!("the guard cannot read {}: {e}", d.display()));
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    let rel = p
                        .strip_prefix(&root)
                        .unwrap_or(&p)
                        .to_string_lossy()
                        .into_owned();
                    out.push((rel, std::fs::read_to_string(&p).unwrap()));
                }
            }
        }
        out
    }

    /// THE STANDING GUARD. A `CREATE TABLE` cannot gain (or lose, or rename) a
    /// column without the INVENTORY changing to match — and the INVENTORY is the
    /// migration, because [`ensure`] adds whatever it declares and the disk lacks.
    ///
    /// This is the gate the 25 tables never had: the only reason the upgrade bug has
    /// not fired yet is that no shipped table has ever changed shape (measured
    /// across all 610 commits). The next one would have, silently.
    #[test]
    fn the_create_table_statements_match_the_inventory() {
        let sources = rust_sources();
        assert!(
            sources.len() > 100,
            "the guard scanned only {} .rs file(s); the source walk has rotted and \
             this test is reading nothing",
            sources.len()
        );

        // ANTI-SELF-MATCH: this module talks ABOUT `CREATE TABLE IF NOT EXISTS` in
        // its docs. Exclude it, and PROVE the exclusion removed nothing.
        let own = sources
            .iter()
            .find(|(n, _)| n == "schema.rs")
            .expect("schema.rs is in the walk");
        assert!(
            scrape(&own.1).is_empty(),
            "schema.rs itself now declares a table; the guard is excluding real \
             production schema instead of only its own prose"
        );

        let mut inventory: std::collections::BTreeMap<&str, Vec<&str>> =
            std::collections::BTreeMap::new();
        for (_db, tables) in ALL {
            for tb in *tables {
                inventory.insert(tb.name, tb.cols.iter().map(|c| c.name).collect());
            }
        }

        let mut files_with_tables = 0usize;
        let mut sites = 0usize;
        let mut drift: Vec<String> = Vec::new();
        let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for (name, src) in &sources {
            if name == "schema.rs" {
                continue;
            }
            let found = scrape(src);
            if found.is_empty() {
                continue;
            }
            files_with_tables += 1;
            for (table, cols) in found {
                sites += 1;
                seen.insert(table.clone());
                match inventory.get(table.as_str()) {
                    None => drift.push(format!(
                        "{name}: table `{table}` has no entry in schema.rs INVENTORY \
                         (add it, with a `repair` decl per column, or ensure() will \
                         never migrate it)"
                    )),
                    Some(want) => {
                        if cols.iter().map(|s| s.as_str()).collect::<Vec<_>>() != *want {
                            drift.push(format!(
                                "{name}: `{table}` declares {cols:?} but schema.rs \
                                 INVENTORY says {want:?} — a column changed WITHOUT a \
                                 migration. Update the INVENTORY (and bump REVISION) \
                                 so ensure() adds it on upgrade; otherwise every \
                                 existing install keeps the old shape silently."
                            ))
                        }
                    }
                }
            }
        }

        // ROT CHECKS: both the file count and the site count must stay at least
        // what exists today, so a needle that stops matching fails instead of
        // reporting a vacuous all-clear.
        assert!(
            files_with_tables >= 9,
            "the guard found CREATE TABLE in only {files_with_tables} file(s); 9 have \
             one today, so the scrape has rotted"
        );
        assert!(
            sites >= 16,
            "the guard found only {sites} create-table site(s); 16 exist today, so \
             the scrape has rotted"
        );
        assert!(drift.is_empty(), "schema drift with no migration:\n{}", drift.join("\n"));

        let declared: std::collections::BTreeSet<String> =
            inventory.keys().map(|k| k.to_string()).collect();
        assert_eq!(
            declared, seen,
            "the INVENTORY and the CREATE TABLE statements name different tables"
        );
    }

    /// Every non-empty `repair` decl must actually be `ALTER TABLE ADD COLUMN`-legal
    /// — otherwise the migration would fail on the one machine it exists for.
    #[test]
    fn every_repair_declaration_is_alter_addable() {
        let mut checked = 0usize;
        for (db, tables) in ALL {
            for tb in *tables {
                for col in tb.cols {
                    if col.repair.is_empty() {
                        continue;
                    }
                    let conn = Connection::open_in_memory().unwrap();
                    // POPULATED on purpose: an ALTER that is legal on an empty table
                    // can still be refused once rows exist, and the owner's table has
                    // rows. Probing empty would pass vacuously.
                    conn.execute_batch("CREATE TABLE probe(anchor INTEGER); INSERT INTO probe VALUES(1);")
                        .unwrap();
                    conn.execute(
                        &format!("ALTER TABLE probe ADD COLUMN {} {}", col.name, col.repair),
                        [],
                    )
                    .unwrap_or_else(|e| {
                        panic!("{db}:{}.{} repair `{}` is not ALTER-addable: {e}", tb.name, col.name, col.repair)
                    });
                    checked += 1;
                }
            }
        }
        assert!(
            checked >= 60,
            "only {checked} repair decls exercised; 60 exist today, so the roster \
             shrank (or the walk over it stopped reaching them)"
        );

        // THE OTHER SIDE OF THE BOUNDARY: the `DEFAULT` in those decls is load
        // bearing, not decoration. MEASURED: SQLite only refuses a NOT NULL column
        // with no default when the table HAS ROWS — on an EMPTY table the same ALTER
        // succeeds. So this probe must be run against a POPULATED table, which is
        // exactly the upgrade case (the owner's table is never empty). Probed empty
        // first to pin that asymmetry, so this assertion cannot pass vacuously.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE probe(anchor INTEGER);").unwrap();
        conn.execute("ALTER TABLE probe ADD COLUMN ok_when_empty TEXT NOT NULL", [])
            .expect("on an EMPTY table SQLite allows it — hence the probe below");
        conn.execute("INSERT INTO probe(anchor, ok_when_empty) VALUES(1, 'x')", [])
            .unwrap();
        let e = conn
            .execute("ALTER TABLE probe ADD COLUMN x TEXT NOT NULL", [])
            .expect_err("with rows present SQLite must refuse a NOT NULL, no-default column");
        assert!(
            e.to_string().to_lowercase().contains("not null"),
            "unexpected refusal: {e}"
        );
    }

    /// THE REGRESSION TEST for the class. An `state/darwin.db` written before
    /// `facts.confidence` existed: today the opener says nothing and `upsert_fact`
    /// dies with "no such column: confidence". After the fix, the column is added
    /// and the write lands.
    #[tokio::test]
    async fn an_older_memory_db_is_migrated_and_the_write_that_used_to_fail_lands() {
        let dir = std::env::temp_dir().join(format!("darwin-schema-mig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("darwin.db");
        {
            // The 2026-era shape: no `confidence`, no `response`.
            let c = Connection::open(&path).unwrap();
            c.execute_batch(
                "CREATE TABLE events(id INTEGER PRIMARY KEY, ts TEXT NOT NULL, source TEXT NOT NULL, kind TEXT NOT NULL, payload TEXT);
                 CREATE TABLE facts(id INTEGER PRIMARY KEY, ts TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL);
                 INSERT INTO facts(ts, key, value) VALUES('2026-01-01T00:00:00Z','user.name','Darwin');
                 CREATE TABLE transcripts(id INTEGER PRIMARY KEY, ts TEXT NOT NULL, wav_path TEXT, text TEXT NOT NULL, intent TEXT, routed_to TEXT);
                 CREATE TABLE episodes(id INTEGER PRIMARY KEY, ts TEXT NOT NULL, agent_namespace TEXT NOT NULL, utterance_redacted TEXT NOT NULL, topic TEXT NOT NULL, salient_entities TEXT NOT NULL, outcome TEXT NOT NULL, summary TEXT NOT NULL);
                 CREATE TABLE notebook_entries(id INTEGER PRIMARY KEY, ts TEXT NOT NULL, agent_namespace TEXT NOT NULL, topic_key TEXT NOT NULL, topic TEXT NOT NULL, synthesized TEXT NOT NULL);
                 CREATE TABLE notebook_citations(id INTEGER PRIMARY KEY, entry_id INTEGER NOT NULL, source_id INTEGER NOT NULL, title TEXT NOT NULL, url TEXT NOT NULL);",
            )
            .unwrap();
        }
        let mem = crate::memory::Memory::open(&path).expect("the upgrade opens the old file");
        mem.upsert_fact("user.mood", "curious")
            .await
            .expect("upsert_fact must not die on `no such column: confidence`");
        assert_eq!(
            mem.get_fact("user.mood").await.unwrap().as_deref(),
            Some("curious")
        );
        // THE OWNER'S EXISTING ROW SURVIVED — the migration adds, never rebuilds.
        assert_eq!(
            mem.get_fact("user.name").await.unwrap().as_deref(),
            Some("Darwin"),
            "the pre-upgrade fact must still be there"
        );
        drop(mem);
        let c = Connection::open(&path).unwrap();
        assert_eq!(
            c.query_row("PRAGMA user_version", [], |r| r.get::<_, i32>(0)).unwrap(),
            REVISION,
            "a migrated file is stamped so the next open takes the fast path"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A missing INTEGRITY column is a NAMED error and touches nothing. The audit
    /// chain must never be "repaired" by backfilling an empty hash.
    #[test]
    fn a_missing_integrity_column_is_named_and_deletes_nothing() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE audit(seq INTEGER PRIMARY KEY, ts TEXT NOT NULL, agent TEXT NOT NULL, tool TEXT NOT NULL, target_redacted TEXT NOT NULL, decision TEXT NOT NULL, outcome TEXT NOT NULL, prev_hash TEXT NOT NULL);
             INSERT INTO audit(ts, agent, tool, target_redacted, decision, outcome, prev_hash) VALUES('t','a','tl','tg','ask','proposed','GENESIS');",
        )
        .unwrap();
        let e = ensure(&conn, "audit.db").expect_err("a missing chain column must be loud");
        let msg = e.to_string();
        assert!(msg.contains("audit.db"), "the error must name the db: {msg}");
        assert!(msg.contains("entry_hash"), "the error must name the column: {msg}");
        assert!(
            msg.contains("MUST NOT be backfilled"),
            "the error must say why it refused: {msg}"
        );
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM audit", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "the refusal must not have deleted the owner's audit row");
        let cols = column_names(&conn, "audit").unwrap();
        assert!(
            !cols.iter().any(|c| c == "entry_hash"),
            "no forged hash column was added"
        );
    }

    /// A renamed/absent table is named, not tolerated into a later "no such table".
    #[test]
    fn an_absent_table_is_named() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE spend_v2(id INTEGER PRIMARY KEY);").unwrap();
        let msg = ensure(&conn, "obol.db").unwrap_err().to_string();
        assert!(msg.contains("no table `spend`"), "must name the table: {msg}");
        assert!(msg.contains("NOTHING WAS CHANGED"), "must promise no damage: {msg}");
    }

    /// Idempotent: a second `ensure` on an already-migrated file changes nothing and
    /// takes the fast path.
    #[test]
    fn ensure_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE traces(id INTEGER PRIMARY KEY, ts INTEGER NOT NULL, utterance_redacted TEXT NOT NULL, intent TEXT NOT NULL, agent TEXT NOT NULL, mode TEXT NOT NULL, tool_or_skill TEXT NOT NULL, outcome TEXT NOT NULL);",
        )
        .unwrap();
        ensure(&conn, "optimize.db").unwrap();
        let after = column_names(&conn, "traces").unwrap();
        assert!(after.iter().any(|c| c == "latency_ms"));
        ensure(&conn, "optimize.db").unwrap();
        ensure(&conn, "optimize.db").unwrap();
        assert_eq!(after, column_names(&conn, "traces").unwrap(), "no drift on re-open");
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i32>(0)).unwrap(),
            REVISION
        );
    }

    /// THE DOWNGRADE SIDE. A file written by a NEWER build carries columns this one
    /// does not know. `ensure` leaves them alone, keeps the higher stamp, and does
    /// not error — every read/write names its columns, so an unknown NULLABLE column
    /// is inert.
    #[test]
    fn an_extra_nullable_column_is_inert_and_the_higher_stamp_survives() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE traces(id INTEGER PRIMARY KEY, ts INTEGER NOT NULL, utterance_redacted TEXT NOT NULL, intent TEXT NOT NULL, agent TEXT NOT NULL, mode TEXT NOT NULL, tool_or_skill TEXT NOT NULL, outcome TEXT NOT NULL, latency_ms INTEGER NOT NULL, future_col TEXT);
             PRAGMA user_version = 99;",
        )
        .unwrap();
        ensure(&conn, "optimize.db").expect("a newer file must still open");
        assert!(column_names(&conn, "traces").unwrap().iter().any(|c| c == "future_col"));
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i32>(0)).unwrap(),
            99,
            "a newer file's stamp must not be lowered"
        );
    }

    /// A DB opened without a roster entry is a NAMED error, not a silent skip —
    /// otherwise a future store could be wired up and never migrate.
    #[test]
    fn a_db_absent_from_the_roster_is_named() {
        let conn = Connection::open_in_memory().unwrap();
        let msg = ensure(&conn, "brand_new.db").unwrap_err().to_string();
        assert!(msg.contains("brand_new.db"), "must name the db: {msg}");
        assert!(msg.contains("schema::ALL roster"), "must name the remedy: {msg}");
    }

    /// The roster has no duplicate table and no duplicate db name.
    #[test]
    fn the_inventory_roster_is_consistent() {
        let mut dbs: Vec<&str> = ALL.iter().map(|(d, _)| *d).collect();
        let n = dbs.len();
        dbs.sort_unstable();
        dbs.dedup();
        assert_eq!(dbs.len(), n, "duplicate db file in ALL");
        let mut tables: Vec<&str> =
            ALL.iter().flat_map(|(_, ts)| ts.iter().map(|t| t.name)).collect();
        let n = tables.len();
        assert_eq!(n, 15, "15 production tables exist today; ALL lists {n}");
        tables.sort_unstable();
        tables.dedup();
        assert_eq!(tables.len(), n, "duplicate table in ALL");
    }

    /// THE FAST PATH'S FUSE — the one hole the drift guard above cannot see.
    /// [`ensure`] returns early when `PRAGMA user_version` already equals
    /// [`REVISION`], so an INVENTORY that changes WITHOUT a REVISION bump never runs
    /// against a file that is already stamped — which, one release after this ships,
    /// is EVERY install. MEASURED on a constructed old-shape `darwin.db` stamped at
    /// REVISION: `ensure` returns Ok, `facts` keeps `["id","ts","key","value"]`, and
    /// the write still dies with `table facts has no column named confidence`. That
    /// is the original upgrade defect, re-armed, and
    /// `the_create_table_statements_match_the_inventory` passes throughout (the
    /// sources and the INVENTORY agree with each other — they just never reach the
    /// disk). The fingerprint below covers every db, table, column and repair decl,
    /// so ANY inventory edit fails here until REVISION moves with it.
    #[test]
    fn the_inventory_fingerprint_is_pinned_to_the_revision() {
        let mut spec = String::new();
        for (db, tables) in ALL {
            spec.push_str(db);
            for tb in *tables {
                spec.push('|');
                spec.push_str(tb.name);
                for col in tb.cols {
                    spec.push('|');
                    spec.push_str(col.name);
                    spec.push(':');
                    spec.push_str(col.repair);
                }
            }
            spec.push('\n');
        }
        // FNV-1a/64 over that spec: stable, dependency-free, and sensitive to a
        // RENAME as well as an addition (a rename leaves the column count alone but
        // still needs the migration to run).
        let mut fp: u64 = 0xcbf2_9ce4_8422_2325;
        for b in spec.as_bytes() {
            fp ^= u64::from(*b);
            fp = fp.wrapping_mul(0x0100_0000_01b3);
        }
        assert_eq!(
            (REVISION, fp),
            (1, 0x9237_78c8_a78c_1010),
            "THE INVENTORY CHANGED WITHOUT A REVISION BUMP (or vice versa). Bump \
             REVISION and paste the fingerprint printed above into this pin, in the \
             SAME commit: ensure() short-circuits on every DB already stamped at the \
             old REVISION, so without the bump the new column is never added to an \
             existing install and the first query naming it dies on the owner's \
             machine — the exact defect this module exists to close."
        );
    }

    /// AN EMPTY `repair` IS A REFUSAL TO OPEN, and that is a startup decision.
    /// `ensure` returns Err for a missing column whose repair is `""`, and
    /// `open_memory` / `open_audit` in main.rs propagate it with `?` out of `run`,
    /// so the daemon does not start. That is the right trade for the audit hash
    /// chain and the owner's enrollment, and a catastrophic default for an ordinary
    /// new column — and nothing else here would notice one, because
    /// `every_repair_declaration_is_alter_addable` SKIPS empty decls. So the set is
    /// pinned: 28 columns today, each either an identity/PRIMARY KEY column SQLite
    /// cannot ADD at all, or a documented integrity column.
    #[test]
    fn only_the_documented_columns_refuse_repair() {
        let mut got: Vec<String> = Vec::new();
        for (_db, tables) in ALL {
            for tb in *tables {
                for col in tb.cols {
                    if col.repair.is_empty() {
                        got.push(format!("{}.{}", tb.name, col.name));
                    }
                }
            }
        }
        let want: Vec<String> = [
            // Identity / PRIMARY KEY columns: SQLite cannot ADD these.
            "events.id",
            "facts.id",
            "transcripts.id",
            "episodes.id",
            "notebook_entries.id",
            "notebook_citations.id",
            // The audit hash chain: a backfilled hash would make a forged chain
            // verify. Every column of the canonical form is in it.
            "audit.seq",
            "audit.ts",
            "audit.agent",
            "audit.tool",
            "audit.target_redacted",
            "audit.decision",
            "audit.outcome",
            "audit.prev_hash",
            "audit.entry_hash",
            "doc_chunks.id",
            "doc_meta.key",
            "traces.id",
            "spend.id",
            "tcc_baseline.client",
            "tcc_baseline.service",
            "persistence_baseline.surface",
            "persistence_baseline.key",
            "egress_baseline.process",
            "egress_baseline.host",
            "egress_baseline.port",
            // The voice gate: a fabricated '' profile reads back as "not enrolled".
            "owner.id",
            "owner.profile_json",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
        assert_eq!(
            got, want,
            "A `repair` DECL WENT EMPTY, or a new column shipped with one. An empty \
             decl means ensure() REFUSES TO OPEN any DB missing that column, which \
             stops the daemon on upgrade instead of migrating it. Give the column an \
             ALTER-addable decl, or add it to this list deliberately and say why."
        );
    }
}

