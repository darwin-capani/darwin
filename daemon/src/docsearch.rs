//! ON-DEVICE FILE RAG — the confined indexer + the cited search over the user's
//! OWN text-like files. 100% on-device; nothing here ever reaches the network.
//!
//! This is the read-only document-retrieval counterpart to MNEMOSYNE's fact +
//! episodic recall. It walks the EXPLICITLY-ALLOWLISTED `[docsearch].roots` with
//! `std::fs` (no new dependency), chunks each accepted file into overlapping
//! windows that keep a citation offset, embeds the chunks ON-DEVICE via the
//! inference `embed` op (falling back to lexical BM25 when that server is down),
//! stores the chunks (and any vectors) in a BOUNDED local SQLite table, and
//! answers a query with CITED top-k results (file path + snippet + score),
//! reporting WHICH ranking backend actually ran.
//!
//! ## The CONTRACT (non-negotiable — this reads the user's OWN files)
//!   * PRIVACY: file CONTENTS + EMBEDDINGS NEVER LEAVE THE DEVICE. Embedding is the
//!     on-device MLX embed op ([`crate::inference::InferenceClient::embed`]);
//!     nothing is uploaded. Search degrades to lexical BM25 when the embedder is
//!     down and reports which ran ([`crate::recall::RankMethod`]) — it never claims
//!     neural on fallback.
//!   * CONFINED: the index reads ONLY files under an allowlisted root. Every
//!     candidate is PATH-CONFINED ([`confine`]): canonicalize it, then assert the
//!     real path starts_with a canonicalized allowed root. A symlink that escapes a
//!     root, a `..` traversal, and an absolute-elsewhere path all RESOLVE OUTSIDE
//!     the root and are REJECTED. There is NO whole-disk scan: an empty `roots`
//!     allowlist indexes nothing even with `enabled` true.
//!   * BOUNDED: total files / total chunks / total bytes are capped, plus a
//!     per-file size cap and a recursion-depth bound. The store is finite.
//!   * FORGETTABLE: [`DocIndex::forget`] clears the index (a user can make JARVIS
//!     forget every indexed file).
//!   * HONEST: a search returns ONLY chunks that were really indexed (the snippet
//!     is the stored chunk text, the citation is its real file + offset). An empty
//!     index or a no-match query returns NOTHING — never a fabricated citation.
//!   * OFF by default: gated by `[docsearch].enabled` (ships false) AND a non-empty
//!     `roots` (ships empty). The daemon checks both before ever indexing.
//!
//! v1 indexes TEXT-LIKE files only (an extension allowlist). PDFs / binaries are
//! OUT OF SCOPE — a PDF needs a parser dependency; such files are skipped, never
//! silently treated as indexed.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use tokio::sync::Mutex;

use crate::recall::{cosine_similarity, Bm25Params, Embedder, Fact, LexicalProvider, RankMethod};

/// The extension allowlist (lowercased, no dot). TEXT-LIKE only: prose/notes +
/// common source/config formats. Anything else (pdf, images, archives, office
/// binaries, ...) is skipped — v1 does NOT parse binaries.
pub const ALLOWED_EXTENSIONS: &[&str] = &[
    // prose / notes
    "md", "markdown", "txt", "text", "rst", "org", "tex", "log",
    // config / data
    "toml", "yaml", "yml", "json", "ini", "cfg", "conf", "csv", "tsv", "env",
    // code
    "rs", "py", "js", "ts", "tsx", "jsx", "go", "java", "kt", "c", "h", "cpp",
    "cc", "hpp", "rb", "sh", "bash", "zsh", "sql", "php", "swift", "scala",
    "lua", "pl", "r", "m", "html", "htm", "css", "scss", "xml",
];

/// Default / max number of results a single [`DocIndex::search`] returns. Bounded
/// so a search is small and focused on the relevant few.
pub const DOCSEARCH_DEFAULT_K: usize = 5;
pub const DOCSEARCH_MAX_K: usize = 20;

/// How many characters of a chunk are returned as the citation SNIPPET (the full
/// chunk is stored; the snippet is a bounded preview for display).
const SNIPPET_CHARS: usize = 280;

// ---------------------------------------------------------------------------
// Bounds — the finite ceilings on a walk/index, mirrored from [docsearch] config
// ---------------------------------------------------------------------------

/// The bounded parameters of one index pass, lifted from [`crate::config::DocSearchConfig`]
/// so the indexer is testable without a full Config. All are real, finite ceilings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexBounds {
    pub max_files: usize,
    pub max_chunks: usize,
    pub max_file_bytes: usize,
    pub max_depth: usize,
    pub chunk_chars: usize,
    pub chunk_overlap: usize,
}

impl IndexBounds {
    /// Build bounds from the parsed config section, clamping to safe minimums so a
    /// degenerate config (e.g. chunk_chars 0, or overlap >= chunk size) can never
    /// produce a non-terminating chunker or a zero-progress walk.
    pub fn from_config(c: &crate::config::DocSearchConfig) -> Self {
        let chunk_chars = c.chunk_chars.max(64);
        // Overlap must be strictly less than the window or chunking never advances.
        let chunk_overlap = c.chunk_overlap.min(chunk_chars.saturating_sub(1));
        Self {
            max_files: c.max_files.max(1),
            max_chunks: c.max_chunks.max(1),
            max_file_bytes: c.max_file_bytes.max(1),
            max_depth: c.max_depth.max(1),
            chunk_chars,
            chunk_overlap,
        }
    }
}

#[cfg(test)]
impl Default for IndexBounds {
    fn default() -> Self {
        Self::from_config(&crate::config::DocSearchConfig::default())
    }
}

// ---------------------------------------------------------------------------
// PATH CONFINEMENT — the red-team-validated no-escape check
// ---------------------------------------------------------------------------

/// Canonicalize each configured root once. A root that does not exist / cannot be
/// canonicalized is DROPPED (it can confine nothing), so a typo'd or missing root
/// silently indexes nothing rather than widening the surface. Returns the real,
/// absolute, symlink-resolved roots.
pub fn canonical_roots(roots: &[String]) -> Vec<PathBuf> {
    roots
        .iter()
        .filter_map(|r| std::fs::canonicalize(r).ok())
        .collect()
}

