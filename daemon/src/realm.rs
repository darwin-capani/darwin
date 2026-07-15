//! SCRATCH REALMS (realm.rs) — a disposable, confined build+test sandbox that
//! VERIFIES a proposed code change (from [`crate::code::code_propose_diff`]) BEFORE
//! a human ever applies it. It closes the honesty gap `code.rs` admits: a proposed
//! diff's compile/test CORRECTNESS is NOT guaranteed by the propose path, so a
//! Realm actually BUILDS + TESTS the change — in a throwaway, network-denied copy —
//! and attaches the REAL pass/fail verdict to the proposal card.
//!
//! ## The pipeline (device-gated runner)
//!   1. MATERIALIZE a confined COPY-ON-WRITE checkout of an allowlisted
//!      [`crate::config::CodeConfig::roots`] repo into `state/realms/<ts>/` — the
//!      realm. The user's real tree is READ for the copy but NEVER written.
//!   2. APPLY the proposed diff INTO the realm ONLY (never the user's tree).
//!   3. RUN the configured build/test command inside the realm via the EXISTING
//!      device-gated [`crate::shell::run_sandboxed`] seam under a DENY-DEFAULT,
//!      NETWORK-DENIED SBPL profile whose only writable location is the realm.
//!   4. CAPTURE the REAL exit + bounded output, MAP it to a [`RealmVerdict`], and
//!      attach it to the proposal (a `realm_verdict.md` artifact + a secret-free
//!      `realm.verdict` telemetry frame).
//!   5. TEAR the realm down.
//!
//! ## The CONTRACT (non-negotiable, mirrors code.rs / shell.rs)
//!   * READ-THE-TREE, NEVER-WRITE-IT: the realm is a COW COPY under `state/realms/`.
//!     The daemon NEVER writes the user's real tree. Apply-to-real stays the
//!     SEPARATE, human-gated `scripts/apply_code_diff.sh` with its own
//!     re-validation — this module does not touch it.
//!   * ARMED BUT INERT WITHOUT DEPS: gated by [`realm_permitted`] — `[realm].enabled`
//!     ships true, but a Realm can only run with an allowlisted `[code].roots` repo
//!     AND `[shell].enabled` (it reuses the sandboxed-exec seam). With either unmet
//!     the feature is inert.
//!   * CONFINED BY CONSTRUCTION: a realm path is validated to sit strictly UNDER
//!     `state/realms/` ([`is_confined_realm`]) before any runner touches it, and the
//!     build runs under the deny-default write-confined SBPL profile — never the
//!     user's tree, never the network.
//!   * HONEST-UNVERIFIED: when the shell sandbox / git tooling is unavailable, the
//!     checkout/apply fails, or no verify command is configured, the Realm reports
//!     [`RealmVerdict::Unverified`] — it NEVER fakes a pass. A `Passed` verdict means
//!     the build/test REALLY ran and exited zero.
//!   * DEVICE-GATED EXEC: the orchestration ([`verify_proposal`]), path
//!     construction, confinement, and verdict mapping are PURE + unit-tested; the
//!     actual worktree+exec ([`SandboxedRealmRunner`]) is the device-gated runner
//!     (built, NEVER invoked under `cargo test`), exactly like
//!     [`crate::shell::run_sandboxed`].

use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;

use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// (0) GATE — may a Realm run at all? ARMED BY DEFAULT, INERT WITHOUT DEPS.
// Mirrors code::code_permitted / shell::shell_permitted.
// ---------------------------------------------------------------------------

/// Whether Scratch Realms may run: `[realm].enabled` is on AND at least one
/// `[code].roots` repo is allowlisted (the tree to copy) AND `[shell].enabled` is
/// on (the Realm reuses the sandboxed-exec seam to run the build/test). With any of
/// the three unmet the feature is inert — armed by default, but it can do nothing
/// without a codebase to verify and the shell sandbox to verify it in.
pub fn realm_permitted(enabled: bool, code_roots: &[String], shell_enabled: bool) -> bool {
    enabled && !code_roots.is_empty() && shell_enabled
}

// ---------------------------------------------------------------------------
// (1) PATH CONSTRUCTION (PURE) — every realm lives under state/realms/<ts>/.
// ---------------------------------------------------------------------------

/// The root every realm is materialized under: `<state_dir>/realms`. Distinct from
/// the code proposal store (`state/code`) and the shell scratch (`state/shell`), so
/// a realm never overlaps another subsystem's confinement.
pub fn realms_root(state_dir: &Path) -> PathBuf {
    state_dir.join("realms")
}

/// The confined directory for the realm verifying proposal `ts`:
/// `<state_dir>/realms/<ts>`. Always a strict descendant of [`realms_root`], so
/// [`is_confined_realm`] holds for it by construction.
pub fn realm_dir(state_dir: &Path, ts: u64) -> PathBuf {
    realms_root(state_dir).join(ts.to_string())
}

