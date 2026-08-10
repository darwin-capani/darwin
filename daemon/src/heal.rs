//! Self-heal v2: an error-burst watchdog that DIAGNOSES, drafts MULTIPLE
//! candidate fixes, validates each independently behind the same hard gates,
//! adversarially self-reviews the survivors, and proposes the best one for a
//! human to apply.
//!
//! Pipeline (every gate is hard and NEVER weakened):
//!   1. TRIGGER — edge-triggered ERROR burst in state/logs/daemon.log
//!      (>= 5 ERROR-level lines in 60s), or a single total-loss line
//!      ("audio capture stopped": the capture thread died once and is never
//!      respawned — one line, permanent deafness, no burst will follow).
//!   2. GATES — [self_heal] enabled must be true (else heal.suppressed); at
//!      most one draft attempt per 6h (meta.heal_last_attempt); a cloud key
//!      must resolve (else heal.blocked{reason:"no_api_key"}).
//!   3. DIAGNOSIS (v2) — extract the error signature(s), the cited source
//!      files + line numbers, a window of surrounding log context, and the
//!      implicated subsystem (audio/inference/router/...) by module path.
//!      Emits heal.diagnosing{signature, files, subsystem}.
//!   4. MULTI-CANDIDATE DRAFT (v2) — ask the heavy model ([cloud].heavy_model) for
//!      N=2-3 ALTERNATIVE minimal unified-diff patches (distinct approaches,
//!      each minimal, no new deps). Each is parsed/cleaned; non-diffs rejected.
//!   5. STAGE + VALIDATE EACH (v2) — every candidate is staged independently
//!      in state/heal/staging-<ts>-c<i>/ (sources copied, diff applied with
//!      /usr/bin/patch -p1 --batch, then check -> clippy -D warnings -> test
//!      -> mutation probe: the fix is reverse-applied and the patch's own test
//!      must then FAIL, else the candidate is rejected as unproven). Any candidate
//!      that fails a hunk/compile/test is DISCARDED. Gates reused unchanged.
//!      EVERY ONE OF THOSE GATES IS BLIND TO THE DIAGNOSIS — a patch that fixes
//!      something ELSE, with a real test that really bites, clears all of them —
//!      so each candidate is also scored for RESPONSIVENESS (DIRECT / SUBSYSTEM
//!      / SIGNATURE / UNRELATED / INDETERMINATE) against the burst that
//!      triggered it. That verdict is SURFACED, NEVER ENFORCED (a correct fix
//!      often lives one layer up from the line that screamed): it goes into the
//!      validation tail the reviewer reads, report.md's header and
//!      heal.proposal, and scripts/apply_heal.sh re-derives it from the same
//!      code via `darwind --heal-responsiveness` — one implementation, two
//!      callers, like `--split-heal-diff`. Running OUT OF BUDGET is reported as
//!      its own stage, `deadline`, and never as a rejection on the merits.
//!      THE BUDGET IS AN ATTEMPT-WIDE CEILING, NOT A PER-CANDIDATE ALLOWANCE:
//!      each candidate may take up to VALIDATE_TIMEOUT, but only out of the one
//!      attempt budget all CANDIDATE_COUNT of them share, and each stage is
//!      capped so a slow one cannot starve the stages behind it. A candidate the
//!      attempt cannot afford is reported as `deadline` WITHOUT being staged.
//!      ONLY TIME SPENT INSIDE stage_and_validate IS CHARGED TO IT — the draft
//!      and review cloud calls carry their own timeouts and are not the
//!      machine's to pay for.
//!   6. ADVERSARIAL SELF-REVIEW (v2) — a second cloud call judges each
//!      surviving (validated) diff against the diagnosis + its test output:
//!      does it fix the ROOT CAUSE (not just silence the symptom)? Returns a
//!      verdict + confidence 0..1. This is a CLOUD call, and it is NOT charged
//!      to the staged-validation budget described under (5): that budget bounds
//!      what the MACHINE compiles, and a slow review API must not spend it.
//!   7. SELECT — prefer the MINIMAL patch with the HIGHEST review confidence
//!      among those that PASSED validation, and PROPOSE NOTHING when even the
//!      best of them is below the review-confidence floor: the staged gates are blind
//!      to whether the patch is a good idea, the reviewer is the only stage that
//!      judges that, and "best of three the reviewer disbelieved" is not a
//!      proposal. Rejected-at-the-floor attempts still write every candidate,
//!      the review and the diagnosis under state/heal/rejected/<ts>/, and
//!      scripts/apply_heal.sh enforces the same floor on the report.md of
//!      whatever it is asked to install, via `darwind --heal-confidence`.
//!      8a. mode="propose" (default) — write state/heal/proposals/<ts>/{patch.diff,
//!      report.md, diagnosis.json, candidates.md, review.md}, stamp
//!      meta.heal_pending=<ts>, emit heal.proposal{ts, files, validated:true,
//!      confidence}. scripts/apply_heal.sh <ts> applies it on human request.
//!      8b. mode="auto" (requires enabled=true; documented DANGEROUS) — apply the
//!      same validated diff to the real daemon/, cargo build --release, emit
//!      heal.applied, then EXIT cleanly for a supervised restart. UNCHANGED
//!      from v1: there is still no NEW live-auto-apply path.
//!      Any patch/validation failure of ALL candidates → state/heal/rejected/<ts>/
//!   + heal.rejected{ts, stage}.
//!
//! SAFETY CONTRACT (non-negotiable): self-heal ships enabled=TRUE /
//! mode=propose. NOTE ON THE POSTURE: this line stated the opposite default for a long
//! time, on a block labelled SAFETY CONTRACT — the worst place for a stale claim, since
//! an operator reads exactly this to decide whether autonomy is armed. The master
//! switch is ON. What is load-bearing, and true, is the rest: mode=propose is the
//! default, and
//! there is NO path that touches the live daemon/ without an
//! explicit human running scripts/apply_heal.sh (except the pre-existing,
//! documented-dangerous opt-in auto mode); the staged `cargo check` + full
//! `cargo test` gates are NEVER dropped or weakened. The cloud is reached ONLY
//! through the HealBrain trait — unit tests mock it; the only real cloud path
//! is the verifier's --heal-drill.
//!
//! The watchdog's own output must never feed back into its trigger: every log
//! line this module writes is WARN/INFO level, and the detector matches the
//! level *token*, never message text.

use std::future::Future;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

use crate::anthropic;
use crate::config::Config;
use crate::memory::Memory;
use crate::telemetry;

const CHECK_INTERVAL: Duration = Duration::from_secs(10);
const BURST_WINDOW_SECS: i64 = 60;
const BURST_LIMIT: usize = 5;
const TAIL_BYTES: u64 = 64 * 1024;
/// One of these inside an ERROR-level line is an immediate trigger even
/// alone: a total-loss event that emits exactly one line and never recurs
/// (the audio capture thread exits and is not respawned), so the burst
/// counter would never see it (audit fix).
const TOTAL_LOSS_MARKERS: &[&str] = &["audio capture stopped"];

/// Rate limit: at most one draft attempt (cloud call) per this many seconds.
const ATTEMPT_INTERVAL_SECS: u64 = 6 * 3600;
const META_HEAL_LAST_ATTEMPT: &str = "meta.heal_last_attempt";
const META_HEAL_PENDING: &str = "meta.heal_pending";

/// daemon.log context handed to the drafter.
const CONTEXT_LINES: usize = 80;
/// Burst lines kept for the prompt and the report.
const BURST_LINE_CAP: usize = 20;

/// How many alternative candidate diffs we ask the heavy model for (v2).
const CANDIDATE_COUNT: usize = 3;

/// Draft call: heavy model, latency-insensitive, room for thinking + diffs.
const DRAFT_MAX_TOKENS: u32 = 8192;
const DRAFT_TIMEOUT: Duration = Duration::from_secs(240);
/// Review call: a verdict + confidence is short; still allow thinking room.
const REVIEW_MAX_TOKENS: u32 = 4096;
const REVIEW_TIMEOUT: Duration = Duration::from_secs(180);

const DRAFT_SYSTEM: &str = "You are DARWIN's self-repair drafter: an expert Rust engineer who \
     produces minimal unified diffs. Respond with ONLY the diff(s) — no prose outside the \
     requested structure, no code fences inside a diff.";
const REVIEW_SYSTEM: &str = "You are DARWIN's adversarial self-repair reviewer: a skeptical \
     senior Rust engineer. You judge whether a candidate patch fixes the ROOT CAUSE of a fault \
     (not merely silences the symptom) and has no obvious side effects. Be harsh; a passing \
     test suite is necessary but NOT sufficient.";

/// PER-CANDIDATE staged-validation budget: check, clippy, test and the mutation
/// probe share it.
///
/// MEASURED COLD on an M1 Pro, in a tree staged exactly as `stage_sources`
/// builds one: check 92s + clippy --all-targets -D warnings 184s + test 214s +
/// mutation re-run 33s = 523s of the 600s this replaces — 87% of the budget,
/// ~77s of headroom, on a machine that (by the definition of self-heal firing)
/// is ALREADY MISBEHAVING and is very likely slower than the one that number
/// came from. A candidate that runs 15% slow is not judged badly; it is not
/// judged AT ALL, and the attempt reports "NO CANDIDATE WAS EVER JUDGED".
///
/// This is NOT a licence to spend more wall clock. The whole attempt is capped
/// at [`attempt_budget()`] across all [`CANDIDATE_COUNT`] candidates, so the
/// worst case is UNCHANGED from the 3 x 600s it replaces — the time is simply
/// allowed to go where it is needed instead of being handed out in three equal
/// slices that each fall just short.
const VALIDATE_TIMEOUT: Duration = Duration::from_secs(900);

/// THE CEILING THAT ACTUALLY BOUNDS THE MACHINE.
///
/// Before this existed, each of [`CANDIDATE_COUNT`] candidates got its own
/// independent [`VALIDATE_TIMEOUT`], so one attempt could occupy every core for
/// `CANDIDATE_COUNT * VALIDATE_TIMEOUT` of compilation — on a box that is
/// already misbehaving, which is the only time any of this runs. Raising the
/// per-candidate budget without this would have multiplied that.
///
/// 1800s is exactly the old 3 x 600s worst case: the total a self-heal attempt
/// may spend in cargo is unchanged, and a slow machine now spends it finishing
/// ONE OR TWO real verdicts instead of timing out three times at 87%.
///
/// IT IS A CARGO BUDGET, NOT A WALL CLOCK — only time spent inside
/// `stage_and_validate` is charged to it.
///
/// It was first written as a wall-clock deadline spanning the whole candidate
/// loop, which charged the ADVERSARIAL REVIEW's cloud latency to the machine.
/// The arithmetic is unforgiving: at the MEASURED 523s cycle the first two
/// candidates leave 1800 - 1046 = 754s, and the third is only staged when at
/// least [`minimum_viable_budget`] (750s) is left — so roughly TWO SECONDS of
/// total review latency was enough to refuse candidate 3, on a perfectly
/// healthy machine, with the message "the earlier candidates used the machine
/// up". A third of every attempt's drafted (and paid-for) candidates was being
/// discarded because an API was slow, and the operator was told their box was.
/// THE DEFAULT, and what `config/darwin.toml` ships. The EFFECTIVE value is
/// [`attempt_budget()`], set once from `[self_heal].attempt_budget_secs`.
const ATTEMPT_BUDGET_DEFAULT: Duration = Duration::from_secs(1800);

/// Clamp ends for `[self_heal].attempt_budget_secs`. **DERIVED, NOT PICKED.**
///
/// * The LOW end is [`minimum_viable_budget`] (750s): below it,
///   `stage_and_validate` refuses to stage even the FIRST candidate, so the
///   whole gate could never reach a verdict on anything — a configured "0" would
///   not be a small budget, it would be a silently disabled self-heal that still
///   pays for three heavy-model drafts every attempt. It is read from the same
///   function the staging path uses, so raising a stage floor moves this end too.
/// * The HIGH end is 4h. Self-heal only runs on a machine that is ALREADY
///   MISBEHAVING; there is no operator intent behind "occupy every core for a
///   day", and an unbounded ceiling makes the one knob that bounds the machine
///   not bound it.
const ATTEMPT_BUDGET_MAX_SECS: u64 = 4 * 60 * 60;

/// The process-wide effective attempt budget, seeded once from config by
/// [`configure`]. Unset (every unit test, and any path that never loaded a
/// config) reads [`ATTEMPT_BUDGET_DEFAULT`] — so the default is the behavior,
/// not a fallback that differs from it.
static ATTEMPT_BUDGET_CELL: OnceLock<Duration> = OnceLock::new();

/// The attempt-wide cargo budget in force. See [`ATTEMPT_BUDGET_DEFAULT`].
fn attempt_budget() -> Duration {
    *ATTEMPT_BUDGET_CELL.get().unwrap_or(&ATTEMPT_BUDGET_DEFAULT)
}

/// The configured attempt budget, clamped. PURE — the clamp is unit-tested
/// without touching the process-wide cell, so no test can race another by
/// changing a global out from under it.
pub fn effective_attempt_budget(sh: &crate::config::SelfHealConfig) -> Duration {
    let lo = minimum_viable_budget().as_secs();
    Duration::from_secs(sh.attempt_budget_secs.clamp(lo, ATTEMPT_BUDGET_MAX_SECS))
}

/// Floor for the `check` stage — the cheapest gate, and the one whose failure
/// is the plain "this does not compile" message an operator needs first.
const CHECK_STAGE_FLOOR: Duration = Duration::from_secs(150);
/// Floor for `clippy --all-targets -D warnings`, the widest compile of the four
/// (it builds the test targets too).
const CLIPPY_STAGE_FLOOR: Duration = Duration::from_secs(300);
/// Seconds of a candidate's budget reserved for the `test` stage, so a
/// pathologically slow earlier stage cannot eat the budget and leave the stage
/// that actually exercises the patch with a few seconds and a `deadline`.
const TEST_STAGE_FLOOR: Duration = Duration::from_secs(240);
/// ...and for the mutation re-run, the one stage that proves the patch's own
/// test would catch the bug coming back. Measured at ~33s; 60s is a floor, not
/// an allowance.
const MUTATION_STAGE_FLOOR: Duration = Duration::from_secs(60);

/// THE ADVERSARIAL REVIEWER'S FLOOR. Below this confidence nothing is proposed.
///
/// `select_winner` takes the BEST-reviewed survivor, and "best" of three bad
/// patches is still a bad patch: a candidate the reviewer scored 0.05 — or one
/// whose review call never came back, which is recorded as 0.0 — cleared the
/// four mechanical gates and was surfaced to the operator as a
/// PROPOSAL READY FOR REVIEW with an ACCEPT & APPLY button next to it. The
/// gates are blind to intent (see the responsiveness probe); the reviewer is
/// the only stage that judges whether the patch is a good idea, and its verdict
/// had no effect on whether anything was proposed at all.
///
/// 0.25 is deliberately low: this is a floor against "the reviewer did not
/// believe this", not a quality bar. Everything a below-floor attempt produced
/// is still written under `state/heal/rejected/<ts>/`, so nothing is lost —
/// the operator can read every candidate, they just do not get a one-click
/// apply for a patch nobody vouched for. `scripts/apply_heal.sh` enforces the
/// SAME constant through `darwind --heal-confidence` (one implementation, two
/// callers, like `--split-heal-diff`), so a proposal written by an older daemon
/// cannot be applied under a weaker bar either.
/// (continued) THE DEFAULT, and what `config/darwin.toml` ships. The EFFECTIVE
/// value is [`confidence_floor()`], set once from `[self_heal].confidence_floor`.
///
/// MAKING IT CONFIGURABLE IS NOT THE SAME AS KNOWING WHAT IT SHOULD BE.
/// Replacing 0.25 with a different judged number would have been a guess in a
/// decision's clothes, so the default is unchanged and every attempt instead
/// emits the reviewer's confidence (and the per-stage timings) into telemetry
/// under `calibration`, so the right floor becomes readable from N real
/// attempts rather than argued.
const CONFIDENCE_FLOOR_DEFAULT: f64 = 0.25;

/// Clamp ends for `[self_heal].confidence_floor`. **DERIVED, NOT PICKED.**
///
/// * The LOW end must be strictly above the score a FAILED review call is
///   recorded as (0.0) — at 0.0 the floor is not a floor, it admits "no review
///   happened" as if it were a verdict, which is the exact defect this gate was
///   added to close (`the_confidence_floor_is_inclusive_at_the_bar` pins it).
///   It must ALSO stay strictly above the BELOW_FLOOR probe
///   `scripts/apply_heal.sh` proves its own gate with (0.01), or that
///   self-proof stops discriminating and the apply gate fails closed on every
///   proposal.
/// * The HIGH end must stay at or below that script's ABOVE_FLOOR probe (0.95),
///   for the same reason from the other side.
///
/// `the_confidence_clamp_brackets_the_apply_scripts_own_self_proof` reads both
/// probe values back out of `scripts/apply_heal.sh` and asserts this bracket, so
/// the derivation cannot rot if either file is edited alone.
const CONFIDENCE_FLOOR_MIN: f64 = 0.05;
const CONFIDENCE_FLOOR_MAX: f64 = 0.95;

/// The process-wide effective floor, seeded once from config by [`configure`].
/// Unset reads [`CONFIDENCE_FLOOR_DEFAULT`].
static CONFIDENCE_FLOOR_CELL: OnceLock<f64> = OnceLock::new();

/// The review-confidence floor in force. `pub` for the `--heal-confidence` CLI
/// and the HUD wire payload. See [`CONFIDENCE_FLOOR_DEFAULT`].
pub fn confidence_floor() -> f64 {
    *CONFIDENCE_FLOOR_CELL.get().unwrap_or(&CONFIDENCE_FLOOR_DEFAULT)
}

/// The configured floor, clamped. PURE (see [`effective_attempt_budget`]).
/// A NaN — which no comparison would reject and which would make
/// `meets_confidence_floor` answer `false` for every score, walling off the
/// gate — falls back to the default rather than propagating.
pub fn effective_confidence_floor(sh: &crate::config::SelfHealConfig) -> f64 {
    if !sh.confidence_floor.is_finite() {
        return CONFIDENCE_FLOOR_DEFAULT;
    }
    sh.confidence_floor.clamp(CONFIDENCE_FLOOR_MIN, CONFIDENCE_FLOOR_MAX)
}

/// Seed BOTH effective values from `[self_heal]`. Idempotent and first-wins
/// (`OnceLock::set`), so the daemon and the `--heal-confidence` CLI reach the
/// same numbers from the same file and a later call cannot move the bar
/// mid-attempt. Logs when a configured value was clamped — an operator who
/// wrote `attempt_budget_secs = 60` must not be silently given 750.
pub fn configure(sh: &crate::config::SelfHealConfig) {
    let floor = effective_confidence_floor(sh);
    if (floor - sh.confidence_floor).abs() > f64::EPSILON {
        warn!(
            configured = sh.confidence_floor,
            effective = floor,
            "self_heal.confidence_floor is outside [{CONFIDENCE_FLOOR_MIN}, {CONFIDENCE_FLOOR_MAX}] and was clamped"
        );
    }
    let budget = effective_attempt_budget(sh);
    if budget.as_secs() != sh.attempt_budget_secs {
        warn!(
            configured = sh.attempt_budget_secs,
            effective = budget.as_secs(),
            "self_heal.attempt_budget_secs is outside [{}, {ATTEMPT_BUDGET_MAX_SECS}] and was clamped",
            minimum_viable_budget().as_secs(),
        );
    }
    let _ = CONFIDENCE_FLOOR_CELL.set(floor);
    let _ = ATTEMPT_BUDGET_CELL.set(budget);
}

const PATCH_BIN: &str = "/usr/bin/patch";
/// Validation output tail kept in report.md / candidates.md.
const REPORT_TAIL_CHARS: usize = 4000;

// ---------------------------------------------------------------------------
// Cloud seam (trait) — the ONLY route to the cloud. Production uses CloudBrain
// (anthropic::complete_plain); unit tests inject a mock so no cloud call is
// ever made under `cargo test`. The verifier's --heal-drill is the one real
// cloud path.
// ---------------------------------------------------------------------------

/// A `Send` future returned by the trait methods. Spelled out explicitly so
/// the trait stays object-safe (`&dyn HealBrain`) WITHOUT pulling in the
/// async-trait crate (the "no new dependencies" rule applies to the daemon
/// too): the production path and every mock implement these two methods.
type BrainFuture<'a> = Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;

/// The drafter+reviewer seam. Both methods are latency-insensitive cloud
/// calls; impls own their own timeouts. Errors are surfaced (the pipeline
/// rejects the attempt rather than guessing). This is the ONLY route to the
/// cloud — unit tests inject a mock so no cloud call is made under
/// `cargo test`; the verifier's --heal-drill is the one real cloud path.
pub trait HealBrain: Send + Sync {
    /// Draft up to `n` ALTERNATIVE minimal unified-diff patches for the given
    /// diagnosis. Returns the raw model text (multi-diff, parsed by the
    /// caller via split_candidate_diffs/clean_diff).
    fn draft_candidates<'a>(&'a self, diagnosis: &'a Diagnosis, n: usize) -> BrainFuture<'a>;

    /// Adversarially review one surviving (validated) diff against the
    /// diagnosis + its captured validation output. Returns the raw model text
    /// (parsed by the caller via parse_review).
    fn review<'a>(
        &'a self,
        diagnosis: &'a Diagnosis,
        diff: &'a str,
        validation_tail: &'a str,
    ) -> BrainFuture<'a>;
}

/// Production HealBrain: the heavy Anthropic model via anthropic.rs. Holds the
/// model id so the drill and the watchdog share one impl.
pub struct CloudBrain {
    pub model: String,
}

impl HealBrain for CloudBrain {
    fn draft_candidates<'a>(&'a self, diagnosis: &'a Diagnosis, n: usize) -> BrainFuture<'a> {
        Box::pin(async move {
            anthropic::complete_plain(
                &self.model,
                DRAFT_MAX_TOKENS,
                DRAFT_SYSTEM,
                &draft_prompt(diagnosis, n),
                DRAFT_TIMEOUT,
            )
            .await
        })
    }

    fn review<'a>(
        &'a self,
        diagnosis: &'a Diagnosis,
        diff: &'a str,
        validation_tail: &'a str,
    ) -> BrainFuture<'a> {
        Box::pin(async move {
            anthropic::complete_plain(
                &self.model,
                REVIEW_MAX_TOKENS,
                REVIEW_SYSTEM,
                &review_prompt(diagnosis, diff, validation_tail),
                REVIEW_TIMEOUT,
            )
            .await
        })
    }
}

/// Every 10s, tail state/logs/daemon.log and look for an error burst
/// (>= 5 ERROR-level lines within the last 60s) or a total-loss line.
/// Edge-triggered: one pipeline run per episode, re-armed only after the
/// burst clears.
pub async fn watchdog(root: PathBuf, cfg: Arc<Config>, memory: Arc<Memory>) {
    let log_path = root.join("state").join("logs").join("daemon.log");
    let mut interval = tokio::time::interval(CHECK_INTERVAL);
    let mut in_burst = false;
    let brain = CloudBrain {
        model: cfg.cloud.heavy_model.clone(),
    };
    loop {
        interval.tick().await;
        let scan = match scan_log(&log_path) {
            Ok(scan) => scan,
            Err(_) => continue, // log not written yet; nothing to inspect
        };
        if !scan.triggered() {
            in_burst = false; // episode over; re-arm
            continue;
        }
        if in_burst {
            continue; // already handled this episode
        }
        in_burst = true;
        if !cfg.self_heal.enabled {
            warn!(
                errors_last_60s = scan.burst_count,
                total_loss = scan.total_loss,
                "heal: error burst detected but self_heal.enabled = false; would diagnose, draft \
                 N candidate diffs via the heavy model, stage+validate each, adversarially review \
                 the survivors, and propose (or auto-apply) per [self_heal].mode"
            );
            telemetry::emit(
                "system",
                "heal.suppressed",
                json!({
                    "errors_last_60s": scan.burst_count,
                    "total_loss": scan.total_loss,
                    "reason": "self_heal.enabled = false",
                }),
            );
            continue;
        }
        telemetry::emit(
            "system",
            "heal.triggered",
            json!({"errors_last_60s": scan.burst_count, "total_loss": scan.total_loss}),
        );
        run_pipeline(&root, &cfg, &memory, &brain, &scan).await;
    }
}

// ---------------------------------------------------------------------------
// Trigger detection
// ---------------------------------------------------------------------------

/// What one tail inspection saw.
#[derive(Debug, Default)]
struct LogScan {
    /// ERROR-level lines inside the burst window.
    burst_count: usize,
    /// An in-window ERROR line carried a total-loss marker.
    total_loss: bool,
    /// The in-window ERROR lines, oldest first, capped at BURST_LINE_CAP.
    burst_lines: Vec<String>,
    /// The raw log tail (for the ~80-line drafter context).
    tail: String,
}

impl LogScan {
    fn triggered(&self) -> bool {
        self.burst_count >= BURST_LIMIT || self.total_loss
    }
}

/// True only when the line's level field is ERROR. The tracing fmt layout is
/// `<rfc3339-ts> <LEVEL> <target>: <msg>` (the level may be space-padded), so
/// the level is the second whitespace-separated token — substring-matching
/// the whole line would also count INFO lines whose message text quotes
/// "ERROR" (logged responses/utterances) and the watchdog's own warnings.
fn is_error_line(line: &str) -> bool {
    let mut fields = line.split_whitespace();
    let _ts = fields.next();
    fields.next() == Some("ERROR")
}

/// An ERROR line announcing an unrecoverable one-shot loss.
fn is_total_loss_line(line: &str) -> bool {
    is_error_line(line) && TOTAL_LOSS_MARKERS.iter().any(|m| line.contains(m))
}

/// Inspect the log tail: count ERROR-level lines whose leading RFC3339
/// timestamp falls within the burst window, collect them for the drafter,
/// and flag total-loss lines. Lines without a parseable timestamp count
/// conservatively (better a false trigger than a missed one in a watchdog).
fn scan_log(path: &Path) -> std::io::Result<LogScan> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)))?;
    let mut raw = Vec::new();
    file.read_to_end(&mut raw)?;
    let tail = String::from_utf8_lossy(&raw).into_owned();
    Ok(scan_tail(tail))
}

/// Pure half of scan_log, separable for tests.
fn scan_tail(tail: String) -> LogScan {
    let cutoff = Utc::now() - chrono::Duration::seconds(BURST_WINDOW_SECS);
    let mut scan = LogScan::default();
    for line in tail.lines().rev() {
        if !is_error_line(line) {
            continue;
        }
        let ts = line
            .split_whitespace()
            .next()
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok());
        match ts {
            Some(t) if t.with_timezone(&Utc) >= cutoff => {}
            // Older than the window; everything before is older still.
            Some(_) => break,
            // Unparseable timestamp: count conservatively.
            None => {}
        }
        scan.burst_count += 1;
        scan.total_loss = scan.total_loss || is_total_loss_line(line);
        if scan.burst_lines.len() < BURST_LINE_CAP {
            scan.burst_lines.push(line.to_string());
        }
    }
    scan.burst_lines.reverse(); // collected newest-first; report oldest-first
    scan.tail = tail;
    scan
}

// ---------------------------------------------------------------------------
// (3) Root-cause diagnosis (v2) — pure, unit-tested
// ---------------------------------------------------------------------------

/// A structured root-cause diagnosis built from the burst, before any cloud
/// work. Serialized verbatim to state/heal/proposals/<ts>/diagnosis.json.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Diagnosis {
    /// The dominant error signature(s) — the message text of the ERROR lines
    /// with volatile tails (timestamps, paths, "error=...") trimmed, so a
    /// recurring fault collapses to one stable line per distinct cause.
    pub signatures: Vec<String>,
    /// Cited daemon source files (and any `:line`s found alongside them).
    pub files: Vec<String>,
    /// Line numbers cited next to a src/<file>.rs:<line> reference, in
    /// first-seen order (a hint for the drafter; may be empty).
    pub line_numbers: Vec<u32>,
    /// The implicated subsystem inferred from the module path in the ERROR
    /// target field (audio/inference/router/...) or "unknown".
    pub subsystem: String,
    /// The window of surrounding log context (the last CONTEXT_LINES of tail).
    pub log_context: String,
    /// The burst lines verbatim, oldest first (also in the report).
    pub burst_lines: Vec<String>,
    /// Current contents of the cited source files, read from the crate being
    /// healed (path -> body), so the drafter can produce a unified diff whose
    /// hunk context actually matches the tree and applies cleanly with
    /// `patch -p1`. Empty until attach_source_excerpts() runs (build_diagnosis
    /// stays pure/IO-free); a file that cannot be read is simply omitted.
    #[serde(default)]
    pub source_excerpts: Vec<(String, String)>,
}

impl Diagnosis {
    /// The one-line signature the heal.diagnosing event carries (the first /
    /// dominant signature, or a fallback when none parsed).
    fn primary_signature(&self) -> String {
        self.signatures
            .first()
            .cloned()
            .unwrap_or_else(|| "unclassified error burst".to_string())
    }
}

/// Known daemon subsystems, matched against the module-path target token of an
/// ERROR line (`darwin_core::<subsystem>::...`). First match in burst order
/// wins; "unknown" when nothing matches (e.g. a bare `darwin_core` target).
const SUBSYSTEMS: &[&str] = &[
    "audio",
    "inference",
    "router",
    "speech",
    "playback",
    "actions",
    "anthropic",
    "memory",
    "apps",
    "genproxy",
    "proactive",
    "reflect",
    "heal",
];

/// The tracing target token of a log line: the 3rd whitespace field, stripped
/// of a trailing ':'. For `<ts> ERROR darwin_core::router: msg` that is
/// `darwin_core::router`.
fn target_token(line: &str) -> Option<&str> {
    let mut fields = line.split_whitespace();
    fields.next()?; // ts
    fields.next()?; // level
    fields.next().map(|t| t.trim_end_matches(':'))
}

/// Infer the subsystem from the module path of the first burst line whose
/// target names a known subsystem.
fn infer_subsystem(burst_lines: &[String]) -> String {
    for line in burst_lines {
        if let Some(target) = target_token(line) {
            for sub in SUBSYSTEMS {
                // Match `darwin_core::<sub>` or `darwin_core::<sub>::...`.
                let needle = format!("::{sub}");
                if target.ends_with(&needle) || target.contains(&format!("{needle}::")) {
                    return (*sub).to_string();
                }
            }
        }
    }
    "unknown".to_string()
}

/// Reduce one ERROR line to a stable signature: drop the leading timestamp +
/// level + target, then trim the volatile `error=...`/`err=...` tail so the
/// same recurring fault collapses to one signature regardless of the exact
/// transient detail.
fn error_signature(line: &str) -> Option<String> {
    if !is_error_line(line) {
        return None;
    }
    // Everything after the first ": " (the message), else after the target.
    let msg = line.split_once(": ").map(|x| x.1).unwrap_or(line).trim();
    // Trim a volatile detail tail introduced by " error=" / " err=".
    let cut = msg
        .find(" error=")
        .or_else(|| msg.find(" err="))
        .unwrap_or(msg.len());
    let sig = msg[..cut].trim().to_string();
    (!sig.is_empty()).then_some(sig)
}

/// Distinct error signatures across the burst, in first-seen order.
fn extract_signatures(burst_lines: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in burst_lines {
        if let Some(sig) = error_signature(line) {
            if !out.contains(&sig) {
                out.push(sig);
            }
        }
    }
    out
}

/// Line numbers cited as `src/<file>.rs:<line>` across the text, first-seen,
/// deduplicated.
fn extract_line_numbers(text: &str) -> Vec<u32> {
    let mut out: Vec<u32> = Vec::new();
    for (idx, _) in text.match_indices(".rs:") {
        let rest = &text[idx + 4..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(n) = digits.parse::<u32>() {
            if !out.contains(&n) {
                out.push(n);
            }
        }
    }
    out
}

/// Build the structured diagnosis from a scan. Pure (no cloud, no IO).
fn build_diagnosis(scan: &LogScan) -> Diagnosis {
    let burst_lines = scan.burst_lines.clone();
    let burst_excerpt = burst_lines.join("\n");
    Diagnosis {
        signatures: extract_signatures(&burst_lines),
        files: extract_source_files(&burst_excerpt),
        line_numbers: extract_line_numbers(&burst_excerpt),
        subsystem: infer_subsystem(&burst_lines),
        log_context: last_lines(&scan.tail, CONTEXT_LINES),
        burst_lines,
        source_excerpts: Vec::new(),
    }
}

/// Largest source file body handed to the drafter, per file (chars). A patch
/// drafter needs the real lines to produce an applying hunk; cap so a huge file
/// cannot blow the prompt budget — the cited line numbers still point the model
/// at the right region.
const SOURCE_EXCERPT_CAP: usize = 12_000;

/// Read the current contents of each cited source file from `crate_dir`
/// (impure; kept OUT of build_diagnosis so that stays unit-testable without
/// IO). Files that cannot be read are skipped. Paths are crate-root-relative
/// (e.g. "src/router.rs"), exactly as they appear in the burst — the same form
/// the drafted diff's a//b/ headers use, so the model sees and patches the same
/// path. Reading is confined to <crate_dir>/src to avoid escaping the tree via
/// a crafted log path.
fn attach_source_excerpts(d: &mut Diagnosis, crate_dir: &Path) {
    let src_root = crate_dir.join("src");
    for rel in &d.files {
        // Only files under src/ (the crate sources we ever patch); a path that
        // does not normalize to within src_root is ignored.
        let full = crate_dir.join(rel);
        let Ok(canon) = full.canonicalize() else { continue };
        let Ok(src_canon) = src_root.canonicalize() else { continue };
        if !canon.starts_with(&src_canon) {
            continue;
        }
        if let Ok(body) = std::fs::read_to_string(&canon) {
            let body = first_chars(&body, SOURCE_EXCERPT_CAP);
            d.source_excerpts.push((rel.clone(), body));
        }
    }
}

/// The first `n` chars of `s` (mirrors anthropic::first_chars, kept local).
fn first_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

// ---------------------------------------------------------------------------
// Pure pipeline helpers (each unit-tested)
// ---------------------------------------------------------------------------

/// What the enabled/mode pair permits. Unknown modes degrade to Propose —
/// never to Auto — so a typo can only make self-heal safer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HealAction {
    Disabled,
    Propose,
    Auto,
}

fn heal_action(enabled: bool, mode: &str) -> HealAction {
    if !enabled {
        return HealAction::Disabled;
    }
    match mode.trim() {
        "auto" => HealAction::Auto,
        _ => HealAction::Propose, // "propose" and anything unknown
    }
}

/// Rate-limit math: a draft attempt is allowed when no stamp exists, the
/// stamp is unparseable, or it is older than ATTEMPT_INTERVAL_SECS. A stamp
/// from the future (clock skew) blocks — saturating_sub yields 0.
fn attempt_allowed(last_attempt: Option<&str>, now_secs: u64) -> bool {
    match last_attempt.and_then(|v| v.trim().parse::<u64>().ok()) {
        Some(last) => now_secs.saturating_sub(last) > ATTEMPT_INTERVAL_SECS,
        None => true,
    }
}

/// Daemon source files named in log text: every "src/<path>.rs" occurrence
/// (the panic/log convention is "src/<file>.rs:<line>"), deduplicated in
/// first-seen order. Also applied to a drafted diff to list files touched.
fn extract_source_files(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for (idx, _) in text.match_indices("src/") {
        let rest = &text[idx..];
        let mut end = 0;
        for (i, c) in rest.char_indices() {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '/' | '.' | '-') {
                end = i + c.len_utf8();
            } else {
                break;
            }
        }
        let token = &rest[..end];
        if let Some(pos) = token.find(".rs") {
            let path = token[..pos + 3].to_string();
            if !found.contains(&path) {
                found.push(path);
            }
        }
    }
    found
}

/// Staging directory name for candidate `i` (0-based) of one attempt. v2
/// stages each candidate independently so survivors never collide.
fn staging_dir_name(ts: u64, candidate: usize) -> String {
    format!("staging-{ts}-c{candidate}")
}