/// PATH CONFINEMENT (the security primitive). Given a candidate path and the
/// already-canonicalized allowed roots, return the candidate's REAL path IFF it
/// resolves to a location inside one of the roots — else `None` (REJECTED).
///
/// `std::fs::canonicalize` resolves symlinks and `..` and makes the path
/// absolute, so:
///   * a symlink under a root that points OUTSIDE the root canonicalizes to its
///     real (outside) location, which fails the `starts_with` -> REJECTED;
///   * a `..` traversal resolves to the real parent -> REJECTED if outside;
///   * an absolute-elsewhere path canonicalizes to itself -> REJECTED;
///   * a file genuinely under a root canonicalizes to under the (canonicalized)
///     root -> ACCEPTED.
/// A non-existent path cannot be canonicalized -> `None` (we never index a path we
/// cannot prove resolves inside a root). The check is on the REAL path, never the
/// lexical/symlink path, so it cannot be fooled by a crafted name.
pub fn confine(candidate: &Path, canonical_roots: &[PathBuf]) -> Option<PathBuf> {
    let real = std::fs::canonicalize(candidate).ok()?;
    if canonical_roots.iter().any(|root| real.starts_with(root)) {
        Some(real)
    } else {
        None
    }
}

/// Whether a path component is a hidden entry (dotfile/dotdir) we skip — except
/// the root itself, which the walk never passes here. `.` / `..` are never walked
/// (read_dir yields neither), so any leading-dot name is a real hidden entry.
fn is_hidden(name: &str) -> bool {
    name.starts_with('.')
}

/// Whether a file's extension is on the text-like allowlist. No extension, or an
/// extension not on the list, is rejected (no binary/PDF parsing in v1).
fn extension_allowed(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => ALLOWED_EXTENSIONS.contains(&ext.to_lowercase().as_str()),
        None => false,
    }
}

/// A fast binary sniff: a file is treated as binary (and skipped) if its first
/// bytes contain a NUL. Text-like files never carry an interior NUL; this catches
/// a mislabeled binary that slipped through the extension allowlist.
fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|&b| b == 0)
}

// ---------------------------------------------------------------------------
// CHUNKING — overlapping windows with a citation offset
// ---------------------------------------------------------------------------

/// One chunk carved from a file: its text and the BYTE offset of its start in the
/// file, kept for citation ("path:offset"). Chunking is over CHARACTERS (so a
/// window never splits a UTF-8 codepoint) but the offset is the byte position of
/// the window's first character, so a caller can seek the original file exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub text: String,
    pub byte_offset: usize,
}

/// Split `content` into overlapping character windows of `chunk_chars` with
/// `overlap` carried between consecutive windows. Deterministic and TERMINATING:
/// `overlap` is clamped below `chunk_chars` by the caller (IndexBounds), so the
/// stride (`chunk_chars - overlap`) is always >= 1 and the walk always advances.
/// Empty / whitespace-only content yields no chunks. Each chunk records the BYTE
/// offset of its first character.
pub fn chunk_text(content: &str, chunk_chars: usize, overlap: usize) -> Vec<Chunk> {
    if content.trim().is_empty() || chunk_chars == 0 {
        return Vec::new();
    }
    let stride = chunk_chars.saturating_sub(overlap).max(1);
    // (char_index, byte_offset) for every character — lets us map a window's
    // start char to its byte position for the citation offset.
    let indices: Vec<(usize, char)> = content.char_indices().map(|(b, c)| (b, c)).collect();
    let n = indices.len();
    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < n {
        let end = (start + chunk_chars).min(n);
        let byte_start = indices[start].0;
        let byte_end = if end < n { indices[end].0 } else { content.len() };
        let text = content[byte_start..byte_end].to_string();
        if !text.trim().is_empty() {
            chunks.push(Chunk {
                text,
                byte_offset: byte_start,
            });
        }
        if end == n {
            break;
        }
        start += stride;
    }
    chunks
}

// ---------------------------------------------------------------------------
// THE WALK — confined, bounded discovery of indexable files
// ---------------------------------------------------------------------------

/// One discovered, confined, accepted file ready to chunk: its REAL canonical
/// path (the citation path) and the allowlisted root it lives under (so a result
/// can name which root surfaced it). Discovery NEVER reads file CONTENTS — only
/// metadata — so a non-text file is rejected before any content is loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    pub path: PathBuf,
    pub root: PathBuf,
}

/// Walk the allowlisted roots with `std::fs` (recursive, bounded depth/count) and
/// return the confined, accepted files. PURE w.r.t. the network. The walk:
///   * descends only into directories that CONFINE under a canonical root (so a
///     symlinked-out subdir is never entered);
///   * skips hidden entries (dotfiles/dotdirs);
///   * accepts a FILE only when: it confines under a root, its extension is on the
///     allowlist, and its size is within `max_file_bytes` (a metadata stat, no read);
///   * stops at `max_files` total and `max_depth` recursion depth.
/// Symlink loops cannot run away: a visited-set of real paths plus the depth bound
/// terminate the walk. Errors on any single entry are skipped, never fatal.
pub fn walk(roots: &[String], bounds: &IndexBounds) -> Vec<Discovered> {
    let canon = canonical_roots(roots);
    let mut out: Vec<Discovered> = Vec::new();
    let mut visited: HashSet<PathBuf> = HashSet::new();
    for root in &canon {
        walk_dir(root, root, 0, bounds, &canon, &mut visited, &mut out);
        if out.len() >= bounds.max_files {
            break;
        }
    }
    out.truncate(bounds.max_files);
    out
}