// ---------------------------------------------------------------------------
// (2) CONFINEMENT CHECK (PURE) — a realm path MUST sit strictly under
// state/realms/. The single defensive chokepoint before any runner touches a
// path; mirrors forge::is_confined_relpath's `..`/absolute stance.
// ---------------------------------------------------------------------------

/// Is `candidate` a legitimate realm path — i.e. a STRICT descendant of
/// `realms_root`, with no parent-dir (`..`) traversal? Purely LEXICAL (the realm
/// dir may not exist yet, so no canonicalization): a candidate that contains a `..`
/// component, that is not prefixed by `realms_root`, or that IS `realms_root`
/// itself (not a subdir) is REFUSED. This is the defense-in-depth gate the
/// orchestrator runs before handing a path to the device-gated runner, so a
/// runner can never operate on a path that escaped `state/realms/`.
pub fn is_confined_realm(candidate: &Path, realms_root: &Path) -> bool {
    // Any explicit parent-dir traversal is refused outright (a `..` could, once
    // resolved, climb out of the realms root even if the lexical prefix matched).
    if candidate.components().any(|c| matches!(c, Component::ParentDir)) {
        return false;
    }
    // Must be a STRICT descendant: strip_prefix succeeds AND leaves a non-empty
    // remainder (an exact match — the realms root itself — is not a realm).
    match candidate.strip_prefix(realms_root) {
        Ok(rest) => rest.components().next().is_some(),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// (3) THE RUNNER SEAM (trait) — the ONLY route to the actual worktree+exec.
// Production uses SandboxedRealmRunner (git/COW checkout + shell::run_sandboxed);
// unit tests inject a mock so NO real copy/exec is ever made under `cargo test`.
// Mirrors code::CodeBrain.
// ---------------------------------------------------------------------------

/// What the device-gated runner is asked to verify: copy `code_root` into the
/// confined `realm_dir`, apply `diff` there, and run `verify_command` inside it.
/// Borrows only — the runner owns no state to leak.
pub struct RealmSpec<'a> {
    /// The allowlisted `[code].roots` repo to COPY (read-only) into the realm.
    pub code_root: &'a Path,
    /// The confined destination under `state/realms/<ts>/` (already confinement-checked).
    pub realm_dir: &'a Path,
    /// The proposed unified diff to apply INTO the realm (never the user's tree).
    pub diff: &'a str,
    /// The build/test command run INSIDE the realm under the sandboxed-exec seam.
    pub verify_command: &'a str,
}

/// The raw outcome of a runner: either the verify command REALLY ran (carrying its
/// real exit + bounded output), or the realm could not be verified at all (sandbox
/// / git absent, checkout or apply failed). NEVER a fabricated pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RealmRunOutcome {
    /// The build/test command actually executed. `exit_code` is `None` if it was
    /// killed by signal; `timed_out` marks the wall-clock kill.
    Ran { exit_code: Option<i32>, output: String, timed_out: bool },
    /// The realm could not be verified — the sandbox/git tooling is unavailable, or
    /// the COW checkout / diff apply failed. `reason` is a daemon-authored, secret-
    /// free explanation. HONEST: this is NEVER a pass.
    Unavailable { reason: String },
}

/// A `Send` future spelled out so the trait stays object-safe (`&dyn RealmRunner`)
/// without the async-trait crate (no new deps). Mirrors [`crate::code::CodeBrainFuture`].
pub type RealmRunFuture<'a> = Pin<Box<dyn Future<Output = RealmRunOutcome> + Send + 'a>>;

/// The runner seam — the ONLY route to the actual worktree+exec. Production is
/// [`SandboxedRealmRunner`]; unit tests inject a mock returning a canned outcome so
/// no real copy/exec happens under `cargo test`.
pub trait RealmRunner: Send + Sync {
    /// Materialize the realm, apply the diff into it, run the verify command inside
    /// it under the sandboxed-exec seam, capture the real result, and tear it down.
    fn run<'a>(&'a self, spec: &'a RealmSpec<'a>) -> RealmRunFuture<'a>;
}

// ---------------------------------------------------------------------------
// (4) VERDICT — map a raw run outcome to the honest proposal annotation.
// ---------------------------------------------------------------------------

/// The verdict attached to a proposal card. `Passed` means the build/test REALLY
/// ran and exited zero; `Failed` means it ran and exited non-zero; `Unverified` is
/// the HONEST fallback — the Realm could not determine a pass/fail (sandbox absent,
/// checkout/apply failed, timed out, killed, or no verify command). It is NEVER a
/// silent pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RealmVerdict {
    /// The verify command ran and exited zero — the change builds+tests clean.
    Passed { output: String },
    /// The verify command ran and exited non-zero — the change does NOT verify.
    Failed { exit_code: Option<i32>, output: String },
    /// The Realm could not determine a verdict. Honest, never a faked pass.
    Unverified { reason: String },
}