/// The last `n` lines of `text`, newline-joined.
fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// The last `n` chars of `s` (validation output can be huge).
fn tail_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    s.chars().skip(count.saturating_sub(n)).collect()
}

/// Crude size of a diff for the min-patch tiebreak: number of added/removed
/// lines (lines starting with a single +/- that are not the ---/+++ headers).
fn diff_size(diff: &str) -> usize {
    diff.lines()
        .filter(|l| {
            (l.starts_with('+') && !l.starts_with("+++"))
                || (l.starts_with('-') && !l.starts_with("---"))
        })
        .count()
}

// ---------------------------------------------------------------------------
// RESPONSIVENESS — does this candidate answer the fault that TRIGGERED it?
//
// check -> clippy -> test -> mutation prove that a candidate COMPILES, LINTS,
// PASSES, and that its own new test BITES. Not one of those four stages ever
// looks at the DIAGNOSIS. A patch that fixes something else entirely — a real
// bug, in a file the burst never named, with a genuine regression test that
// really does fail when its fix is reversed — clears every gate and is handed
// to the owner as "the fix" for an error burst it never touched. That is not a
// hypothetical: `an_unresponsive_candidate_clears_every_gate` below builds one
// and walks it through the whole chain.
//
// THIS IS ADVISORY AND NEVER REJECTS. A correct fix routinely lives in a file
// the log never cited — the cause is usually one layer up from the line that
// screamed, and a hard reject on a heuristic would throw exactly those away.
// So the verdict is COMPUTED and SURFACED: into the validation tail (which the
// adversarial reviewer reads and is told to weight), into report.md's header,
// onto heal.proposal for the HUD, and re-derived and re-printed by
// scripts/apply_heal.sh before the human commits.
//
// ONE IMPLEMENTATION, TWO CALLERS — the daemon gate calls `responsiveness()`
// directly; the shell gate shells out to `darwind --heal-responsiveness`, the
// same pattern `--split-heal-diff` uses, because two gates that each carry
// their own copy of a rule is the defect shape this file has produced more than
// any other.
// ---------------------------------------------------------------------------

/// How well a candidate patch answers the diagnosis that triggered the heal.
/// These are KINDS OF EVIDENCE, not a ranking; only `Unrelated` is a warning,
/// and even that is a warning and not a verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Responsiveness {
    /// The patch edits a file the error burst itself named.
    Direct,
    /// No cited file is edited, but an edited path belongs to the implicated
    /// subsystem (`src/<subsystem>.rs`, or anything under `src/<subsystem>/`).
    Subsystem,
    /// Neither, but an error signature from the burst appears verbatim in the
    /// patch body — the drafter is editing the code that logs it.
    Signature,
    /// The diagnosis carried something to match on, and NOTHING matched.
    Unrelated,
    /// The diagnosis carried nothing to match on (no cited file, no known
    /// subsystem, no distinctive message text). No opinion is possible, and
    /// manufacturing one would be the same dishonesty in the other direction.
    Indeterminate,
}

impl Responsiveness {
    /// The single word both gates print and telemetry carries.
    pub fn word(self) -> &'static str {
        match self {
            Responsiveness::Direct => "DIRECT",
            Responsiveness::Subsystem => "SUBSYSTEM",
            Responsiveness::Signature => "SIGNATURE",
            Responsiveness::Unrelated => "UNRELATED",
            Responsiveness::Indeterminate => "INDETERMINATE",
        }
    }
}

/// Shortest error signature distinctive enough to search for verbatim inside a
/// diff. Below this, a message fragment ("failed", "timeout", "no such file")
/// matches half the crate and the signal is noise.
const SIGNATURE_MATCH_MIN: usize = 12;

/// The files a unified diff actually EDITS: the `+++` headers, with the one
/// component `patch -p1` eats stripped off.
///
/// Deliberately NOT `extract_source_files()`, which scans the entire text — that
/// would count a `src/…` path merely MENTIONED in a context line, an added
/// comment or a doc string as a file the patch touches, and a drafter that
/// name-drops the cited file in a comment would score DIRECT for free. Only a
/// `+++` header means "this patch writes here".
fn patched_files(diff: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in diff.lines() {
        let Some(rest) = line.strip_prefix("+++ ") else { continue };
        let path = rest.split('\t').next().unwrap_or(rest).trim();
        if path == "/dev/null" {
            continue; // new-file/deleted-file sentinel, not a target
        }
        let stripped = match path.find('/') {
            Some(i) => &path[i + 1..],
            None => path,
        };
        if !stripped.is_empty() && !out.iter().any(|p| p == stripped) {
            out.push(stripped.to_string());
        }
    }
    out
}

/// The same file, tolerating a differing number of leading components
/// (`src/router.rs` vs `daemon/src/router.rs`). Compared at a path BOUNDARY: a
/// bare `ends_with` would call `src/tools.rs` a match for a cited `s.rs`.
fn same_file(a: &str, b: &str) -> bool {
    a == b || a.ends_with(&format!("/{b}")) || b.ends_with(&format!("/{a}"))
}

/// Does `path` belong to `subsystem`? `src/audio.rs`, or anything under a
/// directory component named `audio`. Component-exact — no substring matching,
/// so `src/audiobook.rs` is NOT the `audio` subsystem.
fn path_in_subsystem(path: &str, subsystem: &str) -> bool {
    let p = Path::new(path);
    if p.file_stem().and_then(|s| s.to_str()) == Some(subsystem) {
        return true;
    }
    p.parent()
        .map(|d| d.components().any(|c| c.as_os_str() == subsystem))
        .unwrap_or(false)
}

/// Note naming the files the burst cited but the patch does not edit, for the
/// weaker verdicts. Empty when the burst cited nothing.
fn cited_but_untouched(files: &[String]) -> String {
    if files.is_empty() {
        String::new()
    } else {
        format!(" (the burst named {}, which this patch does not edit)", files.join(", "))
    }
}

/// Judge a candidate patch against the diagnosis that triggered the heal.
///
/// PURE — no cloud, no filesystem, no build — so the rule is unit-tested
/// directly and BOTH gates can share this one implementation.
pub fn responsiveness(d: &Diagnosis, diff: &str) -> (Responsiveness, String) {
    let touched = patched_files(diff);
    let shown = if touched.is_empty() {
        "(no +++ header — nothing identifiable)".to_string()
    } else {
        touched.join(", ")
    };

    let hit: Vec<&str> = d
        .files
        .iter()
        .filter(|c| touched.iter().any(|t| same_file(t, c)))
        .map(|c| c.as_str())
        .collect();
    if !hit.is_empty() {
        return (
            Responsiveness::Direct,
            format!(
                "DIRECT — the patch edits {}, which the error burst itself named.",
                hit.join(", ")
            ),
        );
    }

    let known_subsystem = !d.subsystem.trim().is_empty() && d.subsystem != "unknown";
    if known_subsystem {
        if let Some(t) = touched.iter().find(|t| path_in_subsystem(t, &d.subsystem)) {
            return (
                Responsiveness::Subsystem,
                format!(
                    "SUBSYSTEM — the patch edits {t}, which belongs to the implicated subsystem \
                     `{}`{}.",
                    d.subsystem,
                    cited_but_untouched(&d.files)
                ),
            );
        }
    }

    let lower_diff = diff.to_lowercase();
    let sig_hit = d
        .signatures
        .iter()
        .map(|s| s.trim())
        .filter(|s| s.chars().count() >= SIGNATURE_MATCH_MIN)
        .find(|s| lower_diff.contains(&s.to_lowercase()));
    if let Some(sig) = sig_hit {
        return (
            Responsiveness::Signature,
            format!(
                "SIGNATURE — the patch body carries the burst's own error text \"{sig}\", so it \
                 edits the code that logs it{}.",
                cited_but_untouched(&d.files)
            ),
        );
    }

    let matchable = !d.files.is_empty()
        || known_subsystem
        || d
            .signatures
            .iter()
            .any(|s| s.trim().chars().count() >= SIGNATURE_MATCH_MIN);
    if !matchable {
        return (
            Responsiveness::Indeterminate,
            format!(
                "INDETERMINATE — the burst named no source file, no known subsystem and no \
                 distinctive error text, so there is nothing to check the patch ({shown}) \
                 against. Judge it on the diff alone."
            ),
        );
    }
    (
        Responsiveness::Unrelated,
        format!(
            "UNRELATED — the patch edits {shown}. The diagnosis implicated {} (subsystem `{}`), \
             and none of its error signatures appear in the patch. THIS IS NOT A REJECTION — a \
             correct fix often lives one layer up from the line that screamed — but NOTHING here \
             connects this patch to the fault that triggered the heal. Read it on that basis.",
            if d.files.is_empty() {
                "no file by name".to_string()
            } else {
                d.files.join(", ")
            },
            d.subsystem
        ),
    )
}

/// The v2 multi-candidate drafter prompt: diagnosis in, N labelled diffs out.
fn draft_prompt(d: &Diagnosis, n: usize) -> String {
    let file_list = if d.files.is_empty() {
        "(no src/<file>.rs paths appeared in the burst; infer the most likely file from the log \
         and touch only that one)"
            .to_string()
    } else {
        d.files.join(", ")
    };
    let sigs = if d.signatures.is_empty() {
        "(no clean signature extracted; read the burst lines below)".to_string()
    } else {
        d.signatures.join("\n  - ")
    };
    let burst_excerpt = d.burst_lines.join("\n");
    let sources = if d.source_excerpts.is_empty() {
        "(source contents unavailable; infer the surrounding code from the log)".to_string()
    } else {
        d.source_excerpts
            .iter()
            .map(|(path, body)| format!("--- {path} (current contents) ---\n{body}"))
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    format!(
        "The DARWIN daemon (a Rust crate; sources under src/) hit an error burst and needs a \
         minimal source fix.\n\n\
         Diagnosis:\n\
         - subsystem: {subsystem}\n\
         - implicated files: {file_list}\n\
         - error signature(s):\n  - {sigs}\n\n\
         Error-burst lines:\n{burst_excerpt}\n\n\
         Current contents of the implicated source file(s) — your diff MUST match these exact \
         lines so it applies with `patch -p1`:\n{sources}\n\n\
         Recent daemon.log context (last {CONTEXT_LINES} lines):\n{log_context}\n\n\
         Propose {n} ALTERNATIVE, DISTINCT minimal fixes that address the ROOT CAUSE (not just \
         silence the symptom). Output EXACTLY {n} unified diffs, each preceded by a header line \
         of the form `=== CANDIDATE i ===` (i = 1..{n}). Rules for every diff:\n\
         - Paths relative to the crate root with a/ and b/ prefixes (e.g. --- a/src/router.rs).\n\
         - Touch only the implicated files; make the smallest change that fixes the cause.\n\
         - No new dependencies; do not modify Cargo.toml or Cargo.lock.\n\
         - Each diff must apply cleanly with `patch -p1` and pass `cargo check`, \
           `cargo clippy --all-targets -- -D warnings`, and `cargo test`.\n\
         - EVERY diff MUST add a regression test, inside the file's `#[cfg(test)] mod tests`, \
           that FAILS without your fix. The gate re-runs the suite with your fix REVERSED and \
           your test kept: if the test still passes, the candidate is REJECTED as unproven. \
           Assert the specific behaviour the bug got wrong — a test that merely exercises the \
           code path, or asserts something true before and after, will be thrown out.\n\
         - Put the fix and the test in SEPARATE hunks (fix at the call site, test in the test \
           module). A single hunk containing both cannot be separated and cannot be proven.\n\
         - No prose inside or between the diffs beyond the `=== CANDIDATE i ===` markers.",
        subsystem = d.subsystem,
        sigs = sigs,
        log_context = d.log_context,
    )
}

/// The v2 adversarial review prompt: diagnosis + one validated diff + its test
/// output in, a strict verdict + confidence out.
fn review_prompt(d: &Diagnosis, diff: &str, validation_tail: &str) -> String {
    let sigs = if d.signatures.is_empty() {
        "(none extracted)".to_string()
    } else {
        d.signatures.join("; ")
    };
    format!(
        "A candidate patch PASSED staged validation (`cargo check`, `cargo clippy -D warnings`, \
         full `cargo test`, and a mutation probe that re-ran the suite with the fix REVERSED and \
         the test kept — see the validation tail for whether that came back PROVEN, UNPROVEN or \
         INCONCLUSIVE, and weight your confidence accordingly). Judge \
         whether it fixes the ROOT CAUSE of the fault below, not merely silences the symptom, and \
         whether it has any obvious side effects or regressions. The tail also carries a \
         RESPONSIVENESS line (DIRECT / SUBSYSTEM / SIGNATURE / UNRELATED / INDETERMINATE): the \
         staged gates are BLIND to the diagnosis, so an UNRELATED patch passes all of them. \
         Treat UNRELATED as a strong reason for LOW confidence unless the diff itself explains \
         why the true cause lives where it does.\n\n\
         Fault diagnosis:\n\
         - subsystem: {subsystem}\n\
         - signature(s): {sigs}\n\n\
         Candidate diff:\n{diff}\n\n\
         Staged validation output (tail):\n{validation_tail}\n\n\
         Respond on EXACTLY two lines, nothing else:\n\
         VERDICT: <one sentence: does it fix the root cause, and any side-effect concerns>\n\
         CONFIDENCE: <a single number 0.0-1.0>",
        subsystem = d.subsystem,
        sigs = sigs,
    )
}

/// Belt-and-braces cleanup of one model diff: strip code fences, any leading
/// prose before the first diff header, and a trailing `=== CANDIDATE ... ===`
/// marker. None when no unified diff is present at all (a refusal or prose
/// answer must never reach patch).
fn clean_diff(raw: &str) -> Option<String> {
    let mut lines: Vec<&str> = Vec::new();
    let mut started = false;
    for line in raw.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim_start().starts_with("```") {
            continue; // fence open/close
        }
        // A candidate marker terminates this diff (defensive: split should
        // already have removed it).
        if trimmed.trim_start().starts_with("=== CANDIDATE") {
            if started {
                break;
            }
            continue;
        }
        if !started {
            if trimmed.starts_with("--- ")
                || trimmed.starts_with("diff ")
                || trimmed.starts_with("Index: ")
            {
                started = true;
            } else {
                continue; // leading prose
            }
        }
        lines.push(line);
    }
    if !lines.iter().any(|l| l.starts_with("--- "))
        || !lines.iter().any(|l| l.starts_with("+++ "))
        || !lines.iter().any(|l| l.starts_with("@@"))
    {
        return None;
    }
    // Path-confinement: `patch -p1` is run with cwd = the target dir on this
    // model-drafted diff. macOS /usr/bin/patch honors `..` in `---`/`+++` hunk
    // headers, so a header like `+++ b/src/../../../../tmp/x` would write OUTSIDE
    // the staging dir (and, on auto_apply, outside daemon/). This is the single
    // chokepoint every candidate flows through (split_candidate_diffs ->
    // clean_diff), so reject any header that, after the `-p1` strip, is empty,
    // absolute, or contains a `..` component — mirroring forge::is_confined_relpath
    // and dropping the candidate exactly like any non-diff. Legitimate heal diffs
    // use `a/src/...`/`b/src/...` headers, which strip to `src/...` and survive.
    for line in &lines {
        if let Some(rest) = line.strip_prefix("--- ").or_else(|| line.strip_prefix("+++ ")) {
            // The path token is the field before any trailing tab/whitespace+timestamp.
            let path = rest.split('\t').next().unwrap_or(rest).trim_end();
            if path == "/dev/null" {
                continue; // new-file / deleted-file sentinel — not a real target
            }
            // Mirror `-p1`: strip exactly one leading path component (up to and
            // including the first '/').
            let stripped = match path.find('/') {
                Some(i) => &path[i + 1..],
                None => "",
            };
            if stripped.is_empty()
                || stripped.starts_with('/')
                || stripped.split('/').any(|seg| seg == "..")
            {
                return None; // escape attempt — drop the candidate before patch runs
            }
        }
    }
    let mut out = lines.join("\n");
    out.push('\n'); // patch(1) wants a final newline
    Some(out)
}

/// Split the multi-candidate model response on `=== CANDIDATE i ===` markers
/// and clean each block into a diff. Blocks that are not valid diffs are
/// dropped. If NO markers appear at all, fall back to treating the whole
/// response as a single diff (a model that ignored the format still gives us
/// one candidate). Returns diffs in document order, deduplicated.
fn split_candidate_diffs(raw: &str) -> Vec<String> {
    let mut blocks: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut saw_marker = false;
    for line in raw.lines() {
        if line.trim_start().starts_with("=== CANDIDATE") {
            saw_marker = true;
            if !current.trim().is_empty() {
                blocks.push(std::mem::take(&mut current));
            } else {
                current.clear();
            }
            continue;
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.trim().is_empty() {
        blocks.push(current);
    }
    if !saw_marker {
        // No markers: the whole thing is at most one candidate.
        blocks = vec![raw.to_string()];
    }
    let mut diffs: Vec<String> = Vec::new();
    for block in blocks {
        if let Some(d) = clean_diff(&block) {
            if !diffs.contains(&d) {
                diffs.push(d);
            }
        }
    }
    diffs
}

// ---------------------------------------------------------------------------
// (6)+(7) Adversarial review parsing + survivor selection — pure, unit-tested
// ---------------------------------------------------------------------------

/// A surviving candidate that PASSED staged validation, plus its review.
#[derive(Debug, Clone)]
struct Survivor {
    /// 1-based candidate index, for the report.
    index: usize,
    diff: String,
    files: Vec<String>,
    validation_tail: String,
    review_verdict: String,
    confidence: f64,
    /// Did the adversarial reviewer actually ANSWER? A failed review call is
    /// recorded as confidence 0.0, which is indistinguishable from a reviewer
    /// saying "I do not believe this patch" — and they are opposite facts. The
    /// confidence floor rejects both, but the report has to say which happened.
    reviewed: bool,
    /// Added/removed line count — the min-patch tiebreak.
    size: usize,
}

/// Parse the reviewer's `VERDICT:`/`CONFIDENCE:` reply into (verdict,
/// confidence). A missing/garbled confidence is treated as 0.0 (conservative:
/// an unparseable review never wins selection over a clearly-scored peer). The
/// confidence is clamped to 0..1.
fn parse_review(raw: &str) -> (String, f64) {
    let mut verdict = String::new();
    let mut confidence = 0.0f64;
    for line in raw.lines() {
        let t = line.trim();
        if let Some(rest) = strip_label(t, "VERDICT") {
            verdict = rest.trim().to_string();
        } else if let Some(rest) = strip_label(t, "CONFIDENCE") {
            confidence = parse_confidence(rest);
        }
    }
    if verdict.is_empty() {
        verdict = raw.trim().lines().next().unwrap_or("").trim().to_string();
    }
    (verdict, confidence.clamp(0.0, 1.0))
}

/// Case-insensitive `LABEL:` / `LABEL ` prefix strip.
fn strip_label<'a>(line: &'a str, label: &str) -> Option<&'a str> {
    let lower = line.to_ascii_lowercase();
    let lab = label.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix(&lab) {
        // Re-slice the ORIGINAL (preserve case) past the matched prefix and a
        // following ':' / whitespace.
        let consumed = line.len() - rest.len();
        let after = line[consumed..].trim_start_matches([':', ' ', '\t']);
        Some(after)
    } else {
        None
    }
}

/// First float-looking token in `s`, clamped later by the caller.
fn parse_confidence(s: &str) -> f64 {
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
        } else if !num.is_empty() {
            break;
        }
    }
    num.parse::<f64>().unwrap_or(0.0)
}

/// Does a review confidence clear [`confidence_floor()`]?
///
/// THE ONE COMPARISON. The daemon's own selection and
/// `darwind --heal-confidence` — which `scripts/apply_heal.sh` shells out to,
/// exactly like `--split-heal-diff` — both call this, so the two gates cannot
/// drift into two different bars. `pub` for that CLI caller.
pub fn meets_confidence_floor(confidence: f64) -> bool {
    confidence >= confidence_floor()
}

/// What the shared confidence gate says about one proposal. Printed as its word
/// by `darwind --heal-confidence` and consumed by `scripts/apply_heal.sh`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConfidenceGate {
    Above,
    Below,
    /// report.md carries no parseable confidence at all. NOT the same as a low
    /// score, and the apply gate refuses on it rather than guessing.
    NoScore,
}

impl ConfidenceGate {
    pub fn word(self) -> &'static str {
        match self {
            ConfidenceGate::Above => "ABOVE_FLOOR",
            ConfidenceGate::Below => "BELOW_FLOOR",
            ConfidenceGate::NoScore => "NO_SCORE",
        }
    }
}

/// Read the adversarial-review confidence back out of a proposal's `report.md`.
///
/// The line it reads is the one [`render_report`] writes, in this same module —
/// one type owns both ends, so the format cannot drift out from under the
/// reader (`report_confidence_survives_the_round_trip` pins that). Returns None
/// when there is no such line or it carries no digits, which is deliberately
/// distinguishable from a real `0.00`.
pub fn parse_report_confidence(report: &str) -> Option<f64> {
    for line in report.lines() {
        let t = line.trim().trim_start_matches("- ").trim();
        if let Some(rest) = strip_label(t, "review confidence") {
            if !rest.chars().any(|c| c.is_ascii_digit()) {
                return None;
            }
            return Some(parse_confidence(rest).clamp(0.0, 1.0));
        }
    }
    None
}

/// THE SHARED CONFIDENCE GATE — one implementation, two callers.
///
/// The daemon applies [`meets_confidence_floor`] when it decides whether to
/// propose at all; `scripts/apply_heal.sh` applies THIS to the report.md of the
/// proposal it is about to install, through `darwind --heal-confidence`. A
/// proposal written by an older daemon (or by hand) therefore cannot be applied
/// under a weaker bar than the one that would have blocked it being written.
/// Returns the verdict and the sentence a human reads.
pub fn confidence_gate(report: &str) -> (ConfidenceGate, String) {
    match parse_report_confidence(report) {
        None => (
            ConfidenceGate::NoScore,
            "report.md carries no `review confidence` line — there is no adversarial review \
             to stand behind this patch, and this gate does not guess one"
                .to_string(),
        ),
        Some(c) if meets_confidence_floor(c) => (
            ConfidenceGate::Above,
            format!("review confidence {c:.2} clears the {:.2} floor", confidence_floor()),
        ),
        Some(c) => (
            ConfidenceGate::Below,
            format!(
                "review confidence {c:.2} is BELOW the {:.2} floor — the \
                 adversarial reviewer did not back this patch, so it is not installed by a \
                 one-click apply",
                confidence_floor()
            ),
        ),
    }
}

/// Selection policy (v2): among PASSED candidates, prefer the HIGHEST review
/// confidence; break ties toward the MINIMAL patch (smallest add/remove count).
/// Returns the index into `survivors` of the winner, or None when empty. Pure
/// so the rule is unit-tested without the cloud.
///
/// SELECTING A WINNER IS NOT THE SAME AS HAVING ONE WORTH PROPOSING: this
/// function answers "which is best", and `meets_confidence_floor` answers
/// "is best good enough". Both are needed — the best of three patches the
/// reviewer disbelieved is still a patch the reviewer disbelieved.
fn select_winner(survivors: &[Survivor]) -> Option<usize> {
    survivors
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| {
            a.confidence
                .partial_cmp(&b.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
                // Higher confidence wins; on a tie, SMALLER size wins, so
                // reverse the size comparison.
                .then(b.size.cmp(&a.size))
        })
        .map(|(i, _)| i)
}

// ---------------------------------------------------------------------------
// Artifact rendering — pure, unit-tested
// ---------------------------------------------------------------------------

/// report.md for a v2 proposal: diagnosis, chosen diff, validation tail,
/// review verdict + confidence, and the EXACT apply command.
fn render_report(ts: u64, model: &str, d: &Diagnosis, winner: &Survivor) -> String {
    let files = if winner.files.is_empty() {
        "(none parsed from the diff)".to_string()
    } else {
        winner.files.join(", ")
    };
    let sigs = if d.signatures.is_empty() {
        "(none extracted)".to_string()
    } else {
        d.signatures.join("\n  - ")
    };
    format!(
        "# Self-heal proposal — {ts}\n\n\
         - verdict: VALIDATED (cargo check + clippy -D warnings + cargo test + mutation probe)\n\
         - responsiveness: {responsiveness}\n\
         - model: {model}\n\
         - subsystem: {subsystem}\n\
         - files touched: {files}\n\
         - chosen candidate: #{index}\n\
         - review confidence: {confidence:.2} (floor {floor:.2} — cleared)\n\
         - adversarial reviewer said: {verdict_line}\n\n\
         ## Diagnosis\n\n\
         - signature(s):\n  - {sigs}\n\
         - cited line numbers: {lines}\n\n\
         ## Chosen diff\n\n```diff\n{diff}```\n\n\
         ## Adversarial review verdict\n\n{verdict}\n\n\
         ## Validation output (tail)\n\n```\n{validation_tail}\n```\n\n\
         ## To apply\n\n\
         This patch was validated in a STAGING copy only; the live daemon/ is untouched.\n\
         Review the diff above, then apply it with:\n\n\
         ```\nscripts/apply_heal.sh {ts}\n```\n",
        subsystem = d.subsystem,
        // VALIDATED says the patch is SOUND. This says whether it is an ANSWER.
        // The four staged gates never look at the diagnosis, so a patch that
        // fixes something else entirely reaches this report with the same
        // "VALIDATED" on the line above.
        responsiveness = responsiveness(d, &winner.diff).1,
        index = winner.index,
        confidence = winner.confidence,
        floor = confidence_floor(),
        // THE HEADER, NOT ONLY THE SECTION 60 LINES DOWN. A reader skimming the
        // top of report.md (and the HUD, which shows its first lines) got a bare
        // number with no bar beside it and no word from the only stage that
        // judged whether the patch is a good IDEA.
        verdict_line = first_chars(winner.review_verdict.trim(), 200),
        sigs = sigs,
        lines = if d.line_numbers.is_empty() {
            "(none)".to_string()
        } else {
            d.line_numbers
                .iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        },
        diff = winner.diff,
        verdict = winner.review_verdict,
        validation_tail = winner.validation_tail,
    )
}

/// A short report.md for a fully-rejected attempt (no candidate validated).
fn render_rejection_report(ts: u64, model: &str, d: &Diagnosis, summary: &str) -> String {
    let sigs = if d.signatures.is_empty() {
        "(none extracted)".to_string()
    } else {
        d.signatures.join("\n  - ")
    };
    format!(
        "# Self-heal REJECTED — {ts}\n\n\
         - verdict: REJECTED (no candidate passed every gate)\n\
         - model: {model}\n\
         - subsystem: {subsystem}\n\n\
         ## Diagnosis\n\n- signature(s):\n  - {sigs}\n\n\
         ## Why every candidate was discarded\n\n{summary}\n",
        subsystem = d.subsystem,
    )
}

/// candidates.md: every candidate diff with why it was kept or discarded.
fn render_candidates_md(outcomes: &[CandidateOutcome]) -> String {
    let mut out = String::from("# Self-heal candidates\n\n");
    for o in outcomes {
        out.push_str(&format!(
            "## Candidate #{index} — {verdict}\n\n{detail}\n\n```diff\n{diff}```\n\n",
            index = o.index,
            verdict = o.verdict_label(),
            detail = o.detail,
            diff = o.diff,
        ));
    }
    out
}

/// review.md: the chosen candidate's adversarial review verdict + confidence,
/// against the floor it had to clear (a bare score with no bar beside it is not
/// something an operator can act on).
fn render_review_md(winner: &Survivor) -> String {
    format!(
        "# Adversarial self-review — chosen candidate #{index}\n\n\
         - confidence: {confidence:.2} (floor {floor:.2}: {stance})\n\
         - reviewed: {reviewed}\n\n## Verdict\n\n{verdict}\n",
        index = winner.index,
        confidence = winner.confidence,
        floor = confidence_floor(),
        stance = if meets_confidence_floor(winner.confidence) { "cleared" } else { "BELOW" },
        reviewed = if winner.reviewed {
            "yes"
        } else {
            "NO — the review call failed, so 0.00 is the absence of a review, not a verdict"
        },
        verdict = winner.review_verdict,
    )
}

// ---------------------------------------------------------------------------
// Pipeline (impure half)
// ---------------------------------------------------------------------------

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One candidate's fate, for candidates.md.
struct CandidateOutcome {
    index: usize,
    diff: String,
    /// "validated" | "rejected"
    validated: bool,
    /// e.g. "kept (review confidence 0.82)", "discarded at cargo test".
    detail: String,
}

impl CandidateOutcome {
    fn verdict_label(&self) -> &'static str {
        if self.validated {
            "VALIDATED"
        } else {
            "DISCARDED"
        }
    }
}

async fn run_pipeline(
    root: &Path,
    cfg: &Config,
    memory: &Memory,
    brain: &dyn HealBrain,
    scan: &LogScan,
) {
    let ts = now_secs();
    // SEED THE TWO TUNABLES FROM CONFIG, once, before anything reads them.
    // `configure` is first-wins, so the numbers cannot move mid-attempt, and the
    // `--heal-confidence` CLI seeds them from the SAME file — the daemon's
    // propose-side bar and apply_heal.sh's install-side bar stay one number.
    configure(&cfg.self_heal);
    // LOCKDOWN OVERLAY (task #12): self-heal is autonomy, so it is FORCED off
    // while the emergency stop is engaged — the enabled bit is ANDed with
    // `!is_locked_down()`, so the pure `heal_action` returns Disabled and the
    // pipeline exits before any cloud drafting. `heal_action` itself stays pure
    // (the global read lives here, at the one live call site). With lockdown OFF
    // this is byte-for-byte the configured `[self_heal].enabled`.
    let enabled = cfg.self_heal.enabled && !crate::lockdown::is_locked_down();
    let action = heal_action(enabled, &cfg.self_heal.mode);
    if action == HealAction::Disabled {
        return; // caller already gates; belt and braces
    }

    // Rate limit BEFORE any cloud work: one draft attempt per 6h.
    let last = match memory.get_fact(META_HEAL_LAST_ATTEMPT).await {
        Ok(last) => last,
        Err(e) => {
            // Conservative: broken bookkeeping must not unleash unmetered
            // cloud drafting.
            warn!(error = %e, "heal: cannot read the attempt stamp; skipping this episode");
            return;
        }
    };
    if !attempt_allowed(last.as_deref(), ts) {
        info!("heal: rate-limited (one draft attempt per 6h); skipping this episode");
        telemetry::emit("system", "heal.blocked", json!({"reason": "rate_limited", "ts": ts}));
        return;
    }

    // Drafting needs the cloud: no key, no pipeline.
    if anthropic::resolve_api_key().await.is_none() {
        warn!("heal: triggered but no Anthropic API key is available; cannot draft a patch");
        telemetry::emit("system", "heal.blocked", json!({"reason": "no_api_key", "ts": ts}));
        return;
    }

    // Stamp the attempt right before any cloud call so failed attempts count
    // toward the limit too (each one is a paid cloud call).
    if let Err(e) = memory.upsert_fact(META_HEAL_LAST_ATTEMPT, &ts.to_string()).await {
        // FAIL-SAFE (mirrors forge_gap): if the attempt stamp cannot be persisted
        // we CANNOT enforce the one-draft-per-6h rate limit, so we must NOT make
        // the paid cloud draft call — broken bookkeeping must never unleash
        // unmetered drafting (the conservative rule above). Skip this episode.
        warn!(error = %e, "heal: failed to stamp the attempt time; skipping to avoid unmetered drafting");
        return;
    }

    let daemon_dir = root.join("daemon");
    let heal_root = root.join("state").join("heal");
    match run_attempt(&daemon_dir, &heal_root, ts, &cfg.cloud.heavy_model, brain, scan).await {
        AttemptResult::Proposed { diff, report, files, confidence, calibration, responsiveness, extra } => match action {
            HealAction::Propose => {
                // Pass `extra` THROUGH. It used to be swallowed by the `..` in this
                // destructuring pattern and the no-extra `propose` wrapper hard-coded
                // `None`, so diagnosis.json / candidates.md / review.md were computed,
                // carried out of run_attempt, and then thrown away on the ONLY path
                // that ships (mode="propose" is the default). The operator reviewing a
                // real proposal got patch.diff + report.md and nothing else, while the
                // module doc, docs/ARCHITECTURE.md and the HUD's SelfHealPanel all
                // promised five files — so the alternative candidates and their
                // per-gate fates, the entire point of the multi-candidate v2 design,
                // were invisible. The only writer of those three files was the
                // #[ignore]d cloud drill and the REJECTED path.
                propose(
                    memory,
                    &heal_root,
                    ts,
                    &diff,
                    &report,
                    &files,
                    confidence,
                    responsiveness,
                    &calibration,
                    &extra,
                )
                .await;
                // CHANGE QUEUE (changeq.rs): ALSO register this propose-only artifact
                // into the unified git-native review lane. Pure bookkeeping — the
                // validated patch was already written to state/heal/proposals/<ts>/;
                // this mirrors it into the queue (and, on-device, onto darwin/changeq)
                // with secret-free provenance. It changes NOTHING about the
                // propose-only contract; apply still routes to scripts/apply_heal.sh.
                crate::changeq::on_proposal(
                    crate::changeq::ChangeKind::Heal,
                    ts,
                    crate::changeq::Provenance::new(
                        "self-heal",
                        cfg.cloud.heavy_model.clone(),
                        ts.to_string(),
                        crate::changeq::fingerprint(diff.as_bytes()),
                    ),
                    format!(
                        "validated patch, {} file{}, review confidence {confidence:.2}, \
                         responsiveness {responsiveness}",
                        files.len(),
                        if files.len() == 1 { "" } else { "s" }
                    ),
                );
            }
            HealAction::Auto => {
                // SAFETY SNAPSHOT (snapshot.rs): anchor an APFS restore point
                // BEFORE the validated diff is applied to the live daemon/, so a
                // later "undo that" can name a concrete OS-level rollback target.
                // Additive-benign (a COW marker; writes/deletes none of the user's
                // data) and armed by default; a non-APFS/no-space/no-permission
                // volume degrades to an honest would-have and changes nothing. It
                // NEVER rolls back on its own — auto_apply still applies exactly as
                // before; the snapshot is only a recorded restore point.
                crate::snapshot::anchor_before(crate::snapshot::Reason::HealApply, cfg).await;
                auto_apply(&daemon_dir, &heal_root, ts, &diff, &report).await;
            }
            HealAction::Disabled => unreachable!("gated above"),
        },
        AttemptResult::Rejected { stage, diff, report, calibration } => {
            warn!(stage, "heal: all candidates rejected");
            // record a best-effort patch.diff (last attempted) + report for audit.
            let dir = heal_root.join("rejected");
            record_artifact(&dir, ts, "patch.diff", &diff);
            record_artifact(&dir, ts, "report.md", &report);
            telemetry::emit(
                "system",
                "heal.rejected",
                json!({"ts": ts, "stage": stage, "calibration": calibration.to_json()}),
            );
        }
        AttemptResult::Aborted { stage } => {
            warn!(stage, "heal: attempt aborted (no verdict on any patch)");
            // An abort has no per-candidate data (nothing was staged or reviewed),
            // but it STILL reports the settings in force -- otherwise the one
            // event class that means "the pipeline could not even start" is also
            // the one that hides which configuration it could not start under.
            telemetry::emit(
                "system",
                "heal.rejected",
                json!({
                    "ts": ts,
                    "stage": stage,
                    "calibration": Calibration::default().to_json(),
                }),
            );
        }
    }
}