#[allow(clippy::too_many_arguments)]
fn walk_dir(
    dir: &Path,
    root: &Path,
    depth: usize,
    bounds: &IndexBounds,
    canon: &[PathBuf],
    visited: &mut HashSet<PathBuf>,
    out: &mut Vec<Discovered>,
) {
    if depth > bounds.max_depth || out.len() >= bounds.max_files {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() >= bounds.max_files {
            return;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if is_hidden(&name) {
            continue;
        }
        let path = entry.path();
        // Metadata WITHOUT following the final symlink, so we classify the link
        // itself; the confine() canonicalization below resolves it for the real
        // location check (a symlink escaping the root is rejected there).
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            // Confine the directory: a symlinked-out subdir resolves outside the
            // root and is never descended into.
            let Some(real_dir) = confine(&path, canon) else {
                continue;
            };
            if !visited.insert(real_dir.clone()) {
                continue; // symlink loop / already visited
            }
            walk_dir(&real_dir, root, depth + 1, bounds, canon, visited, out);
        } else if meta.is_file() || meta.file_type().is_symlink() {
            // A symlink whose target is a file is handled here; confine resolves
            // it and rejects an escape.
            if !extension_allowed(&path) {
                continue;
            }
            let Some(real) = confine(&path, canon) else {
                continue; // symlink-escape / outside-root -> REJECTED
            };
            // Re-stat the REAL path for the size cap (the link's own metadata may
            // be the link size, not the target's).
            let Ok(real_meta) = std::fs::metadata(&real) else {
                continue;
            };
            if !real_meta.is_file() || real_meta.len() as usize > bounds.max_file_bytes {
                continue;
            }
            if visited.insert(real.clone()) {
                out.push(Discovered {
                    path: real,
                    root: root.to_path_buf(),
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// THE STORE — a bounded SQLite chunk/vector table + forget
// ---------------------------------------------------------------------------

/// One stored chunk row, materialized for search/citation. `vector` is `Some`
/// only when the chunk was embedded on-device at index time; `None` means it will
/// be ranked by lexical BM25 (and the whole search reports lexical honestly).
#[derive(Debug, Clone)]
struct ChunkRow {
    /// The SQLite row id. Selected for completeness + future incremental-update /
    /// per-file delete by id; the search path ranks by index into the loaded
    /// slice, so it is not read today.
    #[allow(dead_code)]
    id: i64,
    root: String,
    file_path: String,
    byte_offset: i64,
    chunk_text: String,
    vector: Option<Vec<f64>>,
}

/// One CITED search result: the file it came from, the chunk's byte offset (the
/// citation anchor), a bounded snippet of the chunk, and the relevance score.
/// Only ever built from a REAL stored chunk — never fabricated.
#[derive(Debug, Clone, PartialEq)]
pub struct DocHit {
    pub file_path: String,
    pub root: String,
    pub byte_offset: i64,
    pub snippet: String,
    pub score: f64,
}

/// A complete search result: the cited hits plus the ranking backend that
/// ACTUALLY ran, so the caller reports the method honestly (neural on-device
/// embeddings, or lexical BM25 on fallback) — never claims neural when it fell
/// back.
#[derive(Debug, Clone, PartialEq)]
pub struct DocSearchResult {
    pub hits: Vec<DocHit>,
    pub method: RankMethod,
}

/// The status of the index, for the HUD telemetry surface: how many files and
/// chunks are stored, and how many of those chunks carry an on-device vector
/// (vs. will be ranked by BM25). All read from the live store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexStatus {
    pub files: u64,
    pub chunks: u64,
    pub embedded_chunks: u64,
}

/// The bounded, local, FORGETTABLE chunk-vector store. Mirrors the `memory.rs`
/// SQLite pattern: open/migrate, WAL, an async Mutex so `&DocIndex` is shareable.
/// The store NEVER reaches the network; it only persists chunks the confined
/// indexer produced.
pub struct DocIndex {
    conn: Mutex<Connection>,
}

impl DocIndex {
    /// Open (creating + migrating) the chunk store at `path` PLAINTEXT (today's
    /// behavior, byte-for-byte). Reached when `[security].encrypt_memory` is OFF
    /// (the default). Same pragmas as the memory store (busy_timeout + WAL).
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("cannot open docsearch index at {}", path.display()))?;
        Self::init_conn(conn)
    }

    /// Open the chunk store ENCRYPTED (transparent whole-file SQLCipher AES-256).
    /// `key` is applied via `PRAGMA key` immediately after open, before any other
    /// pragma/statement. Reached only when `[security].encrypt_memory` is ON;
    /// tests pass an explicit in-test key (no Keychain).
    pub fn open_encrypted(path: &Path, key: &crate::crypto::SecretKey) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("cannot open docsearch index at {}", path.display()))?;
        crate::crypto::apply_key(&conn, key)?;
        Self::init_conn(conn)
    }

    /// Shared setup (pragmas + schema), run AFTER any `PRAGMA key`.
    fn init_conn(conn: Connection) -> Result<Self> {
        conn.busy_timeout(Duration::from_millis(250))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS doc_chunks(
                id INTEGER PRIMARY KEY,
                root TEXT NOT NULL,
                file_path TEXT NOT NULL,
                byte_offset INTEGER NOT NULL,
                chunk_text TEXT NOT NULL,
                vector TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_doc_chunks_file ON doc_chunks(file_path);",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Insert one chunk. `vector` is the on-device embedding when available (stored
    /// as a JSON array of f64), else `None` (the chunk is BM25-ranked). Returns the
    /// new row id. Internal: the public write path is [`Self::reindex`].
    async fn insert_chunk(
        &self,
        root: &str,
        file_path: &str,
        byte_offset: usize,
        chunk_text: &str,
        vector: Option<&[f64]>,
    ) -> Result<i64> {
        let vec_json = match vector {
            Some(v) => Some(serde_json::to_string(v)?),
            None => None,
        };
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO doc_chunks(root, file_path, byte_offset, chunk_text, vector)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![root, file_path, byte_offset as i64, chunk_text, vec_json],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// FORGET: clear the entire index (every stored chunk + vector), returning how
    /// many chunk rows were removed and VACUUMing so the file actually shrinks. The
    /// forgettable contract — a user can make JARVIS forget every indexed file.
    pub async fn forget(&self) -> Result<u64> {
        let conn = self.conn.lock().await;
        let deleted = conn.execute("DELETE FROM doc_chunks", [])?;
        if deleted > 0 {
            conn.execute_batch("VACUUM")?;
        }
        Ok(deleted as u64)
    }

    /// The current index status (files / chunks / embedded-chunks) for telemetry.
    pub async fn status(&self) -> Result<IndexStatus> {
        let conn = self.conn.lock().await;
        let chunks: i64 = conn.query_row("SELECT COUNT(*) FROM doc_chunks", [], |r| r.get(0))?;
        let files: i64 =
            conn.query_row("SELECT COUNT(DISTINCT file_path) FROM doc_chunks", [], |r| r.get(0))?;
        let embedded: i64 = conn.query_row(
            "SELECT COUNT(*) FROM doc_chunks WHERE vector IS NOT NULL",
            [],
            |r| r.get(0),
        )?;
        Ok(IndexStatus {
            files: files.max(0) as u64,
            chunks: chunks.max(0) as u64,
            embedded_chunks: embedded.max(0) as u64,
        })
    }

    /// Load every stored chunk (bounded by the store's own size). Internal to
    /// search; materializes the vectors from their JSON.
    async fn all_chunks(&self) -> Result<Vec<ChunkRow>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, root, file_path, byte_offset, chunk_text, vector FROM doc_chunks",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let vec_json: Option<String> = row.get(5)?;
                let vector = vec_json
                    .and_then(|s| serde_json::from_str::<Vec<f64>>(&s).ok())
                    .filter(|v| !v.is_empty());
                Ok(ChunkRow {
                    id: row.get(0)?,
                    root: row.get(1)?,
                    file_path: row.get(2)?,
                    byte_offset: row.get(3)?,
                    chunk_text: row.get(4)?,
                    vector,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Every stored chunk reduced to what the KNOWLEDGE-GRAPH build
    /// ([`crate::knowledge_graph`]) needs: the citation `file_path`, the chunk's
    /// `byte_offset` (the provenance anchor), and the chunk `text` the extractor
    /// mines. READ-ONLY: it returns exactly the chunks the confined, allowlisted
    /// indexer already produced (so the graph build never re-walks the disk and can
    /// only ever see allowlisted content), bounded by the store's own `max_chunks`
    /// ceiling. Vectors are not needed for extraction, so they are not loaded.
    pub async fn chunks_for_graph(&self) -> Result<Vec<(String, i64, String)>> {
        let rows = self.all_chunks().await?;
        Ok(rows
            .into_iter()
            .map(|c| (c.file_path, c.byte_offset, c.chunk_text))
            .collect())
    }

    /// REINDEX: clear the store and rebuild it from the allowlisted roots. This is
    /// the public WRITE path the daemon's "index my documents" / "reindex" intent
    /// calls. It:
    ///   1. forgets the old index (reindex is a full rebuild — bounded + idempotent);
    ///   2. walks the CONFINED, bounded roots ([`walk`]);
    ///   3. reads + chunks each accepted file (binary sniff skips a mislabeled blob);
    ///   4. embeds the chunks ON-DEVICE in one batched call via `embedder`; if that
    ///      errs (server down / no embed op), stores the chunks WITHOUT vectors so
    ///      search falls back to BM25 — never failing the index;
    ///   5. enforces the `max_chunks` total bound.
    /// Returns the resulting [`IndexStatus`]. NETWORK: never — embedding is the
    /// on-device op; file contents + embeddings never leave the device.
    pub async fn reindex(
        &self,
        roots: &[String],
        bounds: &IndexBounds,
        embedder: &dyn Embedder,
    ) -> Result<IndexStatus> {
        self.forget().await?;
        let discovered = walk(roots, bounds);

        // Gather (root, path, chunk) triples up to the chunk cap, reading content
        // ONLY here (after the confined+extension+size gates already passed).
        struct Pending {
            root: String,
            file_path: String,
            byte_offset: usize,
            text: String,
        }
        let mut pending: Vec<Pending> = Vec::new();
        'files: for d in &discovered {
            if pending.len() >= bounds.max_chunks {
                break;
            }
            let Ok(bytes) = std::fs::read(&d.path) else {
                continue;
            };
            if looks_binary(&bytes) {
                continue; // mislabeled binary that slipped the extension allowlist
            }
            // Lossy UTF-8: a stray invalid byte becomes U+FFFD rather than dropping
            // the whole file — we still index the readable text.
            let content = String::from_utf8_lossy(&bytes);
            let chunks = chunk_text(&content, bounds.chunk_chars, bounds.chunk_overlap);
            let root = d.root.display().to_string();
            let file_path = d.path.display().to_string();
            for c in chunks {
                if pending.len() >= bounds.max_chunks {
                    break 'files;
                }
                pending.push(Pending {
                    root: root.clone(),
                    file_path: file_path.clone(),
                    byte_offset: c.byte_offset,
                    text: c.text,
                });
            }
        }

        // Embed all chunk texts ON-DEVICE in one batched call. On ANY error (server
        // down / no embed op / wrong count), store WITHOUT vectors -> BM25 search.
        let texts: Vec<String> = pending.iter().map(|p| p.text.clone()).collect();
        let vectors: Option<Vec<Vec<f64>>> = if texts.is_empty() {
            None
        } else {
            match embedder.embed(&texts).await {
                Ok(v) if v.len() == texts.len() && v.iter().all(|x| !x.is_empty()) => Some(v),
                _ => None,
            }
        };

        for (i, p) in pending.iter().enumerate() {
            let vec = vectors.as_ref().map(|vs| vs[i].as_slice());
            self.insert_chunk(&p.root, &p.file_path, p.byte_offset, &p.text, vec)
                .await?;
        }
        self.status().await
    }

    /// SEARCH: rank the stored chunks against `query` and return at most `k` CITED
    /// hits, most-relevant first, reporting WHICH backend ran. NEURAL when EVERY
    /// stored chunk carries an on-device vector AND the query embeds — cosine over
    /// the stored vectors; otherwise LEXICAL BM25 over the chunk text (the honest
    /// fallback, used whenever the embedder is/was unavailable). Zero-score
    /// (irrelevant) chunks are dropped, so an empty index or a no-match query
    /// returns NOTHING — never a fabricated citation.
    ///
    /// The query embedding is the ONE runtime/MLX-gated call here; tests inject a
    /// mock `embedder`. A failed store read degrades to an empty result.
    pub async fn search(
        &self,
        query: &str,
        k: usize,
        embedder: &dyn Embedder,
    ) -> DocSearchResult {
        let k = k.clamp(1, DOCSEARCH_MAX_K);
        let chunks = self.all_chunks().await.unwrap_or_default();
        if chunks.is_empty() || query.trim().is_empty() {
            // Nothing to rank (or a contentless query): honest empty. Report the
            // backend that WOULD run lexically (no embed call made).
            return DocSearchResult {
                hits: Vec::new(),
                method: RankMethod::Lexical,
            };
        }

        // Prefer NEURAL only when every chunk has a stored vector — a mixed store
        // (some embedded, some not) cannot be ranked coherently by cosine, so it
        // falls back to BM25 wholesale (honest: the method names what actually ran).
        let all_embedded = chunks.iter().all(|c| c.vector.is_some());
        if all_embedded {
            if let Ok(qvecs) = embedder.embed(&[query.to_string()]).await {
                if qvecs.len() == 1 && !qvecs[0].is_empty() {
                    let qvec = &qvecs[0];
                    let mut scored: Vec<(usize, f64)> = chunks
                        .iter()
                        .enumerate()
                        .map(|(i, c)| {
                            let sim = c
                                .vector
                                .as_ref()
                                .map(|v| cosine_similarity(qvec, v))
                                .unwrap_or(0.0);
                            // Clamp negatives to 0 (anti-correlated is not a hit).
                            (i, if sim > 0.0 { sim } else { 0.0 })
                        })
                        .collect();
                    return DocSearchResult {
                        hits: rank_and_cite(&chunks, &mut scored, k),
                        method: RankMethod::Embedding,
                    };
                }
            }
            // Query embed failed / degenerate -> fall through to BM25, honestly.
        }

        // LEXICAL BM25 over the chunk text (reusing recall.rs's shipped ranker).
        let lexical = LexicalProvider {
            params: Bm25Params::default(),
        };
        // recall::score wants Facts; the chunk text IS the searchable value.
        use crate::recall::EmbeddingProvider;
        let facts: Vec<Fact> = chunks
            .iter()
            .map(|c| Fact {
                key: String::new(),
                value: c.chunk_text.clone(),
            })
            .collect();
        let scores = lexical.score(query, &facts);
        let mut scored: Vec<(usize, f64)> = scores.into_iter().enumerate().collect();
        DocSearchResult {
            hits: rank_and_cite(&chunks, &mut scored, k),
            method: RankMethod::Lexical,
        }
    }
}

/// Sort `(chunk_index, score)` by score DESC then index ASC (deterministic tie
/// break), drop non-positive (irrelevant) scores, take `k`, and materialize a
/// CITED [`DocHit`] for each from the real chunk. No-match -> empty (no
/// fabrication). The snippet is a bounded, char-boundary-safe preview of the
/// stored chunk text.
fn rank_and_cite(chunks: &[ChunkRow], scored: &mut [(usize, f64)], k: usize) -> Vec<DocHit> {
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    scored
        .iter()
        .filter(|(_, s)| s.is_finite() && *s > 0.0)
        .take(k)
        .filter_map(|(i, s)| {
            let c = chunks.get(*i)?;
            Some(DocHit {
                file_path: c.file_path.clone(),
                root: c.root.clone(),
                byte_offset: c.byte_offset,
                snippet: snippet_of(&c.chunk_text),
                score: *s,
            })
        })
        .collect()
}

/// A bounded, char-boundary-safe snippet of a chunk for display/citation.
fn snippet_of(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() <= SNIPPET_CHARS {
        return s.to_string();
    }
    let cut: String = s.chars().take(SNIPPET_CHARS).collect();
    format!("{}…", cut.trim_end())
}

/// Whether a configured roots list + enabled flag actually permit any indexing:
/// the master switch must be on AND at least one root must be configured. The
/// daemon checks this before ever walking — an OFF subsystem or an empty allowlist
/// indexes NOTHING (no whole-disk scan).
pub fn indexing_permitted(enabled: bool, roots: &[String]) -> bool {
    enabled && !roots.is_empty()
}

/// The DAEMON ENTRY POINT for the "index my documents" / "reindex" intent:
/// CONFIG-GATED reindex over the allowlisted roots. This is the single function
/// the daemon's index/reindex trigger calls — it enforces the OFF-by-default gate
/// ([`indexing_permitted`]: `[docsearch].enabled` AND a non-empty `roots`) BEFORE
/// touching the disk, so an OFF subsystem or an empty allowlist indexes NOTHING
/// (never a whole-disk scan). When permitted, it lifts the bounds from config and
/// runs [`DocIndex::reindex`] (the confined, bounded, on-device walk+chunk+embed).
///
/// Returns `Ok(None)` when indexing is NOT permitted (the daemon then tells the
/// user file search is off / no folder is allowlisted — it never silently scans),
/// or `Ok(Some(status))` with the resulting index status. The `embedder` is the
/// on-device socket in the live path (runtime/MLX-gated) and a mock in tests; on
/// any embed error the chunks are stored vector-less and search falls back to BM25.
pub async fn index_documents(
    cfg: &crate::config::DocSearchConfig,
    index: &DocIndex,
    embedder: &dyn Embedder,
) -> Result<Option<IndexStatus>> {
    if !indexing_permitted(cfg.enabled, &cfg.roots) {
        return Ok(None); // OFF / no allowlisted root -> index NOTHING.
    }
    let bounds = IndexBounds::from_config(cfg);
    let status = index.reindex(&cfg.roots, &bounds, embedder).await?;
    Ok(Some(status))
}

/// Defensive: reject a configured root that is not an absolute path or that
/// contains a `..` component (a relative or traversing root is a misconfiguration
/// that could widen the surface). The walk additionally canonicalizes every root,
/// but this catches an obviously-unsafe entry early for an honest config warning.
#[allow(dead_code)] // surfaced by the HUD/config validation path; unit-tested here
pub fn root_is_safe(root: &str) -> bool {
    let p = Path::new(root);
    p.is_absolute() && !p.components().any(|c| matches!(c, Component::ParentDir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// A unique temp dir tree per test, cleaned on drop. All file I/O in these
    /// tests stays inside this dir — never the user's real home.
    struct TempTree(PathBuf);

    impl TempTree {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "jarvis-docsearch-test-{}-{}",
                std::process::id(),
                tag
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            TempTree(path)
        }
        fn join(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }
        fn write(&self, rel: &str, contents: &str) -> PathBuf {
            let p = self.join(rel);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&p, contents).unwrap();
            p
        }
        fn db_path(&self) -> PathBuf {
            self.join("index.db")
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn roots_of(t: &TempTree, sub: &str) -> Vec<String> {
        vec![t.join(sub).display().to_string()]
    }

    // ---- a mock embedder: NEVER touches a socket/MLX/network -----------------

    /// A deterministic mock [`Embedder`]. Each input text maps to a fixed small
    /// vector by a keyword rule so a test can pin which chunk is "near" a query.
    struct KeywordEmbedder;
    impl Embedder for KeywordEmbedder {
        fn embed<'a>(&'a self, texts: &'a [String]) -> crate::recall::EmbedFuture<'a> {
            // axis 0 = "subaru"/"car", axis 1 = "corgi"/"pet", axis 2 = other.
            let vecs: Vec<Vec<f64>> = texts
                .iter()
                .map(|t| {
                    let l = t.to_lowercase();
                    let car = l.contains("subaru") || l.contains("car") || l.contains("outback");
                    let pet = l.contains("corgi") || l.contains("pet") || l.contains("watson");
                    if car {
                        vec![1.0, 0.0, 0.0]
                    } else if pet {
                        vec![0.0, 1.0, 0.0]
                    } else {
                        vec![0.0, 0.0, 1.0]
                    }
                })
                .collect();
            Box::pin(async move { Ok(vecs) })
        }
    }

    /// A mock embedder that is always DOWN (Err) — drives the BM25 fallback.
    struct DownEmbedder;
    impl Embedder for DownEmbedder {
        fn embed<'a>(&'a self, _texts: &'a [String]) -> crate::recall::EmbedFuture<'a> {
            Box::pin(async move { Err(anyhow::anyhow!("inference socket unavailable (simulated)")) })
        }
    }

    // =====================================================================
    // SECURITY: path confinement REJECTS every escape
    // =====================================================================

    #[test]
    fn confinement_rejects_symlink_escape_dotdot_and_absolute_elsewhere() {
        let t = TempTree::new("confine");
        // An allowlisted root with one real file inside.
        let root = t.join("vault");
        fs::create_dir_all(&root).unwrap();
        let inside = t.write("vault/note.md", "a secret note inside the vault");
        // A file OUTSIDE the root (a sibling) the index must never reach.
        let outside = t.write("outside/secret.md", "OUTSIDE the vault — must never index");

        let canon = canonical_roots(&[root.display().to_string()]);
        assert!(!canon.is_empty(), "the real root must canonicalize");

        // 1. A genuine in-root file is ACCEPTED (its real path is under the root).
        let accepted = confine(&inside, &canon).expect("an in-root file must confine");
        assert!(accepted.starts_with(&canon[0]), "accepted path must be under the root");

        // 2. A `..` traversal that climbs OUT of the root is REJECTED.
        let traversal = root.join("..").join("outside").join("secret.md");
        assert!(
            confine(&traversal, &canon).is_none(),
            "a `..` escape to a sibling must be rejected"
        );

        // 3. An absolute-elsewhere path (the outside file directly) is REJECTED.
        assert!(
            confine(&outside, &canon).is_none(),
            "an absolute path outside every root must be rejected"
        );

        // 4. A SYMLINK inside the root pointing OUTSIDE it is REJECTED — the
        //    canonicalized real target is outside the root.
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let escape_link = root.join("escape.md");
            symlink(&outside, &escape_link).unwrap();
            // The link's lexical path is under the root, but its REAL target is not.
            assert!(
                confine(&escape_link, &canon).is_none(),
                "a symlink whose target escapes the root must be rejected"
            );
        }
    }

    #[test]
    fn walk_never_indexes_a_symlink_escape_or_outside_file() {
        let t = TempTree::new("walk-confine");
        let root = t.join("vault");
        fs::create_dir_all(&root).unwrap();
        t.write("vault/keep.md", "in-vault note, indexable");
        let outside = t.write("outside/secret.md", "OUTSIDE — must never appear");

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            // A symlink inside the vault that escapes to the outside file, and a
            // symlinked subdir that escapes the vault.
            symlink(&outside, root.join("escape.md")).unwrap();
            symlink(t.join("outside"), root.join("escape_dir")).unwrap();
        }

        let bounds = IndexBounds::default();
        let found = walk(&roots_of(&t, "vault"), &bounds);
        let paths: Vec<String> = found.iter().map(|d| d.path.display().to_string()).collect();
        // The in-vault file is found...
        assert!(
            paths.iter().any(|p| p.ends_with("keep.md")),
            "the in-vault file must be indexed: {paths:?}"
        );
        // ...and NOTHING resolving to the outside file ever is.
        assert!(
            !paths.iter().any(|p| p.contains("secret.md") || p.contains("/outside/")),
            "no escape may be indexed: {paths:?}"
        );
    }

    // =====================================================================
    // INDEXING + CHUNKING over a temp dir
    // =====================================================================

    #[test]
    fn chunking_produces_overlapping_windows_with_offsets() {
        // 10 chars, window 4, overlap 1 -> stride 3 -> starts at 0,3,6; the window
        // at start=6 reaches the end (chars 6..10 = "ghij") and terminates, so the
        // overlapping windows are "abcd","defg","ghij" — consecutive windows share
        // their `overlap` boundary char (d, g) for citation continuity.
        let content = "abcdefghij";
        let chunks = chunk_text(content, 4, 1);
        assert_eq!(chunks.len(), 3, "{chunks:?}");
        assert_eq!(chunks[0].text, "abcd");
        assert_eq!(chunks[0].byte_offset, 0);
        assert_eq!(chunks[1].text, "defg");
        assert_eq!(chunks[1].byte_offset, 3);
        assert_eq!(chunks[2].text, "ghij");
        assert_eq!(chunks[2].byte_offset, 6);
        // Overlap: window 1 starts at 'd' (the last char of window 0) -> the
        // overlap of 1 char is preserved between consecutive windows.
        assert!(chunks[1].text.starts_with('d'), "the overlap char carries over");
        // A longer run yields a final short tail window.
        let tail = chunk_text("abcdefghijkl", 4, 1); // starts 0,3,6,9 -> last is "jkl"
        assert_eq!(tail.last().unwrap().text, "jkl");
        // Empty / whitespace content yields no chunks (never a fabricated chunk).
        assert!(chunk_text("", 100, 10).is_empty());
        assert!(chunk_text("   \n  ", 100, 10).is_empty());
    }

    #[test]
    fn extension_allowlist_skips_binaries_and_unknown_types() {
        assert!(extension_allowed(Path::new("notes.md")));
        assert!(extension_allowed(Path::new("main.rs")));
        assert!(extension_allowed(Path::new("config.TOML"))); // case-insensitive
        // Out of scope in v1: PDFs/binaries/no-extension.
        assert!(!extension_allowed(Path::new("paper.pdf")));
        assert!(!extension_allowed(Path::new("photo.png")));
        assert!(!extension_allowed(Path::new("archive.zip")));
        assert!(!extension_allowed(Path::new("Makefile")));
    }

    #[tokio::test]
    async fn reindex_walks_chunks_and_stores_only_allowlisted_files() {
        let t = TempTree::new("reindex");
        t.write("docs/a.md", "the quarterly budget meeting covered the launch plan");
        t.write("docs/b.txt", "a corgi named Watson sleeps on the rug");
        t.write("docs/skip.pdf", "this PDF must be skipped (binary, out of scope)");
        t.write("docs/sub/c.rs", "fn main() { println!(\"hello rust\"); }");

        let idx = DocIndex::open(&t.db_path()).unwrap();
        let bounds = IndexBounds::default();
        let status = idx
            .reindex(&roots_of(&t, "docs"), &bounds, &KeywordEmbedder)
            .await
            .unwrap();
        // 3 text-like files indexed (md, txt, rs); the pdf is skipped.
        assert_eq!(status.files, 3, "only allowlisted text-like files: {status:?}");
        assert!(status.chunks >= 3, "each small file is at least one chunk: {status:?}");
        // The pdf content must never appear in any stored chunk.
        let all = idx.all_chunks().await.unwrap();
        assert!(
            all.iter().all(|c| !c.chunk_text.contains("PDF must be skipped")),
            "the skipped PDF's content must not be stored"
        );
    }

    // =====================================================================
    // AT-REST ENCRYPTION (#11)
    // =====================================================================

    #[tokio::test]
    async fn open_encrypted_round_trips_and_chunk_text_is_ciphertext_at_rest() {
        let t = TempTree::new("enc");
        t.write("docs/a.md", "the doc-canary phrase lives in this chunk");
        // Encrypted open with an EXPLICIT in-test key (no Keychain, no network).
        let key = crate::crypto::SecretKey::from_bytes([2u8; crate::crypto::KEY_BYTES]);
        {
            let idx = DocIndex::open_encrypted(&t.db_path(), &key).unwrap();
            idx.reindex(&roots_of(&t, "docs"), &IndexBounds::default(), &KeywordEmbedder)
                .await
                .unwrap();
        }
        // On-disk bytes are ciphertext: the chunk text is not in the clear, and the
        // SQLite magic header is absent (it's a SQLCipher file).
        let raw = fs::read(&t.db_path()).unwrap();
        assert!(
            !raw.windows(b"doc-canary".len()).any(|w| w == b"doc-canary"),
            "chunk text must not appear in plaintext on disk"
        );
        assert!(!raw.starts_with(b"SQLite format 3\0"), "index must be encrypted");
        // Reopen WITH the key: the chunk reads back.
        {
            let idx = DocIndex::open_encrypted(&t.db_path(), &key).unwrap();
            let all = idx.all_chunks().await.unwrap();
            assert!(
                all.iter().any(|c| c.chunk_text.contains("doc-canary")),
                "the chunk must read back with the key"
            );
        }
        // The WRONG key cannot open it.
        let wrong = crate::crypto::SecretKey::from_bytes([1u8; crate::crypto::KEY_BYTES]);
        assert!(
            DocIndex::open_encrypted(&t.db_path(), &wrong).is_err(),
            "wrong key must fail"
        );
    }

    // =====================================================================
    // SEARCH: ranks the right chunk + CITES the right file
    // =====================================================================

    #[tokio::test]
    async fn search_neural_ranks_and_cites_the_right_file() {
        let t = TempTree::new("search-neural");
        let car = t.write("docs/car.md", "I drive a blue Subaru Outback wagon");
        t.write("docs/pet.md", "a corgi named Watson is my dog");

        let idx = DocIndex::open(&t.db_path()).unwrap();
        let bounds = IndexBounds::default();
        idx.reindex(&roots_of(&t, "docs"), &bounds, &KeywordEmbedder)
            .await
            .unwrap();

        // A car query: the KeywordEmbedder puts the car chunk on axis 0, the query
        // on axis 0 too -> cosine 1; the pet chunk is orthogonal -> dropped.
        // The store cites the REAL (canonicalized, symlink-resolved) path — on
        // macOS /var canonicalizes to /private/var — so compare against that.
        let car_real = fs::canonicalize(&car).unwrap().display().to_string();
        let result = idx.search("what kind of car do I drive", 5, &KeywordEmbedder).await;
        assert_eq!(result.method, RankMethod::Embedding, "all chunks embedded -> neural ran");
        assert!(!result.hits.is_empty(), "the car chunk must be retrieved");
        assert_eq!(
            result.hits[0].file_path, car_real,
            "the top hit must CITE the real car file: {:?}",
            result.hits
        );
        assert!(result.hits[0].snippet.contains("Subaru"), "snippet is the real chunk text");
        assert!(result.hits[0].score > 0.0, "only positive hits are returned");
        // The orthogonal pet file is NOT surfaced for a car query.
        assert!(
            result.hits.iter().all(|h| !h.file_path.contains("pet.md")),
            "an irrelevant file must not be cited: {:?}",
            result.hits
        );
    }

    #[tokio::test]
    async fn search_falls_back_to_bm25_when_embedder_is_down_and_reports_it() {
        let t = TempTree::new("search-bm25");
        let budget = t.write("docs/budget.md", "the quarterly budget review and forecast");
        t.write("docs/pet.md", "a corgi named Watson naps a lot");

        let idx = DocIndex::open(&t.db_path()).unwrap();
        let bounds = IndexBounds::default();
        // Index with the embedder DOWN: chunks are stored WITHOUT vectors.
        idx.reindex(&roots_of(&t, "docs"), &bounds, &DownEmbedder)
            .await
            .unwrap();
        let status = idx.status().await.unwrap();
        assert_eq!(status.embedded_chunks, 0, "no vectors stored when the embedder is down");

        // Search (embedder still down) -> BM25, reported honestly.
        let budget_real = fs::canonicalize(&budget).unwrap().display().to_string();
        let result = idx.search("quarterly budget forecast", 5, &DownEmbedder).await;
        assert_eq!(result.method, RankMethod::Lexical, "no vectors -> BM25 fallback");
        assert_eq!(result.method.as_str(), "lexical-bm25");
        assert!(!result.hits.is_empty(), "BM25 still ranks the budget file");
        assert_eq!(
            result.hits[0].file_path, budget_real,
            "BM25 must cite the budget file: {:?}",
            result.hits
        );
    }

    #[tokio::test]
    async fn search_no_match_returns_nothing_never_fabricates_a_citation() {
        let t = TempTree::new("no-match");
        t.write("docs/a.md", "notes about gardening and tomatoes");
        let idx = DocIndex::open(&t.db_path()).unwrap();
        idx.reindex(&roots_of(&t, "docs"), &IndexBounds::default(), &DownEmbedder)
            .await
            .unwrap();
        // A query with zero term overlap -> BM25 scores 0 -> no hits.
        let result = idx.search("quantum chromodynamics lecture", 5, &DownEmbedder).await;
        assert!(
            result.hits.is_empty(),
            "a no-match query must cite nothing: {:?}",
            result.hits
        );
    }

    #[tokio::test]
    async fn search_empty_index_is_honest_empty() {
        let t = TempTree::new("empty");
        let idx = DocIndex::open(&t.db_path()).unwrap();
        let result = idx.search("anything at all", 5, &KeywordEmbedder).await;
        assert!(result.hits.is_empty(), "an empty index returns nothing");
        assert_eq!(result.method, RankMethod::Lexical);
    }

    // =====================================================================
    // BOUNDED: caps are enforced
    // =====================================================================

    #[tokio::test]
    async fn bounds_cap_files_chunks_and_skip_oversize() {
        let t = TempTree::new("bounds");
        // 5 files, but max_files = 2.
        for i in 0..5 {
            t.write(&format!("docs/f{i}.md"), &format!("file number {i} content here"));
        }
        // One oversize file that the per-file byte cap must skip.
        t.write("docs/big.md", &"x ".repeat(10_000));

        let bounds = IndexBounds {
            max_files: 2,
            max_chunks: 3,
            max_file_bytes: 100, // skips big.md (and any file > 100 bytes)
            max_depth: 8,
            chunk_chars: 64,
            chunk_overlap: 8,
        };
        let found = walk(&roots_of(&t, "docs"), &bounds);
        assert!(found.len() <= 2, "max_files caps the walk: {}", found.len());
        assert!(
            found.iter().all(|d| !d.path.ends_with("big.md")),
            "the oversize file must be skipped: {found:?}"
        );

        let idx = DocIndex::open(&t.db_path()).unwrap();
        let status = idx
            .reindex(&roots_of(&t, "docs"), &bounds, &DownEmbedder)
            .await
            .unwrap();
        assert!(status.files <= 2, "file cap honored: {status:?}");
        assert!(status.chunks <= 3, "chunk cap honored: {status:?}");
    }

    // =====================================================================
    // FORGET clears the index
    // =====================================================================

    #[tokio::test]
    async fn forget_clears_the_entire_index() {
        let t = TempTree::new("forget");
        t.write("docs/a.md", "something to index then forget");
        let idx = DocIndex::open(&t.db_path()).unwrap();
        idx.reindex(&roots_of(&t, "docs"), &IndexBounds::default(), &DownEmbedder)
            .await
            .unwrap();
        assert!(idx.status().await.unwrap().chunks > 0, "index has chunks before forget");

        let cleared = idx.forget().await.unwrap();
        assert!(cleared > 0, "forget removes the stored chunks");
        let status = idx.status().await.unwrap();
        assert_eq!(status.chunks, 0, "the index is empty after forget");
        assert_eq!(status.files, 0);
        // A search after forget is honestly empty (never a stale citation).
        let result = idx.search("something", 5, &DownEmbedder).await;
        assert!(result.hits.is_empty(), "no citation survives a forget");
    }

    // =====================================================================
    // OFF / no-whole-disk-scan gate + safe roots
    // =====================================================================

    #[test]
    fn indexing_is_not_permitted_when_off_or_no_roots() {
        // OFF -> never index, even with a root configured.
        assert!(!indexing_permitted(false, &["/some/root".to_string()]));
        // ON but EMPTY allowlist -> still nothing (no whole-disk scan).
        assert!(!indexing_permitted(true, &[]));
        // ON + a root -> permitted.
        assert!(indexing_permitted(true, &["/some/root".to_string()]));
    }

    #[test]
    fn root_safety_rejects_relative_and_traversing_roots() {
        assert!(root_is_safe("/Users/me/Documents"));
        assert!(!root_is_safe("relative/path"), "a relative root is unsafe");
        assert!(!root_is_safe("/Users/me/../etc"), "a traversing root is unsafe");
        assert!(!root_is_safe(""), "an empty root is unsafe");
    }

    #[tokio::test]
    async fn reindex_with_no_roots_indexes_nothing() {
        let t = TempTree::new("no-roots");
        let idx = DocIndex::open(&t.db_path()).unwrap();
        // An empty allowlist walks/indexes nothing — the no-whole-disk-scan guard
        // at the store layer (the daemon also checks indexing_permitted upstream).
        let status = idx.reindex(&[], &IndexBounds::default(), &DownEmbedder).await.unwrap();
        assert_eq!(status.files, 0, "empty roots -> no files indexed");
        assert_eq!(status.chunks, 0);
    }

    #[tokio::test]
    async fn index_documents_is_config_gated_off_by_default() {
        let t = TempTree::new("gated");
        t.write("docs/a.md", "real content that exists on disk");
        let idx = DocIndex::open(&t.db_path()).unwrap();

        // OFF (the shipped default) with a REAL root present -> still indexes
        // NOTHING (the gate runs before any disk walk; no whole-disk scan).
        let off = crate::config::DocSearchConfig {
            enabled: false,
            roots: roots_of(&t, "docs"),
            ..crate::config::DocSearchConfig::default()
        };
        let status = index_documents(&off, &idx, &DownEmbedder).await.unwrap();
        assert!(status.is_none(), "OFF must index nothing even with a real root");
        assert_eq!(idx.status().await.unwrap().chunks, 0, "nothing stored while OFF");

        // ON but EMPTY allowlist -> still nothing (no whole-disk scan).
        let on_no_roots = crate::config::DocSearchConfig {
            enabled: true,
            roots: Vec::new(),
            ..crate::config::DocSearchConfig::default()
        };
        assert!(
            index_documents(&on_no_roots, &idx, &DownEmbedder).await.unwrap().is_none(),
            "ON + empty allowlist must index nothing"
        );

        // ON + a real allowlisted root -> indexes the confined files.
        let on = crate::config::DocSearchConfig {
            enabled: true,
            roots: roots_of(&t, "docs"),
            ..crate::config::DocSearchConfig::default()
        };
        let status = index_documents(&on, &idx, &DownEmbedder).await.unwrap();
        let status = status.expect("ON + a root indexes");
        assert_eq!(status.files, 1, "the one allowlisted file is indexed: {status:?}");
        assert!(status.chunks >= 1);
    }

    // =====================================================================
    // HONESTY: binary sniff + hidden files
    // =====================================================================

    #[test]
    fn binary_sniff_catches_a_nul_blob() {
        assert!(looks_binary(b"text\0with a nul"));
        assert!(!looks_binary(b"plain readable text"));
    }

    #[tokio::test]
    async fn hidden_files_and_dirs_are_skipped() {
        let t = TempTree::new("hidden");
        t.write("docs/visible.md", "visible note");
        t.write("docs/.secret.md", "a dotfile that must be skipped");
        t.write("docs/.hidden/inside.md", "inside a hidden dir, skipped");
        let found = walk(&roots_of(&t, "docs"), &IndexBounds::default());
        let paths: Vec<String> = found.iter().map(|d| d.path.display().to_string()).collect();
        assert!(paths.iter().any(|p| p.ends_with("visible.md")), "{paths:?}");
        assert!(
            paths.iter().all(|p| !p.contains(".secret") && !p.contains(".hidden")),
            "hidden entries must be skipped: {paths:?}"
        );
    }
}