/// Map a raw [`RealmRunOutcome`] to the honest [`RealmVerdict`]:
///   * exit 0 (not timed out) => [`RealmVerdict::Passed`];
///   * non-zero exit (not timed out) => [`RealmVerdict::Failed`];
///   * timed out, or killed with no exit code => [`RealmVerdict::Unverified`] — we
///     could NOT determine pass/fail, so we never claim either (conservatively
///     honest: a hung/killed run is unverified, not a silent pass or a false fail);
///   * [`RealmRunOutcome::Unavailable`] => [`RealmVerdict::Unverified`].
pub fn verdict_from_outcome(outcome: RealmRunOutcome) -> RealmVerdict {
    match outcome {
        RealmRunOutcome::Ran { timed_out: true, .. } => RealmVerdict::Unverified {
            reason: "the build/test run exceeded the time limit before completing".to_string(),
        },
        RealmRunOutcome::Ran { exit_code: Some(0), output, timed_out: false } => {
            RealmVerdict::Passed { output }
        }
        RealmRunOutcome::Ran { exit_code: Some(code), output, timed_out: false } => {
            RealmVerdict::Failed { exit_code: Some(code), output }
        }
        // No exit code and not a timeout => killed by signal; we cannot honestly
        // call that a pass OR a fail.
        RealmRunOutcome::Ran { exit_code: None, .. } => RealmVerdict::Unverified {
            reason: "the build/test process was terminated before it finished".to_string(),
        },
        RealmRunOutcome::Unavailable { reason } => RealmVerdict::Unverified { reason },
    }
}

/// The one-word verdict label used in telemetry + the proposal artifact.
pub fn verdict_label(verdict: &RealmVerdict) -> &'static str {
    match verdict {
        RealmVerdict::Passed { .. } => "passed",
        RealmVerdict::Failed { .. } => "failed",
        RealmVerdict::Unverified { .. } => "unverified",
    }
}

/// Cap on the build/test output folded into the `realm_verdict.md` artifact, so a
/// runaway build log cannot bloat the proposal store. The runner already bounds its
/// capture ([`crate::shell::MAX_OUTPUT_BYTES`]); this is a second, artifact-side cap.
pub const MAX_ANNOTATION_OUTPUT_BYTES: usize = 16 * 1024;

/// The reviewable `realm_verdict.md` attached to the proposal card. States the
/// verdict plainly, names WHY (exit code / unverified reason), and includes the
/// bounded build/test output for a human reviewer. This artifact lives UNDER the
/// proposal store (`state/`), never on the network — so it may carry the real
/// output; the telemetry frame ([`verdict_telemetry`]) stays secret-free.
pub fn verdict_annotation(verdict: &RealmVerdict) -> String {
    let mut s = String::new();
    s.push_str("# Realm verification\n\n");
    match verdict {
        RealmVerdict::Passed { output } => {
            s.push_str(
                "## Verdict: PASSED\n\nThe proposed change was applied to a disposable, \
                 network-denied copy of your codebase and the configured build/test command \
                 ran there and exited 0. This verdict is REAL — it is not a claim; the build/test \
                 actually ran. Apply-to-real is still your separate, human-reviewed step.\n\n",
            );
            push_output_block(&mut s, output);
        }
        RealmVerdict::Failed { exit_code, output } => {
            let code = exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            s.push_str(&format!(
                "## Verdict: FAILED (exit {code})\n\nThe proposed change was applied to a \
                 disposable copy of your codebase and the configured build/test command ran there \
                 and exited non-zero. The change does NOT verify as-is — review the output below \
                 before applying it.\n\n",
            ));
            push_output_block(&mut s, output);
        }
        RealmVerdict::Unverified { reason } => {
            s.push_str(&format!(
                "## Verdict: UNVERIFIED\n\nThe change could NOT be built/tested in a Realm \
                 ({reason}). This is reported honestly — it is NOT a pass. Review the diff with \
                 extra care before applying it.\n",
            ));
        }
    }
    s
}

/// Append a bounded fenced output block (empty output => an honest "no output").
fn push_output_block(s: &mut String, output: &str) {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        s.push_str("## Build/test output\n(no output)\n");
        return;
    }
    let bounded = bounded_str(trimmed, MAX_ANNOTATION_OUTPUT_BYTES);
    s.push_str("## Build/test output\n```\n");
    s.push_str(&bounded);
    if !bounded.ends_with('\n') {
        s.push('\n');
    }
    s.push_str("```\n");
}

/// Truncate `text` to at most `max` bytes on a char boundary, appending an honest
/// marker when it was cut.
fn bounded_str(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[output truncated]", &text[..end])
}