/// The full v2 attempt, factored out so the --heal-drill reuses it verbatim
/// against a planted-fault crate. `daemon_dir` is the crate to heal (the live
/// daemon/ for the watchdog; a throwaway temp crate for the drill); `heal_root`
/// is where staging dirs and artifacts go. NEVER applies to `daemon_dir`.
async fn run_attempt(
    daemon_dir: &Path,
    heal_root: &Path,
    ts: u64,
    model: &str,
    brain: &dyn HealBrain,
    scan: &LogScan,
) -> AttemptResult {
    // (3) Diagnosis. build_diagnosis is pure; attach the current contents of
    // the cited source files (impure IO, confined to <crate_dir>/src) so the
    // drafter can produce a hunk whose context matches the tree and applies
    // cleanly. This strengthens drafting only — every staged gate is unchanged.
    let mut diagnosis = build_diagnosis(scan);
    attach_source_excerpts(&mut diagnosis, daemon_dir);
    info!(
        subsystem = %diagnosis.subsystem,
        files = ?diagnosis.files,
        "heal: diagnosed the burst"
    );
    telemetry::emit(
        "system",
        "heal.diagnosing",
        json!({
            "signature": diagnosis.primary_signature(),
            "files": diagnosis.files,
            "subsystem": diagnosis.subsystem,
        }),
    );

    // (4) Multi-candidate draft.
    let raw = match brain.draft_candidates(&diagnosis, CANDIDATE_COUNT).await {
        Ok(raw) => raw,
        Err(e) => {
            warn!(error = %e, "heal: draft call failed");
            return AttemptResult::Aborted { stage: "draft" };
        }
    };
    let candidate_diffs = split_candidate_diffs(&raw);
    if candidate_diffs.is_empty() {
        warn!("heal: the model returned no usable unified diff");
        let report = render_rejection_report(
            ts,
            model,
            &diagnosis,
            "The model returned no parseable unified diff in any candidate.",
        );
        return AttemptResult::Rejected {
            stage: "draft",
            diff: tail_chars(&raw, REPORT_TAIL_CHARS),
            report,
            calibration: Calibration::default(),
        };
    }

    // (5) Stage + validate EACH candidate independently (gates unchanged).
    let mut survivors: Vec<Survivor> = Vec::new();
    let mut outcomes: Vec<CandidateOutcome> = Vec::new();
    let mut last_stage = "patch";
    // Deadline exhaustion and a real gate failure are OPPOSITE messages to the
    // operator; counted separately so the rejection report can tell them apart.
    let mut deadline_hits = 0usize;
    let mut merit_rejects = 0usize;
    // THE ATTEMPT-WIDE CEILING. Each candidate used to get its own independent
    // budget, so an attempt could occupy every core for CANDIDATE_COUNT times
    // that — on a machine that is misbehaving, which is the only time this code
    // runs at all. One clock for the whole attempt: a candidate may take up to
    // VALIDATE_TIMEOUT, but only out of what is left of attempt_budget().
    let mut attempt_spent = Duration::ZERO;
    // CALIBRATION: what this attempt cost and what it judged. Accumulated across
    // every candidate (including the rejected ones -- those are the informative
    // ones for the budget) and shipped on whichever terminal event fires.
    let mut calib = Calibration { drafted: candidate_diffs.len(), ..Calibration::default() };
    for (i, diff) in candidate_diffs.iter().enumerate() {
        let files = extract_source_files(diff);
        let budget = candidate_budget(attempt_budget().saturating_sub(attempt_spent), VALIDATE_TIMEOUT);
        if budget < minimum_viable_budget() {
            // Counted, not just refused: a nonzero `candidates_unaffordable` is
            // the one number that says "your attempt_budget_secs is too small for
            // this machine" without anyone having to read a rejection report.
            calib.unaffordable += 1;
        }
        let started = tokio::time::Instant::now();
        let staged = stage_and_validate(
            daemon_dir,
            heal_root,
            CandidateRef { ts, index: i },
            diff,
            &diagnosis,
            budget,
            &mut calib.stages,
        )
        .await;
        // CHARGE THE MACHINE, NOT THE NETWORK.
        //
        // This accumulates only what the STAGED VALIDATION used. A wall-clock
        // deadline across the whole loop also charged the adversarial review
        // below — a cloud call with its own REVIEW_TIMEOUT of up to 180s — to a
        // budget whose entire purpose is stopping one heal from occupying every
        // core. At the measured 523s cycle that left candidate 3 exactly 4s of
        // slack (1800 - 2 x 523 - 750), so any real review latency refused it,
        // and refused it as `deadline` with "the attempt-wide budget was already
        // spent by the earlier candidates" — a statement about the machine, for
        // something the machine did not do.
        attempt_spent = attempt_spent.saturating_add(started.elapsed());
        calib.spent_secs = attempt_spent.as_secs();
        match staged {
            Ok(StageResult::Validated { validation_tail }) => {
                // (6) Adversarial review of this survivor.
                let (verdict, confidence, reviewed) =
                    match brain.review(&diagnosis, diff, &validation_tail).await {
                        Ok(raw) => {
                            let (v, c) = parse_review(&raw);
                            (v, c, true)
                        }
                        Err(e) => {
                            warn!(error = %e, candidate = i + 1, "heal: review call failed; \
                                 treating as zero-confidence");
                            // NOT the same fact as "the reviewer scored it 0.0",
                            // and the confidence floor's report must not say it
                            // was. Carried as `reviewed: false`.
                            ("review call failed".to_string(), 0.0, false)
                        }
                    };
                calib.reviews.push(json!({
                    "candidate": i + 1,
                    "confidence": confidence,
                    "reviewed": reviewed,
                }));
                outcomes.push(CandidateOutcome {
                    index: i + 1,
                    diff: diff.clone(),
                    validated: true,
                    detail: format!(
                        "kept — passed cargo check + clippy + cargo test + mutation probe; \
                         review confidence {confidence:.2}"
                    ),
                });
                survivors.push(Survivor {
                    index: i + 1,
                    diff: diff.clone(),
                    files,
                    validation_tail,
                    review_verdict: verdict,
                    confidence,
                    reviewed,
                    size: diff_size(diff),
                });
            }
            Ok(StageResult::Rejected { stage, detail }) => {
                last_stage = stage;
                if stage == "deadline" {
                    deadline_hits += 1;
                } else {
                    merit_rejects += 1;
                }
                outcomes.push(CandidateOutcome {
                    index: i + 1,
                    diff: diff.clone(),
                    validated: false,
                    detail: format!(
                        "discarded at {stage}:\n```\n{}\n```",
                        tail_chars(&detail, 1200)
                    ),
                });
            }
            Err(e) => {
                warn!(error = %e, candidate = i + 1, "heal: staging infrastructure failed");
                merit_rejects += 1;
                outcomes.push(CandidateOutcome {
                    index: i + 1,
                    diff: diff.clone(),
                    validated: false,
                    detail: format!("discarded: staging infrastructure error: {e}"),
                });
            }
        }
    }

    let candidates_md = render_candidates_md(&outcomes);

    // (7) Select the winner.
    let Some(win_idx) = select_winner(&survivors) else {
        warn!("heal: no candidate passed validation");
        let report = render_rejection_report(
            ts,
            model,
            &diagnosis,
            &rejection_summary(deadline_hits, merit_rejects),
        );
        // Persist candidates.md alongside the rejection so the human can see
        // what was tried.
        let dir = heal_root.join("rejected");
        record_artifact(&dir, ts, "candidates.md", &candidates_md);
        record_artifact(&dir, ts, "diagnosis.json", &diagnosis_json(&diagnosis));
        return AttemptResult::Rejected {
            stage: last_stage,
            diff: candidate_diffs
                .last()
                .cloned()
                .unwrap_or_default(),
            report,
            calibration: calib,
        };
    };
    let winner = survivors[win_idx].clone();

    // (7b) THE CONFIDENCE FLOOR. Every gate above is mechanical and blind to
    // whether the patch is a good IDEA; the adversarial reviewer is the only
    // stage that judges that, and until now its verdict could not stop anything
    // — `select_winner` returned the best of the survivors and the best of three
    // patches the reviewer disbelieved was proposed with an ACCEPT & APPLY
    // button beside it. Below the floor nothing is proposed. Nothing is lost:
    // the diffs, the reviews and the diagnosis are written under
    // state/heal/rejected/<ts>/ for anyone who wants to apply one by hand.
    if !meets_confidence_floor(winner.confidence) {
        warn!(
            confidence = winner.confidence,
            floor = confidence_floor(),
            reviewed = winner.reviewed,
            "heal: the best candidate is below the review-confidence floor; proposing nothing"
        );
        let summary = below_floor_summary(&winner, survivors.len());
        let report = render_rejection_report(ts, model, &diagnosis, &summary);
        let dir = heal_root.join("rejected");
        record_artifact(&dir, ts, "candidates.md", &candidates_md);
        record_artifact(&dir, ts, "diagnosis.json", &diagnosis_json(&diagnosis));
        record_artifact(&dir, ts, "review.md", &render_review_md(&winner));
        return AttemptResult::Rejected {
            stage: "confidence",
            diff: winner.diff.clone(),
            report,
            calibration: calib,
        };
    }

    let responsiveness_word = responsiveness(&diagnosis, &winner.diff).0.word();
    let report = render_report(ts, model, &diagnosis, &winner);
    let review_md = render_review_md(&winner);
    let diagnosis_json = diagnosis_json(&diagnosis);

    AttemptResult::Proposed {
        diff: winner.diff.clone(),
        report,
        files: winner.files.clone(),
        confidence: winner.confidence,
        calibration: calib,
        responsiveness: responsiveness_word,
        extra: ProposalArtifacts {
            diagnosis_json,
            candidates_md,
            review_md,
        },
    }
}

/// Outcome of one heal attempt, decoupled from how it is acted on (propose,
/// auto, drill).
enum AttemptResult {
    Proposed {
        diff: String,
        report: String,
        files: Vec<String>,
        confidence: f64,
        /// Per-attempt CALIBRATION (see [`Calibration`]) -- every candidate's
        /// review score and every stage's real wall time, so the two tunables
        /// can be set from measurement instead of judgement.
        calibration: Calibration,
        /// Whether the chosen patch actually answers the diagnosis
        /// (`Responsiveness::word()`). Carried out so the propose path can put
        /// it on the wire — the gate chain itself is blind to it.
        responsiveness: &'static str,
        extra: ProposalArtifacts,
    },
    /// A model/patch/validation failure — we have a verdict (no candidate is
    /// good), so it is recorded as a rejection.
    Rejected {
        stage: &'static str,
        diff: String,
        report: String,
        /// Carried on rejections TOO, and that is the important half: a
        /// `deadline` rejection is the machine telling you the budget is wrong,
        /// and a `confidence` rejection is the reviewer telling you the floor
        /// may be. Both are invisible without the numbers.
        calibration: Calibration,
    },
    /// Infrastructure trouble before any verdict (draft call failed). No
    /// statement about any patch.
    Aborted { stage: &'static str },
}

/// The supplementary proposal artifacts written alongside patch.diff/report.md.
struct ProposalArtifacts {
    diagnosis_json: String,
    candidates_md: String,
    review_md: String,
}

fn diagnosis_json(d: &Diagnosis) -> String {
    serde_json::to_string_pretty(d).unwrap_or_else(|_| "{}".to_string())
}

/// (8a) Propose: artifacts + meta.heal_pending + heal.proposal. The
/// first-contact brief reads meta.heal_pending, so DARWIN tells the user.
///
/// `extra` carries the v2 supplementary artifacts (diagnosis.json /
/// candidates.md / review.md) and is REQUIRED, not `Option`.
///
/// WHAT WENT WRONG WHEN IT WAS OPTIONAL: a no-extra `propose` wrapper sat in
/// front of this function hard-coding `None`, and run_pipeline's Propose arm
/// destructured `AttemptResult::Proposed { .., .. }` — the `..` swallowed
/// `extra`. So on the ONLY path that ships (mode="propose" is the default) the
/// three v2 artifacts were computed, carried out of run_attempt, and thrown away:
/// the operator reviewing a real proposal saw patch.diff + report.md while
/// heal.rs's own module doc, docs/ARCHITECTURE.md and hud SelfHealPanel all named
/// five files. Making the parameter non-optional means that regression can no
/// longer compile. The drill and the rejected path write their own artifact sets
/// directly and never call this.
#[allow(clippy::too_many_arguments)]
async fn propose(
    memory: &Memory,
    heal_root: &Path,
    ts: u64,
    diff: &str,
    report: &str,
    files: &[String],
    confidence: f64,
    responsiveness: &str,
    calibration: &Calibration,
    extra: &ProposalArtifacts,
) {
    let dir = heal_root.join("proposals");
    if !write_proposal_artifacts(&dir, ts, diff, report, extra) {
        return; // already warned
    }
    if let Err(e) = memory.upsert_fact(META_HEAL_PENDING, &ts.to_string()).await {
        warn!(error = %e, "heal: proposal written but meta.heal_pending could not be stamped");
    }
    info!(ts, confidence, "heal: validated proposal written; apply with scripts/apply_heal.sh");
    telemetry::emit(
        "system",
        "heal.proposal",
        json!({
            "ts": ts,
            "files": files,
            "validated": true,
            "confidence": confidence,
            // THE BAR THE SCORE HAD TO CLEAR, sent with the score. Without it the
            // HUD had to hard-code a second copy of the floor to render "is this
            // a good number?", and two copies of a threshold drift. The daemon
            // owns the number; the HUD renders what it is told.
            "confidence_floor": confidence_floor(),
            // VALIDATED IS NOT RESPONSIVE. Four gates prove the patch compiles,
            // lints, passes and that its test bites; none of them prove it
            // addresses the burst. The HUD's Accept button needs both words.
            "responsiveness": responsiveness,
            // CALIBRATION -- see `Calibration`. `confidence` above is the
            // WINNER's score (what the gauge renders); this carries EVERY
            // candidate's, including the losers, plus what each stage actually
            // cost. The floor and the budget are set from this, not from taste.
            "calibration": calibration.to_json(),
        }),
    );
}

/// Write the FIVE files a propose-mode proposal is documented to contain into
/// `<proposals>/<ts>/`: patch.diff, report.md, diagnosis.json, candidates.md and
/// review.md. Returns false (already warned) when the patch itself could not be
/// written, which is the only case the caller aborts on. Store-free so the
/// artifact set is directly unit-testable.
fn write_proposal_artifacts(
    dir: &Path,
    ts: u64,
    diff: &str,
    report: &str,
    extra: &ProposalArtifacts,
) -> bool {
    if record_artifact(dir, ts, "patch.diff", diff).is_none() {
        return false;
    }
    record_artifact(dir, ts, "report.md", report);
    record_artifact(dir, ts, "diagnosis.json", &extra.diagnosis_json);
    record_artifact(dir, ts, "candidates.md", &extra.candidates_md);
    record_artifact(dir, ts, "review.md", &extra.review_md);
    true
}

/// (8b) Auto: apply the validated diff to the REAL daemon/, rebuild release,
/// emit heal.applied, then exit cleanly. UNCHANGED from v1 — no NEW
/// live-auto-apply path. Under launchd KeepAlive the exit is a restart into
/// the new binary; under `cargo run` it is a stop.
async fn auto_apply(daemon_dir: &Path, heal_root: &Path, ts: u64, diff: &str, report: &str) {
    let dir = heal_root.join("applied");
    record_artifact(&dir, ts, "patch.diff", diff); // audit trail
    record_artifact(&dir, ts, "report.md", report);
    match apply_patch(daemon_dir, diff).await {
        Ok(out) if out.ok => {}
        Ok(out) => {
            warn!(output = %tail_chars(&out.output, 800), "heal: auto-apply patch failed on the live tree");
            telemetry::emit("system", "heal.rejected", json!({"ts": ts, "stage": "apply"}));
            return;
        }
        Err(e) => {
            warn!(error = %e, "heal: auto-apply could not run patch");
            telemetry::emit("system", "heal.rejected", json!({"ts": ts, "stage": "apply"}));
            return;
        }
    }
    match run_cargo(daemon_dir, &["build", "--release"], VALIDATE_TIMEOUT).await {
        Ok(out) if out.ok => {}
        Ok(out) => {
            warn!(output = %tail_chars(&out.output, 800), "heal: release rebuild failed after auto-apply");
            telemetry::emit("system", "heal.rejected", json!({"ts": ts, "stage": "build"}));
            return;
        }
        Err(e) => {
            warn!(error = %e, "heal: release rebuild could not run");
            telemetry::emit("system", "heal.rejected", json!({"ts": ts, "stage": "build"}));
            return;
        }
    }
    telemetry::emit("system", "heal.applied", json!({"ts": ts}));
    info!(
        ts,
        "heal: patch applied and rebuilt; exiting for a clean restart (launchd KeepAlive \
         restarts darwind into the new binary; under `cargo run` this is a stop)"
    );
    // Give the telemetry hub a beat to flush heal.applied to the HUD.
    tokio::time::sleep(Duration::from_millis(500)).await;
    std::process::exit(0);
}

/// Write `<dir_root>/<ts>/<name>` with `body`. Returns the file's directory on
/// success (so a missing dir is created once and reused for sibling files).
fn record_artifact(dir_root: &Path, ts: u64, name: &str, body: &str) -> Option<PathBuf> {
    let dir = dir_root.join(ts.to_string());
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join(name), body)?;
        Ok(())
    };
    match write() {
        Ok(()) => Some(dir),
        Err(e) => {
            warn!(error = %e, dir = %dir.display(), name, "heal: failed to write artifact");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// (5) Staging + validation (real patch, real cargo — exercised in tests
// against a synthetic crate in a tempdir, NEVER against the real daemon/).
// REUSED UNCHANGED from v1; the only difference is the per-candidate staging
// dir name (staging-<ts>-c<i>).
// ---------------------------------------------------------------------------

/// How staging ended: a verdict on the patch, or Err for infrastructure
/// trouble (copy failed, spawn failed) that says nothing about the patch.
#[derive(Debug)]
enum StageResult {
    Validated { validation_tail: String },
    Rejected { stage: &'static str, detail: String },
}

/// Output of one child process: combined stdout+stderr and its success bit.
struct CmdOutput {
    ok: bool,
    output: String,
}

/// Copy the crate sources (src/, Cargo.toml, Cargo.lock if present — NOT
/// target/) into the staging dir, apply the diff with patch -p1 --batch
/// (reject on any hunk failure), then check + clippy + test + mutation under the
/// `budget` the caller hands down: at most VALIDATE_TIMEOUT, and only out of the
/// single attempt budget every candidate of the attempt shares. (This read "one
/// 10-minute deadline" for as long as the constant was 600s and per-candidate.
/// Neither of those is true any more.)
/// WHICH candidate of WHICH attempt is being staged.
///
/// Bundled rather than passed as two positional scalars for two reasons: it
/// keeps the staging entry points under clippy's argument ceiling (which the
/// calibration sink pushed them over), and these two values name the staging
/// directory together (`staging-<ts>-c<i>`) -- a pair that must always travel
/// together is a struct, not an argument-order convention.
#[derive(Clone, Copy)]
struct CandidateRef {
    ts: u64,
    /// ZERO-BASED. The operator-facing candidate number is `index + 1`
    /// everywhere it is reported; only the staging path uses the raw index.
    index: usize,
}

/// Wrapper that guarantees the staging tree is REMOVED on every exit.
///
/// stage_and_validate creates state/heal/staging-<ts>-c<i>/ per candidate and copies
/// daemon/src + Cargo.toml + Cargo.lock into it — three per pass, on a loop that runs
/// unattended. Nothing ever deleted them, so the autonomy path grew the state dir
/// without bound. The artifacts that matter (the diff and the captured validation
/// tail) are already carried out in StageResult, so the tree itself is pure scratch.
///
/// It has several early returns, hence a wrapper rather than a cleanup line per exit:
/// a new `return` cannot leak a tree.
async fn stage_and_validate(
    source_dir: &Path,
    heal_root: &Path,
    cand: CandidateRef,
    diff: &str,
    diagnosis: &Diagnosis,
    budget: Duration,
    timings: &mut Vec<serde_json::Value>,
) -> anyhow::Result<StageResult> {
    // NOT ENOUGH TIME LEFT IN THE ATTEMPT TO REACH A VERDICT. Say so before
    // copying a crate tree and applying a patch: staging costs disk and IO on a
    // machine that is already misbehaving, and every gate below would then be
    // handed a slice too small to finish, producing the same rejection with more
    // work done. Reported as `deadline`, never as a verdict on the patch.
    if budget < minimum_viable_budget() {
        return Ok(budget_exhausted("check", "", None, budget, BudgetStop::Attempt));
    }
    let staging = heal_root.join(staging_dir_name(cand.ts, cand.index));
    let out =
        stage_and_validate_inner(source_dir, heal_root, cand, diff, diagnosis, budget, timings)
            .await;
    // Best-effort: a tree we could not remove is wasted disk, never a broken heal.
    if let Err(e) = tokio::fs::remove_dir_all(&staging).await {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!(path = %staging.display(), error = %e, "heal: could not remove the staging tree");
        }
    }
    out
}

const UNRUNNABLE_IN_STAGE: &[&str] = &[
    "forge::tests::apply_forge_accepts_legit_multiline_manifest",
    "forge::tests::apply_forge_refuses_multiline_overbroad_manifests",
    "heal::tests::full_pipeline_via_mock_brain_rejects_when_no_candidate_validates",
];

async fn stage_and_validate_inner(
    source_dir: &Path,
    heal_root: &Path,
    cand: CandidateRef,
    diff: &str,
    diagnosis: &Diagnosis,
    budget: Duration,
    // CALIBRATION SINK. Every cargo stage that actually RUNS appends its real
    // wall time here (see `record_stage_timing`). This is the whole point of the
    // exercise: `attempt_budget_secs` was picked by judgement, and the only way
    // to replace judgement with measurement is for each attempt to report what
    // the cycle cost ON THIS MACHINE. Passed by &mut rather than returned in
    // StageResult so the timings survive a candidate that is REJECTED -- the
    // rejected runs are the ones that tell you the budget is too small.
    timings: &mut Vec<serde_json::Value>,
) -> anyhow::Result<StageResult> {
    let staging = heal_root.join(staging_dir_name(cand.ts, cand.index));
    // `staging` is a miniature REPO ROOT; the crate itself lands one level down.
    // Both `patch -p1` (whose headers are `a/src/...`) and cargo run against the
    // CRATE dir, not the staging root.
    let crate_dir = stage_sources(source_dir, &staging)?;

    let patched = apply_patch(&crate_dir, diff).await?;
    if !patched.ok {
        return Ok(StageResult::Rejected {
            stage: "patch",
            detail: patched.output,
        });
    }

    let deadline = tokio::time::Instant::now() + budget;
    let mut combined = String::new();
    // TESTS THAT CANNOT RUN IN A STAGE, skipped BY NAME and stated in the report.
    //
    // Three of the crate's tests stage-and-build a tree of their own — the two
    // `apply_forge` tests execute scripts/apply_forge.sh (which cd's into a full
    // repo layout), and the heal pipeline test runs this very staging routine
    // nested inside a stage. They pass in the real tree and fail in a stage for
    // reasons that have nothing to do with the candidate patch.
    //
    // Counting them as failures made the gate reject EVERY candidate and report
    // "No candidate passed the staged cargo check + cargo test gates" — which an
    // operator reads as "the model drafted three bad patches", not "three tests
    // cannot run here". Skipping them SILENTLY would be the other failure: a gate
    // that quietly stops covering things is how a real regression walks through.
    // So they are named, skipped, and the skip is in the transcript.
    // `--skip` belongs to the TEST HARNESS, not to cargo, so it goes after `--`.
    // Without the separator cargo answers "unexpected argument '--skip' found" and
    // the gate rejects every candidate for a reason that is not the patch.
    let mut test_args: Vec<&str> = vec!["test", "--"];
    for t in UNRUNNABLE_IN_STAGE {
        test_args.push("--skip");
        test_args.push(t);
    }
    // THE GATE MUST BE AT LEAST AS STRICT AS THE ONE A HUMAN PASSES.
    //
    // It was `check` + `test`. This project's actual merge standard is
    // `cargo clippy --all-targets -- -D warnings` — zero warnings — and a
    // self-heal patch could satisfy check+test, be APPROVED, be APPLIED to live
    // sources, and only then break the gate its author has to pass. The system
    // would be handing its owner a patch it had "validated" and a broken lint run.
    //
    // Ordered clippy BEFORE test: it subsumes `check`, catches the cheap class
    // (unused fields, dead methods, expect-with-format) in a fraction of the test
    // suite's time, and — the reason it matters here — a never-called method in
    // the wrong impl block compiles and passes tests. That exact mistake happened
    // in this file's own capture-teardown work; `-D warnings` was what caught it.
    //
    // `check` is kept as a separate first stage so a plain compile error reports
    // as "check" rather than as a lint failure, which is what the operator needs
    // to read.
    let clippy_args = vec!["clippy", "--all-targets", "--", "-D", "warnings"];
    // Each stage carries the FLOOR it needs. A stage may use everything left
    // except the floors of the stages after it (plus MUTATION_STAGE_FLOOR for
    // the probe, which is not in this table) — so a pathologically slow clippy
    // is stopped at its own share instead of eating the budget and leaving
    // `test`, the stage that actually exercises the patch, three seconds and a
    // `deadline`. The floors never bite a healthy run: measured cold on an M1
    // Pro they are ~2x what each stage actually takes.
    let stages: [(&str, Vec<&str>, Duration); 3] = [
        ("check", vec!["check"], CHECK_STAGE_FLOOR),
        ("clippy", clippy_args, CLIPPY_STAGE_FLOOR),
        ("test", test_args.clone(), TEST_STAGE_FLOOR),
    ];
    for i in 0..stages.len() {
        let (stage, args, _) = &stages[i];
        let (stage, args) = (*stage, args);
        // Everything after this stage still has to run: their floors, plus the
        // mutation probe's.
        let reserved_for_later: Duration = stages[i + 1..]
            .iter()
            .map(|(_, _, floor)| *floor)
            .sum::<Duration>()
            + MUTATION_STAGE_FLOOR;
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            // RUNNING OUT OF TIME IS NOT A VERDICT ON THE PATCH. Filed under the
            // stage name it never reached, this reported as `heal.rejected
            // {stage:"test"}` and rendered as "no candidate passed the gates" —
            // which an operator reads as "the model drafted three bad patches".
            return Ok(budget_exhausted(stage, &combined, None, budget, BudgetStop::Candidate));
        }
        let cap = stage_cap(remaining, budget, reserved_for_later);
        // The cap bit, not the clock: this stage is out of its SHARE while the
        // candidate still has time reserved for the stages behind it.
        let capped = cap < remaining;
        if cap.is_zero() {
            return Ok(budget_exhausted(stage, &combined, None, budget, BudgetStop::StageShare));
        }
        let stage_started = tokio::time::Instant::now();
        let stage_out = run_cargo(&crate_dir, args, cap).await;
        record_stage_timing(timings, cand.index, stage, stage_started.elapsed(), &stage_out);
        match stage_out {
            Ok(out) => {
                combined.push_str(&format!("\n$ cargo {}\n", args.join(" ")));
                combined.push_str(&out.output);
                if !out.ok {
                    return Ok(StageResult::Rejected { stage, detail: combined });
                }
            }
            Err(e) if capped && is_deadline_error(&e) => {
                return Ok(budget_exhausted(
                    stage,
                    &combined,
                    Some(&e),
                    budget,
                    BudgetStop::StageShare,
                ))
            }
            Err(e) => return Ok(stage_failure(stage, &combined, &e, budget)),
        }
    }

    // ---- RESPONSIVENESS (advisory; NEVER rejects) --------------------------
    //
    // The three stages above, and the mutation probe below, are all blind to the
    // DIAGNOSIS. This is the one line in the tail that says whether the patch has
    // anything to do with the burst that triggered it. It is written into
    // `combined`, so it reaches the adversarial reviewer (whose prompt is handed
    // this very tail), report.md and candidates.md — all three from one place.
    combined.push_str(&format!(
        "\n$ responsiveness probe\n{}\n",
        responsiveness(diagnosis, diff).1
    ));

    // ---- STAGE 4: MUTATION PROOF -------------------------------------------
    //
    // The three stages above prove the patch COMPILES, LINTS and PASSES. None of
    // them prove its new test would CATCH THE BUG COMING BACK — and a test that
    // passes against the broken code is the failure this project produces most.
    // In one sweep it happened five separate times: three source-anchored guards
    // that matched their own definition, a mutation that edited a string no
    // longer in the file (a no-op, indistinguishable from a surviving test), and
    // a fix one layer too shallow whose test still went green.
    //
    // So: take the patch's TEST hunks and REVERSE-APPLY everything else. That
    // leaves the new test present and the fix absent — the precise state the
    // test claims to detect. The suite must now FAIL. If it still passes, the
    // test does not demonstrate the defect and the candidate is rejected.
    //
    // The staged tree is not read after this point (only `validation_tail` is),
    // so the reverse patch is applied in place: the build is warm, so this costs
    // an incremental rebuild rather than a second full one.
    let split = split_test_hunks(diff, &|f: &str| cfg_test_boundary(&crate_dir, f));
    let (test_hunks, source_hunks) = match split {
        Ok(pair) => pair,
        Err(e) => (String::new(), format!("__unsplittable__ {e}")),
    };
    let mutation = if source_hunks.starts_with("__unsplittable__") {
        "INCONCLUSIVE: the fix and its test share a hunk and cannot be separated."
    } else if test_hunks.trim().is_empty() {
        // Not a rejection. A doc fix or a pure refactor legitimately adds no
        // test — but the reviewer must see "green" and "proven" as different
        // words, so this says which one it got.
        "UNPROVEN: the patch adds no test, so nothing here shows the fix works."
    } else if source_hunks.trim().is_empty() {
        "N/A: the patch is tests only — there is no fix to take away."
    } else {
        match reverse_patch(&crate_dir, &source_hunks).await {
            // Fix removed, test kept. The suite MUST now fail.
            Ok(r) if r.ok => {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    // No guard lived here, so a zero budget went straight into
                    // `tokio::time::timeout(Duration::ZERO, …)`, which fires
                    // instantly, and the Err arm below returned VALIDATED with
                    // "the probe could not run (… timed out after 0s)". A
                    // never-run probe must not read as a technical hiccup.
                    combined.push_str(
                        "\n$ mutation probe\nINCONCLUSIVE: the staged-validation budget was \
                         exhausted before the probe could run, so this patch is NOT \
                         mutation-proven.\n",
                    );
                    return Ok(StageResult::Validated {
                        validation_tail: tail_chars(&combined, REPORT_TAIL_CHARS),
                    });
                }
                let mut_started = tokio::time::Instant::now();
                let mut_out = run_cargo(&crate_dir, &test_args, remaining).await;
                record_stage_timing(timings, cand.index, "mutation", mut_started.elapsed(), &mut_out);
                match mut_out {
                    Ok(o) if !o.ok => "PROVEN: the patch's test FAILS once its fix is taken away.",
                    Ok(o) => {
                        return Ok(StageResult::Rejected {
                            stage: "mutation",
                            detail: format!(
                                "{combined}\n$ mutation probe (fix reverted, test kept)\n{}\n\
                                 [the patch's own test PASSES without the patch's fix, so it does \
                                 not demonstrate the defect — this candidate is unproven]",
                                tail_chars(&o.output, 2000)
                            ),
                        })
                    }
                    Err(e) => {
                        // Say inconclusive rather than claim a proof we do not have.
                        combined.push_str(&format!(
                            "\n$ mutation probe\nINCONCLUSIVE: the probe could not run ({e})\n"
                        ));
                        return Ok(StageResult::Validated {
                            validation_tail: tail_chars(&combined, REPORT_TAIL_CHARS),
                        });
                    }
                }
            }
            // The source hunks do not lift out on their own — they share context
            // with the test hunks. Honest, and not the candidate's fault.
            _ => "INCONCLUSIVE: the fix could not be separated from the test to take it away.",
        }
    };
    combined.push_str(&format!("\n$ mutation probe\n{mutation}\n"));

    Ok(StageResult::Validated {
        validation_tail: tail_chars(&combined, REPORT_TAIL_CHARS),
    })
}

/// The marker `run_cargo` puts in its timeout error, and the ONLY thing
/// `is_deadline_error` looks for. Both sides read this one constant so the
/// classifier cannot drift away from the message it classifies.
const CARGO_DEADLINE_MARKER: &str = "exceeded the staged-validation budget";

/// Append ONE cargo stage's real wall time to the attempt's calibration sink.
///
/// WHY THIS EXISTS. `[self_heal].attempt_budget_secs` and `.confidence_floor`
/// ship at numbers a human picked. Making them settable does not make them
/// RIGHT; it just moves the guess. What makes them right is data, and the only
/// place that data exists is here: the seconds a check / clippy / test /
/// mutation re-run actually took, on the operator's machine, on the run that
/// mattered. After N attempts `calibration.stages` answers "what is my cycle?"
/// arithmetically, and the budget follows from it instead of from an argument.
///
/// SECRET-FREE by construction: a candidate index, a fixed stage name from a
/// literal table, an integer, and a bool. No paths, no diff text, no cargo
/// output — the compiler's stdout can carry source lines, and this is a HUD
/// wire payload.
///
/// `ok` distinguishes the three outcomes that take very different times: the
/// stage passed, the stage FAILED on merit (usually much faster — one error
/// aborts the build), or the stage was cut off by its cap. A mean taken across
/// all three is meaningless, so the flag is recorded rather than inferred.
fn record_stage_timing(
    sink: &mut Vec<serde_json::Value>,
    candidate: usize,
    stage: &str,
    elapsed: Duration,
    out: &anyhow::Result<CmdOutput>,
) {
    let (ok, cut_off) = match out {
        Ok(o) => (o.ok, false),
        Err(e) => (false, is_deadline_error(e)),
    };
    sink.push(json!({
        "candidate": candidate + 1,
        "stage": stage,
        "secs": elapsed.as_secs(),
        "ok": ok,
        "cut_off": cut_off,
    }));
}

/// Everything one attempt learned about ITS OWN COST AND ITS OWN JUDGEMENTS,
/// carried out to telemetry so the two tunables can be set from measurement.
///
/// EXTENDS the existing `heal.proposal` / `heal.rejected` events rather than
/// adding a parallel topic: those are the two events an attempt already ends on,
/// the HUD already subscribes to both, and a third topic would have to be
/// joined back to them by timestamp to be useful. `heal.proposal` keeps its
/// top-level `confidence` (the WINNER's, which the HUD gauge renders) and
/// `confidence_floor`; this adds what neither carried — EVERY candidate's score
/// including the ones that lost, and what the machine actually spent.
/// `confidence_floor` is repeated inside here on purpose: `heal.rejected` has no
/// top-level copy, and a below-floor rejection is precisely the event an
/// analysis needs the floor for.
#[derive(Default, Clone)]
struct Calibration {
    /// One per candidate that reached the adversarial reviewer:
    /// `{candidate, confidence, reviewed}`. `reviewed:false` is a review call
    /// that never came back, recorded as 0.0 — that is NOT the reviewer scoring
    /// the patch zero, and averaging the two together is how a floor gets
    /// calibrated against an outage instead of against opinions.
    reviews: Vec<serde_json::Value>,
    /// One per cargo stage that actually ran (see [`record_stage_timing`]).
    stages: Vec<serde_json::Value>,
    /// Seconds of the attempt-wide budget actually consumed by staged validation.
    spent_secs: u64,
    /// How many candidates were drafted, and how many were never staged at all
    /// because the attempt could not afford them. A nonzero `unaffordable` is
    /// the signal that `attempt_budget_secs` is too small for THIS machine.
    drafted: usize,
    unaffordable: usize,
}

impl Calibration {
    fn to_json(&self) -> serde_json::Value {
        json!({
            "confidences": self.reviews,
            "stages": self.stages,
            "attempt_spent_secs": self.spent_secs,
            "attempt_budget_secs": attempt_budget().as_secs(),
            "candidate_budget_secs": VALIDATE_TIMEOUT.as_secs(),
            "confidence_floor": confidence_floor(),
            "candidates_drafted": self.drafted,
            "candidates_unaffordable": self.unaffordable,
        })
    }
}

/// True when a `run_cargo` error is the deadline running out rather than the
/// toolchain being missing or unspawnable.
fn is_deadline_error(e: &anyhow::Error) -> bool {
    e.to_string().contains(CARGO_DEADLINE_MARKER)
}

/// How much of [`attempt_budget()`] one candidate may take: its own ceiling,
/// clamped by whatever the earlier candidates left of the attempt-wide one.
/// PURE, so the rule is unit-tested without a clock.
fn candidate_budget(attempt_remaining: Duration, per_candidate: Duration) -> Duration {
    if attempt_remaining < per_candidate {
        attempt_remaining
    } else {
        per_candidate
    }
}

/// The floors every stage of one candidate needs before that candidate is worth
/// starting at all. A candidate handed less than this cannot reach a verdict, so
/// staging the tree and applying the patch would only burn the machine.
fn minimum_viable_budget() -> Duration {
    CHECK_STAGE_FLOOR + CLIPPY_STAGE_FLOOR + TEST_STAGE_FLOOR + MUTATION_STAGE_FLOOR
}

/// The slice one stage may take: everything left EXCEPT the floors reserved for
/// the stages that still have to run after it.
///
/// The ceiling is computed from the candidate's WHOLE budget, not from what is
/// left of it: a reserve subtracted from an already-depleted remainder shrinks
/// every stage as the budget drains, and would hand a late stage zero while real
/// time was still on the clock. `remaining` still wins when it is the smaller of
/// the two — a stage never gets more time than exists.
fn stage_cap(remaining: Duration, budget: Duration, reserved_for_later: Duration) -> Duration {
    let ceiling = budget.saturating_sub(reserved_for_later);
    if remaining < ceiling {
        remaining
    } else {
        ceiling
    }
}

/// WHICH ceiling stopped a candidate. They are opposite instructions to the
/// operator and used to render as one sentence: a spent candidate budget says
/// this machine cannot finish a validation cycle, a blown stage share says ONE
/// stage ran away with the time, and a spent attempt budget says the candidate
/// was never started because its predecessors used the machine up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BudgetStop {
    Candidate,
    StageShare,
    Attempt,
}

/// THE GATE RAN OUT OF TIME. Reported under its own stage name, never under the
/// stage it was trying to run, because `heal.rejected{stage:"test"}` +
/// "no candidate passed the staged gates" is read by an operator as "the model
/// drafted three bad patches" when in fact no patch was ever judged.
fn budget_exhausted(
    stage: &'static str,
    combined: &str,
    err: Option<&anyhow::Error>,
    budget: Duration,
    stop: BudgetStop,
) -> StageResult {
    let secs = budget.as_secs();
    let when = match err {
        Some(e) => format!("ran out DURING `cargo {stage}` ({e})"),
        None => format!("was already spent before `cargo {stage}` could start"),
    };
    let detail = match stop {
        BudgetStop::Candidate => format!(
            "the {secs}s staged-validation budget for this candidate {when}"
        ),
        BudgetStop::StageShare => format!(
            "`cargo {stage}` exceeded its SHARE of the {secs}s candidate budget \
             (the rest is reserved so a slow stage cannot starve the ones after it) ({})",
            err.map(|e| e.to_string()).unwrap_or_else(|| "no time left for it at all".into())
        ),
        BudgetStop::Attempt => format!(
            "the {}s attempt-wide budget was already spent by the earlier candidates, so \
             this one was never started (it would have had {secs}s, below the {}s a \
             candidate needs to reach any verdict)",
            attempt_budget().as_secs(),
            minimum_viable_budget().as_secs()
        ),
    };
    StageResult::Rejected {
        stage: "deadline",
        detail: format!(
            "{combined}\n[{detail}. This candidate was never judged on its merits — it is a \
             capacity failure of the gate, not a verdict on the patch.]"
        ),
    }
}

/// Classify a `run_cargo` error: a blown deadline is `budget_exhausted`,
/// anything else (no toolchain, spawn failure) stays filed under the stage.
fn stage_failure(
    stage: &'static str,
    combined: &str,
    e: &anyhow::Error,
    budget: Duration,
) -> StageResult {
    if is_deadline_error(e) {
        return budget_exhausted(stage, combined, Some(e), budget, BudgetStop::Candidate);
    }
    StageResult::Rejected {
        stage,
        detail: format!("{combined}\n[cargo {stage} failed to run: {e}]"),
    }
}

/// Summarize WHY every candidate was discarded, distinguishing "all three were
/// bad" from "the gate could not finish in its budget". They are opposite
/// instructions to the operator and used to render as the same sentence.
fn rejection_summary(deadline_hits: usize, merit_rejects: usize) -> String {
    if deadline_hits > 0 && merit_rejects == 0 {
        return format!(
            "NO CANDIDATE WAS EVER JUDGED. The staged-validation budget ran out on every \
             candidate ({deadline_hits} of them) before a gate could return a verdict. This says \
             nothing about the drafted patches — it says the gate cannot finish a full \
             check + clippy + test + mutation-rerun cycle on this machine inside {}s per \
             candidate / {}s per attempt.",
            VALIDATE_TIMEOUT.as_secs(),
            attempt_budget().as_secs()
        );
    }
    let mut out =
        "No candidate passed the staged check / clippy / test / mutation gates.".to_string();
    if deadline_hits > 0 {
        out.push_str(&format!(
            " ({deadline_hits} of them never finished: the {}s per-candidate / {}s per-attempt \
             validation budget ran out, so those were not judged on their merits.)",
            VALIDATE_TIMEOUT.as_secs(),
            attempt_budget().as_secs()
        ));
    }
    out
}

/// A rejection STAGE token as a sentence, for the surfaces that only have the
/// token. `deadline` and `confidence` are NOT gate failures and must not be
/// reported as "rejected every candidate at stage X": the first means nothing
/// was ever judged, and the second means every mechanical gate PASSED and only
/// the adversarial reviewer withheld its backing. The HUD draws the same
/// distinction from the same token (`hud/src/core/heal.ts::rejectionDetail`);
/// `--heal-drill` — the surface the verifier actually reads — had only the bare
/// word, and `confidence` became reachable the moment the floor landed.
fn drill_rejection_sentence(stage: &str) -> String {
    match stage {
        "deadline" => "the staged-validation budget ran out before ANY candidate could be \
             judged. That is a capacity failure of the gate on this machine, not a verdict on \
             the drafted patches"
            .to_string(),
        "confidence" => format!(
            "candidates passed every staged gate, but the adversarial reviewer backed none of \
             them (below the {:.2} confidence floor), so nothing was proposed. \
             That is the reviewer's verdict, not a gate failure",
            confidence_floor()
        ),
        other => format!("every candidate was rejected at stage `{other}`"),
    }
}

/// Everything a fully-rejected attempt owes the operator when candidates DID
/// pass every mechanical gate but none of them convinced the adversarial
/// reviewer. This is not a gate failure and must not read as one: the patches
/// compile, lint, pass and their tests bite — the reviewer simply did not back
/// any of them, and all of them are on disk under `state/heal/rejected/<ts>/`
/// for a human who wants to look.
fn below_floor_summary(best: &Survivor, survivors: usize) -> String {
    let scored = if best.reviewed {
        format!("the best of them scored {:.2}", best.confidence)
    } else {
        "the adversarial review call never came back for any of them (recorded as 0.00, \
         which is NOT the reviewer saying the patch is bad — it is no review at all)"
            .to_string()
    };
    format!(
        "NOTHING WAS PROPOSED, AND NOT BECAUSE THE PATCHES FAILED A GATE. {survivors} \
         candidate(s) passed staged check + clippy -D warnings + test + the mutation probe, but \
         {scored}, below the {:.2} review-confidence floor. A patch nobody \
         vouched for does not get a one-click APPLY button next to it. Every candidate, its \
         diff and its review are in this directory — apply one by hand if you disagree.",
        confidence_floor()
    )
}

/// Stage the crate for validation. `staging` becomes a miniature REPO ROOT and
/// the crate is copied to `<staging>/<crate-dir-name>/`; the returned path is that
/// CRATE ROOT (what `patch -p1` and cargo must run in). `target/` and dotfiles are
/// never copied.
///
/// WHAT WENT WRONG BEFORE: this copied exactly three things — `src/`, `Cargo.toml`
/// and `Cargo.lock` — straight into `staging`, and the real darwin-core TEST target
/// needs four more inputs that were never staged:
///   * `daemon/build.rs` + `daemon/csrc/thermal_shim.m`, which produce the static
///     lib `power.rs` links with `#[link(name = "darwin_thermal_shim", ...)]`;
///   * three test-only `include_str!("../../…")` that reach OUTSIDE the crate
///     (`inference/server.py`, `config/darwin.toml`, `apps/vision/manifest.toml`).
///
/// `cargo check` does not link and does not evaluate those `#[cfg(test)]` macros,
/// so the FIRST gate passed and the second could not even COMPILE — every
/// candidate died with `StageResult::Rejected { stage: "test" }` no matter what
/// the patch was. Self-heal therefore never proposed and never applied, while its
/// report told the operator "no candidate passed the staged cargo check + cargo
/// test gates", i.e. "the model drafted three bad patches" rather than "the gate
/// cannot run". Every one of the in-module pipeline tests and `--heal-drill` uses
/// a synthetic ONE-FILE crate with no build.rs, no csrc and no out-of-crate
/// includes, so none of them could ever catch it.
///
/// The fix is to copy the WHOLE crate directory (so the next file the crate grows
/// is staged automatically) and to MIRROR the repo-root siblings that the staged
/// sources actually name — discovered by scanning them, so a new
/// `include_str!("../../…")` cannot silently break the gate again.
fn stage_sources(source_dir: &Path, staging: &Path) -> anyhow::Result<PathBuf> {
    let crate_name = source_dir
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("crate"));
    let crate_dir = staging.join(&crate_name);
    std::fs::create_dir_all(&crate_dir)?;
    copy_crate_tree(source_dir, &crate_dir)?;
    mirror_out_of_crate_includes(source_dir, &crate_dir, staging);
    mirror_runtime_test_inputs(source_dir, staging);
    Ok(crate_dir)
}

/// Copy every entry of the crate dir except `target/` and dotfiles (.git, .DS_Store,
/// .gitignore — build inputs never live there and `target/` is gigabytes).
fn copy_crate_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str == "target" || name_str.starts_with('.') {
            continue;
        }
        let dest = to.join(&name);
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

/// Mirror every file the STAGED sources reference with an `include_str!`/
/// `include_bytes!` whose relative path escapes the crate, so those macros resolve
/// under `staging` exactly as they do under the repo root. Best-effort: a target
/// that is not present in the real tree is skipped (the compiler then reports it,
/// which is the honest outcome), and a path that would escape `staging` is
/// refused.
/// Repo directories the crate's TESTS read at RUNTIME, relative to the repo root.
///
/// WHAT WENT WRONG: staging mirrored only the paths named by `include_str!` — a
/// COMPILE-time scan. That made `cargo check` pass and left `cargo test` failing
/// 29 tests in staging that pass in the real tree, so the gate still rejected
/// every candidate. The operator was told "no candidate passed the staged
/// cargo check + cargo test gates", which reads as "the model drafted three bad
/// patches" rather than "the harness cannot run the suite".
///
/// The failures were unambiguous once staged and run:
///     cannot read <staging>/daemon/../config/agents.toml: No such file
///     app registered            (the app registry is empty: no apps/*/manifest.toml)
///
/// These are small and data-only — the whole set is well under a megabyte — and
/// mirroring them is what lets the suite that gates a self-heal patch actually
/// run. A gate that cannot run is not a gate.
const RUNTIME_TEST_INPUTS: &[&str] = &[
    "config",  // agents.toml, darwin.toml — read by agents:: and config:: tests
    "scripts", // apply_forge.sh and friends — forge:: tests execute these
    // docs/ — MEASURED, not guessed. `apps::tests::the_sandbox_doc_worked_
    // example_names_an_app_whose_manifest_validates` reads
    // <CARGO_MANIFEST_DIR>/../docs/SANDBOX.md and `.expect("docs/SANDBOX.md is
    // present")`. It was added AFTER this list, so it panicked in every staged
    // tree with `Os { code: 2, kind: NotFound }` — one test failing for a
    // HARNESS reason, which fails the whole `test` stage, which discards EVERY
    // candidate at stage `test`, which the operator reads as "the model drafted
    // three bad patches". The gate could not pass, ever. Staging a tree by hand
    // exactly as `stage_sources` builds one and running the suite in it is what
    // found this; the whole directory is 296K. THE THREE ENTRIES HERE ARE NOW
    // THE THREE `join("../<dir>")` ROOTS THE SUITE READS — `../config`,
    // `../docs`, `../apps` (apps is handled separately below) — and
    // `the_staged_mirror_carries_the_repo_dirs_the_suite_reads` pins that.
    "docs",
];

/// Mirror the repo inputs the staged suite reads at runtime: whole directories
/// from [`RUNTIME_TEST_INPUTS`], plus every app's `manifest.toml` and any sibling
/// `.toml` beside it (feeds.toml and the like), which the apps/plugin_sdk/proxy
/// tests need in order to register an app at all.
fn mirror_runtime_test_inputs(source_dir: &Path, staging: &Path) {
    let Some(repo_root) = source_dir.parent() else {
        return;
    };
    for rel in RUNTIME_TEST_INPUTS {
        let src = repo_root.join(rel);
        if src.is_dir() {
            let dest = staging.join(rel);
            if let Err(e) = copy_tree(&src, &dest) {
                warn!(path = %src.display(), error = %e, "heal: could not stage a runtime test input");
            }
        }
    }
    // Apps: the manifests, plus each app's ENTRY file. The registry needs the
    // manifest; `shipped_manifests_all_validate_and_declared_tools_are_served`
    // additionally asserts that a tool-exposing app HAS its entry point, and
    // fails "apps/<name>: tool-exposing app has no main.py" without it. The
    // entry files are a few hundred KB in total. The rest of each app — tests,
    // fixtures, vendored deps — is not staged.
    let apps = repo_root.join("apps");
    let Ok(entries) = std::fs::read_dir(&apps) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let Ok(files) = std::fs::read_dir(entry.path()) else {
            continue;
        };
        for f in files.flatten() {
            let p = f.path();
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let is_manifest_or_data = p.extension().and_then(|e| e.to_str()) == Some("toml");
            let is_entry = matches!(name, "main.py" | "main.rs" | "main.swift");
            if !is_manifest_or_data && !is_entry {
                continue;
            }
            let Ok(rel) = p.strip_prefix(repo_root) else {
                continue;
            };
            let dest = staging.join(rel);
            if let Some(parent) = dest.parent() {
                if std::fs::create_dir_all(parent).is_err() {
                    continue;
                }
            }
            let _ = std::fs::copy(&p, &dest);
        }
    }
}

fn mirror_out_of_crate_includes(source_dir: &Path, crate_dir: &Path, staging: &Path) {
    let repo_root = match source_dir.parent() {
        Some(p) => p.to_path_buf(),
        None => return,
    };
    for rel in out_of_crate_includes(&crate_dir.join("src"), crate_dir, &repo_root) {
        let src = repo_root.join(&rel);
        let dest = staging.join(&rel);
        if !dest.starts_with(staging) {
            continue;
        }
        if let Some(parent) = dest.parent() {
            if std::fs::create_dir_all(parent).is_err() {
                continue;
            }
        }
        if let Err(e) = std::fs::copy(&src, &dest) {
            warn!(path = %src.display(), error = %e, "heal: could not stage an out-of-crate include");
        }
    }
}

/// Scan every `.rs` file under `src_dir` for an `include_str!` / `include_bytes!`
/// string literal that resolves OUTSIDE `crate_dir`, and return each such target as
/// a path relative to the crate's PARENT. Only literals that name a file which
/// actually EXISTS under `repo_root` are returned — that is what keeps a mention of
/// the macro inside a COMMENT (this module's own doc comments name one) from being
/// mistaken for a real compilation input, and a genuinely missing include is left
/// for the compiler to report honestly. Deterministic + deduped.
fn out_of_crate_includes(src_dir: &Path, crate_dir: &Path, repo_root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut files: Vec<PathBuf> = Vec::new();
    collect_rs(src_dir, &mut files);
    files.sort();
    for file in files {
        let Ok(body) = std::fs::read_to_string(&file) else { continue };
        let Some(dir) = file.parent() else { continue };
        for macro_name in ["include_str!(\"", "include_bytes!(\""] {
            let mut rest = body.as_str();
            while let Some(idx) = rest.find(macro_name) {
                rest = &rest[idx + macro_name.len()..];
                let Some(end) = rest.find('"') else { break };
                let literal = &rest[..end];
                rest = &rest[end..];
                if !literal.starts_with("../") {
                    continue; // in-crate: already copied with the tree
                }
                let Some(abs) = normalize_lexically(&dir.join(literal)) else { continue };
                if abs.starts_with(crate_dir) {
                    continue;
                }
                if let Ok(rel) = abs.strip_prefix(crate_dir.parent().unwrap_or(crate_dir)) {
                    let rel = rel.to_path_buf();
                    if repo_root.join(&rel).is_file() && !out.contains(&rel) {
                        out.push(rel);
                    }
                }
            }
        }
    }
    out
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Resolve `.` / `..` components LEXICALLY (the file need not exist yet, so
/// `canonicalize` is unusable). `None` when the path climbs above its own root.
fn normalize_lexically(p: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    Some(out)
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

/// /usr/bin/patch -p1 --batch with the diff on stdin, cwd = `dir`. Exit
/// status != 0 (any failed hunk, malformed input) is a rejection.
/// Line number (1-based) of the first `#[cfg(test)]` in `<root>/<path>`, or
/// `usize::MAX` when the file has no test module — which makes every line of it
/// "before the boundary", i.e. fix-side, which is the honest default.
pub fn cfg_test_boundary(root: &Path, path: &str) -> usize {
    std::fs::read_to_string(root.join(path))
        .ok()
        .and_then(|t| {
            t.lines()
                .position(|l| l.trim_start().starts_with("#[cfg(test)]"))
                .map(|i| i + 1)
        })
        .unwrap_or(usize::MAX)
}

/// Split a unified diff into (TEST side, FIX side) so the fix can be taken back
/// out while the test stays.
///
/// Classification is POSITIONAL, not keyword-based: for each hunk, every added
/// line's position in the patched file is compared against that file's
/// `#[cfg(test)]` line. Added lines at or past it are test code; before it, fix
/// code. Matching on `#[test]` text instead would call a hunk containing BOTH a
/// fix and a test "a test hunk" and silently skip the probe on the exact patch
/// shape it most needs to check.
///
/// A hunk whose added lines fall on BOTH sides cannot be separated, and this
/// returns `Err` rather than guess — the probe then reports itself inconclusive.
/// Deletion-only hunks are placed by the position they delete at.
///
/// `boundary` is injected so this is testable without a filesystem.
pub fn split_test_hunks(
    diff: &str,
    boundary: &dyn Fn(&str) -> usize,
) -> Result<(String, String), String> {
    let (mut tests, mut fixes) = (String::new(), String::new());
    let mut header = String::new();
    let mut file = String::new();
    let mut cur = String::new();
    let mut newln = 0usize;
    let mut saw_test = false;
    let mut saw_fix = false;
    let mut pending: Option<()> = None;

    // Close the hunk held in `cur`, appending it to whichever side it belongs to.
    macro_rules! flush {
        () => {
            if pending.take().is_some() {
                if saw_test && saw_fix {
                    return Err(format!(
                        "a hunk in {file} spans both the fix and its test; they cannot be \
                         separated"
                    ));
                }
                let dest = if saw_test { &mut tests } else { &mut fixes };
                if !dest.ends_with(&header) {
                    dest.push_str(&header);
                }
                dest.push_str(&cur);
                cur.clear();
                saw_test = false;
                saw_fix = false;
            }
        };
    }

    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("--- ") {
            flush!();
            let _ = rest;
            header = format!("{line}\n");
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            header.push_str(line);
            header.push('\n');
            // `+++ b/src/foo.rs` -> `src/foo.rs`; trailing tab-separated
            // timestamps are stripped, as is the `b/` level `patch -p1` eats.
            let raw = rest.split('\t').next().unwrap_or(rest).trim();
            file = raw.split_once('/').map(|(_, r)| r).unwrap_or(raw).to_string();
        } else if line.starts_with("@@") {
            flush!();
            // `@@ -a,b +c,d @@` — `c` is where this hunk starts in the new file.
            newln = line
                .split('+')
                .nth(1)
                .and_then(|s| s.split([',', ' ']).next())
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(1);
            cur.push_str(line);
            cur.push('\n');
            pending = Some(());
        } else if pending.is_some() {
            let b = boundary(&file);
            match line.chars().next() {
                Some('+') => {
                    if newln >= b {
                        saw_test = true;
                    } else {
                        saw_fix = true;
                    }
                    newln += 1;
                }
                // A removal does not advance the new-file counter, but it still
                // has a side: whichever region it is being removed from.
                Some('-') => {
                    if newln >= b {
                        saw_test = true;
                    } else {
                        saw_fix = true;
                    }
                }
                // Context (' '), and the `\ No newline` marker, advance only.
                _ => newln += 1,
            }
            cur.push_str(line);
            cur.push('\n');
        }
    }
    flush!();
    // The final `flush!` resets these; nothing reads them afterwards.
    let _ = (saw_test, saw_fix);
    Ok((tests, fixes))
}

/// Reverse-apply `diff` — used by the mutation probe to take a patch's FIX back
/// out of an already-patched tree while leaving its new test in place.
async fn reverse_patch(dir: &Path, diff: &str) -> anyhow::Result<CmdOutput> {
    run_patch(dir, diff, &["-p1", "--batch", "-R"]).await
}

async fn run_patch(dir: &Path, diff: &str, args: &[&str]) -> anyhow::Result<CmdOutput> {
    let mut child = tokio::process::Command::new(PATCH_BIN)
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(diff.as_bytes()).await?;
        // Dropping stdin closes the pipe so patch sees EOF.
    }
    let out = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output()).await??;
    Ok(CmdOutput {
        ok: out.status.success(),
        output: format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    })
}

async fn apply_patch(dir: &Path, diff: &str) -> anyhow::Result<CmdOutput> {
    run_patch(dir, diff, &["-p1", "--batch"]).await
}