/// The SECRET-FREE telemetry frame emitted with the verdict. Carries ONLY the
/// proposal ts, the verdict label, the exit code (a number), and — for an
/// unverified verdict — the daemon-authored reason string. It NEVER includes the
/// build/test OUTPUT, the diff, or any filesystem path, so a HUD/telemetry consumer
/// learns the outcome without ever seeing code or paths.
pub fn verdict_telemetry(ts: u64, verdict: &RealmVerdict) -> Value {
    let exit_code = match verdict {
        RealmVerdict::Failed { exit_code, .. } => *exit_code,
        _ => None,
    };
    let reason = match verdict {
        RealmVerdict::Unverified { reason } => Some(reason.as_str()),
        _ => None,
    };
    json!({
        "ts": ts,
        "verdict": verdict_label(verdict),
        "exit_code": exit_code,
        "reason": reason,
    })
}

/// A one-line spoken summary of the verdict folded into the propose tool's reply,
/// so the user hears the REAL verification outcome alongside the apply command.
pub fn verdict_reply_line(verdict: &RealmVerdict) -> String {
    match verdict {
        RealmVerdict::Passed { .. } => {
            "I also built and tested it in a throwaway, network-denied copy of your codebase \
             and it passed there — but review it yourself before applying."
                .to_string()
        }
        RealmVerdict::Failed { exit_code, .. } => {
            let code = exit_code
                .map(|c| format!(" (exit {c})"))
                .unwrap_or_default();
            format!(
                "Heads up: I built and tested it in a throwaway copy of your codebase and it \
                 FAILED there{code} — I would not apply it as-is. The details are in the proposal."
            )
        }
        RealmVerdict::Unverified { reason } => {
            format!(
                "I could not build/test it in a Realm ({reason}), so this change is UNVERIFIED — \
                 I am not claiming it works; review it with care."
            )
        }
    }
}

// ---------------------------------------------------------------------------
// (5) ORCHESTRATOR (PURE given a runner) — tie path construction + confinement +
// verdict mapping together. Fully unit-tested with a mock runner; the real
// worktree+exec is the injected device-gated runner.
// ---------------------------------------------------------------------------

/// Verify a proposed `diff` for proposal `ts` in a Scratch Realm. PURE
/// orchestration given the `runner` seam:
///   1. construct the confined realm path under `state/realms/<ts>/`;
///   2. DEFENSE IN DEPTH — refuse (honest Unverified) if that path is not confined;
///   3. HONEST — refuse (Unverified) if no verify command is configured (never a
///      faked pass);
///   4. hand the spec to the runner (production: git/COW checkout + sandboxed
///      build/test; tests: a mock) and map its outcome to a [`RealmVerdict`].
///
/// It NEVER writes the user's tree — the runner copies the tree read-only into the
/// confined realm and applies the diff THERE. The gate ([`realm_permitted`]) is the
/// caller's responsibility (it owns the live config), exactly like the code/shell
/// tool cores.
pub async fn verify_proposal(
    runner: &dyn RealmRunner,
    state_dir: &Path,
    code_root: &Path,
    ts: u64,
    diff: &str,
    verify_command: &str,
) -> RealmVerdict {
    let root = realms_root(state_dir);
    let dir = realm_dir(state_dir, ts);
    // (2) Defense in depth: a realm path that is not strictly confined under
    // state/realms/ is refused before any runner touches it. `realm_dir` builds a
    // confined path by construction; this guards a future caller / a tampered ts.
    if !is_confined_realm(&dir, &root) {
        return RealmVerdict::Unverified {
            reason: "the realm path failed confinement under state/realms/".to_string(),
        };
    }
    // (3) Honest: with nothing to run we cannot verify — say so, never fake a pass.
    if verify_command.trim().is_empty() {
        return RealmVerdict::Unverified {
            reason: "no build/test command is configured for the Realm".to_string(),
        };
    }
    let spec = RealmSpec { code_root, realm_dir: &dir, diff, verify_command };
    verdict_from_outcome(runner.run(&spec).await)
}

// ---------------------------------------------------------------------------
// (6) DEVICE-GATED RUNNER — the real worktree+exec. Built, NEVER invoked under
// `cargo test` (the vision-capture / apply-heal / shell::run_sandboxed precedent).
// It COW-copies the allowlisted repo into the confined realm, applies the diff
// THERE, runs the build/test under crate::shell::run_sandboxed, and tears down.
// ---------------------------------------------------------------------------

/// `/bin/cp` — used with `-Rc` for an APFS COPY-ON-WRITE clone (cheap, reads the
/// source, writes only the realm). Falls back to a plain recursive copy if `-c`
/// (clonefile) is unsupported by the filesystem.
const CP: &str = "/bin/cp";

/// `git` — used ONLY to apply the diff INSIDE the realm (`git -C <realm> apply`),
/// never against the user's real tree. Resolved via PATH by the runner.
const GIT: &str = "git";