/// `cargo <args>` in `dir`, output captured, bounded by `timeout`. Uses the
/// $CARGO that invoked us when set (tests run under cargo) else PATH lookup.
/// Resolve the cargo binary EXPLICITLY rather than trusting PATH.
///
/// The deployed daemon runs under launchd with PATH=/usr/bin:/bin:/usr/sbin:/sbin and
/// no $CARGO. cargo lives in ~/.cargo/bin, which is not on that PATH — so spawning it
/// by bare name failed for every candidate, and self-heal's staged validation could
/// never run on the machine it is meant to heal. Every drafted candidate was discarded
/// at the first gate, and the discard looked like a normal validation failure.
fn resolve_cargo() -> Option<std::path::PathBuf> {
    if let Ok(c) = std::env::var("CARGO") {
        let p = std::path::PathBuf::from(&c);
        if p.exists() {
            return Some(p);
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let candidates = [
        std::env::var("CARGO_HOME").ok().map(|h| std::path::PathBuf::from(h).join("bin/cargo")),
        Some(std::path::PathBuf::from(&home).join(".cargo/bin/cargo")),
        Some(std::path::PathBuf::from("/usr/local/bin/cargo")),
        Some(std::path::PathBuf::from("/opt/homebrew/bin/cargo")),
    ];
    candidates.into_iter().flatten().find(|p| p.exists())
}

async fn run_cargo(dir: &Path, args: &[&str], timeout: Duration) -> anyhow::Result<CmdOutput> {
    let cargo = resolve_cargo().ok_or_else(|| {
        // A DISTINCT failure, not a per-candidate "validation failed". The operator
        // needs to know the toolchain is missing, not that three drafts were bad.
        crate::telemetry::emit(
            "system",
            "heal.blocked",
            serde_json::json!({"reason": "no_cargo"}),
        );
        anyhow::anyhow!(
            "cargo not found (checked $CARGO, $CARGO_HOME/bin, ~/.cargo/bin, \
             /usr/local/bin, /opt/homebrew/bin); self-heal cannot validate a candidate \
             on this machine"
        )
    })?;
    let child = tokio::process::Command::new(cargo)
        .args(args)
        // PIN THE TARGET DIR TO THIS CRATE. An inherited CARGO_TARGET_DIR makes
        // every staged validation share ONE build directory with whatever
        // invoked the daemon — including, under `cargo test`, the test binary
        // running this very function. Two consequences, both bad: concurrent
        // validations serialize on (or fail outright at) cargo's build-directory
        // lock, and — far worse — the MUTATION PROBE loses its isolation. The
        // probe reverses a patch's fix and requires the patch's own test to
        // fail; sharing a build cache with the un-reversed run lets a stale
        // artifact satisfy the rebuild, the test passes, and the probe reports
        // "the patch's own test PASSES without the fix, so it does not
        // demonstrate the defect" for a patch that was actually fine. A gate
        // that silently mis-scores its own evidence is worse than no gate.
        // `forge.rs` already pins its nested build the same way.
        .env("CARGO_TARGET_DIR", dir.join("target"))
        .current_dir(dir)
        // THE STAGED BUILD MUST NOT SHARE A TARGET DIR WITH ANYTHING.
        //
        // This inherited the daemon's whole environment, so an exported
        // CARGO_TARGET_DIR — or a `.cargo/config.toml` `build.target-dir`, which is
        // an ordinary developer setting — sent every staged check / clippy / test /
        // mutation-probe build into ONE shared directory. The staged crate is a
        // byte-for-byte copy of daemon/: SAME package name, SAME version. MEASURED
        // on this repo at HEAD: two staged crates with DIFFERENT sources resolved to
        // the same artifact, `synthetic_clamp-0eb738ba7de7e68e`, and the second run
        // reported `Finished ... in 0.34s` and then executed the FIRST one's test
        // binary — so `heal::tests::the_gate_validates_and_marks_proven_a_patch_whose
        // _test_bites` and its rejecting twin swapped verdicts between runs.
        //
        // A gate whose "cargo test passed in staging" can be answered by a DIFFERENT
        // candidate's compiled code — or by the unpatched live tree's, which shares
        // the same name and version — is not a gate. Pin the build inside the staging
        // tree, which is where `stage_sources` already assumes it goes (it refuses to
        // copy `target/`, and apply_heal.sh's selftest asserts that). An env var beats
        // `.cargo/config.toml`, so this closes the config route too.
        .env("CARGO_TARGET_DIR", dir.join("target"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(result) => result?,
        Err(_) => anyhow::bail!(
            "cargo {} {CARGO_DEADLINE_MARKER} ({}s remained)",
            args.join(" "),
            timeout.as_secs()
        ),
    };
    Ok(CmdOutput {
        ok: out.status.success(),
        output: format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    })
}

// ---------------------------------------------------------------------------
// (6 of contract) HEAL DRILL — the ONE real cloud path, invoked by the
// verifier via `darwind --heal-drill`. It runs the FULL real pipeline
// (diagnose -> Opus draft -> stage -> validate -> review -> propose) against a
// PLANTED FAULT in a throwaway temp crate. It NEVER touches the real daemon/.
// ---------------------------------------------------------------------------

/// A self-contained throwaway crate carrying a PLANTED COMPILE FAULT, and a
/// synthetic ERROR burst that names it. The drill heals THIS, not daemon/.
/// `[workspace]` keeps cargo from walking up into any enclosing workspace.
const DRILL_PLANTED_LIB: &str =
    "/// Multiply by two. (PLANTED FAULT: `y` is undefined — does not compile.)\n\
     pub fn double(x: i32) -> i32 {\n    x * y\n}\n";

fn drill_burst_scan() -> LogScan {
    let now = Utc::now().to_rfc3339();
    let lines = [
        format!("{now} ERROR darwin_core::router: compile guard failed in src/lib.rs:3 error=cannot find value `y` in this scope"),
        format!("{now} ERROR darwin_core::router: compile guard failed in src/lib.rs:3 error=cannot find value `y` in this scope"),
        format!("{now} ERROR darwin_core::router: compile guard failed in src/lib.rs:3 error=cannot find value `y` in this scope"),
        format!("{now} ERROR darwin_core::router: compile guard failed in src/lib.rs:3 error=cannot find value `y` in this scope"),
        format!("{now} ERROR darwin_core::router: compile guard failed in src/lib.rs:3 error=cannot find value `y` in this scope"),
    ];
    scan_tail(lines.join("\n"))
}

/// Run the full real self-heal pipeline against a planted fault in a temp
/// crate, drafting + reviewing via the REAL cloud (CloudBrain). Requires the
/// Anthropic key. Writes a real proposal artifact under `<tmp>/state/heal/
/// proposals/<ts>/`. Returns the proposal dir on success. The real daemon/ is
/// never touched.
///
/// Invoked by `darwind --heal-drill` (see main.rs); the model id is the
/// configured heavy model so the drill exercises exactly the production path.
pub async fn run_heal_drill(model: &str) -> anyhow::Result<PathBuf> {
    if anthropic::resolve_api_key().await.is_none() {
        anyhow::bail!("heal drill requires an Anthropic API key (none resolved)");
    }
    telemetry::init(); // safe if already initialized (OnceLock no-op)

    // Throwaway sandbox: <tmpdir>/darwin-heal-drill-<pid>-<ts>/.
    let ts = now_secs();
    let sandbox = std::env::temp_dir().join(format!(
        "darwin-heal-drill-{}-{ts}",
        std::process::id()
    ));
    let crate_dir = sandbox.join("daemon");
    let heal_root = sandbox.join("state").join("heal");
    std::fs::create_dir_all(crate_dir.join("src"))?;
    std::fs::write(
        crate_dir.join("Cargo.toml"),
        "[package]\nname = \"darwin-heal-drill\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )?;
    std::fs::write(crate_dir.join("src").join("lib.rs"), DRILL_PLANTED_LIB)?;

    info!(sandbox = %sandbox.display(), model, "heal drill: running the FULL pipeline against a planted fault (cloud)");

    let brain = CloudBrain { model: model.to_string() };
    let scan = drill_burst_scan();

    let result = run_attempt(&crate_dir, &heal_root, ts, model, &brain, &scan).await;

    // The drill must end in a real proposal artifact — and must NOT have
    // touched the planted source (propose mode never applies to the source).
    let planted = std::fs::read_to_string(crate_dir.join("src").join("lib.rs"))?;
    if !planted.contains("x * y") {
        anyhow::bail!(
            "heal drill SAFETY VIOLATION: the planted source was modified (propose mode must \
             never touch the source tree)"
        );
    }

    match result {
        AttemptResult::Proposed { diff, report, files, confidence, calibration, responsiveness, extra } => {
            // Write the proposal artifacts exactly as the propose path does,
            // WITHOUT touching meta or emitting heal.proposal into a live HUD
            // (no Memory here): the drill proves the loop, it is not a live heal.
            let dir = heal_root.join("proposals").join(ts.to_string());
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join("patch.diff"), &diff)?;
            std::fs::write(dir.join("report.md"), &report)?;
            std::fs::write(dir.join("diagnosis.json"), &extra.diagnosis_json)?;
            std::fs::write(dir.join("candidates.md"), &extra.candidates_md)?;
            std::fs::write(dir.join("review.md"), &extra.review_md)?;
            telemetry::emit(
                "system",
                "heal.proposal",
                json!({"ts": ts, "files": files, "validated": true, "confidence": confidence, "confidence_floor": confidence_floor(), "responsiveness": responsiveness, "calibration": calibration.to_json(), "drill": true}),
            );
            info!(
                proposal = %dir.display(),
                confidence,
                "heal drill: PASSED — full pipeline produced a validated, reviewed proposal"
            );
            Ok(dir)
        }
        AttemptResult::Rejected { stage, .. } => {
            anyhow::bail!("heal drill: {}", drill_rejection_sentence(stage))
        }
        AttemptResult::Aborted { stage } => {
            anyhow::bail!("heal drill: pipeline aborted at stage `{stage}` (cloud/infra failure)")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- trigger detection ---------------------------------------------------

    #[test]
    fn matches_only_the_level_field() {
        assert!(is_error_line(
            "2026-06-12T01:02:03.456789Z ERROR darwin_core::audio: capture stopped"
        ));
        // Level field is space-padded by the fmt layer; split_whitespace copes.
        assert!(is_error_line(
            "2026-06-12T01:02:03.456789Z  ERROR darwin_core::audio: capture stopped"
        ));
        // INFO line quoting "ERROR" in the message must not count.
        assert!(!is_error_line(
            "2026-06-12T01:02:03.456789Z  INFO darwin_core: responding response=\"The log shows ERROR entries\""
        ));
        // The watchdog's own WARN must not count.
        assert!(!is_error_line(
            "2026-06-12T01:02:03.456789Z  WARN darwin_core::heal: heal: error burst detected but self_heal.enabled = false"
        ));
    }

    /// Audit regression: the detector must fire on the EXACT lines the daemon
    /// now emits during a simulated inference outage (these messages were
    /// WARN-level before the fix, so the watchdog could never trigger).
    #[test]
    fn detector_fires_on_a_simulated_inference_outage() {
        let now = Utc::now().to_rfc3339();
        let tail = [
            format!("{now}  INFO darwin_core: darwind starting"),
            format!("{now} ERROR darwin_core: transcription failed; is the inference server up? error=inference socket unavailable at state/ipc/inference.sock"),
            format!("{now} ERROR darwin_core: classification failed error=inference classify timed out after 30s"),
            format!("{now} ERROR darwin_core::router: converse failed before any audio; falling back to generate+speak error=..."),
            format!("{now} ERROR darwin_core::router: local generate unavailable; falling back to raw data error=..."),
            format!("{now} ERROR darwin_core::router: cloud completion failed; degrading to local generate error=..."),
            format!("{now}  WARN darwin_core: fact extraction failed"),
        ]
        .join("\n");
        let scan = scan_tail(tail);
        assert_eq!(scan.burst_count, 5, "exactly the 5 ERROR lines: {:?}", scan.burst_lines);
        assert!(scan.triggered(), "an inference outage must trigger the pipeline");
        assert!(!scan.total_loss);
        assert_eq!(scan.burst_lines.len(), 5, "burst lines collected for the drafter");
        assert!(
            scan.burst_lines[0].contains("transcription failed"),
            "burst lines must be oldest-first: {:?}",
            scan.burst_lines
        );
    }

    /// The capture thread dies ONCE (it is never respawned) — a single line
    /// must trigger immediately; no burst will ever follow it.
    #[test]
    fn a_single_total_loss_line_triggers() {
        let now = Utc::now().to_rfc3339();
        let tail = format!(
            "{now}  INFO darwin_core::audio: audio capture running\n\
             {now} ERROR darwin_core::audio: audio capture stopped error=no default input device"
        );
        let scan = scan_tail(tail);
        assert_eq!(scan.burst_count, 1);
        assert!(scan.total_loss);
        assert!(scan.triggered());

        // The same words inside an INFO line must NOT trigger.
        let now = Utc::now().to_rfc3339();
        let scan = scan_tail(format!(
            "{now}  INFO darwin_core: responding response=\"audio capture stopped earlier, sir\""
        ));
        assert!(!scan.triggered());
    }

    #[test]
    fn stale_errors_outside_the_window_do_not_trigger() {
        let tail = (0..6)
            .map(|i| {
                format!("2020-01-01T00:00:0{i}.000000Z ERROR darwin_core: transcription failed; is the inference server up?")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let scan = scan_tail(tail);
        assert_eq!(scan.burst_count, 0);
        assert!(!scan.triggered());
    }

    #[test]
    fn four_errors_are_below_the_burst_limit() {
        let now = Utc::now().to_rfc3339();
        let tail = (0..4)
            .map(|_| format!("{now} ERROR darwin_core: classification failed"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!scan_tail(tail).triggered());
    }

    // -- (3) diagnosis extraction (v2) ---------------------------------------

    #[test]
    fn diagnosis_extracts_signature_files_subsystem_from_synthetic_lines() {
        let now = Utc::now().to_rfc3339();
        let tail = [
            format!("{now}  INFO darwin_core: darwind starting"),
            format!("{now} ERROR darwin_core::router: converse failed at src/router.rs:122 error=socket closed"),
            format!("{now} ERROR darwin_core::router: converse failed at src/router.rs:122 error=timed out after 30s"),
            format!("{now} ERROR darwin_core::router: classification failed error=inference classify timed out"),
            format!("{now} ERROR darwin_core::router: cloud completion failed; degrading to local"),
            format!("{now} ERROR darwin_core::router: local generate unavailable in src/inference.rs:88"),
        ]
        .join("\n");
        let scan = scan_tail(tail);
        let d = build_diagnosis(&scan);

        assert_eq!(d.subsystem, "router", "subsystem from the module-path target");
        // The volatile `error=...` tail is trimmed, so the two converse lines
        // collapse to ONE signature.
        assert!(
            d.signatures
                .iter()
                .any(|s| s == "converse failed at src/router.rs:122"),
            "stable signature with the error= tail trimmed: {:?}",
            d.signatures
        );
        // Distinct causes are kept distinct.
        assert!(d.signatures.iter().any(|s| s.contains("classification failed")));
        assert!(d.signatures.iter().any(|s| s.contains("cloud completion failed")));
        // Files cited in the burst, first-seen order, deduped.
        assert_eq!(d.files, vec!["src/router.rs", "src/inference.rs"]);
        // Line numbers cited next to a src/<file>.rs:<line>.
        assert!(d.line_numbers.contains(&122));
        assert!(d.line_numbers.contains(&88));
        // The primary signature feeds heal.diagnosing.
        assert!(!d.primary_signature().is_empty());
    }

    #[test]
    fn diagnosis_subsystem_falls_back_to_unknown() {
        let now = Utc::now().to_rfc3339();
        // Bare `darwin_core` target (no subsystem segment).
        let tail = (0..5)
            .map(|_| format!("{now} ERROR darwin_core: transcription failed error=x"))
            .collect::<Vec<_>>()
            .join("\n");
        let d = build_diagnosis(&scan_tail(tail));
        assert_eq!(d.subsystem, "unknown");
        assert_eq!(d.signatures, vec!["transcription failed".to_string()]);
    }

    #[test]
    fn attach_source_excerpts_reads_cited_files_into_the_prompt() {
        // A planted crate whose cited file has KNOWN contents; the drafter
        // prompt must carry those exact lines so the model's diff can apply.
        let root = TempRoot::new("excerpts");
        let crate_dir = root.0.join("daemon");
        write_synthetic_crate(&crate_dir); // src/lib.rs = "pub fn double(x: i32) -> i32 {\n    x * y\n}\n"

        let now = Utc::now().to_rfc3339();
        let tail = (0..5)
            .map(|_| format!("{now} ERROR darwin_core::router: compile failed in src/lib.rs:2 error=cannot find value `y`"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut d = build_diagnosis(&scan_tail(tail));
        assert_eq!(d.files, vec!["src/lib.rs"]);
        assert!(d.source_excerpts.is_empty(), "build_diagnosis stays IO-free");

        attach_source_excerpts(&mut d, &crate_dir);
        assert_eq!(d.source_excerpts.len(), 1, "the cited file was read");
        assert_eq!(d.source_excerpts[0].0, "src/lib.rs");
        assert!(d.source_excerpts[0].1.contains("x * y"), "real contents present");

        // The prompt now carries the real source so a generated diff can match.
        let prompt = draft_prompt(&d, 3);
        assert!(prompt.contains("current contents"), "prompt advertises the source");
        assert!(prompt.contains("pub fn double(x: i32) -> i32"), "real lines in the prompt");
        assert!(prompt.contains("x * y"));

        // A cited file that does not exist is simply skipped (no panic, no
        // escape outside src/).
        let mut d2 = d.clone();
        d2.source_excerpts.clear();
        d2.files = vec!["src/nonexistent.rs".to_string(), "src/../Cargo.toml".to_string()];
        attach_source_excerpts(&mut d2, &crate_dir);
        assert!(d2.source_excerpts.is_empty(), "missing/escaping paths are skipped");
    }

    #[test]
    fn diagnosis_json_roundtrips() {
        let now = Utc::now().to_rfc3339();
        let tail = (0..5)
            .map(|_| format!("{now} ERROR darwin_core::audio: audio capture stopped error=device gone"))
            .collect::<Vec<_>>()
            .join("\n");
        let d = build_diagnosis(&scan_tail(tail));
        let json = diagnosis_json(&d);
        assert!(json.contains("\"subsystem\": \"audio\""), "json:\n{json}");
        assert!(json.contains("audio capture stopped"));
        // Valid JSON.
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["subsystem"], "audio");
    }

    // -- pure pipeline helpers ------------------------------------------------

    #[test]
    fn source_files_are_extracted_from_log_lines() {
        let text = "thread 'main' panicked at src/router.rs:122: oh no\n\
                    error in daemon/src/heal.rs: bad\n\
                    also src/router.rs:99 again, and src/anthropic.rs too";
        assert_eq!(
            extract_source_files(text),
            vec!["src/router.rs", "src/heal.rs", "src/anthropic.rs"],
            "dedup, first-seen order"
        );
        assert!(extract_source_files("no rust paths here").is_empty());
        // Diff headers parse too (used for the files-touched report field).
        assert_eq!(
            extract_source_files("--- a/src/lib.rs\n+++ b/src/lib.rs"),
            vec!["src/lib.rs"]
        );
    }

    #[test]
    fn staging_dir_name_embeds_ts_and_candidate() {
        assert_eq!(staging_dir_name(1_760_000_000, 0), "staging-1760000000-c0");
        assert_eq!(staging_dir_name(1_760_000_000, 2), "staging-1760000000-c2");
    }

    #[test]
    fn rate_limit_allows_one_attempt_per_six_hours() {
        let now = 1_760_000_000u64;
        assert!(attempt_allowed(None, now), "never attempted -> allowed");
        assert!(attempt_allowed(Some("garbage"), now), "unparseable stamp -> allowed");
        assert!(
            attempt_allowed(Some(&(now - ATTEMPT_INTERVAL_SECS - 1).to_string()), now),
            "older than 6h -> allowed"
        );
        assert!(
            !attempt_allowed(Some(&(now - ATTEMPT_INTERVAL_SECS).to_string()), now),
            "exactly 6h -> still blocked"
        );
        assert!(!attempt_allowed(Some(&(now - 60).to_string()), now), "1min ago -> blocked");
        assert!(
            !attempt_allowed(Some(&(now + 9999).to_string()), now),
            "future stamp (clock skew) must not underflow into allowed"
        );
    }

    /// Gating truth table — UNCHANGED contract: "auto" requires enabled=true,
    /// and any unknown mode degrades only toward the safer Propose.
    #[test]
    fn mode_gating_truth_table() {
        assert_eq!(heal_action(false, "propose"), HealAction::Disabled);
        assert_eq!(heal_action(false, "auto"), HealAction::Disabled);
        assert_eq!(heal_action(false, ""), HealAction::Disabled);
        assert_eq!(heal_action(true, "propose"), HealAction::Propose);
        assert_eq!(heal_action(true, "auto"), HealAction::Auto);
        assert_eq!(heal_action(true, " auto "), HealAction::Auto);
        assert_eq!(heal_action(true, ""), HealAction::Propose);
        assert_eq!(heal_action(true, "AUTO"), HealAction::Propose, "no case games");
        assert_eq!(heal_action(true, "yolo"), HealAction::Propose);
    }

    #[test]
    fn last_lines_takes_the_tail() {
        let text = "a\nb\nc\nd";
        assert_eq!(last_lines(text, 2), "c\nd");
        assert_eq!(last_lines(text, 10), "a\nb\nc\nd");
        assert_eq!(last_lines("", 5), "");
    }

    #[test]
    fn diff_size_counts_added_and_removed_lines() {
        let diff = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n pub fn f() {\n-    a\n+    b\n }\n";
        // one '-' and one '+' content line; headers excluded.
        assert_eq!(diff_size(diff), 2);
    }

    // -- (2) candidate diff cleaning + splitting + rejection (v2) ------------

    #[test]
    fn clean_diff_strips_fences_and_prose_and_rejects_non_diffs() {
        let raw = "Here is the fix you asked for:\n\
                   ```diff\n\
                   --- a/src/lib.rs\n\
                   +++ b/src/lib.rs\n\
                   @@ -1,3 +1,3 @@\n \
                   pub fn double(x: i32) -> i32 {\n\
                   -    x * y\n\
                   +    x * 2\n \
                   }\n\
                   ```";
        let diff = clean_diff(raw).expect("a real diff must survive cleaning");
        assert!(diff.starts_with("--- a/src/lib.rs\n"), "prose/fence must be gone:\n{diff}");
        assert!(diff.ends_with("}\n"), "must keep the final newline:\n{diff:?}");
        assert!(!diff.contains("```"));
        assert!(!diff.contains("Here is"));

        // Prose, refusals, fragments: never reach patch(1).
        assert!(clean_diff("I cannot patch this safely.").is_none());
        assert!(clean_diff("--- a/src/lib.rs\nno hunks here").is_none());
        assert!(clean_diff("").is_none());
    }

    #[test]
    fn clean_diff_rejects_path_traversal_headers() {
        // A `..`-laden target in either header escapes the staging/daemon dir via
        // `patch -p1` (macOS patch honors `..`); such a candidate must be dropped.
        let traverse_plus = "--- a/src/lib.rs\n\
                             +++ b/src/../../../../tmp/escape.txt\n\
                             @@ -1,1 +1,1 @@\n-x\n+y\n";
        assert!(clean_diff(traverse_plus).is_none(), "`..` in +++ header must be rejected");

        let traverse_minus = "--- a/src/../../../../tmp/target.txt\n\
                              +++ b/src/lib.rs\n\
                              @@ -1,1 +1,1 @@\n-x\n+y\n";
        assert!(clean_diff(traverse_minus).is_none(), "`..` in --- header must be rejected");

        // A `..`-first target (escapes immediately after the `-p1` strip) is rejected.
        let dotdot_first = "--- a/../etc/shadow\n\
                            +++ b/../etc/shadow\n\
                            @@ -1,1 +1,1 @@\n-x\n+y\n";
        assert!(clean_diff(dotdot_first).is_none(), "leading `..` after -p1 must be rejected");

        // A legitimate `a/src...`/`b/src...` diff (the only shape heal authors)
        // strips to `src/...` with no `..`/absolute and survives unchanged.
        let ok = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,1 +1,1 @@\n-x\n+y\n";
        assert!(clean_diff(ok).is_some(), "a confined a/src diff must survive");

        // New-file creation (`--- /dev/null`) is still allowed when the `+++`
        // target is confined.
        let new_file = "--- /dev/null\n+++ b/src/new.rs\n@@ -0,0 +1,1 @@\n+y\n";
        assert!(clean_diff(new_file).is_some(), "confined new-file diff must survive");
    }

    #[test]
    fn split_candidate_diffs_parses_labelled_alternatives() {
        let raw = "Here are three options.\n\
                   === CANDIDATE 1 ===\n\
                   --- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n pub fn d() {\n-    x * y\n+    x * 2\n }\n\
                   === CANDIDATE 2 ===\n\
                   ```diff\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n pub fn d() {\n-    x * y\n+    x + x\n }\n```\n\
                   === CANDIDATE 3 ===\n\
                   I could not find a third distinct approach.\n";
        let diffs = split_candidate_diffs(raw);
        assert_eq!(diffs.len(), 2, "two real diffs; the prose candidate is dropped: {diffs:?}");
        assert!(diffs[0].contains("x * 2"));
        assert!(diffs[1].contains("x + x"));
        assert!(diffs.iter().all(|d| !d.contains("CANDIDATE")));
        assert!(diffs.iter().all(|d| !d.contains("```")));
    }

    #[test]
    fn split_candidate_diffs_falls_back_to_single_unlabelled_diff() {
        let raw = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n pub fn d() {\n-    x * y\n+    x * 2\n }\n";
        let diffs = split_candidate_diffs(raw);
        assert_eq!(diffs.len(), 1);
        assert!(diffs[0].contains("x * 2"));
    }

    #[test]
    fn split_candidate_diffs_dedups_identical_candidates() {
        let one = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n pub fn d() {\n-    x * y\n+    x * 2\n }\n";
        let raw = format!("=== CANDIDATE 1 ===\n{one}=== CANDIDATE 2 ===\n{one}");
        assert_eq!(split_candidate_diffs(&raw).len(), 1, "identical diffs collapse to one");
    }

    #[test]
    fn split_candidate_diffs_rejects_an_all_prose_response() {
        assert!(split_candidate_diffs("I cannot safely patch this; please investigate manually.").is_empty());
    }

    // -- (6) review parsing + (7) survivor selection (v2) --------------------

    #[test]
    fn parse_review_extracts_verdict_and_confidence() {
        let raw = "VERDICT: Fixes the root cause; the undefined binding is replaced, no side effects.\n\
                   CONFIDENCE: 0.88";
        let (verdict, confidence) = parse_review(raw);
        assert!(verdict.contains("root cause"));
        assert!((confidence - 0.88).abs() < 1e-9);

        // Case-insensitive labels, stray text around the number.
        let (_, c2) = parse_review("verdict: ok\nconfidence: about 0.5 maybe");
        assert!((c2 - 0.5).abs() < 1e-9);

        // Garbled confidence -> 0.0 (conservative).
        let (_, c3) = parse_review("VERDICT: unsure\nCONFIDENCE: high");
        assert_eq!(c3, 0.0);

        // Out-of-range clamps to 0..1.
        let (_, c4) = parse_review("VERDICT: ok\nCONFIDENCE: 1.9");
        assert_eq!(c4, 1.0);
    }

    fn survivor(index: usize, confidence: f64, size: usize) -> Survivor {
        Survivor {
            index,
            diff: "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@\n".to_string(),
            files: vec!["src/lib.rs".to_string()],
            validation_tail: "ok".to_string(),
            review_verdict: "v".to_string(),
            confidence,
            reviewed: true,
            size,
        }
    }

    #[test]
    fn select_winner_prefers_highest_confidence_then_minimal_patch() {
        // Highest confidence wins outright.
        let s = vec![survivor(1, 0.4, 2), survivor(2, 0.9, 10), survivor(3, 0.7, 1)];
        assert_eq!(select_winner(&s), Some(1), "candidate #2 (0.9) wins");

        // Tie on confidence -> the SMALLER patch wins.
        let s = vec![survivor(1, 0.8, 20), survivor(2, 0.8, 3), survivor(3, 0.8, 9)];
        assert_eq!(select_winner(&s), Some(1), "the 3-line patch (#2) wins the tie");

        // No survivors.
        assert_eq!(select_winner(&[]), None);

        // Single survivor.
        assert_eq!(select_winner(&[survivor(1, 0.0, 5)]), Some(0));
    }

    // -- (5) proposal artifact rendering (v2) --------------------------------

    #[test]
    fn report_carries_diagnosis_diff_validation_review_and_apply_command() {
        let now = Utc::now().to_rfc3339();
        let tail = (0..5)
            .map(|_| format!("{now} ERROR darwin_core::router: classification failed error=x in src/router.rs:42"))
            .collect::<Vec<_>>()
            .join("\n");
        let d = build_diagnosis(&scan_tail(tail));
        let winner = Survivor {
            index: 2,
            diff: "--- a/src/router.rs\n+++ b/src/router.rs\n@@\n-bad\n+good\n".to_string(),
            files: vec!["src/router.rs".to_string()],
            validation_tail: "$ cargo check\n    Finished dev profile\n$ cargo test\ntest result: ok".to_string(),
            review_verdict: "Fixes the root cause; no side effects.".to_string(),
            confidence: 0.91,
            reviewed: true,
            size: 2,
        };
        let report = render_report(1_760_000_000, "claude-opus-4-8", &d, &winner);
        assert!(report.contains("1760000000"));
        assert!(report.contains("claude-opus-4-8"));
        assert!(report.contains("router"), "subsystem in report");
        assert!(report.contains("src/router.rs"));
        assert!(report.contains("classification failed"), "diagnosis signature");
        assert!(report.contains("Finished dev profile"), "validation tail");
        assert!(report.contains("Fixes the root cause"), "review verdict");
        assert!(report.contains("0.91"), "review confidence");
        assert!(report.contains("VALIDATED"));
        assert!(report.contains("chosen candidate: #2"));
        assert!(report.contains("scripts/apply_heal.sh 1760000000"), "exact apply command");
    }

    #[test]
    fn candidates_md_lists_kept_and_discarded() {
        let outcomes = vec![
            CandidateOutcome {
                index: 1,
                diff: "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@\n+x\n".to_string(),
                validated: false,
                detail: "discarded at check:\n```\nerror[E0425]\n```".to_string(),
            },
            CandidateOutcome {
                index: 2,
                diff: "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@\n+y\n".to_string(),
                validated: true,
                detail: "kept — review confidence 0.80".to_string(),
            },
        ];
        let md = render_candidates_md(&outcomes);
        assert!(md.contains("Candidate #1 — DISCARDED"));
        assert!(md.contains("discarded at check"));
        assert!(md.contains("Candidate #2 — VALIDATED"));
        assert!(md.contains("review confidence 0.80"));
    }

    #[test]
    fn review_md_renders_verdict_and_confidence() {
        let winner = survivor(3, 0.77, 4);
        let md = render_review_md(&winner);
        assert!(md.contains("chosen candidate #3"));
        assert!(md.contains("0.77"));
    }

    // -- (4) the trait seam: a MOCK brain drives the full pipeline with NO
    //        cloud, proving multi-candidate -> validate-each -> review ->
    //        select -> propose end to end against a planted-fault temp crate.

    struct MockBrain {
        draft: String,
        reviews: Vec<(String, f64)>,
    }

    impl HealBrain for MockBrain {
        fn draft_candidates<'a>(&'a self, _d: &'a Diagnosis, _n: usize) -> BrainFuture<'a> {
            let draft = self.draft.clone();
            Box::pin(async move { Ok(draft) })
        }
        fn review<'a>(&'a self, _d: &'a Diagnosis, diff: &'a str, _tail: &'a str) -> BrainFuture<'a> {
            // Return a scripted review keyed by which fix the diff carries, so
            // selection is deterministic.
            let mut out = "VERDICT: unknown\nCONFIDENCE: 0.0".to_string();
            for (needle, conf) in &self.reviews {
                if diff.contains(needle.as_str()) {
                    out = format!("VERDICT: mock review for {needle}\nCONFIDENCE: {conf}");
                    break;
                }
            }
            Box::pin(async move { Ok(out) })
        }
    }

    fn write_synthetic_crate(dir: &Path) {
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"synthetic-heal\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src").join("lib.rs"),
            "pub fn double(x: i32) -> i32 {\n    x * y\n}\n",
        )
        .unwrap();
    }

    /// A crate whose fix site and test module are far enough apart that a
    /// unified diff cannot merge them into one hunk — the realistic shape.
    fn write_clamp_crate(dir: &Path) {
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"synthetic-clamp\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src").join("lib.rs"),
            "pub fn clamp_pct(x: i32) -> i32 {\n\
             \x20   x\n\
             }\n\
             \n\
             pub fn pad_a() {}\n\
             pub fn pad_b() {}\n\
             pub fn pad_c() {}\n\
             pub fn pad_d() {}\n\
             pub fn pad_e() {}\n\
             pub fn pad_f() {}\n\
             \n\
             #[cfg(test)]\n\
             mod tests {\n\
             \x20   #[test]\n\
             \x20   fn baseline() {\n\
             \x20       assert_eq!(super::clamp_pct(5), 5);\n\
             \x20   }\n\
             }\n",
        )
        .unwrap();
    }

    /// The fix hunk, identical in both candidates below.
    const CLAMP_FIX_HUNK: &str = "--- a/src/lib.rs\n\
                                  +++ b/src/lib.rs\n\
                                  @@ -1,4 +1,4 @@\n \
                                  pub fn clamp_pct(x: i32) -> i32 {\n\
                                  -    x\n\
                                  +    x.min(100)\n \
                                  }\n \
                                  \n";

    /// STAGE 4, THE POINT OF IT. Two candidates carry the SAME fix. One's test
    /// asserts the clamp; the other's asserts a value the bug never affected.
    /// Both compile, both lint clean, both pass — the first three stages cannot
    /// tell them apart. Only taking the fix away can.
    #[tokio::test]
    async fn the_gate_rejects_a_patch_whose_test_passes_without_its_fix() {
        let root = TempRoot::new("mutvacuous");
        let crate_dir = root.0.join("daemon");
        let heal_root = root.0.join("state").join("heal");
        write_clamp_crate(&crate_dir);

        // Asserts clamp_pct(5) == 5 — true with the fix AND without it.
        let vacuous = format!(
            "{CLAMP_FIX_HUNK}\
             @@ -15,4 +15,9 @@\n \
             \x20   fn baseline() {{\n \
             \x20       assert_eq!(super::clamp_pct(5), 5);\n \
             \x20   }}\n\
             +\n\
             +    #[test]\n\
             +    fn passes_small_values_through() {{\n\
             +        assert_eq!(super::clamp_pct(5), 5);\n\
             +    }}\n \
             }}\n"
        );

        // CALIBRATION SINK, exercised for real. This test is the only one that
        // drives all four stages against a live cargo, so it is the only place
        // the per-stage timing capture can be shown to actually fire. Without
        // this the whole "set the budget from measurement" story would be an
        // untested branch -- a knob that emits nothing is the same defect as a
        // knob nothing reads.
        let mut timings: Vec<serde_json::Value> = Vec::new();
        let result =
            stage_and_validate(&crate_dir, &heal_root, CandidateRef { ts: 1_770_000_001, index: 0 }, &vacuous, &diag_for(&["src/lib.rs"], "router"), VALIDATE_TIMEOUT, &mut timings)
                .await
                .unwrap();
        let staged: Vec<&str> = timings
            .iter()
            .filter_map(|t| t.get("stage").and_then(|v| v.as_str()))
            .collect();
        for want in ["check", "clippy", "test", "mutation"] {
            assert!(
                staged.contains(&want),
                "stage `{want}` recorded no timing -- the calibration payload that \
                 attempt_budget_secs is supposed to be set FROM is incomplete: {staged:?}"
            );
        }
        assert!(
            timings.iter().all(|t| t.get("secs").and_then(|v| v.as_u64()).is_some()),
            "a recorded stage carries no `secs`: {timings:?}"
        );
        // SECRET-FREE: only the five declared keys, nothing carrying compiler
        // output or a path (this rides the HUD wire).
        for t in &timings {
            let obj = t.as_object().expect("each timing is an object");
            let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
            keys.sort_unstable();
            assert_eq!(
                keys,
                ["candidate", "cut_off", "ok", "secs", "stage"],
                "an unexpected field entered the calibration payload: {t:?}"
            );
        }
        match result {
            StageResult::Rejected { stage, detail } => {
                assert_eq!(stage, "mutation", "wrong stage rejected:\n{detail}");
                assert!(
                    detail.contains("PASSES without the patch's fix"),
                    "rejection does not say why:\n{detail}"
                );
                // It must have got PAST the first three stages — otherwise this
                // test is passing for the wrong reason entirely.
                assert!(detail.contains("cargo clippy"), "never reached clippy:\n{detail}");
            }
            StageResult::Validated { validation_tail } => {
                panic!("a test that survives its own fix's removal must not validate:\n{validation_tail}")
            }
        }
    }

    /// The other half of the pair: same fix, a test that DOES bite. It must
    /// validate, and say so — a gate that rejected both would be useless.
    #[tokio::test]
    async fn the_gate_validates_and_marks_proven_a_patch_whose_test_bites() {
        let root = TempRoot::new("mutproven");
        let crate_dir = root.0.join("daemon");
        let heal_root = root.0.join("state").join("heal");
        write_clamp_crate(&crate_dir);

        // Asserts clamp_pct(150) == 100 — only true once the fix is present.
        let biting = format!(
            "{CLAMP_FIX_HUNK}\
             @@ -15,4 +15,9 @@\n \
             \x20   fn baseline() {{\n \
             \x20       assert_eq!(super::clamp_pct(5), 5);\n \
             \x20   }}\n\
             +\n\
             +    #[test]\n\
             +    fn clamps_above_one_hundred() {{\n\
             +        assert_eq!(super::clamp_pct(150), 100);\n\
             +    }}\n \
             }}\n"
        );

        let result =
            stage_and_validate(&crate_dir, &heal_root, CandidateRef { ts: 1_770_000_002, index: 0 }, &biting, &diag_for(&["src/lib.rs"], "router"), VALIDATE_TIMEOUT, &mut Vec::new())
                .await
                .unwrap();
        match result {
            StageResult::Validated { validation_tail } => assert!(
                validation_tail.contains("PROVEN: the patch's test FAILS"),
                "a biting test must be reported as proven:\n{validation_tail}"
            ),
            StageResult::Rejected { stage, detail } => {
                panic!("a biting test must validate, rejected at {stage}:\n{detail}")
            }
        }
    }

    /// A minimal diagnosis for the staged-gate tests: what the burst cited.
    fn diag_for(files: &[&str], subsystem: &str) -> Diagnosis {
        Diagnosis {
            signatures: vec!["synthetic burst".to_string()],
            files: files.iter().map(|f| (*f).to_string()).collect(),
            line_numbers: vec![],
            subsystem: subsystem.to_string(),
            log_context: String::new(),
            burst_lines: vec![],
            source_excerpts: vec![],
        }
    }

    // -- RESPONSIVENESS: the gate proves SOUND, not RESPONSIVE ---------------

    /// A crate with the DIAGNOSED subsystem in one file and an entirely separate
    /// latent bug in another. The burst names `src/audio.rs`; the patch below
    /// never goes near it.
    fn write_two_module_crate(dir: &Path) {
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"synthetic-split\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(dir.join("src").join("lib.rs"), "pub mod audio;\npub mod colorlab;\n")
            .unwrap();
        // The implicated subsystem. Untouched by the candidate under test.
        std::fs::write(
            dir.join("src").join("audio.rs"),
            "pub fn capture_gain() -> i32 {\n    1\n}\n",
        )
        .unwrap();
        // A DIFFERENT file with a DIFFERENT, genuine bug (no upper clamp) and
        // enough padding that its fix and its test cannot land in one hunk.
        std::fs::write(
            dir.join("src").join("colorlab.rs"),
            "pub fn clamp_pct(x: i32) -> i32 {\n\
             \x20   x\n\
             }\n\
             \n\
             pub fn pad_a() {}\n\
             pub fn pad_b() {}\n\
             pub fn pad_c() {}\n\
             pub fn pad_d() {}\n\
             pub fn pad_e() {}\n\
             pub fn pad_f() {}\n\
             \n\
             #[cfg(test)]\n\
             mod tests {\n\
             \x20   #[test]\n\
             \x20   fn baseline() {\n\
             \x20       assert_eq!(super::clamp_pct(5), 5);\n\
             \x20   }\n\
             }\n",
        )
        .unwrap();
    }

    /// THE GAP THIS WHOLE SECTION EXISTS FOR, CONSTRUCTED AND WALKED THROUGH.
    ///
    /// The burst implicates `src/audio.rs`, subsystem `audio`. This candidate
    /// patches `src/colorlab.rs` instead: a real bug, a real fix, and a
    /// regression test that genuinely FAILS when the fix is reversed. It
    /// therefore clears `patch`, `cargo check`, `cargo clippy -D warnings`,
    /// `cargo test` AND the mutation probe — every stage the gate had — and
    /// before this change it was proposed to the owner as the fix for an error
    /// burst it has nothing to do with, under the header "verdict: VALIDATED".
    ///
    /// The fix is NOT to reject it (a correct fix often lives one layer up from
    /// the line that screamed). It is to SAY SO. So this asserts both halves:
    /// the candidate still validates, and the validation tail — the same text
    /// the adversarial reviewer is handed and report.md embeds — now names it
    /// UNRELATED.
    #[tokio::test]
    async fn an_unresponsive_candidate_clears_every_gate() {
        let root = TempRoot::new("unresponsive");
        let crate_dir = root.0.join("daemon");
        let heal_root = root.0.join("state").join("heal");
        write_two_module_crate(&crate_dir);

        let elsewhere = "--- a/src/colorlab.rs\n\
                         +++ b/src/colorlab.rs\n\
                         @@ -1,4 +1,4 @@\n \
                         pub fn clamp_pct(x: i32) -> i32 {\n\
                         -    x\n\
                         +    x.min(100)\n \
                         }\n \
                         \n\
                         @@ -15,4 +15,9 @@\n \
                         \x20   fn baseline() {\n \
                         \x20       assert_eq!(super::clamp_pct(5), 5);\n \
                         \x20   }\n\
                         +\n\
                         +    #[test]\n\
                         +    fn clamps_above_one_hundred() {\n\
                         +        assert_eq!(super::clamp_pct(150), 100);\n\
                         +    }\n \
                         }\n";

        let diagnosis = Diagnosis {
            signatures: vec!["audio capture stopped".to_string()],
            files: vec!["src/audio.rs".to_string()],
            line_numbers: vec![2],
            subsystem: "audio".to_string(),
            log_context: String::new(),
            burst_lines: vec![],
            source_excerpts: vec![],
        };

        let result =
            stage_and_validate(&crate_dir, &heal_root, CandidateRef { ts: 1_780_000_001, index: 0 }, elsewhere, &diagnosis, VALIDATE_TIMEOUT, &mut Vec::new())
                .await
                .unwrap();

        let tail = match result {
            StageResult::Validated { validation_tail } => validation_tail,
            StageResult::Rejected { stage, detail } => panic!(
                "the gap must be reproduced, not hidden: this candidate is SOUND and must still \
                 validate — rejected at {stage}:\n{detail}"
            ),
        };
        // It really did clear all four stages — otherwise this test proves nothing.
        assert!(tail.contains("cargo clippy"), "never reached clippy:\n{tail}");
        assert!(
            tail.contains("PROVEN: the patch's test FAILS"),
            "the candidate must be genuinely mutation-proven, or it is not the \
             hard case:\n{tail}"
        );
        // ...and the gate now SAYS it answers a different question than the one asked.
        assert!(
            tail.contains("$ responsiveness probe"),
            "the validation tail carries no responsiveness verdict at all:\n{tail}"
        );
        assert!(
            tail.contains("UNRELATED — the patch edits src/colorlab.rs"),
            "a patch that touches neither the cited file nor the implicated subsystem \
             must be flagged UNRELATED:\n{tail}"
        );
        assert!(
            tail.contains("NOT A REJECTION"),
            "the flag must say plainly that it is advisory, or the next reader will \
             treat it as a verdict:\n{tail}"
        );
    }

    /// The other half of the pair: a patch that DOES answer the burst must not be
    /// smeared. A flag that fires on everything is not a flag.
    #[tokio::test]
    async fn a_responsive_candidate_is_marked_direct() {
        let root = TempRoot::new("responsive");
        let crate_dir = root.0.join("daemon");
        let heal_root = root.0.join("state").join("heal");
        write_clamp_crate(&crate_dir);

        let biting = format!(
            "{CLAMP_FIX_HUNK}\
             @@ -15,4 +15,9 @@\n \
             \x20   fn baseline() {{\n \
             \x20       assert_eq!(super::clamp_pct(5), 5);\n \
             \x20   }}\n\
             +\n\
             +    #[test]\n\
             +    fn clamps_above_one_hundred() {{\n\
             +        assert_eq!(super::clamp_pct(150), 100);\n\
             +    }}\n \
             }}\n"
        );
        let result = stage_and_validate(
            &crate_dir,
            &heal_root,
            CandidateRef { ts: 1_780_000_002, index: 0 },
            &biting,
            &diag_for(&["src/lib.rs"], "router"),
            VALIDATE_TIMEOUT,
            &mut Vec::new(),
        )
        .await
        .unwrap();
        match result {
            StageResult::Validated { validation_tail } => assert!(
                validation_tail.contains("DIRECT — the patch edits src/lib.rs"),
                "a patch on the cited file must read DIRECT:\n{validation_tail}"
            ),
            StageResult::Rejected { stage, detail } => panic!("rejected at {stage}:\n{detail}"),
        }
    }

    /// The five verdicts, each on its own evidence. Pure — no build, no cloud.
    #[test]
    fn responsiveness_grades_each_kind_of_evidence() {
        let hdr = |f: &str| format!("--- a/{f}\n+++ b/{f}\n@@ -1,1 +1,1 @@\n-a\n+b\n");
        let full = Diagnosis {
            signatures: vec!["audio capture stopped".to_string()],
            files: vec!["src/nexus.rs".to_string()],
            line_numbers: vec![],
            subsystem: "audio".to_string(),
            log_context: String::new(),
            burst_lines: vec![],
            source_excerpts: vec![],
        };
        assert_eq!(responsiveness(&full, &hdr("src/nexus.rs")).0, Responsiveness::Direct);
        assert_eq!(responsiveness(&full, &hdr("src/audio.rs")).0, Responsiveness::Subsystem);
        assert_eq!(
            responsiveness(&full, &hdr("src/audio/mixer.rs")).0,
            Responsiveness::Subsystem,
            "a file under src/<subsystem>/ belongs to that subsystem"
        );
        assert_eq!(responsiveness(&full, &hdr("src/colorlab.rs")).0, Responsiveness::Unrelated);
        // The signature route: a different file, but it carries the burst's text.
        let sig_diff = "--- a/src/pipeline.rs\n+++ b/src/pipeline.rs\n@@ -1,1 +1,2 @@\n \
                        ok\n+    error!(\"audio capture stopped\");\n";
        assert_eq!(responsiveness(&full, sig_diff).0, Responsiveness::Signature);
        // Nothing to match on -> no opinion, and NOT a warning.
        let blind = Diagnosis {
            signatures: vec!["boom".to_string()],
            files: vec![],
            line_numbers: vec![],
            subsystem: "unknown".to_string(),
            log_context: String::new(),
            burst_lines: vec![],
            source_excerpts: vec![],
        };
        assert_eq!(
            responsiveness(&blind, &hdr("src/colorlab.rs")).0,
            Responsiveness::Indeterminate,
            "an empty diagnosis must not be reported as an unrelated patch"
        );
    }

    /// A patch scores DIRECT only when it WRITES the cited file. Merely naming it
    /// in a context line, an added comment or a doc string is not a fix, and
    /// `extract_source_files` — which scans the whole text — would have scored it
    /// DIRECT for free, which is how a heuristic becomes a rubber stamp.
    #[test]
    fn only_the_plus_plus_plus_header_counts_as_touching_a_file() {
        let d = Diagnosis {
            signatures: vec![],
            files: vec!["src/router.rs".to_string()],
            line_numbers: vec![],
            subsystem: "router".to_string(),
            log_context: String::new(),
            burst_lines: vec![],
            source_excerpts: vec![],
        };
        let name_dropping = "--- a/src/colorlab.rs\n\
                             +++ b/src/colorlab.rs\n\
                             @@ -1,1 +1,2 @@\n \
                             fn f() {}\n\
                             +// see also src/router.rs for the real dispatch\n";
        assert_eq!(patched_files(name_dropping), vec!["src/colorlab.rs".to_string()]);
        assert_eq!(
            responsiveness(&d, name_dropping).0,
            Responsiveness::Unrelated,
            "mentioning the cited file in a comment must not score as fixing it"
        );
        // ...and the boundary rule: src/tools.rs is not a cited `s.rs`.
        assert!(!same_file("src/tools.rs", "s.rs"));
        assert!(same_file("daemon/src/router.rs", "src/router.rs"));
        // ...and a subsystem match is component-exact, not a substring.
        assert!(!path_in_subsystem("src/audiobook.rs", "audio"));
        assert!(path_in_subsystem("src/audio.rs", "audio"));
    }

    /// The verdict has to REACH the humans and the reviewer, not just exist.
    /// report.md is what a person reads before running apply_heal.sh.
    #[test]
    fn the_report_header_carries_the_responsiveness_verdict() {
        let d = Diagnosis {
            signatures: vec!["audio capture stopped".to_string()],
            files: vec!["src/nexus.rs".to_string()],
            line_numbers: vec![],
            subsystem: "audio".to_string(),
            log_context: String::new(),
            burst_lines: vec![],
            source_excerpts: vec![],
        };
        let w = Survivor {
            index: 1,
            diff: "--- a/src/colorlab.rs\n+++ b/src/colorlab.rs\n@@ -1,1 +1,1 @@\n-a\n+b\n"
                .to_string(),
            files: vec!["src/colorlab.rs".to_string()],
            validation_tail: "tail".to_string(),
            review_verdict: "ok".to_string(),
            confidence: 0.9,
            reviewed: true,
            size: 2,
        };
        let report = render_report(7, "m", &d, &w);
        assert!(
            report.contains("- responsiveness: UNRELATED"),
            "report.md must state responsiveness beside VALIDATED:\n{report}"
        );
        // And the adversarial reviewer must be told what the line means, or it
        // will read UNRELATED as noise in a validation dump.
        let rp = review_prompt(&d, &w.diff, "…RESPONSIVENESS…");
        assert!(
            rp.contains("RESPONSIVENESS line") && rp.contains("BLIND to the diagnosis"),
            "the reviewer is never told the gates cannot see the diagnosis:\n{rp}"
        );
    }

    // -- the staged-validation budget: exhaustion is not a verdict ------------

    /// A blown deadline must be classifiable from the error `run_cargo` really
    /// produces — not from a string this test invents. So this drives the REAL
    /// producer with a zero budget and asserts the REAL classifier agrees.
    #[tokio::test]
    async fn a_blown_cargo_deadline_is_recognizable_as_one() {
        let root = TempRoot::new("deadline");
        // `expect_err` would require CmdOutput: Debug; match instead so the
        // struct's (potentially huge) captured output stays out of Debug.
        let err = match run_cargo(&root.0, &["--version"], Duration::ZERO).await {
            Err(e) => e,
            Ok(_) => panic!("a zero budget must not succeed"),
        };
        assert!(
            is_deadline_error(&err),
            "run_cargo's timeout message and its classifier have drifted apart: {err}"
        );
        // ...and an error that is NOT a timeout must not be mistaken for one.
        assert!(!is_deadline_error(&anyhow::anyhow!("cargo not found on this machine")));
    }

    /// EXHAUSTION MUST NOT LOOK LIKE A REJECTION ON THE MERITS. Filed under the
    /// stage it never reached, a blown budget reported as
    /// `heal.rejected{stage:"test"}` + "no candidate passed the staged gates" —
    /// which an operator reads as "the model drafted three bad patches".
    #[test]
    fn deadline_exhaustion_reports_itself_as_a_deadline() {
        let timeout = anyhow::anyhow!("cargo test {CARGO_DEADLINE_MARKER} (0s remained)");
        match stage_failure("test", "…", &timeout, VALIDATE_TIMEOUT) {
            StageResult::Rejected { stage, detail } => {
                assert_eq!(stage, "deadline", "a blown budget must not be filed under `test`");
                assert!(detail.contains("never judged on its merits"), "{detail}");
            }
            StageResult::Validated { .. } => panic!("a blown budget is not a validation"),
        }
        // A genuine infrastructure failure still reports under its stage.
        match stage_failure("check", "…", &anyhow::anyhow!("cargo not found"), VALIDATE_TIMEOUT) {
            StageResult::Rejected { stage, .. } => assert_eq!(stage, "check"),
            StageResult::Validated { .. } => panic!("unexpected"),
        }
        // And the pre-stage guard names the stage that never started.
        match budget_exhausted("clippy", "…", None, VALIDATE_TIMEOUT, BudgetStop::Candidate) {
            StageResult::Rejected { stage, detail } => {
                assert_eq!(stage, "deadline");
                assert!(detail.contains("before `cargo clippy` could start"), "{detail}");
            }
            StageResult::Validated { .. } => panic!("unexpected"),
        }
    }

    /// ...and the rejection REPORT has to say the same thing. "No candidate
    /// passed the gates" is a statement about the patches; when the budget ran
    /// out it is a statement about the machine, and they are opposite
    /// instructions to whoever reads it.
    #[test]
    fn the_rejection_report_separates_a_blown_budget_from_bad_patches() {
        let all_timed_out = rejection_summary(3, 0);
        assert!(
            all_timed_out.contains("NO CANDIDATE WAS EVER JUDGED"),
            "{all_timed_out}"
        );
        assert!(
            !all_timed_out.contains("No candidate passed the staged"),
            "a budget failure must not be phrased as a verdict on the patches: {all_timed_out}"
        );
        let genuinely_bad = rejection_summary(0, 3);
        assert!(genuinely_bad.contains("No candidate passed the staged"), "{genuinely_bad}");
        // Mixed: say both, and say how many were never judged.
        let mixed = rejection_summary(1, 2);
        assert!(mixed.contains("No candidate passed the staged"), "{mixed}");
        assert!(mixed.contains("1 of them never finished"), "{mixed}");
    }

    /// Classification is by POSITION, not by the word `#[test]`: the fix hunk
    /// here contains no test marker and the test hunk does, but what decides is
    /// which side of `#[cfg(test)]` each added line lands on.
    #[test]
    fn split_test_hunks_separates_by_the_cfg_test_boundary() {
        let diff = "--- a/src/lib.rs\n\
                    +++ b/src/lib.rs\n\
                    @@ -1,2 +1,2 @@\n\
                    -    x\n\
                    +    x.min(100)\n\
                    @@ -20,1 +20,3 @@\n\
                    +    #[test]\n\
                    +    fn t() {}\n \
                    }\n";
        let (tests, fixes) = split_test_hunks(diff, &|_| 12).unwrap();
        assert!(fixes.contains("x.min(100)") && !fixes.contains("fn t()"), "fixes:\n{fixes}");
        assert!(tests.contains("fn t()") && !tests.contains("x.min(100)"), "tests:\n{tests}");
        // Each half must carry the file header, or it is not a usable patch.
        assert!(fixes.starts_with("--- a/src/lib.rs\n+++ b/src/lib.rs\n"), "{fixes}");
        assert!(tests.starts_with("--- a/src/lib.rs\n+++ b/src/lib.rs\n"), "{tests}");
    }

    /// A hunk holding the fix AND the test cannot be separated. Guessing would
    /// mean skipping the probe on the very shape it most needs to check, so this
    /// refuses and the gate reports itself inconclusive.
    #[test]
    fn split_test_hunks_refuses_a_hunk_that_spans_both_sides() {
        let diff = "--- a/src/lib.rs\n\
                    +++ b/src/lib.rs\n\
                    @@ -1,2 +1,20 @@\n\
                    -    x\n\
                    +    x.min(100)\n\
                    +    #[test]\n\
                    +    fn t() {}\n";
        let err = split_test_hunks(diff, &|_| 3).unwrap_err();
        assert!(err.contains("cannot be separated"), "{err}");
    }

    /// No test module in the patched file means no test-side hunks at all, so
    /// the gate reports UNPROVEN rather than silently claiming a proof.
    #[test]
    fn split_test_hunks_puts_everything_fix_side_when_there_is_no_test_module() {
        let diff = "--- a/src/lib.rs\n\
                    +++ b/src/lib.rs\n\
                    @@ -1,2 +1,2 @@\n\
                    -    x\n\
                    +    x.min(100)\n";
        let (tests, fixes) = split_test_hunks(diff, &|_| usize::MAX).unwrap();
        assert!(tests.is_empty(), "tests:\n{tests}");
        assert!(fixes.contains("x.min(100)"));
    }

    /// The gate rejects a candidate whose test survives its fix's removal. If the
    /// DRAFTING prompt does not say so, the model keeps producing exactly those
    /// candidates and every cycle burns a cloud draft to be thrown away. The
    /// requirement has to reach the model, not just the validator.
    #[test]
    fn the_draft_prompt_demands_a_test_that_fails_without_the_fix() {
        let d = Diagnosis {
            subsystem: "audio".to_string(),
            signatures: vec!["capture stopped".to_string()],
            log_context: "…".to_string(),
            files: vec!["src/nexus.rs".to_string()],
            line_numbers: vec![],
            burst_lines: vec![],
            source_excerpts: vec![],
        };
        let p = draft_prompt(&d, 3);
        assert!(
            p.contains("FAILS without your fix"),
            "the drafter is never told its test must bite:\n{p}"
        );
        assert!(
            p.contains("REJECTED as unproven"),
            "the drafter is never told the consequence:\n{p}"
        );
        // And the shape requirement, without which the probe cannot separate them.
        assert!(
            p.contains("SEPARATE hunks"),
            "the drafter is never told to keep fix and test in separate hunks:\n{p}"
        );
    }

    struct TempRoot(PathBuf);
    impl TempRoot {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "darwin-heal-test-{}-{tag}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TempRoot(dir)
        }
    }
    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn burst_scan_for_lib() -> LogScan {
        let now = Utc::now().to_rfc3339();
        let tail = (0..5)
            .map(|_| format!("{now} ERROR darwin_core::router: compile failed in src/lib.rs:2 error=cannot find value `y`"))
            .collect::<Vec<_>>()
            .join("\n");
        scan_tail(tail)
    }

    /// THE v2 heart (no cloud): three drafted candidates — one that does not
    /// apply, one that applies but still fails `cargo check`, and one that
    /// truly fixes the planted bug — flow through stage+validate-EACH, the
    /// mock adversarial review, survivor selection, and proposal rendering.
    /// Only the real fix survives, and the source tree is never touched.
    #[tokio::test]
    async fn full_pipeline_via_mock_brain_selects_the_validated_fix() {
        let root = TempRoot::new("mock-e2e");
        let crate_dir = root.0.join("daemon");
        let heal_root = root.0.join("state").join("heal");
        write_synthetic_crate(&crate_dir);
        let ts = 1_760_000_010u64;

        // Candidate 1: wrong context -> rejects at `patch`.
        // Candidate 2: applies but `z` is undefined -> rejects at `check`.
        // Candidate 3: the real fix `x * 2` -> validates.
        let draft = "=== CANDIDATE 1 ===\n\
            --- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n pub fn triple(x: i32) -> i32 {\n-    x * q\n+    x * 3\n }\n\
            === CANDIDATE 2 ===\n\
            --- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n pub fn double(x: i32) -> i32 {\n-    x * y\n+    x * z\n }\n\
            === CANDIDATE 3 ===\n\
            --- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n pub fn double(x: i32) -> i32 {\n-    x * y\n+    x * 2\n }\n";
        let brain = MockBrain {
            draft: draft.to_string(),
            reviews: vec![("x * 2".to_string(), 0.93)],
        };

        let result =
            run_attempt(&crate_dir, &heal_root, ts, "mock-model", &brain, &burst_scan_for_lib())
                .await;

        let (diff, report, confidence, extra) = match result {
            AttemptResult::Proposed { diff, report, confidence, extra, .. } => {
                (diff, report, confidence, extra)
            }
            AttemptResult::Rejected { stage, report, .. } => {
                panic!("expected a proposal, rejected at {stage}:\n{report}")
            }
            AttemptResult::Aborted { stage } => panic!("aborted at {stage}"),
        };

        // The winning diff is the real fix; review confidence flowed through.
        assert!(diff.contains("x * 2"), "the validated candidate must win:\n{diff}");
        assert!((confidence - 0.93).abs() < 1e-9);
        assert!(report.contains("scripts/apply_heal.sh 1760000010"));
        // candidates.md records all three with their fates.
        assert!(extra.candidates_md.contains("Candidate #1 — DISCARDED"));
        assert!(extra.candidates_md.contains("Candidate #2 — DISCARDED"));
        assert!(extra.candidates_md.contains("Candidate #3 — VALIDATED"));
        // diagnosis.json is real JSON naming the subsystem.
        assert!(extra.diagnosis_json.contains("\"subsystem\": \"router\""));

        // SAFETY: the source tree was never patched (propose mode).
        assert!(
            std::fs::read_to_string(crate_dir.join("src").join("lib.rs"))
                .unwrap()
                .contains("x * y"),
            "propose mode must never touch the source tree"
        );
        // Each candidate staged into its OWN dir — and every one of those trees is
        // REMOVED again. They used to be left behind forever: three per pass, each a
        // copy of daemon/src + Cargo.toml + Cargo.lock, on an unattended loop. The
        // artifacts that matter (the diff and the captured validation tail) are carried
        // out in StageResult, so the tree itself is pure scratch.
        for c in 0..3 {
            assert!(
                !heal_root.join(staging_dir_name(ts, c)).exists(),
                "candidate {c}'s staging tree was left on disk; the autonomy path grows \
                 the state dir without bound"
            );
        }
    }

    /// When EVERY candidate fails a gate, the attempt is a rejection (no
    /// proposal, no source touched) and candidates.md still records each fate.
    #[tokio::test]
    async fn full_pipeline_via_mock_brain_rejects_when_no_candidate_validates() {
        let root = TempRoot::new("mock-reject");
        let crate_dir = root.0.join("daemon");
        let heal_root = root.0.join("state").join("heal");
        write_synthetic_crate(&crate_dir);

        // Both candidates apply but leave an undefined binding -> cargo check fails.
        let draft = "=== CANDIDATE 1 ===\n\
            --- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n pub fn double(x: i32) -> i32 {\n-    x * y\n+    x * z\n }\n\
            === CANDIDATE 2 ===\n\
            --- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n pub fn double(x: i32) -> i32 {\n-    x * y\n+    x * w\n }\n";
        let brain = MockBrain { draft: draft.to_string(), reviews: vec![] };

        let result =
            run_attempt(&crate_dir, &heal_root, 1_760_000_011, "mock-model", &brain, &burst_scan_for_lib())
                .await;

        match result {
            AttemptResult::Rejected { stage, .. } => assert_eq!(stage, "check"),
            other => panic!("expected rejection at cargo check, got {:?}", std::mem::discriminant(&other)),
        }
        assert!(
            std::fs::read_to_string(crate_dir.join("src").join("lib.rs"))
                .unwrap()
                .contains("x * y"),
            "a fully-rejected attempt must never touch the source tree"
        );
    }

    /// A non-applying diff rejects at the `patch` stage before any cargo run.
    #[tokio::test]
    async fn staging_pipeline_rejects_a_non_applying_diff() {
        let root = TempRoot::new("badhunk");
        let crate_dir = root.0.join("daemon");
        let heal_root = root.0.join("state").join("heal");
        write_synthetic_crate(&crate_dir);

        let wrong_context_diff = "--- a/src/lib.rs\n\
                                  +++ b/src/lib.rs\n\
                                  @@ -1,3 +1,3 @@\n \
                                  pub fn triple(x: i32) -> i32 {\n\
                                  -    x * q\n\
                                  +    x * 3\n \
                                  }\n";
        let result = stage_and_validate(
            &crate_dir,
            &heal_root,
            CandidateRef { ts: 1_760_000_002, index: 0 },
            wrong_context_diff,
            &diag_for(&["src/lib.rs"], "router"),
            VALIDATE_TIMEOUT,
            &mut Vec::new(),
        )
        .await
        .unwrap();
        match result {
            StageResult::Rejected { stage, .. } => assert_eq!(stage, "patch"),
            StageResult::Validated { .. } => panic!("a failed hunk must reject"),
        }
    }

    /// A diff that applies but does NOT fix the planted compile bug must be
    /// rejected by the real `cargo check` in staging (the gate is unchanged).
    #[tokio::test]
    async fn staging_pipeline_rejects_when_check_still_fails() {
        let root = TempRoot::new("stillbroken");
        let crate_dir = root.0.join("daemon");
        let heal_root = root.0.join("state").join("heal");
        write_synthetic_crate(&crate_dir);

        let useless_diff = "--- a/src/lib.rs\n\
                            +++ b/src/lib.rs\n\
                            @@ -1,3 +1,3 @@\n \
                            pub fn double(x: i32) -> i32 {\n\
                            -    x * y\n\
                            +    x * z\n \
                            }\n";
        let result = stage_and_validate(
            &crate_dir,
            &heal_root,
            CandidateRef { ts: 1_760_000_003, index: 0 },
            useless_diff,
            &diag_for(&["src/lib.rs"], "router"),
            VALIDATE_TIMEOUT,
            &mut Vec::new(),
        )
        .await
        .unwrap();
        match result {
            StageResult::Rejected { stage, detail } => {
                assert_eq!(stage, "check");
                assert!(detail.contains("cargo check"), "captured output missing:\n{detail}");
            }
            StageResult::Validated { .. } => panic!("cargo check must catch the surviving bug"),
        }
    }

    /// The staging path still validates a genuine fix end to end (real patch,
    /// real cargo, tempdir only) — the v1 guarantee, preserved.
    #[tokio::test]
    async fn staging_pipeline_validates_a_planted_fix() {
        let root = TempRoot::new("e2e");
        let crate_dir = root.0.join("daemon");
        let heal_root = root.0.join("state").join("heal");
        write_synthetic_crate(&crate_dir);
        let fixing = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n pub fn double(x: i32) -> i32 {\n-    x * y\n+    x * 2\n }\n";
        let result = stage_and_validate(
            &crate_dir,
            &heal_root,
            CandidateRef { ts: 1_760_000_001, index: 0 },
            fixing,
            &diag_for(&["src/lib.rs"], "router"),
            VALIDATE_TIMEOUT,
            &mut Vec::new(),
        )
        .await
        .expect("staging infrastructure must work");
        match result {
            StageResult::Validated { validation_tail } => {
                assert!(validation_tail.contains("cargo"));
            }
            StageResult::Rejected { stage, detail } => panic!("expected validation, rejected at {stage}:\n{detail}"),
        }
        // Reaching Validated above already proves the STAGED copy was patched — the
        // planted fix cannot compile-and-pass otherwise. What remains to assert is the
        // live source, and that the scratch tree is gone.
        assert!(std::fs::read_to_string(crate_dir.join("src").join("lib.rs")).unwrap().contains("x * y"));
        // And the staging tree is GONE. It used to be left behind forever.
        assert!(
            !heal_root.join(staging_dir_name(1_760_000_001, 0)).exists(),
            "the staging tree survived stage_and_validate"
        );
    }

    /// (6 of contract) THE HEAL DRILL via the REAL cloud. #[ignore] by default
    /// — the ONLY cloud path in this module, run explicitly by the verifier:
    ///   cargo test --release heal_drill_real_cloud -- --ignored --nocapture
    /// (or `darwind --heal-drill`). It heals a planted fault in a TEMP crate,
    /// proving diagnose -> Opus draft -> stage -> validate -> review -> propose
    /// end to end. Skips gracefully (passes) when no API key is present so an
    /// offline `--ignored` run does not spuriously fail.
    #[tokio::test]
    #[ignore = "real cloud spend; run by the verifier with --ignored"]
    async fn heal_drill_real_cloud() {
        if anthropic::resolve_api_key().await.is_none() {
            eprintln!("heal_drill_real_cloud: no API key resolved; skipping (run with the key set)");
            return;
        }
        let model = "claude-opus-5";
        let dir = run_heal_drill(model).await.expect("heal drill must produce a proposal");
        assert!(dir.join("patch.diff").exists(), "drill must write patch.diff");
        assert!(dir.join("report.md").exists());
        assert!(dir.join("diagnosis.json").exists());
        assert!(dir.join("candidates.md").exists());
        assert!(dir.join("review.md").exists());
        let report = std::fs::read_to_string(dir.join("report.md")).unwrap();
        assert!(report.contains("VALIDATED"), "drill proposal must be validated:\n{report}");
        // Clean up the throwaway sandbox (it lives under tmp/darwin-heal-drill-*).
        if let Some(sandbox) = dir.ancestors().find(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("darwin-heal-drill-"))
        }) {
            let _ = std::fs::remove_dir_all(sandbox);
        }
    }

    // -- the staged gate must be able to compile the REAL crate ---------------

    /// REGRESSION: staging the REAL daemon crate carries every input the staged
    /// `cargo test` gate needs.
    ///
    /// `stage_sources` used to copy exactly `src/` + `Cargo.toml` + `Cargo.lock`.
    /// `cargo check` neither links nor expands `#[cfg(test)]` macros, so the first
    /// gate passed — and the second could not COMPILE, for two independent reasons:
    /// three test-only `include_str!("../../…")` reach outside the crate, and
    /// `build.rs` + `csrc/` (which produce the static lib `power.rs` links with
    /// `#[link(name = "darwin_thermal_shim")]`) were never staged. Every candidate
    /// was rejected at stage "test" no matter what the patch was, and the report
    /// blamed the model. EVERY other test in this module runs against a synthetic
    /// one-file crate, which is exactly why none of them noticed.
    #[test]
    fn staging_the_real_crate_carries_every_compilation_input() {
        let root = TempRoot::new("stage-real-crate");
        let staging = root.0.join("staging");
        let real = Path::new(env!("CARGO_MANIFEST_DIR"));
        let crate_dir = stage_sources(real, &staging).expect("staging the real crate");

        assert!(crate_dir.join("Cargo.toml").is_file(), "the manifest is staged");
        assert!(crate_dir.join("src").join("heal.rs").is_file(), "src/ is staged");
        assert!(
            crate_dir.join("build.rs").is_file(),
            "build.rs must be staged — it produces the static lib power.rs links against"
        );
        assert!(
            crate_dir.join("csrc").join("thermal_shim.m").is_file(),
            "csrc/ must be staged — build.rs compiles it"
        );
        assert!(
            !crate_dir.join("target").exists(),
            "target/ must NEVER be staged (gigabytes, and the staged build makes its own)"
        );

        // Every out-of-crate include the staged sources name must resolve under the
        // staging root, or `cargo test` cannot expand it.
        let includes = out_of_crate_includes(&crate_dir.join("src"), &crate_dir, real.parent().unwrap());
        assert!(
            includes.len() >= 3,
            "the real crate has out-of-crate include_str! targets; found {includes:?}"
        );
        for rel in &includes {
            assert!(
                staging.join(rel).is_file(),
                "the staged tree is missing {rel:?} — the staged `cargo test` cannot expand \
                 its include_str! and rejects every candidate at stage \"test\""
            );
        }
    }

    /// The out-of-crate include scanner finds a `../../` target and ignores an
    /// in-crate one (so the mirror step stays minimal and deterministic).
    #[test]
    fn out_of_crate_include_scan_finds_only_escaping_literals() {
        let root = TempRoot::new("include-scan");
        let crate_dir = root.0.join("daemon");
        std::fs::create_dir_all(crate_dir.join("src").join("deep")).unwrap();
        std::fs::create_dir_all(root.0.join("config")).unwrap();
        std::fs::create_dir_all(root.0.join("inference")).unwrap();
        std::fs::write(root.0.join("config").join("darwin.toml"), "# shipped").unwrap();
        std::fs::write(root.0.join("inference").join("server.py"), "# server").unwrap();
        std::fs::write(
            crate_dir.join("src").join("a.rs"),
            "const A: &str = include_str!(\"../../config/darwin.toml\");\n\
             const B: &str = include_str!(\"fixtures/in_crate.txt\");\n\
             // a COMMENT naming include_str!(\"../../nowhere/absent.txt\") is not an input\n",
        )
        .unwrap();
        std::fs::write(
            crate_dir.join("src").join("deep").join("b.rs"),
            "const C: &[u8] = include_bytes!(\"../../../inference/server.py\");\n",
        )
        .unwrap();
        let found = out_of_crate_includes(&crate_dir.join("src"), &crate_dir, &root.0);
        assert!(found.contains(&PathBuf::from("config/darwin.toml")), "{found:?}");
        assert!(found.contains(&PathBuf::from("inference/server.py")), "{found:?}");
        assert_eq!(
            found.len(),
            2,
            "an in-crate include and a non-existent (comment-only) target are not mirrored: {found:?}"
        );
    }

    /// REGRESSION: a propose writes ALL FIVE documented artifacts.
    ///
    /// The live propose path used to discard `extra` (run_pipeline's `..` plus a
    /// wrapper hard-coding `None`), so diagnosis.json / candidates.md / review.md
    /// were computed and then thrown away while the module doc, ARCHITECTURE.md and
    /// the HUD all promised them. `extra` is now non-optional, so that regression
    /// cannot compile; this pins the write itself.
    #[test]
    fn a_proposal_writes_all_five_documented_artifacts() {
        let root = TempRoot::new("propose-artifacts");
        let dir = root.0.join("proposals");
        let ts = 1_760_000_500u64;
        let extra = ProposalArtifacts {
            diagnosis_json: "{\"subsystem\":\"audio\"}".to_string(),
            candidates_md: "# candidates\nCandidate #1 — VALIDATED".to_string(),
            review_md: "# review\nCONFIDENCE: 0.9".to_string(),
        };
        assert!(write_proposal_artifacts(&dir, ts, "the diff", "the report", &extra));
        for name in [
            "patch.diff",
            "report.md",
            "diagnosis.json",
            "candidates.md",
            "review.md",
        ] {
            assert!(
                dir.join(ts.to_string()).join(name).is_file(),
                "a propose-mode proposal must contain {name}; the operator is told it is there"
            );
        }
    }
    /// THE STAGED SUITE MUST BE ABLE TO RUN, or the gate is not a gate.
    ///
    /// Staging mirrored only the paths named by `include_str!` — a COMPILE-time
    /// scan. `cargo check` therefore passed while `cargo test` failed 29 tests in
    /// staging that pass in the real tree, so `select_winner` returned None for
    /// every candidate and every episode ended `heal.rejected{stage:"test"}`. The
    /// report said "No candidate passed the staged cargo check + cargo test
    /// gates", which an operator reads as "the model drafted three bad patches".
    ///
    /// Staged and run, the failures named themselves:
    ///     cannot read <staging>/daemon/../config/agents.toml: No such file
    ///     app registered        (empty registry: no apps/*/manifest.toml)
    #[test]
    fn staging_mirrors_the_repo_inputs_the_suite_reads_at_runtime() {
        let root = std::path::PathBuf::from(format!(
            "/private/tmp/jrv-healmirror-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                % 1_000_000
        ));
        let repo = root.join("repo");
        let src = repo.join("daemon");
        std::fs::create_dir_all(src.join("src")).unwrap();
        std::fs::write(src.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        std::fs::write(src.join("src/lib.rs"), "").unwrap();
        // The runtime inputs a staged suite reads.
        std::fs::create_dir_all(repo.join("config")).unwrap();
        std::fs::write(repo.join("config/agents.toml"), "# roster\n").unwrap();
        std::fs::create_dir_all(repo.join("scripts")).unwrap();
        std::fs::write(repo.join("scripts/apply_forge.sh"), "#!/bin/sh\n").unwrap();
        std::fs::create_dir_all(repo.join("apps/global-scan")).unwrap();
        std::fs::write(repo.join("apps/global-scan/manifest.toml"), "[app]\n").unwrap();
        std::fs::write(repo.join("apps/global-scan/feeds.toml"), "[[feed]]\n").unwrap();
        // ...and something that must NOT be staged: an app's source tree.
        std::fs::write(repo.join("apps/global-scan/main.py"), "print(1)\n").unwrap();
        std::fs::write(repo.join("apps/global-scan/test_global_scan.py"), "").unwrap();

        let staging = root.join("staging");
        stage_sources(&src, &staging).expect("staging succeeds");

        for rel in [
            "config/agents.toml",
            "scripts/apply_forge.sh",
            "apps/global-scan/manifest.toml",
            "apps/global-scan/feeds.toml",
        ] {
            assert!(
                staging.join(rel).is_file(),
                "{rel} must be mirrored — the staged suite reads it at runtime"
            );
        }
        assert!(
            staging.join("apps/global-scan/main.py").is_file(),
            "the app ENTRY must be staged: the manifest suite asserts a tool-exposing \
             app has one, and fails 'tool-exposing app has no main.py' without it"
        );
        assert!(
            !staging.join("apps/global-scan/test_global_scan.py").exists(),
            "the REST of an app must not be staged — manifests and the entry only"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// THE STAGED BUILD MUST BE ISOLATED, NOT MERELY "IN THE STAGING DIR".
    ///
    /// `run_cargo` runs in the staged CRATE, but it also inherited the daemon's
    /// environment — so CARGO_TARGET_DIR (or `.cargo/config.toml`
    /// `build.target-dir`) put the ARTIFACTS somewhere else, shared. Two staged
    /// crates are byte-copies of daemon/ with the same package name and version;
    /// measured on this repo, they resolved to the SAME artifact hash and one
    /// candidate's compiled test binary answered another candidate's gate.
    ///
    /// This is scoped to `run_cargo`'s own body (the needle appears in this test
    /// too) and pinned to ONE definition, so the guard cannot window onto its own
    /// text, and it requires the CALL on a non-comment line rather than the name.
    #[test]
    fn the_staged_cargo_pins_its_target_dir_inside_the_staging_tree() {
        let src = include_str!("heal.rs");
        let anchor = concat!("async fn ", "run_cargo(");
        assert_eq!(
            src.matches(anchor).count(),
            1,
            "`{anchor}` must name exactly ONE site \u{2014} the definition; with more, this \
             guard can window onto the wrong region and pass without reading the spawn"
        );
        let start = src.find(anchor).expect("run_cargo moved; re-point this guard");
        let rest = &src[start..];
        let end = rest
            .find("\n}\n")
            .map(|e| e + 2)
            .expect("run_cargo must close at column 0 so this window is bounded");
        let body = &rest[..end];
        assert!(
            body.lines()
                .map(str::trim_start)
                .any(|l| !l.starts_with("//") && l.contains(".env(\"CARGO_TARGET_DIR\"")),
            "the staged cargo must PIN CARGO_TARGET_DIR into the staging tree on a code \
             line \u{2014} inheriting it lets one staged candidate's artifacts answer \
             another's gate:\n{body}"
        );
    }

    /// THE SKIP FLAGS MUST REACH THE TEST HARNESS, NOT CARGO.
    ///
    /// `--skip` is a libtest flag. Passed as `cargo test --skip X` it produces
    ///     error: unexpected argument '--skip' found
    /// and the gate rejects the candidate for a reason that has nothing to do with
    /// the patch — the same failure mode this whole fix exists to remove. The
    /// separator is load-bearing.
    #[test]
    fn the_staged_test_gate_passes_skips_to_the_harness_not_to_cargo() {
        let src = include_str!("heal.rs");
        let region = src
            .split("let mut test_args: Vec<&str> = vec![")
            .nth(1)
            .expect("the staged test-gate arg builder must exist");
        let head = &region[..region.find("];").unwrap_or(region.len())];
        assert!(
            head.contains("\"--\""),
            "the arg list must open with a `--` separator before any --skip: {head}"
        );
        // ...and every name skipped must be a real test in this crate, or the skip
        // is silently covering nothing (or, worse, a typo hiding a real failure).
        // THE LIST ITSELF, NOT A HAND-WRITTEN COPY OF IT. This iterated a literal
        // duplicate of UNRUNNABLE_IN_STAGE, so the names actually handed to `--skip`
        // were never the names checked: a FOURTH entry added to the real list — or an
        // existing one retyped there — was not existence-checked at all, which is the
        // very "a skip that names nothing is a hole in the gate" case below. The
        // sibling parity test already iterates UNRUNNABLE_IN_STAGE; this one must too.
        // PROVED: appending "heal::tests::this_test_does_not_exist" to
        // UNRUNNABLE_IN_STAGE left this test green.
        for name in UNRUNNABLE_IN_STAGE {
            // THE DEFINITION, NOT THE NAME — this SELF-MATCHED. The leaf already
            // occurs in heal.rs twice: in UNRUNNABLE_IN_STAGE itself and in this
            // test's own array above. `src.contains(leaf)` was therefore satisfied
            // by the very skip list it exists to validate, so renaming or deleting
            // a skipped test left the skip naming nothing, the gate holed, and this
            // guard green. PROVED: renaming
            // forge::tests::apply_forge_accepts_legit_multiline_manifest left this
            // test passing.
            let (path, leaf) =
                name.rsplit_once("::").expect("a skip name is <module>::tests::<leaf>");
            let owner = match path.split("::").next().unwrap() {
                "heal" => src,
                "forge" => include_str!("forge.rs"),
                other => panic!("skip {name} names module {other}, which this guard cannot read"),
            };
            let def = format!("fn {leaf}(");
            assert!(
                owner.contains(def.as_str()),
                "skipped test {name} is not DEFINED (`{def}` is absent from the {path} module) — \
                 a skip that names nothing is a hole in the gate"
            );
        }
    }

    /// THE TWO GATES MUST SKIP THE SAME TESTS.
    ///
    /// The daemon proves a candidate with its own staged `cargo test`
    /// (UNRUNNABLE_IN_STAGE, above). `scripts/apply_heal.sh` re-proves it before
    /// touching live sources — that second gate is the point, since a patch is
    /// drafted at one commit and applied at another.
    ///
    /// If the two lists drift, a patch the daemon PROVED fails on apply for a
    /// reason that is not the patch, and the operator is told "cargo test failed
    /// in staging" about a candidate that already passed. That is exactly the
    /// misleading failure this whole fix removes, reintroduced at the next hop.
    #[test]
    fn the_apply_script_skips_the_same_unrunnable_tests_as_the_daemon_gate() {
        let script = include_str!("../../scripts/apply_heal.sh");
        // Comment lines are NOT the gate. `--skip`, `cargo test` and the test names
        // all appear in this script's prose (it documents the libtest-separator
        // hazard verbatim), so a name-anywhere search passes even with every real
        // invocation deleted — the source-anchored-guard trap. Read CODE only.
        let code: String = script
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");

        // Every `cargo test` gate in the script, and every name it skips.
        let gates = code.matches("cargo test --").count();
        assert!(gates >= 1, "apply_heal.sh must actually run `cargo test --` somewhere");
        // Trim the shell punctuation that abuts the LAST name in each list — the
        // final `--skip <name>);` carries a `);` with no space before it, and a
        // token-equality check against it silently reports a drift that is not
        // there. A test path is only `[A-Za-z0-9_:]`.
        let script_skips: Vec<&str> = code
            .split("--skip ")
            .skip(1)
            .filter_map(|s| s.split_whitespace().next())
            .map(|s| {
                s.trim_end_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == ':'))
            })
            .filter(|s| !s.is_empty())
            .collect();
        assert!(!script_skips.is_empty(), "apply_heal.sh passes no --skip at all");

        // DIRECTION 1 — the script must skip everything the daemon skips, in EVERY
        // gate. A gate that skips only some of them rejects a candidate the daemon
        // proved, for a reason that is not the patch.
        for name in UNRUNNABLE_IN_STAGE {
            let n = script_skips.iter().filter(|s| s == &name).count();
            assert_eq!(
                n, gates,
                "apply_heal.sh has {gates} `cargo test --` gate(s) but skips {name} in \
                 only {n} of them — a candidate the daemon proved is rejected at apply \
                 time for the wrong reason"
            );
        }
        // DIRECTION 2 — and NOTHING MORE. This is the dangerous direction and it was
        // unchecked: a name skipped by the script but RUN by the daemon makes the
        // apply gate WEAKER than the gate that proved the patch, so a patch that
        // genuinely breaks that test can still be applied to live sources by hand or
        // by the HUD Accept button. Containment would never catch it; set equality
        // does.
        for s in &script_skips {
            assert!(
                UNRUNNABLE_IN_STAGE.contains(s),
                "apply_heal.sh skips {s}, which the daemon's own gate RUNS — the apply \
                 gate must never be weaker than the one that proved the patch"
            );
        }
        // ...and the script must pass them to the HARNESS, not to cargo.
        assert!(
            script.contains("cargo test -- \\"),
            "apply_heal.sh must use `cargo test --` before any --skip: cargo answers \
             \"unexpected argument '--skip' found\" without the separator"
        );
        // The script must also mirror the runtime inputs, or its suite cannot run
        // at all — the defect this pair of fixes exists to close.
        // The CALL, not merely the name. Asserting the bare name self-matches on
        // the function DEFINITION, so deleting the call left this green — the
        // source-anchored-guard trap. Scope to the staging routine and require an
        // invocation with its arguments.
        let stage_fn = script
            .split("stage_sources()")
            .nth(1)
            .or_else(|| script.split("mirror_out_of_crate_includes \"$daemon_dir\"").nth(1))
            .expect("apply_heal.sh must have a staging routine");
        let window = &stage_fn[..stage_fn.len().min(600)];
        assert!(
            window.contains("mirror_runtime_test_inputs \"$daemon_dir\"")
                || script.contains("mirror_runtime_test_inputs \"$daemon_dir\" \"$staging\""),
            "apply_heal.sh must CALL mirror_runtime_test_inputs during staging — \
             without it the staged suite cannot run and every apply fails"
        );
    }

    /// THE SELF-HEAL GATE MUST BE AT LEAST AS STRICT AS THE HUMAN ONE.
    ///
    /// It ran `check` + `test`. This project's real merge standard is
    /// `cargo clippy --all-targets -- -D warnings`, so a patch could pass the
    /// staged gate, be APPROVED, be APPLIED to live sources, and only then break
    /// the gate its author has to pass — the system handing its owner a patch it
    /// had "validated" plus a broken lint run.
    ///
    /// Clippy is not a nicety here. A never-called method in the wrong impl block
    /// COMPILES and PASSES TESTS; `-D warnings` is what catches it. That exact
    /// mistake happened in this file's own capture-teardown work, and the full
    /// suite stayed green through it.
    #[test]
    fn the_staged_gate_runs_clippy_between_check_and_test() {
        let src = include_str!("heal.rs");
        // Scope to the validation routine, not the whole file — the strings appear
        // in prose elsewhere, and a file-wide search would self-match on this very
        // test.
        let start = src
            .find("let clippy_args = vec![")
            .expect("the staged gate must build clippy args");
        let region = &src[start..start + 700.min(src.len() - start)];
        assert!(
            region.contains("\"--all-targets\"") && region.contains("\"-D\", \"warnings\""),
            "the staged clippy must be --all-targets with -D warnings, or it is a \
             weaker bar than the human gate: {region}"
        );
        // ORDER: clippy before test. It subsumes check and is far cheaper than the
        // suite, so a lint failure must not wait behind 3000 tests.
        // THE ANCHOR HAS TO TRACK THE CODE IT GUARDS. The stage table grew a
        // per-stage FLOOR (`(&str, Vec<&str>, Duration)`), and a guard whose
        // anchor no longer matches does not fail at the thing it protects — it
        // panics on its own `expect`, which reads as "the gate lost its stages"
        // when nothing about the stages changed. Built with `concat!` so this
        // test's own source can never be what the search finds.
        let stages = src
            .find(concat!("let stages: [(&str, ", "Vec<&str>, Duration); 3]"))
            .expect("the gate must have three stages");
        let tail = &src[stages..stages + 400.min(src.len() - stages)];
        let ci = tail.find("(\"clippy\"").expect("clippy must be a stage");
        let ti = tail.find("(\"test\"").expect("test must be a stage");
        assert!(ci < ti, "clippy must run BEFORE the test suite");
    }

    /// ...and the APPLY script must run the same three, or the two gates disagree.
    ///
    /// A patch the daemon REJECTED for a lint could otherwise still be applied by
    /// hand, and one it ACCEPTED would be re-proven against a weaker bar.
    #[test]
    fn the_apply_script_gate_matches_the_daemon_gate() {
        let script = include_str!("../../scripts/apply_heal.sh");
        // A COMMENT NAMING THE STAGE IS NOT THE STAGE. `cargo check` appears SIX
        // times in this script's prose header and once more inside its own
        // `fail "cargo check failed in staging"` message; `cargo test` appears nine
        // times. A whole-file `contains` was satisfied by all of that, so deleting
        // the real invocation left this green — PROVED: replacing
        // `cd "$CRATE" && cargo check` with `cargo build` did not fail this test.
        // Match the INVOCATION, on a non-comment line: every stage is run as
        // `cd "$CRATE" && <stage>`.
        for stage in ["cargo check", "cargo clippy --all-targets -- -D warnings", "cargo test"] {
            let invocation = format!("cd \"$CRATE\" && {stage}");
            assert!(
                script
                    .lines()
                    .map(str::trim_start)
                    .any(|l| !l.starts_with('#') && l.contains(invocation.as_str())),
                "apply_heal.sh must RUN `{stage}` (as `{invocation}`) on a code line — the \
                 two gates must not disagree, and prose naming a stage is not the stage"
            );
        }
    }

    /// The stage-4 block of apply_heal.sh, BOUNDED at its closing `esac`.
    /// Slicing merely "everything after the marker" would run on to the real
    /// apply below it, and a guard that matches too widely reports the wrong
    /// line — which is exactly what it did the first time this was written.
    fn stage_four_block(script: &str) -> &str {
        let after = script
            .split("STAGE 4: MUTATION PROOF")
            .nth(1)
            .expect("stage 4 block missing");
        let end = after.find("\nesac\n").expect("stage 4 block is not esac-terminated");
        &after[..end]
    }

    /// Stage 4 parity. The daemon rejects a patch whose test survives its fix's
    /// removal; if the apply script did not, that patch could still be applied
    /// by hand or by the HUD Accept button, which is the whole hole.
    #[test]
    fn the_apply_script_runs_the_same_mutation_probe_as_the_daemon_gate() {
        let script = include_str!("../../scripts/apply_heal.sh");
        assert!(
            script.contains("--split-heal-diff"),
            "apply_heal.sh must run the mutation probe"
        );
        assert!(
            script.contains("-R <\"$SPLIT_DIR/fix.diff\""),
            "the probe must REVERSE-apply the fix half — applying it forward proves nothing"
        );
        // It must FAIL the apply, not merely print a note, when the test survives.
        let probe = stage_four_block(script);
        assert!(
            probe.contains("fail \"the patch's own test PASSES without the patch's fix"),
            "a surviving test must FAIL the apply, not just warn:\n{probe}"
        );

        // ...AND IT MUST SPLIT THE PATCH BEING APPLIED. The `script.contains(
        // "--split-heal-diff")` assertion above is satisfied by this stage's own prose
        // header and by the fail-closed argv-comparison grep that only
        // proves the staged BINARY knows the flag — nothing here required the live
        // invocation to name $PATCH_FILE. Re-point it at the self-proof fixture
        // ("$PROBE/sep.diff") and every assertion in this test stays green while the
        // operator's actual patch is never probed: $SPLIT_DIR/fix.diff then describes
        // the wrong hunks, the reverse-apply fails, and stage 4 falls into its
        // ADVISORY "INCONCLUSIVE" branch instead of refusing. PROVED: that exact
        // re-point left this test passing. Require the invocation itself, on a
        // non-comment line, inside stage 4, naming $PATCH_FILE.
        assert!(
            probe.lines().map(str::trim_start).any(|l| {
                !l.starts_with('#')
                    && l.contains("--split-heal-diff")
                    && l.contains("\"$PATCH_FILE\"")
            }),
            "stage 4 must run `--split-heal-diff \"$PATCH_FILE\"` on a code line — a probe \
             that splits anything but the patch being applied proves nothing:\n{probe}"
        );

        // FAIL CLOSED, three ways. An unrecognized flag makes darwind fall
        // through to ORDINARY DAEMON STARTUP — it would boot a daemon instead of
        // answering, and the gate would be skipped rather than enforced.
        // apply_forge.sh documents this hazard for its own gate flag.
        // ...ON THE ARGV COMPARISON, not the flag's name. main.rs spells
        // `--split-heal-diff` in the entrypoint comment above its handler, so the
        // bare `grep -q -- '--split-heal-diff'` this replaces cleared itself on
        // PROSE when the dispatch literal had drifted. Same defect, same shape,
        // as the one already fixed in the test above.
        assert!(
            probe.contains(
                "grep -qE '^[[:space:]]*[^/[:space:]].*a == \"--split-heal-diff\"' \
                 \"$CRATE/src/main.rs\""
            ),
            "the probe must confirm the staged daemon implements it BY ITS ARGV \
             COMPARISON, or a mismatched source boots a daemon instead of answering:\n{probe}"
        );
        assert!(
            probe.contains("does not discriminate"),
            "the probe binary must be PROVEN to discriminate before its verdict is \
             trusted — a gate that always answers the same word is not a gate:\n{probe}"
        );
        // An unknown verdict must refuse, not fall through as an advisory note.
        let default_arm = probe.rsplit("*)").next().unwrap_or("");
        assert!(
            default_arm.contains("fail "),
            "an unrecognized verdict must refuse the apply, not pass through:\n{default_arm}"
        );
    }

    /// The responsiveness block of apply_heal.sh, BOUNDED at BOTH ends. Slicing
    /// "everything after the marker" would run on into the live apply below and
    /// report on unrelated lines; binding only the head is the too-wide-window
    /// trap this file has already been bitten by.
    fn responsiveness_block(script: &str) -> &str {
        let after = script
            .split("RESPONSIVENESS PROBE (advisory)")
            .nth(1)
            .expect("apply_heal.sh has no responsiveness block");
        let end = after
            .find("echo \"RESPONSIVENESS: $RESP_WORD\"")
            .expect("the responsiveness block does not end where it is supposed to");
        &after[..end]
    }

    /// ...and the window must actually BIND. A window can bind so tightly it
    /// matches nothing, and then every assertion inside it passes vacuously.
    #[test]
    fn the_responsiveness_window_binds_to_a_real_block() {
        let script = include_str!("../../scripts/apply_heal.sh");
        let block = responsiveness_block(script);
        assert!(!block.trim().is_empty(), "the responsiveness window is empty");
        assert!(
            block.len() < script.len() / 2,
            "the window swallowed most of the script ({} of {} bytes) — it is not bounded",
            block.len(),
            script.len()
        );
        // It must NOT have run on into the live apply.
        assert!(
            !block.contains("stage \"applying\""),
            "the window runs past its block into the live apply:\n{block}"
        );
    }

    /// BOTH GATES OR NEITHER. The daemon computes responsiveness inside its
    /// staged validation; if the apply script did not re-derive it, a proposal
    /// could be applied by hand or by the HUD Accept button with the one signal
    /// that says "this patch is about something else" never shown to the person
    /// clicking the button.
    #[test]
    fn the_apply_script_runs_the_same_responsiveness_probe_as_the_daemon_gate() {
        let script = include_str!("../../scripts/apply_heal.sh");
        let block = responsiveness_block(script);
        assert!(
            block.contains("--heal-responsiveness \"$DIR/diagnosis.json\" \"$PATCH_FILE\""),
            "the apply gate must judge THIS proposal's patch against THIS \
             proposal's diagnosis:\n{block}"
        );
        // FAIL-SAFE: an unknown flag boots a daemon instead of answering, and
        // this script would hang on it. The flag must be confirmed present in
        // the STAGED source first — BY ITS ARGV COMPARISON. main.rs names
        // `--heal-responsiveness` three more times in prose, so the bare
        // `grep -q -- '--heal-responsiveness'` this replaces cleared the guard on
        // a COMMENT; the script would then invoke a flag the staged daemon does
        // not implement. This literal and the script's must move in LOCKSTEP.
        assert!(
            block.contains(
                "grep -qE '^[[:space:]]*[^/[:space:]].*a == \"--heal-responsiveness\"' \
                 \"$CRATE/src/main.rs\""
            ),
            "the probe must confirm the staged daemon implements the flag BY ITS ARGV \
             COMPARISON, or a mismatched source boots a daemon instead of answering:\n{block}"
        );
        // A probe that always answers the same word is not a probe.
        assert!(
            block.contains("does not discriminate"),
            "the probe must be proven to discriminate before its verdict is \
             believed:\n{block}"
        );
        // An unrecognized verdict must be reported as unknown, never passed on.
        assert!(
            block.contains("unrecognized verdict"),
            "an unknown verdict must not be passed through as a real one:\n{block}"
        );
    }

    /// ADVISORY, IN BOTH GATES. A hard reject on this heuristic would throw away
    /// a correct fix whose true cause lives in a file the log never cited — a
    /// worse failure than the hole it closes. The daemon side is proved
    /// behaviourally by `an_unresponsive_candidate_clears_every_gate` (it still
    /// VALIDATES); this is the shell side.
    #[test]
    fn the_apply_script_never_refuses_on_responsiveness() {
        let script = include_str!("../../scripts/apply_heal.sh");
        let block = responsiveness_block(script);
        for line in block.lines() {
            assert!(
                !line.trim_start().starts_with("fail ") && !line.contains("; fail "),
                "the responsiveness probe must never refuse an apply — a correct fix \
                 often touches a file the log never named:\n{line}"
            );
        }
    }

    /// ONE implementation, TWO callers. A bash (or duplicated Rust) copy of the
    /// scoring rule is the gates-drift-apart defect by construction — the same
    /// reason --split-heal-diff exists as a flag rather than a shell function.
    #[test]
    fn the_responsiveness_rule_has_exactly_one_implementation() {
        let main_rs = include_str!("main.rs");
        // THE ARGV MATCH, ON A CODE LINE. main.rs names `--heal-responsiveness`
        // three more times in PROSE (the entrypoint comment block above the handler
        // and the usage string), so a whole-file `contains` was satisfied with the
        // handler's own literal renamed. PROVED: changing
        // `|a| a == "--heal-responsiveness"` to `"--heal-responsivenes"` left this
        // test green. That was not cosmetic: apply_heal.sh's fail-closed guard used
        // to be a bare `grep -q -- '--heal-responsiveness' "$CRATE/src/main.rs"`,
        // which reads the SAME prose — the test was hardened here and THE SCRIPT
        // WAS NOT, so the script still cleared its own guard and then invoked a flag
        // the staged daemon does not implement, and an unknown flag falls through to
        // ORDINARY DAEMON STARTUP rather than erroring. The script now anchors on
        // this same argv comparison (proved by execution in
        // scripts/test_apply_heal_confinement.sh, Part D), so the two are in
        // LOCKSTEP: rename this literal and BOTH sides notice.
        assert!(
            main_rs.lines().map(str::trim_start).any(|l| {
                !l.starts_with("//") && l.contains("a == \"--heal-responsiveness\"")
            }),
            "main's argv scan must MATCH --heal-responsiveness on a code line, or \
             apply_heal.sh's grep guard passes on a comment and the probe invocation \
             boots a daemon instead of answering"
        );
        // NEGATIVE PIN: the prose copies are what made the old assertion vacuous, so
        // prove the comment filter above is actually doing work. If they ever go,
        // whoever removes them re-reads this instead of inheriting a quiet guard.
        assert!(
            main_rs.lines().map(str::trim_start).any(|l| {
                l.starts_with("//") && l.contains("--heal-responsiveness")
            }),
            "the prose occurrences this guard must NOT count are gone; re-check that \
             the code-line filter above is still doing any work"
        );
        assert!(
            main_rs.contains("heal::responsiveness(&diagnosis, &diff)"),
            "the CLI must CALL heal::responsiveness, not reimplement the rule"
        );
        // The script must not parse the diff itself in that block — that would be
        // a second classifier, free to disagree with the first.
        let block = responsiveness_block(include_str!("../../scripts/apply_heal.sh"));
        for line in block.lines() {
            if line.contains("$PATCH_FILE") {
                assert!(
                    line.contains("--heal-responsiveness"),
                    "the block reads the patch outside the shared probe, so there are \
                     now two classifiers that can disagree:\n{line}"
                );
            }
        }
    }

    /// ORDER, NOT JUST PRESENCE. The mutation probe reverse-applies the patch's
    /// FIX into the staged crate and never restores it — and a crate with its
    /// fix lifted out routinely no longer COMPILES, which that probe ACCEPTS as
    /// PROVEN (all it asks is whether `cargo test` exits non-zero). `cargo run
    /// --bin darwind` cannot build such a tree, so a responsiveness block placed
    /// after the reverse-apply gets two empty self-proof answers, concludes the
    /// probe "does not discriminate", and prints RESPONSIVENESS: UNKNOWN for
    /// every patch of that shape — the shell half of the gate inert, and blaming
    /// the wrong thing. It must run while the crate is still the patched, green
    /// tree the gates above built.
    #[test]
    fn the_responsiveness_probe_runs_before_the_mutation_reverse_apply() {
        let script = include_str!("../../scripts/apply_heal.sh");
        let probe_at = script
            .find("RESPONSIVENESS PROBE (advisory)")
            .expect("apply_heal.sh has no responsiveness block");
        let reverse_at = script
            .find("-R <\"$SPLIT_DIR/fix.diff\"")
            .expect("apply_heal.sh no longer reverse-applies the fix");
        assert!(
            probe_at < reverse_at,
            "the responsiveness probe (at {probe_at}) runs AFTER the mutation \
             reverse-apply (at {reverse_at}); that crate often no longer compiles, so \
             `cargo run --bin darwind` answers nothing and every verdict degrades to \
             UNKNOWN with 'does not discriminate' as the stated reason"
        );
        // ...and still inside the re-validation gate. Printing it once daemon/src
        // has already been mutated would be too late to inform anybody.
        let apply_at = script
            .find("stage \"applying\"")
            .expect("apply_heal.sh has no live-apply stage");
        assert!(
            probe_at < apply_at,
            "the responsiveness verdict prints after the live apply ({probe_at} > {apply_at})"
        );
    }

    /// THE WRITER AND THE READER MUST AGREE ON THE ARTIFACT BETWEEN THEM.
    ///
    /// apply_heal.sh hands `$DIR/diagnosis.json` — written by `diagnosis_json`
    /// — to `darwind --heal-responsiveness`, which parses it straight back into
    /// a `Diagnosis`. That struct carries `#[serde(default)]`, so a renamed or
    /// reshaped field does NOT error: it silently yields an EMPTY diagnosis,
    /// every shell-side verdict collapses to INDETERMINATE, and the script's own
    /// self-proof cannot notice — it feeds a hand-written JSON literal, not the
    /// daemon's real output. `diagnosis_json_roundtrips` only proves the file is
    /// valid JSON, never that this struct can read it back. So round-trip the
    /// REAL artifact and require the VERDICT to survive the trip.
    #[test]
    fn the_written_diagnosis_json_is_what_the_responsiveness_probe_reads_back() {
        let now = Utc::now().to_rfc3339();
        let tail = (0..5)
            .map(|_| {
                format!(
                    "{now} ERROR darwin_core::router: converse failed at src/router.rs:122 \
                     error=dispatch exploded"
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let written = build_diagnosis(&scan_tail(tail));
        assert!(
            !written.files.is_empty() && written.subsystem == "router",
            "the fixture must produce a diagnosis there is something to match on: {written:?}"
        );

        let json = diagnosis_json(&written);
        let read_back: Diagnosis = serde_json::from_str(&json)
            .expect("the daemon writes a diagnosis.json its own --heal-responsiveness cannot parse");
        assert_eq!(
            read_back.files, written.files,
            "`files` did not survive the artifact round-trip, so the apply gate judges \
             every patch against an empty diagnosis and can only ever say INDETERMINATE"
        );
        assert_eq!(read_back.subsystem, written.subsystem, "`subsystem` did not survive");
        assert_eq!(read_back.signatures, written.signatures, "`signatures` did not survive");

        let diff = "--- a/src/router.rs\n+++ b/src/router.rs\n@@ -1,1 +1,1 @@\n-a\n+b\n";
        assert_eq!(
            responsiveness(&written, diff).0,
            Responsiveness::Direct,
            "fixture sanity: the daemon-side verdict must be DIRECT"
        );
        assert_eq!(
            responsiveness(&read_back, diff).0,
            Responsiveness::Direct,
            "the two gates disagree about the SAME patch because the artifact between \
             them lost a field — one implementation, but not one input"
        );
    }

    /// THE MUTATION PROBE'S OWN TEST RUN NEEDS A ZERO-BUDGET GUARD TOO — and
    /// nothing tested the one that was just added.
    ///
    /// This is `run_cargo`'s FOURTH call, handed whatever survives the
    /// per-candidate budget after check + clippy + test — 523s of the old 600s,
    /// ~87%, measured cold on an M1 Pro. MUTATION_STAGE_FLOOR is now reserved
    /// for it by every stage ahead of it, but a reserve is not a guarantee (the
    /// last stage may still overrun into it), so the guard stays. With nothing
    /// left, `tokio::time::timeout(Duration::ZERO, …)`
    /// fires instantly, the Err arm below reports "the probe could not run",
    /// and the candidate is returned VALIDATED as if the probe had merely
    /// hiccuped — a never-run proof reading as a technical blip. The staged
    /// loop above has such a guard; this call site had none.
    ///
    /// SOURCE-ANCHORED because `VALIDATE_TIMEOUT` is a compile-time constant
    /// with no injection seam, so the state cannot be reached from a test. The
    /// window is bounded at BOTH ends — the reverse-patch success arm .. that
    /// fourth `run_cargo` — and both anchors are built with `concat!` so this
    /// test's own source can never satisfy its own search.
    #[test]
    fn the_mutation_probes_second_test_run_is_guarded_against_a_spent_budget() {
        let src = include_str!("heal.rs");
        let arm = concat!("            Ok(r) if ", "r.ok => {");
        // The re-run is now TIMED (its wall clock feeds `calibration.stages`), so
        // the call is bound to a local before the match. The anchor follows the
        // CALL, which is the thing this guard is about.
        let rerun = concat!("let mut_out = run_cargo(&crate_dir, ", "&test_args, remaining).await;");
        let start = src
            .find(arm)
            .expect("the mutation probe no longer has a reverse-patch success arm");
        let rest = &src[start..];
        let end = rest.find(rerun).expect("the mutation probe no longer re-runs the suite");
        let window = &rest[..end];
        // The window must actually BIND: one that matched nothing, or swallowed
        // the rest of the file, would pass every assertion below for free.
        assert!(
            !window.trim().is_empty() && window.len() < 3_000,
            "the guard window did not bind ({} bytes)",
            window.len()
        );
        assert!(
            window.contains("combined.push_str("),
            "the window is not the mutation probe's success arm:
{window}"
        );
        assert!(
            window.contains("if remaining.is_zero()"),
            "the mutation probe re-runs the suite with no check that any budget is              LEFT; a spent budget goes into timeout(Duration::ZERO), returns through              the Err arm as 'the probe could not run', and the candidate is proposed              VALIDATED with a proof that never executed:
{window}"
        );
        assert!(
            window.contains("mutation-proven"),
            "the spent-budget path must SAY the patch is not mutation-proven, not              blame a technical failure:
{window}"
        );
    }

    /// The split must have exactly ONE implementation. A bash reimplementation
    /// of the hunk classifier is the gates-drift-apart defect by construction —
    /// so the script has to call the daemon binary, not parse the diff itself.
    #[test]
    fn the_apply_script_does_not_reimplement_the_hunk_split() {
        let script = include_str!("../../scripts/apply_heal.sh");
        let probe = stage_four_block(script);
        // The precise property: the patch is READ only by the splitter. If bash
        // ever touched $PATCH_FILE for any other purpose here, that would be a
        // second classifier — and the two could then disagree.
        // A FOR-ALL OVER NOTHING IS NOT A CHECK. If $PATCH_FILE stops appearing in
        // stage 4 at all — renamed, or the probe re-pointed at a fixture — this loop
        // iterates zero matching lines and reports "no second classifier" having read
        // none of them. PROVED: re-pointing the split invocation at "$PROBE/sep.diff"
        // left this test green. Pin the floor before scanning.
        assert!(
            probe.lines().any(|l| l.contains("$PATCH_FILE")),
            "stage 4 never names $PATCH_FILE, so this guard scanned nothing:\n{probe}"
        );
        for line in probe.lines() {
            if line.contains("$PATCH_FILE") {
                assert!(
                    line.contains("--split-heal-diff"),
                    "the probe reads the patch outside the splitter, so there are now two \
                     classifiers that can disagree:\n{line}"
                );
            }
        }
        // And what gets reverse-applied must be the file the SPLITTER wrote.
        assert!(
            probe.contains("$SPLIT_DIR/fix.diff"),
            "the probe must reverse the splitter's own fix half:\n{probe}"
        );
    }

    // -- (9) THE BUDGET: one attempt-wide ceiling, floors behind each stage ---

    /// THE CONSTANTS HAVE TO FIT EACH OTHER. Floors that sum past the
    /// per-candidate budget make every stage's cap zero and the gate can never
    /// run; a per-candidate budget above the attempt budget makes the
    /// attempt-wide ceiling decorative; and an attempt budget above the 3x600s
    /// worst case this replaced would be a straight resource regression on a
    /// machine that is, by definition of self-heal firing, already struggling.
    #[test]
    fn the_budget_constants_can_actually_fit_a_candidate() {
        assert!(
            minimum_viable_budget() <= VALIDATE_TIMEOUT,
            "the stage floors ({}s) exceed the per-candidate budget ({}s): every stage's cap \
             would be zero and no candidate could ever be judged",
            minimum_viable_budget().as_secs(),
            VALIDATE_TIMEOUT.as_secs()
        );
        assert!(
            VALIDATE_TIMEOUT <= ATTEMPT_BUDGET_DEFAULT,
            "a candidate may not be allowed more than the whole attempt"
        );
        assert!(
            ATTEMPT_BUDGET_DEFAULT <= Duration::from_secs(1800),
            "the attempt-wide ceiling ({}s) is above the 3 x 600s worst case it replaced — \
             this change must not buy headroom with the operator's machine",
            ATTEMPT_BUDGET_DEFAULT.as_secs()
        );
        // THE BUDGET IS NOW OPERATOR-SETTABLE, so the constants fitting is no
        // longer enough: the CLAMP'S OWN LOW END has to fit a candidate too, or
        // an operator writing `attempt_budget_secs = 1` would get a self-heal
        // that pays for CANDIDATE_COUNT heavy-model drafts every attempt and
        // then refuses all of them at `deadline` without staging one.
        let floored = effective_attempt_budget(&crate::config::SelfHealConfig {
            attempt_budget_secs: 1,
            ..Default::default()
        });
        assert!(
            floored >= minimum_viable_budget(),
            "the configured-budget clamp bottoms out at {}s, below the {}s a candidate needs to \
             be staged at all — a misconfiguration would silently disable the gate while still \
             paying for the drafts",
            floored.as_secs(),
            minimum_viable_budget().as_secs()
        );
        // ...and the budget must actually be bigger than the measured cycle it
        // has to fit (523s cold on an M1 Pro), or nothing was fixed.
        assert!(
            VALIDATE_TIMEOUT >= Duration::from_secs(700),
            "the per-candidate budget is back below the measured 523s cycle plus a slow-machine \
             margin; the 87% squeeze this replaced is reopened"
        );
    }

    /// THE ATTEMPT MUST BE ABLE TO AFFORD EVERY CANDIDATE IT DRAFTS.
    ///
    /// `CANDIDATE_COUNT` diffs are paid for at the heavy model and then staged in
    /// order, and the LAST of them is staged only when its predecessors left at
    /// least `minimum_viable_budget()`. "On a healthy machine all of them still
    /// get a real verdict" is therefore not a hope — it is this inequality, and
    /// nothing else in the file enforces it. Raise a stage floor, or the measured
    /// cycle, without raising the attempt budget and the last candidate is drafted,
    /// paid for, and then SILENTLY never judged: refused as `deadline` and billed
    /// to the operator's machine.
    #[test]
    fn the_attempt_budget_can_afford_every_candidate() {
        // MEASURED cold on an M1 Pro in a tree staged exactly as `stage_sources`
        // builds one: check 92 + clippy 184 + test 214 + mutation re-run 33.
        const MEASURED_CYCLE: Duration = Duration::from_secs(523);
        let predecessors = MEASURED_CYCLE * (CANDIDATE_COUNT as u32 - 1);
        let needed = predecessors + minimum_viable_budget();
        assert!(
            ATTEMPT_BUDGET_DEFAULT >= needed,
            "the {}s default attempt budget cannot afford {CANDIDATE_COUNT} candidates: the \
             first {} at the measured {}s cycle leave {}s, below the {}s the last one needs \
             before it is staged at all",
            ATTEMPT_BUDGET_DEFAULT.as_secs(),
            CANDIDATE_COUNT - 1,
            MEASURED_CYCLE.as_secs(),
            ATTEMPT_BUDGET_DEFAULT.saturating_sub(predecessors).as_secs(),
            minimum_viable_budget().as_secs()
        );
        // STATE THE MARGIN IN NUMBERS, and fail if it is gone. The last change
        // here shipped with 4s of slack and nobody did the subtraction; this
        // does it, in the test, every run.
        let slack = ATTEMPT_BUDGET_DEFAULT.saturating_sub(needed);
        assert!(
            slack.as_secs() < 3600,
            "slack of {}s is implausible — MEASURED_CYCLE is probably stale, not generous",
            slack.as_secs()
        );
        // The slack this passes with is 1800 - 2 x 523 - 750 = 4s. It is REAL,
        // not comfortable, and that is exactly why it is pinned: any nudge to a
        // floor, to CANDIDATE_COUNT or to the cycle now fails loudly here instead
        // of quietly dropping the last candidate in production.
    }

    /// CLOUD LATENCY IS NOT THE MACHINE'S TO PAY FOR.
    ///
    /// The attempt budget bounds COMPILATION. The adversarial review that runs
    /// between candidates is a cloud call with its own REVIEW_TIMEOUT, and a
    /// wall-clock deadline spanning the loop charged it here: with 4s of slack
    /// (see above), two seconds of review latency refused candidate 3 and told
    /// the operator the earlier candidates had used their machine up.
    ///
    /// The loop is driven by a real clock and a real cargo, so this cannot be
    /// reached from a unit test; it is pinned at the source. Bounded at BOTH ends,
    /// and every needle is built with `concat!` so this test's own text cannot
    /// satisfy the search.
    #[test]
    fn the_candidate_loop_does_not_charge_cloud_latency_to_the_machine() {
        let src = include_str!("heal.rs");
        let start_anchor = concat!("let mut attempt_spent = ", "Duration::ZERO;");
        let end_anchor = concat!("let candidates_md = ", "render_candidates_md(&outcomes);");
        let start = src.find(start_anchor).expect("the attempt accumulator is gone");
        let rest = &src[start..];
        let end = rest
            .find(end_anchor)
            .expect("the candidate loop no longer ends where it did");
        let window = &rest[..end];
        assert!(
            !window.trim().is_empty() && window.len() < 8_000,
            "the window did not bind ({} bytes)",
            window.len()
        );
        assert!(
            window.contains(concat!("attempt_budget().saturating_sub(", "attempt_spent)")),
            "the candidate budget is not taken from the ACCUMULATED staged time:\n{window}"
        );
        assert!(
            !window.contains(concat!("attempt_", "deadline")),
            "a wall-clock attempt deadline is back — it charges the review call's latency to \
             the machine and refuses the last candidate with 'the earlier candidates used the \
             machine up':\n{window}"
        );
        let charge = window
            .find(concat!("attempt_spent.saturating_add(", "started.elapsed())"))
            .expect("the loop never charges the staged time it used");
        let review = window
            .find(concat!("brain.", "review("))
            .expect("the loop no longer reviews its survivors");
        assert!(
            charge < review,
            "the staged time is charged AFTER the review call, so the review's latency is \
             inside the charged interval again"
        );
    }

    /// A candidate gets its own ceiling — but only out of what the earlier
    /// candidates left. Without this, CANDIDATE_COUNT independent budgets let one
    /// attempt occupy every core for CANDIDATE_COUNT x VALIDATE_TIMEOUT.
    #[test]
    fn candidate_budget_is_clamped_by_what_the_attempt_has_left() {
        let per = Duration::from_secs(900);
        // Plenty left: the candidate gets its full ceiling, not the remainder.
        assert_eq!(candidate_budget(Duration::from_secs(1800), per), per);
        assert_eq!(candidate_budget(per, per), per);
        // Squeezed: it gets exactly what is left, never more.
        assert_eq!(
            candidate_budget(Duration::from_secs(400), per),
            Duration::from_secs(400)
        );
        assert_eq!(candidate_budget(Duration::ZERO, per), Duration::ZERO);
    }

    /// A slow stage must not eat the budget of the stages behind it. The cap is
    /// computed off the WHOLE budget (not the depleted remainder), so the floors
    /// do not shrink as the clock runs down.
    #[test]
    fn stage_cap_reserves_the_floors_of_the_stages_behind_it() {
        let budget = Duration::from_secs(900);
        // First stage, 600s reserved behind it -> capped at 300s even though the
        // full 900s is still on the clock.
        assert_eq!(
            stage_cap(budget, budget, Duration::from_secs(600)),
            Duration::from_secs(300)
        );
        // Last stage: nothing behind it, so it may use everything that is left.
        assert_eq!(
            stage_cap(Duration::from_secs(120), budget, Duration::ZERO),
            Duration::from_secs(120)
        );
        // REMAINING STILL WINS WHEN IT IS SMALLER — a stage never gets more time
        // than exists.
        assert_eq!(
            stage_cap(Duration::from_secs(90), budget, Duration::from_secs(600)),
            Duration::from_secs(90)
        );
        // A reserve larger than the budget yields zero, not a panic (Duration
        // subtraction underflows by panicking; saturating_sub is load-bearing).
        assert_eq!(
            stage_cap(budget, budget, Duration::from_secs(5000)),
            Duration::ZERO
        );
    }

    /// THE DRILL'S ONLY FAILURE SURFACE. `--heal-drill` is what the verifier
    /// runs, and it reported every outcome as "pipeline rejected every candidate
    /// at stage `X`". With the confidence floor in place that sentence is false
    /// twice for the outcome it is most likely to hit: nothing was rejected by a
    /// gate, and the reader is told the model drafted bad patches.
    #[test]
    fn the_drill_does_not_report_a_review_floor_as_a_gate_failure() {
        let floor = drill_rejection_sentence("confidence");
        assert!(
            floor.contains("passed every staged gate") && floor.contains("0.25"),
            "a below-floor drill must say the gates passed and name the bar:\n{floor}"
        );
        assert!(
            !floor.contains("rejected"),
            "a below-floor outcome is not a rejection by any gate:\n{floor}"
        );
        let budget = drill_rejection_sentence("deadline");
        assert!(
            budget.contains("not a verdict"),
            "a drill that ran out of budget judged nothing:\n{budget}"
        );
        // A real gate failure still says so, plainly.
        assert!(
            drill_rejection_sentence("clippy").contains("rejected at stage `clippy`"),
            "an ordinary gate rejection must still name its stage"
        );
    }

    /// The three ways a candidate runs out of time are three different messages.
    /// They all used to render as one sentence about "the 600s budget", which
    /// tells an operator to do the wrong thing in two cases out of three.
    #[test]
    fn budget_exhausted_says_which_ceiling_stopped_the_candidate() {
        let budget = Duration::from_secs(900);
        let text = |stop| match budget_exhausted("clippy", "tail", None, budget, stop) {
            StageResult::Rejected { stage, detail } => {
                assert_eq!(stage, "deadline", "never filed under the stage it never reached");
                detail
            }
            other => panic!("expected a rejection, got {other:?}"),
        };
        let candidate = text(BudgetStop::Candidate);
        assert!(candidate.contains("900s"), "{candidate}");
        assert!(candidate.contains("for this candidate"), "{candidate}");

        let share = text(BudgetStop::StageShare);
        assert!(share.contains("SHARE"), "a blown stage share must say so:\n{share}");
        assert!(
            share.contains("starve"),
            "it must say WHY the rest was withheld:\n{share}"
        );

        let attempt = text(BudgetStop::Attempt);
        assert!(
            attempt.contains("attempt-wide") && attempt.contains("never started"),
            "a candidate the attempt could not afford must say it was never started, not that \
             a gate failed:\n{attempt}"
        );
        // All three keep the "not a verdict on the patch" clause: that sentence is
        // the whole reason `deadline` is its own stage.
        for d in [candidate, share, attempt] {
            assert!(d.contains("never judged on its merits"), "{d}");
        }
    }

    /// A candidate the attempt cannot afford is refused BEFORE a tree is copied
    /// and a patch applied. Staging costs disk and IO on a machine that is
    /// already misbehaving, and every gate would then be handed a slice too small
    /// to reach a verdict — the same rejection, with the machine punished for it.
    #[tokio::test]
    async fn a_candidate_the_attempt_cannot_afford_is_rejected_without_being_staged() {
        let root = TempRoot::new("budget-skip");
        let crate_dir = root.0.join("daemon");
        let heal_root = root.0.join("state").join("heal");
        write_synthetic_crate(&crate_dir);
        let ts = 1_760_000_099u64;
        let diff = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n pub fn double(x: i32) -> i32 {\n-    x * y\n+    x * 2\n }\n";

        let out = stage_and_validate(
            &crate_dir,
            &heal_root,
            CandidateRef { ts, index: 0 },
            diff,
            &diag_for(&["src/lib.rs"], "router"),
            Duration::from_secs(5),
            &mut Vec::new(),
        )
        .await
        .expect("staging must not error");

        match out {
            StageResult::Rejected { stage, detail } => {
                assert_eq!(stage, "deadline", "a capacity failure is never a merit verdict");
                assert!(detail.contains("attempt-wide"), "{detail}");
            }
            other => panic!("expected a deadline rejection, got {other:?}"),
        }
        assert!(
            !heal_root.join(staging_dir_name(ts, 0)).exists(),
            "the candidate was staged anyway — the whole point of the pre-check is that a \
             candidate the attempt cannot afford costs nothing"
        );
    }

    /// C PROVES THE FUNCTION, NOT THAT ANYONE CALLS IT. The stage loop is driven
    /// by a wall clock and a real cargo, so the capping cannot be reached from a
    /// unit test; this pins that the loop asks `stage_cap` for its slice and
    /// reports a blown share as one. Bounded at BOTH ends, and the anchors are
    /// built with `concat!` so this test's own source cannot satisfy the search.
    #[test]
    fn the_stage_loop_asks_stage_cap_for_every_slice() {
        let src = include_str!("heal.rs");
        let start_anchor = concat!("let stages: [(&str, ", "Vec<&str>, Duration); 3]");
        let end_anchor = concat!("---- RESPONSIVENESS (advisory; ", "NEVER rejects)");
        let start = src.find(start_anchor).expect("the stage table is gone");
        let rest = &src[start..];
        let end = rest.find(end_anchor).expect("the stage loop no longer ends where it did");
        let window = &rest[..end];
        assert!(
            !window.trim().is_empty() && window.len() < 4_000,
            "the window did not bind ({} bytes)",
            window.len()
        );
        assert!(
            window.contains(concat!("stage_cap(remaining, ", "budget, reserved_for_later)")),
            "the loop does not cap its stages, so one slow stage can still eat the budget \
             and leave `test` a few seconds and a deadline:\n{window}"
        );
        assert!(
            window.contains("BudgetStop::StageShare"),
            "a stage stopped at its share must be reported as that, not as the candidate's \
             whole budget running out:\n{window}"
        );
        assert!(
            window.contains("MUTATION_STAGE_FLOOR"),
            "the mutation probe is not in the stage table, so its floor has to be added to \
             every stage's reserve explicitly — or the probe is the stage that gets starved, \
             and an unproven patch is exactly what this gate exists to catch:\n{window}"
        );
    }

    /// THE STAGED TREE MUST CARRY EVERY REPO DIRECTORY THE SUITE READS AT RUNTIME.
    ///
    /// MEASURED, not reasoned: a tree staged exactly as `stage_sources` builds
    /// one, with the suite run inside it, failed
    /// `apps::tests::the_sandbox_doc_worked_example_names_an_app_whose_manifest_validates`
    /// with `docs/SANDBOX.md is present: Os { code: 2, kind: NotFound }`. One
    /// test failing for a HARNESS reason fails the whole `test` stage, which
    /// discards EVERY candidate at stage `test` -- reported to the operator as
    /// "no candidate passed the staged gates", i.e. as a verdict on three
    /// patches that were never actually judged. The gate could not pass at all.
    ///
    /// This pins the mirror against the ONLY thing that makes it correct: the
    /// set of repo roots the crate's own tests reach for. Those are spelled
    /// `join("../<dir>")` off `CARGO_MANIFEST_DIR`, so they are enumerable from
    /// the sources rather than remembered -- `apps` is mirrored by the
    /// per-app arm below the list and is exempted here by name.
    #[test]
    fn the_staged_mirror_carries_the_repo_dirs_the_suite_reads() {
        // Enumerate `join("../<dir>")` across the crate's sources.
        let mut wanted: Vec<String> = Vec::new();
        for src in [
            include_str!("apps.rs"),
            include_str!("config.rs"),
            include_str!("heal.rs"),
            include_str!("agents.rs"),
            include_str!("forge.rs"),
        ] {
            let needle = concat!("join(\"..", "/");
            let mut from = 0usize;
            while let Some(i) = src[from..].find(needle) {
                let start = from + i + needle.len();
                let dir: String = src[start..]
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                    .collect();
                if !dir.is_empty() && !wanted.contains(&dir) {
                    wanted.push(dir);
                }
                from = start;
            }
        }
        assert!(
            wanted.len() >= 3,
            "the enumeration found almost nothing ({wanted:?}) -- it is not scanning what it claims"
        );
        for dir in &wanted {
            // `apps` is mirrored by the per-app arm (manifests + entry files),
            // not as a whole directory: staging every app's vendored deps and
            // fixtures would be hundreds of megabytes.
            if dir == "apps" {
                continue;
            }
            assert!(
                RUNTIME_TEST_INPUTS.contains(&dir.as_str()),
                "the staged suite reads `../{dir}` at runtime but RUNTIME_TEST_INPUTS does not \
                 mirror it ({RUNTIME_TEST_INPUTS:?}) -- that test will fail in EVERY staging tree \
                 for a harness reason, failing the whole `test` stage and discarding every \
                 candidate at stage `test`"
            );
        }
    }

    /// THE TWO STAGING MIRRORS MUST MIRROR THE SAME DIRECTORIES.
    ///
    /// There are TWO implementations of "stage a tree the suite can run in":
    /// `RUNTIME_TEST_INPUTS` here (the daemon's PROPOSE-side gate) and
    /// `mirror_runtime_test_inputs` in `scripts/apply_heal.sh` (the APPLY-side
    /// gate). `apply_heal.sh` says so itself -- "the two MUST agree, or a patch
    /// the daemon proved will fail here for a reason that is not the patch".
    ///
    /// MEASURED, not reasoned. `docs` was added to the list above after
    /// `apps::tests::the_sandbox_doc_worked_example_names_an_app_whose_manifest_validates`
    /// began reading `<crate>/../docs/SANDBOX.md` with an `.expect`. It was NOT
    /// added to the script, and nothing noticed: the existing guard checks only
    /// that the script CALLS its mirror function, not WHAT that function
    /// mirrors. With `docs` absent the staged suite panics, `cargo test` fails,
    /// and `apply_heal.sh` refuses every apply with "cargo test failed in
    /// staging" -- no self-heal patch installable by any path, and the operator
    /// told the patch failed a gate it never reached. Fixing one side of a
    /// two-implementation gate and not the other is how that defect recurs, so
    /// this pins the SET, not the call.
    ///
    /// Bounded at BOTH ends on the script's own function body, and the
    /// extraction is proven non-empty before it is trusted.
    #[test]
    fn the_apply_script_mirrors_the_same_runtime_inputs_as_the_daemon_gate() {
        let script = include_str!("../../scripts/apply_heal.sh");
        // Window: the body of mirror_runtime_test_inputs, from its definition to
        // the `apps` arm that closes the whole-directory loop.
        let start_anchor = concat!("mirror_runtime_test_inputs", "() {");
        let end_anchor = concat!("if [ -d \"$repo_root", "/apps\" ]; then");
        let start = script
            .find(start_anchor)
            .expect("apply_heal.sh no longer defines mirror_runtime_test_inputs");
        let rest = &script[start..];
        let end = rest
            .find(end_anchor)
            .expect("mirror_runtime_test_inputs no longer ends at its apps arm");
        let body = &rest[..end];
        assert!(
            body.len() > 100 && body.len() < 6_000,
            "the mirror-function window did not bind ({} bytes)",
            body.len()
        );
        // The whole-directory list is the `for d in <words>; do` line.
        let needle = concat!("for d in", " ");
        let list_start = body
            .find(needle)
            .expect("mirror_runtime_test_inputs no longer loops over a directory list");
        let after = &body[list_start + needle.len()..];
        let list_end = after.find(';').expect("the `for d in ...` line has no `;`");
        let mirrored: Vec<&str> = after[..list_end].split_whitespace().collect();
        assert!(
            !mirrored.is_empty(),
            "the script's directory list extracted EMPTY -- the guard would pass vacuously"
        );
        for want in RUNTIME_TEST_INPUTS {
            assert!(
                mirrored.contains(want),
                "daemon-side RUNTIME_TEST_INPUTS mirrors `{want}` but apply_heal.sh's \
                 mirror_runtime_test_inputs does not ({mirrored:?}). The apply gate would stage a \
                 tree the suite cannot run in, `cargo test` would fail for a HARNESS reason, and \
                 EVERY apply -- interactive, --yes and the HUD Accept button -- would be refused \
                 with the patch blamed for it"
            );
        }
    }

    // -- (10) THE REVIEW-CONFIDENCE FLOOR -----------------------------------

    /// THE CONFIGURED FLOOR IS ACTUALLY SEEDED BY BOTH GATES THAT ENFORCE IT.
    ///
    /// `effective_confidence_floor` being correct proves nothing on its own: if
    /// nobody CALLS `configure`, both cells stay unset, `confidence_floor()`
    /// returns the compiled default forever, and `[self_heal].confidence_floor`
    /// is a documented knob that does nothing -- this repo's built-but-inert
    /// defect class, in the one place it also means two gates silently disagree.
    ///
    /// MEASURED: with all three seeding call sites deleted, the entire suite
    /// still passed 3467/0. Nothing else in the tree pins the wiring, because
    /// the cells are process-global `OnceLock`s and a runtime test that seeded
    /// them would move a global out from under every other test in the binary.
    /// So the wiring is pinned at the SOURCE, at both callers, ordered:
    ///
    ///   * `run_pipeline` (daemon, PROPOSE side) must seed before it reads a
    ///     floor or a budget;
    ///   * `--heal-confidence` (main.rs, the INSTALL side `apply_heal.sh` shells
    ///     out to) must seed before it calls `confidence_gate`.
    ///
    /// Both windows are bounded at both ends and proven non-empty first, and
    /// every needle is `concat!`-split so this test cannot match on its own text.
    #[test]
    fn both_confidence_gates_seed_the_floor_from_config_before_reading_it() {
        let seed = concat!("configure(&cfg", ".self_heal);");

        // (a) DAEMON / PROPOSE SIDE.
        let src = include_str!("heal.rs");
        let fn_anchor = concat!("async fn run_pipeline", "(");
        let start = src.find(fn_anchor).expect("run_pipeline is gone");
        let rest = &src[start..];
        let end = rest
            .find(concat!("let enabled = cfg.self_heal.enabled", " &&"))
            .expect("run_pipeline no longer computes `enabled` where it did");
        let head = &rest[..end];
        assert!(
            head.len() > 200 && head.len() < 3_000,
            "the run_pipeline window did not bind ({} bytes)",
            head.len()
        );
        assert!(
            head.contains(seed),
            "run_pipeline does not seed the self-heal tunables from config before the \
             attempt runs -- [self_heal].confidence_floor / .attempt_budget_secs would be \
             parsed, clamped and then IGNORED, and the daemon would propose against the \
             compiled default while apply_heal.sh judged against the operator's value:\n{head}"
        );

        // (b) CLI / INSTALL SIDE, and BEFORE the gate reads the floor.
        let main_src = include_str!("main.rs");
        let arm = concat!("usage: darwind --heal", "-confidence <report.md>");
        let astart = main_src.find(arm).expect("the --heal-confidence arm is gone");
        let arest = &main_src[astart..];
        let aend = arest
            .find(concat!("return Ok(", "());"))
            .expect("the --heal-confidence arm no longer returns where it did");
        let window = &arest[..aend];
        assert!(
            window.len() > 200 && window.len() < 3_000,
            "the --heal-confidence window did not bind ({} bytes)",
            window.len()
        );
        let seed_at = window.find(concat!("heal::configure(&cfg", ".self_heal);")).expect(
            "the --heal-confidence gate does not seed the floor from config -- apply_heal.sh \
             would refuse (or admit) proposals against the COMPILED default while the daemon \
             proposed against the configured one, which is the two-gates-disagree defect this \
             one-implementation-two-callers shape exists to prevent",
        );
        let gate_at = window
            .find(concat!("heal::confidence", "_gate(&report)"))
            .expect("the --heal-confidence arm no longer calls confidence_gate");
        assert!(
            seed_at < gate_at,
            "the floor is seeded AFTER the gate reads it, so the gate judges against the \
             compiled default"
        );
    }

    /// BOTH NUMBERS ARE OPERATOR-SETTABLE, AND A SETTING ACTUALLY TAKES.
    ///
    /// The point of making them configurable is defeated by a knob that parses
    /// and is then ignored — this repo's "built-but-inert" defect class. These
    /// are the PURE resolvers the daemon and the `--heal-confidence` CLI both go
    /// through, exercised without touching the process-wide cells, so this test
    /// can never race another by moving a global out from under it.
    #[test]
    fn the_configured_floor_and_budget_resolve_to_the_configured_values() {
        use crate::config::SelfHealConfig;
        let d = SelfHealConfig::default();
        // The shipped defaults ARE the compiled defaults (config lockstep pins
        // the file side; this pins that the resolver does not transform them).
        assert_eq!(effective_confidence_floor(&d), CONFIDENCE_FLOOR_DEFAULT);
        assert_eq!(effective_attempt_budget(&d), ATTEMPT_BUDGET_DEFAULT);

        // ...and a value INSIDE the range is honored exactly, not snapped.
        let tuned = SelfHealConfig {
            confidence_floor: 0.60,
            attempt_budget_secs: 2400,
            ..SelfHealConfig::default()
        };
        assert_eq!(effective_confidence_floor(&tuned), 0.60);
        assert_eq!(effective_attempt_budget(&tuned), Duration::from_secs(2400));
        assert_ne!(
            effective_confidence_floor(&tuned),
            CONFIDENCE_FLOOR_DEFAULT,
            "the resolver returned the default for a configured value — the knob is inert"
        );
    }

    /// THE CLAMPS HOLD AT BOTH ENDS, AND A NaN CANNOT WALL OFF THE GATE.
    ///
    /// A NaN floor is the nasty one: every `>=` against it is false, so
    /// `meets_confidence_floor` would answer NO for a perfect 1.00 and the
    /// daemon would propose nothing, forever, with no error anywhere.
    #[test]
    fn out_of_range_and_nonfinite_settings_are_clamped_not_honored() {
        use crate::config::SelfHealConfig;
        let mk = |f: f64, b: u64| SelfHealConfig {
            confidence_floor: f,
            attempt_budget_secs: b,
            ..SelfHealConfig::default()
        };
        assert_eq!(effective_confidence_floor(&mk(0.0, 1800)), CONFIDENCE_FLOOR_MIN);
        assert_eq!(effective_confidence_floor(&mk(-5.0, 1800)), CONFIDENCE_FLOOR_MIN);
        assert_eq!(effective_confidence_floor(&mk(1.0, 1800)), CONFIDENCE_FLOOR_MAX);
        assert_eq!(effective_confidence_floor(&mk(99.0, 1800)), CONFIDENCE_FLOOR_MAX);
        assert_eq!(effective_confidence_floor(&mk(f64::NAN, 1800)), CONFIDENCE_FLOOR_DEFAULT);
        assert_eq!(
            effective_confidence_floor(&mk(f64::INFINITY, 1800)),
            CONFIDENCE_FLOOR_DEFAULT
        );
        // A clamped floor is still a REAL floor: it must reject the 0.0 a failed
        // review call is recorded as.
        assert!(effective_confidence_floor(&mk(0.0, 1800)) > 0.0);

        assert_eq!(effective_attempt_budget(&mk(0.25, 0)), minimum_viable_budget());
        assert_eq!(effective_attempt_budget(&mk(0.25, 1)), minimum_viable_budget());
        assert_eq!(
            effective_attempt_budget(&mk(0.25, u64::MAX)),
            Duration::from_secs(ATTEMPT_BUDGET_MAX_SECS)
        );
    }

    /// THE FLOOR'S CLAMP RANGE IS DERIVED FROM `scripts/apply_heal.sh`, SO IT
    /// MUST STILL BRACKET THAT SCRIPT'S OWN SELF-PROOF.
    ///
    /// Before installing anything, `apply_heal.sh` proves its confidence gate
    /// discriminates by feeding `darwind --heal-confidence` a HIGH report and a
    /// LOW one and requiring ABOVE_FLOOR / BELOW_FLOOR. Those two probe scores
    /// are literals in the script. Making the floor configurable put a NEW way
    /// to break that proof into the operator's hands: a floor at or above the
    /// high probe, or at or below the low probe, makes the self-proof fail and
    /// the apply gate refuse EVERY proposal (fail-closed, but for a reason no
    /// operator could diagnose from the message).
    ///
    /// So the clamp ends are not taste — they are `low_probe < MIN` and
    /// `MAX <= high_probe`. This reads the two literals back out of the script
    /// (bounded at both ends on the self-proof block, and the extraction is
    /// proven non-empty first) so editing either file alone fails here.
    #[test]
    fn the_confidence_clamp_brackets_the_apply_scripts_own_self_proof() {
        let script = include_str!("../../scripts/apply_heal.sh");
        // Bound the window: the self-proof block starts where the script builds
        // its probe reports and ends where it judges their verdicts.
        let start_anchor = concat!("CP=\"$STAGING/confidence", "-selfproof\"");
        let end_anchor = concat!("the confidence gate does not ", "discriminate");
        let start = script
            .find(start_anchor)
            .expect("apply_heal.sh no longer builds a confidence self-proof");
        let rest = &script[start..];
        let end = rest.find(end_anchor).expect("the self-proof block no longer ends where it did");
        let window = &rest[..end];
        assert!(
            window.len() > 200 && window.len() < 4_000,
            "the self-proof window did not bind ({} bytes)",
            window.len()
        );
        // Every `review confidence: <float>` literal inside the window.
        let mut probes: Vec<f64> = Vec::new();
        for piece in window.split("review confidence:").skip(1) {
            let num: String = piece
                .trim_start()
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            if let Ok(v) = num.parse::<f64>() {
                probes.push(v);
            }
        }
        assert_eq!(
            probes.len(),
            2,
            "expected exactly the HIGH and LOW probe scores in the self-proof, got {probes:?}"
        );
        let high = probes.iter().cloned().fold(f64::MIN, f64::max);
        let low = probes.iter().cloned().fold(f64::MAX, f64::min);
        assert!(high > low, "the two probes are not distinguishable: {probes:?}");
        assert!(
            CONFIDENCE_FLOOR_MAX <= high,
            "a configurable floor may reach {CONFIDENCE_FLOOR_MAX}, at or above apply_heal.sh's \
             ABOVE_FLOOR probe of {high} — at that setting the script's own self-proof fails and \
             it refuses EVERY proposal for a reason the operator cannot read"
        );
        assert!(
            low < CONFIDENCE_FLOOR_MIN,
            "a configurable floor may drop to {CONFIDENCE_FLOOR_MIN}, at or below \
             apply_heal.sh's BELOW_FLOOR probe of {low} — the low probe would then read \
             ABOVE_FLOOR and the self-proof stops discriminating"
        );
        // ...and the DEFAULT must sit inside the same bracket, or the shipped
        // configuration breaks the shipped script.
        assert!(low < CONFIDENCE_FLOOR_DEFAULT && CONFIDENCE_FLOOR_DEFAULT <= high);
    }

    /// The bar is INCLUSIVE. An exclusive comparison would reject the one score
    /// the floor is documented to allow, and the HUD (which renders the same
    /// boundary) would disagree with the daemon about it.
    #[test]
    fn the_confidence_floor_is_inclusive_at_the_bar() {
        assert!(meets_confidence_floor(confidence_floor()));
        assert!(meets_confidence_floor(1.0));
        assert!(!meets_confidence_floor(confidence_floor() - 0.01));
        assert!(!meets_confidence_floor(0.0));
        // A FLOOR OF 0.0 WOULD NOT BE A FLOOR — it would let a failed review
        // call (recorded as 0.0) through as a proposal. That is exactly what the
        // `!meets_confidence_floor(0.0)` assertion above pins, so setting
        // a floor of 0.0 fails this test rather than silently disarming
        // the gate. (Asserting on the constant directly is a clippy
        // `assertions_on_constants` error, and the indirect assertion is the
        // stronger one anyway: it goes through the comparison both gates use.)
    }

    /// THE WRITER AND THE READER OF report.md MUST AGREE. `apply_heal.sh` hands
    /// the file `render_report` wrote to `darwind --heal-confidence`, which parses
    /// the score straight back out of it. A reworded header line would silently
    /// turn every proposal into NO_SCORE and the apply gate into a wall.
    #[test]
    fn report_confidence_survives_the_round_trip() {
        let d = diag_for(&["src/router.rs"], "router");
        let mut w = survivor(2, 0.82, 7);
        w.review_verdict = "Fixes the root cause.".to_string();
        let report = render_report(1_760_000_123, "mock-model", &d, &w);
        assert_eq!(
            parse_report_confidence(&report),
            Some(0.82),
            "the daemon writes a report.md its own confidence gate cannot read back:\n{report}"
        );
        assert_eq!(confidence_gate(&report).0, ConfidenceGate::Above);
        // The reviewer's own sentence must be in the HEADER, not only in the
        // section far below it — the HUD shows the first lines of this file.
        let head: String = report.lines().take(12).collect::<Vec<_>>().join("\n");
        assert!(
            head.contains("Fixes the root cause."),
            "the reviewer's verdict is not in the report header:\n{head}"
        );
        assert!(head.contains("floor"), "the score is stated with no bar beside it:\n{head}");
    }

    /// A GATE THAT ALWAYS ANSWERS THE SAME WORD IS NOT A GATE — and this one is
    /// what apply_heal.sh proves itself against before believing any verdict.
    #[test]
    fn the_confidence_gate_discriminates_across_its_three_verdicts() {
        assert_eq!(confidence_gate("- review confidence: 0.95\n").0, ConfidenceGate::Above);
        assert_eq!(confidence_gate("- review confidence: 0.01\n").0, ConfidenceGate::Below);
        assert_eq!(confidence_gate("- no score in this report\n").0, ConfidenceGate::NoScore);
        // The three words the shell case-arms match on.
        assert_eq!(ConfidenceGate::Above.word(), "ABOVE_FLOOR");
        assert_eq!(ConfidenceGate::Below.word(), "BELOW_FLOOR");
        assert_eq!(ConfidenceGate::NoScore.word(), "NO_SCORE");
        // NO SCORE IS NOT A ZERO SCORE. A missing line parsed as 0.0 would be
        // reported as "the reviewer did not back this", which is a different
        // (and false) statement about what happened.
        assert_eq!(parse_report_confidence("- review confidence: none\n"), None);
        assert_eq!(parse_report_confidence("- review confidence: 0.00\n"), Some(0.0));
        // ...and the below-floor sentence has to name the number and the bar.
        let (_, detail) = confidence_gate("- review confidence: 0.01\n");
        assert!(detail.contains("0.01") && detail.contains("0.25"), "{detail}");
    }

    /// "The reviewer scored it 0.00" and "the review call never came back" are
    /// opposite facts that both arrive as 0.0. The rejection report must say
    /// which one happened.
    #[test]
    fn below_floor_summary_distinguishes_a_low_score_from_no_review() {
        let mut scored = survivor(1, 0.08, 4);
        scored.reviewed = true;
        let s = below_floor_summary(&scored, 2);
        assert!(s.contains("0.08"), "{s}");
        assert!(s.contains("0.25"), "the floor it fell under must be stated:\n{s}");
        assert!(
            s.contains("NOT BECAUSE THE PATCHES FAILED A GATE"),
            "a below-floor attempt must not read as a gate failure:\n{s}"
        );

        let mut unreviewed = survivor(1, 0.0, 4);
        unreviewed.reviewed = false;
        let u = below_floor_summary(&unreviewed, 1);
        assert!(
            u.contains("never came back"),
            "a failed review call must not be reported as the reviewer's verdict:\n{u}"
        );
    }

    /// THE WHOLE POINT, END TO END: three candidates, the best of them scored
    /// below the floor by the adversarial reviewer, and NOTHING is proposed.
    /// Before this, `select_winner` handed that patch to the operator with an
    /// ACCEPT & APPLY button beside it and a 0.10 gauge nobody had to read.
    #[tokio::test]
    async fn nothing_is_proposed_when_the_best_review_is_below_the_floor() {
        let root = TempRoot::new("floor-e2e");
        let crate_dir = root.0.join("daemon");
        let heal_root = root.0.join("state").join("heal");
        write_synthetic_crate(&crate_dir);
        let ts = 1_760_000_011u64;

        // One candidate, and it VALIDATES (it is the real fix) — so nothing
        // mechanical stops it. Only the reviewer's score does.
        let draft = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n pub fn double(x: i32) -> i32 {\n-    x * y\n+    x * 2\n }\n";
        let brain = MockBrain {
            draft: draft.to_string(),
            reviews: vec![("x * 2".to_string(), 0.10)],
        };

        let result =
            run_attempt(&crate_dir, &heal_root, ts, "mock-model", &brain, &burst_scan_for_lib())
                .await;

        match result {
            AttemptResult::Rejected { stage, report, .. } => {
                assert_eq!(
                    stage, "confidence",
                    "a below-floor attempt is its own outcome, not a gate failure"
                );
                assert!(report.contains("0.10"), "the score must be in the report:\n{report}");
                assert!(report.contains("0.25"), "so must the floor:\n{report}");
            }
            AttemptResult::Proposed { confidence, .. } => panic!(
                "a patch the reviewer scored {confidence:.2} was proposed for one-click apply"
            ),
            AttemptResult::Aborted { stage } => panic!("aborted at {stage}"),
        }
        // NOTHING IS LOST: the candidate, its review and the diagnosis are on
        // disk for a human who wants to apply one by hand.
        let dir = heal_root.join("rejected").join(ts.to_string());
        for f in ["candidates.md", "diagnosis.json", "review.md"] {
            assert!(dir.join(f).exists(), "{f} was not written to {}", dir.display());
        }
        assert!(
            !heal_root.join("proposals").join(ts.to_string()).exists(),
            "a below-floor attempt must not write a proposal directory — apply_heal.sh takes \
             a <ts> and would install it"
        );
        assert!(
            std::fs::read_to_string(crate_dir.join("src").join("lib.rs"))
                .unwrap()
                .contains("x * y"),
            "propose mode must never touch the source tree"
        );
    }

    /// The confidence block of apply_heal.sh, BOUNDED at BOTH ends (its own
    /// `esac`). A head-only slice would run on into the mutation probe and the
    /// live apply below it and report on the wrong lines.
    fn confidence_block(script: &str) -> &str {
        let after = script
            .split("REVIEW-CONFIDENCE FLOOR (REFUSES)")
            .nth(1)
            .expect("apply_heal.sh has no review-confidence block");
        let end = after
            .find("\nesac\n")
            .expect("the confidence block is not esac-terminated");
        &after[..end]
    }

    /// ...and the window must actually BIND. One that matched nothing would make
    /// every assertion below pass vacuously.
    #[test]
    fn the_confidence_window_binds_to_a_real_block() {
        let script = include_str!("../../scripts/apply_heal.sh");
        let block = confidence_block(script);
        assert!(!block.trim().is_empty(), "the confidence window is empty");
        assert!(
            block.len() < script.len() / 2,
            "the window swallowed most of the script ({} of {} bytes)",
            block.len(),
            script.len()
        );
        assert!(
            !block.contains("stage \"applying\""),
            "the window runs past its block into the live apply:\n{block}"
        );
    }

    /// BOTH GATES OR NEITHER. The daemon refuses to PROPOSE below the floor; if
    /// the apply script did not refuse to INSTALL below it, a proposal written by
    /// an older daemon — or edited by hand — would be applied by the HUD's ACCEPT
    /// button under a weaker bar than the one that would have blocked writing it.
    #[test]
    fn the_apply_script_enforces_the_same_confidence_floor() {
        let script = include_str!("../../scripts/apply_heal.sh");
        let block = confidence_block(script);
        // It must judge THIS proposal's report, on a code line.
        assert!(
            block.lines().map(str::trim_start).any(|l| {
                !l.starts_with('#')
                    && l.contains("--heal-confidence")
                    && l.contains("\"$DIR/report.md\"")
            }),
            "the gate must run `--heal-confidence \"$DIR/report.md\"` on a code line — a gate \
             that scores anything but the proposal being installed proves nothing:\n{block}"
        );
        // FAIL CLOSED: an unknown flag makes darwind boot a daemon instead of
        // answering, so the flag is confirmed in the STAGED source first — BY ITS
        // ARGV COMPARISON. COUNTED: main.rs names `--heal-confidence` TWO more
        // times besides the dispatch — the entrypoint comment above the handler,
        // and that handler's eprintln! usage string, which is a CODE line. (Four
        // is --split-heal-diff's number, three is --heal-responsiveness's.) Two
        // is enough: the bare `grep -q -- '--heal-confidence'` this replaces
        // cleared the guard on them. Downstream, a booted daemon answers none of
        // the three self-proof probes, so the gate would refuse on "does not
        // discriminate" — or the apply would hang waiting for an answer. The
        // guard buys an immediate, correctly-worded refusal rather than either.
        assert!(
            block.contains(
                "grep -qE '^[[:space:]]*[^/[:space:]].*a == \"--heal-confidence\"' \
                 \"$CRATE/src/main.rs\""
            ),
            "the gate must confirm the staged daemon implements the flag BY ITS ARGV \
             COMPARISON:\n{block}"
        );
        assert!(
            block.contains("does not discriminate"),
            "the binary must be PROVEN to discriminate before its verdict is trusted:\n{block}"
        );
        // AND IT MUST REFUSE — unlike the responsiveness probe, which is advisory.
        for verdict in ["BELOW_FLOOR", "NO_SCORE"] {
            assert!(
                block.lines().any(|l| l.contains(verdict) && l.contains("fail ")),
                "`{verdict}` must FAIL the apply, not print a note:\n{block}"
            );
        }
        // An unrecognized verdict must refuse too, never pass through.
        let default_arm = block.rsplit("*)").next().unwrap_or("");
        assert!(
            default_arm.contains("fail "),
            "an unrecognized verdict must refuse the apply:\n{default_arm}"
        );
    }

    /// ONE implementation, TWO callers — the reason this is a flag and not a
    /// `grep`+`bc` in bash. Two copies of a threshold is the gates-drift-apart
    /// defect by construction.
    #[test]
    fn the_confidence_floor_has_exactly_one_implementation() {
        let main_rs = include_str!("main.rs");
        assert!(
            main_rs.contains("--heal-confidence"),
            "the shared flag must exist, or apply_heal.sh's grep guard fails the apply forever"
        );
        assert!(
            main_rs.contains("heal::confidence_gate(&report)"),
            "the CLI must CALL heal::confidence_gate, not reimplement the comparison"
        );
        // The script must not read the score itself anywhere in that block.
        let block = confidence_block(include_str!("../../scripts/apply_heal.sh"));
        for line in block.lines() {
            if line.contains("$DIR/report.md") {
                assert!(
                    line.contains("--heal-confidence") || line.trim_start().starts_with('#')
                        || line.contains("-f \"$DIR/report.md\""),
                    "the block reads report.md outside the shared gate, so there are now two \
                     parsers that can disagree:\n{line}"
                );
            }
        }
        // ...and the number itself lives in Rust, not in the script.
        assert!(
            !block.contains("0.25"),
            "the threshold is hard-coded in the shell — that is a second copy of the bar:\n{block}"
        );
    }

    /// ORDER. Like the responsiveness probe, this needs a crate that still
    /// COMPILES: the mutation probe below reverse-applies the patch's fix and
    /// leaves it out, and `cargo run --bin darwind` cannot be built from such a
    /// tree — every verdict would degrade to the self-proof failing and the apply
    /// would be refused for a reason that has nothing to do with the patch.
    #[test]
    fn the_confidence_gate_runs_before_the_mutation_reverse_apply() {
        let script = include_str!("../../scripts/apply_heal.sh");
        let gate_at = script
            .find("REVIEW-CONFIDENCE FLOOR (REFUSES)")
            .expect("apply_heal.sh has no review-confidence block");
        let reverse_at = script
            .find("-R <\"$SPLIT_DIR/fix.diff\"")
            .expect("apply_heal.sh no longer reverse-applies the fix");
        let apply_at = script.find("stage \"applying\"").expect("no live-apply stage");
        assert!(
            gate_at < reverse_at,
            "the confidence gate (at {gate_at}) runs after the mutation reverse-apply (at \
             {reverse_at}); that crate often no longer compiles and the gate's own self-proof \
             would fail for reasons unrelated to the patch"
        );
        assert!(gate_at < apply_at, "the confidence gate runs after the live apply");
    }

    /// EVERY staged-flag guard in apply_heal.sh must match the ARGV COMPARISON,
    /// never the flag's name in prose — ENUMERATED, not named one at a time.
    ///
    /// In this codebase an unknown flag is NOT an error: `std::env::args()
    /// .position(|a| a == "--flag")` simply does not match, and darwind falls
    /// through to ORDINARY DAEMON STARTUP. So apply_heal.sh confirms every flag it
    /// is about to invoke against the STAGED source first, or it would boot a
    /// daemon instead of getting an answer and the gate would be SKIPPED rather
    /// than enforced. main.rs discusses each of those flags in the comment block
    /// above its handler, so the guards' original form —
    /// `grep -q -- '--heal-confidence' "$CRATE/src/main.rs"` — matched that PROSE:
    /// on a staged source whose dispatch literal had drifted or been renamed, the
    /// script CLEARED ITS OWN FAIL-CLOSED GUARD and invoked a flag the staged
    /// daemon does not implement. MEASURED, by execution: with every
    /// `a == "--flag"` literal in the real main.rs renamed by one letter and the
    /// prose untouched, all three of those greps still ACCEPTED.
    ///
    /// The three per-flag parity tests above pin their own guard's text. A FOURTH
    /// guard added tomorrow in the prose-matching form would be pinned by nobody,
    /// which is how this defect got in: the TEST for --heal-responsiveness was
    /// hardened and the SCRIPT was not. So this enumerates rather than names.
    #[test]
    fn every_staged_flag_guard_matches_the_argv_comparison_not_the_prose() {
        let script = include_str!("../../scripts/apply_heal.sh");

        // Does this line's grep pattern name a long CLI flag? A `--` that OPENS a
        // quoted token — `'--split-heal-diff'`, `"--heal-confidence"` — does.
        // grep's own bare `--` end-of-options separator (preceded by a space) does
        // not, so a future non-flag source guard is not dragged into this rule.
        fn names_a_cli_flag(line: &str) -> bool {
            line.as_bytes().windows(4).any(|w| {
                (w[0] == b'\'' || w[0] == b'"')
                    && w[1] == b'-'
                    && w[2] == b'-'
                    && w[3].is_ascii_lowercase()
            })
        }

        let guards: Vec<&str> = script
            .lines()
            .map(str::trim_start)
            .filter(|l| {
                !l.starts_with('#')
                    && l.contains("grep")
                    && l.contains("\"$CRATE/src/")
                    && names_a_cli_flag(l)
            })
            .collect();

        // A FOR-ALL OVER NOTHING IS NOT A CHECK. If the enumeration stops binding —
        // the guards renamed, $CRATE spelled differently — the loop below would
        // iterate zero lines and pronounce every guard sound having read none.
        assert!(
            guards.len() >= 3,
            "expected at least the three staged-flag guards (--heal-responsiveness, \
             --heal-confidence, --split-heal-diff); the enumeration found {}, so it no \
             longer binds and this test would pass over nothing:\n{guards:#?}",
            guards.len()
        );

        for g in &guards {
            // FAIL-CLOSED SHAPE: a missing flag must refuse, never fall through.
            assert!(
                g.starts_with("if ! grep"),
                "a staged-flag guard that is not `if ! grep ...` does not fail closed:\n{g}"
            );
            // ...AND IT MUST READ THE DISPATCH, NOT THE DOCUMENTATION.
            assert!(
                g.contains("a == \"--"),
                "this flag guard matches main.rs's PROSE, not its argv comparison. On a \
                 staged source whose dispatch literal drifted it clears itself and then \
                 invokes a flag the daemon does not implement — and an unknown flag boots \
                 the daemon rather than erroring. Anchor it on `a == \"--<flag>\"`:\n{g}"
            );
            // ...and the comment class must be present, or a `//` line quoting the
            // argv form re-opens the same hole one level down. PROVED by execution:
            // without it, a comment carrying `a == "--heal-responsiveness"` clears
            // the guard on a source whose dispatch literal had been renamed.
            assert!(
                g.contains("[^/[:space:]]"),
                "this flag guard would accept the argv form quoted inside a `//` comment; \
                 it must exclude comment lines:\n{g}"
            );
        }

        // ...and each flag the script actually invokes must HAVE one. Enumeration
        // alone is satisfied by three guards that all name the same flag.
        for flag in ["--heal-responsiveness", "--heal-confidence", "--split-heal-diff"] {
            let needle = format!("a == \"{flag}\"");
            let n = guards.iter().filter(|g| g.contains(needle.as_str())).count();
            assert_eq!(
                n, 1,
                "exactly one staged-flag guard must anchor on `{needle}`; found {n}:\n{guards:#?}"
            );
        }
    }
}