/// Wall-clock ceiling for a single realm-setup subprocess (the COW copy / the diff
/// apply). The build/test itself is bounded by [`crate::shell::EXEC_TIMEOUT`].
const SETUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The device-gated production runner. Materializes a confined COW copy of the
/// allowlisted repo, applies the diff into it, runs the build/test under the
/// deny-default, network-denied [`crate::shell`] sandbox, captures the REAL result,
/// and tears the realm down. It holds the `home` + `daemon_state` paths the SBPL
/// secret-denials need (supplied by the caller, exactly like the shell tool).
///
/// IT IS BUILT, NOT INVOKED IN ANY TEST — the real copy/exec only ever happens
/// on-device behind the full gate ([`realm_permitted`] + the code/shell configs).
/// The orchestration, confinement, and verdict mapping are proven hermetically
/// with a mock runner; this actuator is device-gated.
pub struct SandboxedRealmRunner {
    /// The user's home dir (for the SBPL Keychain / ~/.claude / ~/.ssh denials).
    pub home: PathBuf,
    /// The daemon's own `state/` dir, denied (read+write) inside the sandbox so a
    /// realm build can never touch DARWIN's own state.
    pub daemon_state: PathBuf,
}

impl RealmRunner for SandboxedRealmRunner {
    fn run<'a>(&'a self, spec: &'a RealmSpec<'a>) -> RealmRunFuture<'a> {
        Box::pin(async move { self.run_impl(spec).await })
    }
}

impl SandboxedRealmRunner {
    /// The real pipeline. Returns [`RealmRunOutcome::Unavailable`] (=> Unverified,
    /// never a faked pass) at every setup failure; only a build/test that actually
    /// ran yields [`RealmRunOutcome::Ran`].
    async fn run_impl(&self, spec: &RealmSpec<'_>) -> RealmRunOutcome {
        // Preconditions: the sandboxed-exec seam + the copy tool must exist on-device.
        if !Path::new(crate::shell::SANDBOX_EXEC).exists() {
            return RealmRunOutcome::Unavailable {
                reason: "the shell sandbox (/usr/bin/sandbox-exec) is unavailable on this device"
                    .to_string(),
            };
        }
        if !Path::new(CP).exists() {
            return RealmRunOutcome::Unavailable {
                reason: "the copy tool (/bin/cp) is unavailable on this device".to_string(),
            };
        }

        // (1) MATERIALIZE — a fresh, empty realm dir, then a COW copy of the repo
        // INTO it. `cp -Rc <root>/. <realm>` clones the tree (APFS copy-on-write)
        // reading the source and writing ONLY the realm; the user's tree is untouched.
        let realm = spec.realm_dir;
        let _ = std::fs::remove_dir_all(realm); // clear a stale realm from a crashed run
        if let Err(e) = std::fs::create_dir_all(realm) {
            return RealmRunOutcome::Unavailable {
                reason: format!("could not create the realm dir: {e}"),
            };
        }
        // Cleanup guard: whatever happens below, tear the realm down on the way out.
        let _guard = RealmTeardown(realm.to_path_buf());

        if let Err(reason) = self.cow_copy(spec.code_root, realm).await {
            return RealmRunOutcome::Unavailable { reason };
        }

        // (2) APPLY the diff INTO the realm ONLY. Write the diff to a file inside the
        // realm and `git -C <realm> apply` it. A diff that does not apply cleanly is
        // an honest Unverified (the proposal is stale/wrong against the current tree)
        // — never applied to the user's real tree, never a faked pass.
        if let Err(reason) = self.apply_diff(realm, spec.diff).await {
            return RealmRunOutcome::Unavailable { reason };
        }

        // (3) RUN the build/test INSIDE the realm under the deny-default, NETWORK-
        // DENIED shell sandbox whose ONLY writable location is the realm itself.
        let profile = crate::shell::generate_shell_sbpl(realm, &self.home, &self.daemon_state);
        match crate::shell::run_sandboxed(spec.verify_command, &profile, realm).await {
            Ok(result) => {
                let mut output = String::new();
                if !result.stdout.trim().is_empty() {
                    output.push_str(&result.stdout);
                }
                if !result.stderr.trim().is_empty() {
                    if !output.is_empty() {
                        output.push('\n');
                    }
                    output.push_str(&result.stderr);
                }
                if result.truncated {
                    output.push_str("\n[output truncated]");
                }
                RealmRunOutcome::Ran {
                    exit_code: result.exit_code,
                    output,
                    timed_out: result.timed_out,
                }
            }
            Err(e) => RealmRunOutcome::Unavailable {
                reason: format!("the sandboxed build/test could not start: {e}"),
            },
        }
        // `_guard` drops here and tears the realm down.
    }

    /// COW-copy `src` into `dst` with `cp -Rc src/. dst` (APFS clonefile), falling
    /// back to a plain `cp -R` when clonefile is unsupported. Reads `src`, writes
    /// only `dst`. Returns Err(reason) on failure.
    async fn cow_copy(&self, src: &Path, dst: &Path) -> Result<(), String> {
        // `<src>/.` copies the CONTENTS of src into the existing dst dir.
        let src_contents = format!("{}/.", src.display());
        // Try the copy-on-write clone first.
        match run_fixed(CP, &["-Rc".as_ref(), src_contents.as_ref(), dst.as_os_str()]).await {
            Ok(true) => Ok(()),
            _ => {
                // Fall back to a plain recursive copy (clonefile unsupported / cross-fs).
                let _ = std::fs::remove_dir_all(dst);
                if std::fs::create_dir_all(dst).is_err() {
                    return Err("could not re-create the realm dir for the fallback copy".to_string());
                }
                match run_fixed(CP, &["-R".as_ref(), src_contents.as_ref(), dst.as_os_str()]).await {
                    Ok(true) => Ok(()),
                    _ => Err("could not copy the codebase into the realm".to_string()),
                }
            }
        }
    }

    /// Apply `diff` inside `realm` with `git -C <realm> apply <patchfile>`. The
    /// patch file lives inside the realm (the only writable place). A non-clean
    /// apply is surfaced to the caller as an honest failure.
    async fn apply_diff(&self, realm: &Path, diff: &str) -> Result<(), String> {
        let patch_path = realm.join(".realm-proposal.diff");
        if std::fs::write(&patch_path, diff).is_err() {
            return Err("could not stage the proposed diff inside the realm".to_string());
        }
        let ok = run_fixed(
            GIT,
            &[
                "-C".as_ref(),
                realm.as_os_str(),
                "apply".as_ref(),
                patch_path.as_os_str(),
            ],
        )
        .await
        .unwrap_or(false);
        let _ = std::fs::remove_file(&patch_path);
        if ok {
            Ok(())
        } else {
            Err("the proposed diff did not apply cleanly to the realm".to_string())
        }
    }
}

/// RAII teardown: remove the realm dir when the guard drops, so a realm is always
/// disposable — even if the pipeline returns early. Best-effort.
struct RealmTeardown(PathBuf);
impl Drop for RealmTeardown {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run a FIXED-ARG subprocess (no shell, no interpolation) with a timeout, returning
/// Ok(true) on a zero exit. Used ONLY for the daemon-controlled COW copy + diff
/// apply — NEVER for the untrusted build/test (that goes through the sandboxed-exec
/// seam). Device-gated: only reached from [`SandboxedRealmRunner`].
async fn run_fixed(program: &str, args: &[&std::ffi::OsStr]) -> Result<bool, ()> {
    use tokio::process::Command;
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let child = cmd.spawn().map_err(|_| ())?;
    match tokio::time::timeout(SETUP_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(out)) => Ok(out.status.success()),
        _ => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =====================================================================
    // (0) GATE — realm_permitted: armed by default, inert without deps
    // =====================================================================

    #[test]
    fn realm_permitted_needs_the_switch_a_root_and_the_shell_sandbox() {
        let root = vec!["/some/repo".to_string()];
        // The happy path: on + a code root + shell on.
        assert!(realm_permitted(true, &root, true), "on + root + shell => permitted");
        // Each missing dependency makes it inert.
        assert!(!realm_permitted(false, &root, true), "off => inert");
        assert!(!realm_permitted(true, &[], true), "no code root => inert (nothing to verify)");
        assert!(!realm_permitted(true, &root, false), "shell sandbox off => inert (no exec seam)");
        assert!(!realm_permitted(false, &[], false), "nothing on => inert");
    }

    // =====================================================================
    // (1)+(2) PATH CONSTRUCTION + CONFINEMENT
    // =====================================================================

    #[test]
    fn realm_paths_are_built_under_state_realms() {
        let state = Path::new("/proj/state");
        assert_eq!(realms_root(state), Path::new("/proj/state/realms"));
        assert_eq!(realm_dir(state, 1700), Path::new("/proj/state/realms/1700"));
    }

    #[test]
    fn a_realm_path_is_confined_and_a_traversal_is_refused() {
        let state = Path::new("/proj/state");
        let root = realms_root(state);

        // A real realm dir (a strict descendant of state/realms/) is confined.
        let good = realm_dir(state, 42);
        assert!(is_confined_realm(&good, &root), "state/realms/42 is confined");
        assert!(
            is_confined_realm(&root.join("1700/subdir"), &root),
            "a nested path under a realm is confined"
        );

        // A `..` traversal is REFUSED even if it lexically starts under the root —
        // it could climb out once resolved.
        let traversal = root.join("42/../../../etc/victim");
        assert!(!is_confined_realm(&traversal, &root), "a `..` traversal must be refused");
        let sneaky = Path::new("/proj/state/realms/../secrets");
        assert!(!is_confined_realm(sneaky, &root), "climbing out of realms/ must be refused");

        // The realms root ITSELF is not a realm (must be a strict subdir).
        assert!(!is_confined_realm(&root, &root), "the realms root itself is not a realm");

        // A path entirely OUTSIDE the realms root is refused.
        assert!(!is_confined_realm(Path::new("/proj/state/code/proposals/1"), &root), "outside realms/ refused");
        assert!(!is_confined_realm(Path::new("/etc/passwd"), &root), "an absolute foreign path refused");
    }

    // =====================================================================
    // (4) VERDICT MAPPING — pass/fail/unverified, honestly
    // =====================================================================

    #[test]
    fn verdict_mapping_is_honest_about_pass_fail_and_unknown() {
        // Exit 0 => Passed.
        let v = verdict_from_outcome(RealmRunOutcome::Ran {
            exit_code: Some(0),
            output: "ok".into(),
            timed_out: false,
        });
        assert!(matches!(v, RealmVerdict::Passed { .. }), "exit 0 => Passed, got {v:?}");
        assert_eq!(verdict_label(&v), "passed");

        // Non-zero exit => Failed (carrying the code).
        let v = verdict_from_outcome(RealmRunOutcome::Ran {
            exit_code: Some(101),
            output: "boom".into(),
            timed_out: false,
        });
        assert!(matches!(v, RealmVerdict::Failed { exit_code: Some(101), .. }), "non-zero => Failed, got {v:?}");
        assert_eq!(verdict_label(&v), "failed");

        // A timeout is UNVERIFIED — never a silent pass, never a false fail.
        let v = verdict_from_outcome(RealmRunOutcome::Ran {
            exit_code: None,
            output: String::new(),
            timed_out: true,
        });
        assert!(matches!(v, RealmVerdict::Unverified { .. }), "timeout => Unverified, got {v:?}");

        // A kill with no exit code is UNVERIFIED (we cannot claim pass or fail).
        let v = verdict_from_outcome(RealmRunOutcome::Ran {
            exit_code: None,
            output: String::new(),
            timed_out: false,
        });
        assert!(matches!(v, RealmVerdict::Unverified { .. }), "killed => Unverified, got {v:?}");

        // Unavailable => Unverified, preserving the honest reason.
        let v = verdict_from_outcome(RealmRunOutcome::Unavailable { reason: "no sandbox".into() });
        assert_eq!(v, RealmVerdict::Unverified { reason: "no sandbox".into() });
        assert_eq!(verdict_label(&v), "unverified");
    }

    // =====================================================================
    // (4) TELEMETRY — secret-free: the verdict, never the output/diff/paths
    // =====================================================================

    #[test]
    fn verdict_telemetry_is_secret_free() {
        // A Failed verdict whose OUTPUT contains a secret-looking marker + a path.
        let secret = "SECRET_TOKEN_/Users/me/.ssh/id_rsa";
        let v = RealmVerdict::Failed {
            exit_code: Some(1),
            output: format!("error: {secret}"),
        };
        let frame = verdict_telemetry(1700, &v);
        let text = frame.to_string();
        // It carries the verdict + exit code.
        assert_eq!(frame["verdict"], "failed");
        assert_eq!(frame["exit_code"], 1);
        assert_eq!(frame["ts"], 1700);
        // It NEVER carries the build output / the embedded path.
        assert!(!text.contains(secret), "telemetry must not leak the build output: {text}");
        assert!(!text.contains("id_rsa"), "telemetry must not leak a path: {text}");

        // An unverified verdict carries the daemon-authored reason (not user data).
        let v = RealmVerdict::Unverified { reason: "no sandbox".into() };
        let frame = verdict_telemetry(1, &v);
        assert_eq!(frame["verdict"], "unverified");
        assert_eq!(frame["reason"], "no sandbox");

        // A passed verdict emits no exit_code / reason (both null) — the OUTPUT is
        // never in the frame.
        let v = RealmVerdict::Passed { output: secret.into() };
        let frame = verdict_telemetry(2, &v);
        assert_eq!(frame["verdict"], "passed");
        assert!(frame["exit_code"].is_null(), "passed => no exit code");
        assert!(!frame.to_string().contains(secret), "passed telemetry must not leak output");
    }

    #[test]
    fn verdict_annotation_states_the_verdict_plainly() {
        let a = verdict_annotation(&RealmVerdict::Passed { output: "test result: ok".into() });
        assert!(a.contains("PASSED"), "annotation states PASSED: {a}");
        assert!(a.contains("test result: ok"), "annotation includes the real output: {a}");

        let a = verdict_annotation(&RealmVerdict::Failed {
            exit_code: Some(101),
            output: "panicked".into(),
        });
        assert!(a.contains("FAILED (exit 101)"), "annotation states FAILED + code: {a}");
        assert!(a.contains("panicked"), "annotation includes the failing output: {a}");

        let a = verdict_annotation(&RealmVerdict::Unverified { reason: "no sandbox".into() });
        assert!(a.contains("UNVERIFIED"), "annotation states UNVERIFIED: {a}");
        assert!(a.contains("no sandbox"), "annotation names the reason: {a}");
        assert!(a.contains("NOT a pass"), "annotation is explicit it is not a pass: {a}");
    }

    #[test]
    fn annotation_output_is_bounded() {
        let big = "x".repeat(MAX_ANNOTATION_OUTPUT_BYTES + 5_000);
        let a = verdict_annotation(&RealmVerdict::Passed { output: big });
        assert!(a.contains("[output truncated]"), "an oversize build log is bounded in the artifact");
        // The artifact stays finite (bound + a modest markdown envelope).
        assert!(a.len() < MAX_ANNOTATION_OUTPUT_BYTES + 2_000, "the annotation stays finite: {}", a.len());
    }

    // =====================================================================
    // (5) ORCHESTRATOR — with a MOCK runner (no real copy/exec)
    // =====================================================================

    /// A mock runner returning a canned outcome; records whether it was CALLED so a
    /// test can prove the orchestrator short-circuits before touching the device.
    struct MockRunner {
        outcome: RealmRunOutcome,
        called: std::sync::atomic::AtomicBool,
    }
    impl MockRunner {
        fn new(outcome: RealmRunOutcome) -> Self {
            Self { outcome, called: std::sync::atomic::AtomicBool::new(false) }
        }
    }
    impl RealmRunner for MockRunner {
        fn run<'a>(&'a self, _spec: &'a RealmSpec<'a>) -> RealmRunFuture<'a> {
            self.called.store(true, std::sync::atomic::Ordering::SeqCst);
            let outcome = self.outcome.clone();
            Box::pin(async move { outcome })
        }
    }

    /// A runner that PANICS if called — proves a short-circuit path never reaches it.
    struct PanicRunner;
    impl RealmRunner for PanicRunner {
        fn run<'a>(&'a self, _spec: &'a RealmSpec<'a>) -> RealmRunFuture<'a> {
            Box::pin(async move { panic!("the runner must NOT be reached on this path") })
        }
    }

    #[tokio::test]
    async fn orchestrator_maps_a_real_pass_and_fail_through_the_runner() {
        let state = Path::new("/proj/state");
        let root = Path::new("/proj/repo");

        // A runner that reports a clean run => Passed.
        let pass = MockRunner::new(RealmRunOutcome::Ran {
            exit_code: Some(0),
            output: "all tests passed".into(),
            timed_out: false,
        });
        let v = verify_proposal(&pass, state, root, 1, "--- a/x\n+++ b/x\n@@\n", "cargo test").await;
        assert!(matches!(v, RealmVerdict::Passed { .. }), "a clean run => Passed, got {v:?}");
        assert!(pass.called.load(std::sync::atomic::Ordering::SeqCst), "the runner ran");

        // A runner that reports a non-zero exit => Failed.
        let fail = MockRunner::new(RealmRunOutcome::Ran {
            exit_code: Some(1),
            output: "1 test failed".into(),
            timed_out: false,
        });
        let v = verify_proposal(&fail, state, root, 2, "diff", "make test").await;
        assert!(matches!(v, RealmVerdict::Failed { exit_code: Some(1), .. }), "a failing run => Failed, got {v:?}");
    }

    #[tokio::test]
    async fn orchestrator_is_honestly_unverified_when_the_sandbox_is_unavailable() {
        let state = Path::new("/proj/state");
        let root = Path::new("/proj/repo");
        // The device-gated runner reports the sandbox is unavailable — the honest
        // fallback: Unverified, NEVER a faked pass.
        let down = MockRunner::new(RealmRunOutcome::Unavailable {
            reason: "the shell sandbox (/usr/bin/sandbox-exec) is unavailable on this device".into(),
        });
        let v = verify_proposal(&down, state, root, 3, "diff", "cargo test").await;
        match v {
            RealmVerdict::Unverified { reason } => {
                assert!(reason.contains("sandbox"), "the honest reason names the missing sandbox: {reason}");
            }
            other => panic!("an unavailable sandbox must be Unverified, never a pass: {other:?}"),
        }
    }

    #[tokio::test]
    async fn orchestrator_short_circuits_to_unverified_with_no_verify_command() {
        let state = Path::new("/proj/state");
        let root = Path::new("/proj/repo");
        // With NO verify command there is nothing to run: Unverified WITHOUT ever
        // reaching the runner (a PanicRunner proves the short-circuit).
        let v = verify_proposal(&PanicRunner, state, root, 4, "diff", "   ").await;
        assert!(
            matches!(v, RealmVerdict::Unverified { .. }),
            "no verify command => Unverified (never a faked pass), got {v:?}"
        );
    }

    #[test]
    fn reply_line_never_overclaims() {
        // Passed still tells the user to review; Failed warns; Unverified is explicit.
        assert!(verdict_reply_line(&RealmVerdict::Passed { output: String::new() }).contains("review"));
        assert!(verdict_reply_line(&RealmVerdict::Failed { exit_code: Some(2), output: String::new() }).contains("FAILED"));
        let u = verdict_reply_line(&RealmVerdict::Unverified { reason: "no sandbox".into() });
        assert!(u.contains("UNVERIFIED") && u.contains("no sandbox"), "unverified reply is honest: {u}");
    }
}
