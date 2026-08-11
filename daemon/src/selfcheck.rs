//! WS2 daemon self-check / health board.
//!
//! Two entry points share ONE honest check engine:
//!
//!   * `darwind --selftest` — validates the installed environment WITHOUT
//!     starting the full daemon (no audio, no MCP connect, no model). It prints
//!     a truthful PASS / SKIP / FAIL board and exits NON-ZERO on any hard FAIL.
//!     Mirrors `inference/server.py --selftest`'s honesty: a check that could
//!     not actually run is SKIP, never PASS.
//!
//!   * the normal-start STARTUP SELF-CHECK — the same path/dir/perms validation
//!     runs once at boot and, on a hard FAIL of a structural precondition,
//!     aborts with an actionable message instead of limping into the mic loop
//!     with (say) an unwritable state dir. The inference-reachability leg is
//!     NON-FATAL there (lazy-connect resilience is preserved) — it only feeds
//!     the aggregated `daemon.ready` frame.
//!
//! HONESTY CONTRACT (load-bearing): the board NEVER claims healthy/ready for a
//! check that was skipped or whose probe did not actually run. A SKIP carries
//! the reason; a FAIL carries the actionable remediation; only a real,
//! completed, positive check is PASS. The overall verdict is FAIL iff any
//! check FAILs (a SKIP is honest-degraded, not a failure of the harness).

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::inference::InferenceClient;

/// Outcome of one check. SKIP is a first-class, honest state — it is NOT a pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// The check ran and the condition holds.
    Pass,
    /// The check could NOT run (a precondition was absent). Honest unknown —
    /// never rendered as healthy.
    Skip,
    /// The check ran and the condition does NOT hold. Hard failure.
    Fail,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Pass => "PASS",
            Status::Skip => "SKIP",
            Status::Fail => "FAIL",
        }
    }
}

/// One line on the board: a short name, its status, and a human detail
/// (the SKIP reason or the FAIL remediation, or the PASS evidence).
#[derive(Debug, Clone)]
pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
}

impl Check {
    pub fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Self { name, status: Status::Pass, detail: detail.into() }
    }
    pub fn skip(name: &'static str, detail: impl Into<String>) -> Self {
        Self { name, status: Status::Skip, detail: detail.into() }
    }
    pub fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Self { name, status: Status::Fail, detail: detail.into() }
    }
}

/// PURE: did any check hard-FAIL? (A SKIP is honest-degraded, not a failure.)
/// This is the exit-code decision for `--selftest` and the abort decision for
/// the startup self-check. Unit-tested.
pub fn any_failed(checks: &[Check]) -> bool {
    checks.iter().any(|c| c.status == Status::Fail)
}

/// PURE: count of each status, for the one-line summary footer.
pub fn tally(checks: &[Check]) -> (usize, usize, usize) {
    let mut pass = 0;
    let mut skip = 0;
    let mut fail = 0;
    for c in checks {
        match c.status {
            Status::Pass => pass += 1,
            Status::Skip => skip += 1,
            Status::Fail => fail += 1,
        }
    }
    (pass, skip, fail)
}

/// "Has darwind ever started in THIS root?" — the witness that lets a fresh
/// install report honestly without weakening a deployed one.
///
/// `main()` calls [`prepare_state_dirs`] FIRST — before `init_tracing`, before any
/// store is opened — so ANY startup artifact that lives OUTSIDE `state/ipc|logs|tmp`
/// proves those three existed at least once. Two such artifacts sit directly under
/// `state/` and are opened UNCONDITIONALLY on the run path, each with `?`
/// (`main.rs`: `open_memory(state/darwin.db)`, then `open_audit(state/audit.db)`),
/// and NEITHER is an install artifact: `./install.sh` creates `state/` only in order
/// to write `state/env.sh`, and it never runs `scripts/init_memory.py` — the other
/// thing that would create `darwin.db`, and which creates the three subdirs too, so
/// the witness and the dirs cannot disagree in that direction either.
///
/// Absent on a freshly installed, never-started tree; present on any tree where
/// darwind got past its own startup gate once.
fn ever_started(root: &Path) -> bool {
    let state = root.join("state");
    state.join("darwin.db").is_file() || state.join("audit.db").is_file()
}

/// The structural FILESYSTEM checks: root resolution, the daemon binary, the
/// venv python, config readability, the three state subdirs, and the 0700 perms
/// on `state/ipc`. PURE w.r.t. the clock/network — it only stats the tree — so
/// it is the same logic the startup self-check uses to decide whether to abort.
/// `is_dev` is true when the resolver fell back to a tree that is NOT an install
/// home (no binary / no venv): those legs become honest SKIPs instead of FAILs,
/// so running `--selftest` in the dev checkout reports SKIPPED truthfully rather
/// than failing or faking a pass.
pub fn filesystem_checks(root: &Path) -> Vec<Check> {
    let mut checks = Vec::new();

    // Root resolution: we always have a path; report which one + how it looks.
    if root.join("config").join("darwin.toml").exists() || root.join("state").is_dir() {
        checks.push(Check::pass("root", format!("resolved to {}", root.display())));
    } else {
        checks.push(Check::fail(
            "root",
            format!(
                "{} has neither config/darwin.toml nor state/ — set DARWIN_ROOT or run from the install home",
                root.display()
            ),
        ));
    }

    // config/darwin.toml readable.
    let config_path = root.join("config").join("darwin.toml");
    match std::fs::read_to_string(&config_path) {
        Ok(s) if !s.trim().is_empty() => {
            checks.push(Check::pass("config", format!("{} readable", config_path.display())))
        }
        Ok(_) => checks.push(Check::fail("config", format!("{} is empty", config_path.display()))),
        Err(e) => checks.push(Check::fail(
            "config",
            format!("{} unreadable: {e}", config_path.display()),
        )),
    }

    // venv python (install artifact). SKIP in a tree without one (dev checkout
    // may or may not have it) — never claim a venv we cannot see.
    let venv_py = root.join(".venv").join("bin").join("python");
    if is_executable(&venv_py) {
        checks.push(Check::pass("venv", format!("{} present", venv_py.display())));
    } else {
        checks.push(Check::skip(
            "venv",
            format!(
                "{} missing — create it (python3.11 -m venv .venv) before live inference",
                venv_py.display()
            ),
        ));
    }

    // daemon binary.
    let binary = root.join("daemon").join("target").join("release").join("darwind");
    if is_executable(&binary) {
        checks.push(Check::pass("binary", format!("{} present", binary.display())));
    } else {
        checks.push(Check::skip(
            "binary",
            format!("{} not built — cargo build --release", binary.display()),
        ));
    }

    // pdfjail memory-jail helper — the sibling subprocess that extracts PDF text
    // under an RLIMIT_AS cap so a decompression bomb aborts the CHILD, never
    // darwind. PASS when present; SKIP (not FAIL) when absent so a dev checkout
    // reports honestly — but the daemon logs a runtime WARN and silently falls
    // back to the weaker in-process guard, so a production install should have it.
    let jail = root.join("daemon").join("target").join("release").join("pdfjail");
    if is_executable(&jail) {
        checks.push(Check::pass("pdfjail", format!("{} present (PDF memory-jail armed)", jail.display())));
    } else {
        checks.push(Check::skip(
            "pdfjail",
            format!("{} not built — cargo build --release (PDF extraction falls back to the in-process guard)", jail.display()),
        ));
    }

    // The THREE runtime state subdirs the daemon actually creates at startup
    // (main.rs makes exactly state/ipc, state/logs, state/tmp); here we report.
    // This said "four", which implied a fourth directory was validated at startup
    // — scripts/init_memory.py does list a fourth (state/ipc/apps), and that is
    // precisely the thing a reader would then assume is gated here. It is not.
    //
    // THE FRESH-INSTALL WRINKLE. These three are created by a START
    // (`prepare_state_dirs`), and `./install.sh` does not create them — so before
    // the first start a FAIL here declared a freshly installed tree BROKEN in a
    // message that then explained the condition was normal ("missing (the daemon
    // creates it at startup)"): a verdict contradicting its own explanation.
    //
    // Turning that into a blanket SKIP would weaken a real gate on a deployed
    // system, so the SKIP is witnessed, and every other case still FAILs loudly:
    //   * a start witness present (`ever_started`) -> FAIL. The daemon HAS run
    //     here, so a missing subdir is a real fault.
    //   * SOME of the three present and others missing -> FAIL. Nothing creates a
    //     partial set: `prepare_state_dirs` makes all three or returns Err, so a
    //     partial set means one was removed.
    //   * ONLY "no witness AND none of the three" is "not started yet" -> SKIP.
    // `scripts/bringup.sh` already reports this condition as SKIP; the two now
    // agree, and `ipc_perms` below was already an honest SKIP here.
    let subs = ["ipc", "logs", "tmp"];
    let present = subs
        .iter()
        .filter(|s| root.join("state").join(s).is_dir())
        .count();
    let never_started = present == 0 && !ever_started(root);
    for sub in subs {
        let name = match sub {
            "ipc" => "state/ipc",
            "logs" => "state/logs",
            _ => "state/tmp",
        };
        let d = root.join("state").join(sub);
        if d.is_dir() {
            checks.push(Check::pass(name, format!("{} present", d.display())));
        } else if never_started {
            checks.push(Check::skip(
                name,
                format!(
                    "{} missing, and darwind has never started here (no state/darwin.db, \
                     no state/audit.db, and none of state/ipc|logs|tmp) — that is \"not \
                     started yet\", not a fault. Start it (scripts/bringup.sh or the \
                     LaunchAgents) and re-run; if it is still missing AFTER a start, this \
                     leg FAILs",
                    d.display()
                ),
            ));
        } else {
            checks.push(Check::fail(
                name,
                format!("{} missing (the daemon creates it at startup)", d.display()),
            ));
        }
    }

    // state/ipc must be 0700 — the confined socket dir. Defense-in-depth on top
    // of the per-socket 0600 + token gate. A looser mode is a FAIL.
    checks.push(ipc_perms_check(&root.join("state").join("ipc")));

    checks
}

/// 0700 check on the confined socket dir. Honest SKIP when the dir is absent
/// (already FAILed above) or when perms cannot be read on this platform.
fn ipc_perms_check(ipc: &Path) -> Check {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(ipc) {
            Ok(m) => {
                let mode = m.permissions().mode() & 0o777;
                if mode == 0o700 {
                    Check::pass("ipc_perms", "state/ipc is 0700 (confined)")
                } else {
                    Check::fail(
                        "ipc_perms",
                        format!("state/ipc is {:o}, expected 0700 — chmod 700 the confined socket dir", mode),
                    )
                }
            }
            Err(e) => Check::skip("ipc_perms", format!("could not stat state/ipc: {e}")),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ipc;
        Check::skip("ipc_perms", "perms check is unix-only")
    }
}

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p)
            .map(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// Inference reachability leg: connect-probe `state/ipc/inference.sock`. NEVER
/// spends a model call (connect + close). Honest: a missing/unreachable socket
/// is SKIP for `--selftest` semantics where the server may simply not be
/// running — it is the operator's call whether that is a failure. The startup
/// path treats it as informational only (non-fatal, feeds `daemon.ready`).
pub async fn inference_check(root: &Path) -> Check {
    let sock = root.join("state").join("ipc").join("inference.sock");
    if !sock.exists() {
        return Check::skip(
            "inference",
            format!("{} absent — start inference/server.py (boot it first)", sock.display()),
        );
    }
    let client = InferenceClient::new(sock.clone());
    match client.probe_reachable().await {
        Ok(()) => Check::pass("inference", format!("{} reachable", sock.display())),
        Err(e) => Check::skip("inference", format!("{} present but not reachable: {e}", sock.display())),
    }
}

/// Telemetry-port leg: can we BIND 127.0.0.1:<port>? If yes the daemon would be
/// able to host the WS hub (and nothing else owns it). If the bind fails with
/// AddrInUse a daemon is likely already up — that is a SKIP (not a fail) for the
/// `--selftest` no-daemon semantics. Any other bind error is a FAIL. The
/// listener is dropped immediately (we only test bindability).
pub async fn telemetry_port_check(port: u16) -> Check {
    let addr = format!("127.0.0.1:{port}");
    match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => {
            drop(l);
            Check::pass("telemetry_port", format!("{addr} bindable"))
        }
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => Check::skip(
            "telemetry_port",
            format!("{addr} already in use — a darwind is likely already running"),
        ),
        Err(e) => Check::fail("telemetry_port", format!("{addr} not bindable: {e}")),
    }
}

/// Cloud-key presence as a HONEST informational check (never a FAIL — the
/// system runs local-only without a key). `present` is the resolved bool the
/// caller already computed; we never touch the key here.
pub fn cloud_key_check(present: bool) -> Check {
    if present {
        Check::pass("cloud_key", "Anthropic API key resolved (cloud tier available)")
    } else {
        Check::skip("cloud_key", "no Anthropic API key — running LOCAL-ONLY (honest, not a failure)")
    }
}

/// Run the FULL `--selftest` board (filesystem + inference + telemetry-port +
/// cloud-key) against `root`. Returns the checks for the caller to render +
/// decide the exit code. `cloud_key_present` is resolved by the caller (it owns
/// the keychain access); we keep the secret out of this module entirely.
pub async fn run_selftest(root: &Path, telemetry_port: u16, cloud_key_present: bool) -> Vec<Check> {
    let mut checks = filesystem_checks(root);
    checks.push(inference_check(root).await);
    checks.push(telemetry_port_check(telemetry_port).await);
    checks.push(cloud_key_check(cloud_key_present));
    checks
}

/// Render the board to stdout in the honest PASS/SKIP/FAIL form, mirroring
/// server.py's plain selftest output (CI-friendly, no ANSI required).
pub fn print_board(title: &str, checks: &[Check]) {
    println!("\n{title}");
    println!("{}", "-".repeat(title.len().max(8)));
    for c in checks {
        println!("  [{}] {:<16} {}", c.status.label(), c.name, c.detail);
    }
    let (pass, skip, fail) = tally(checks);
    println!("\n  {pass} passed, {skip} skipped, {fail} failed");
    if fail > 0 {
        println!("  VERDICT: FAIL (a hard precondition did not hold)");
    } else if skip > 0 {
        println!("  VERDICT: DEGRADED-OK (no failures; some checks skipped — see reasons above)");
    } else {
        println!("  VERDICT: OK");
    }
}

/// The aggregated `daemon.ready` telemetry frame the startup path emits AFTER
/// the spawns, so readiness is observable without blocking startup. Honest:
/// `inference_reachable` is `None` (UNKNOWN) when the probe was skipped (socket
/// absent), `Some(true/false)` only when a probe actually ran.
pub fn ready_frame(
    root: &Path,
    inference_reachable: Option<bool>,
    cloud_key_present: bool,
    telemetry_bound: bool,
    startup_check_ok: bool,
) -> Value {
    json!({
        "root": root.display().to_string(),
        // None => UNKNOWN (probe skipped), never silently "false".
        "inference_reachable": inference_reachable,
        "cloud_key_present": cloud_key_present,
        "telemetry_bound": telemetry_bound,
        "startup_check_ok": startup_check_ok,
    })
}

/// Create the daemon's state subtree with the modes the startup gate REQUIRES.
///
/// WHAT WENT WRONG: `main.rs` created these with a plain `create_dir_all` — 0777 &
/// ~umask, so 0755 on a normal box — and then ran [`startup_blocking_checks`],
/// which FAILS `ipc_perms` unless `state/ipc` is already 0700. `main.rs` bails on a
/// failed startup check.
///
/// The only code that ever chmodded that dir to 0700 was `command::bind_socket`,
/// roughly 700 lines LATER in boot. So on a clean machine darwind exited non-zero
/// on every launch and launchd restarted it into the same failure forever: it could
/// never reach the point where it would perform the chmod that satisfies its own
/// gate. `darwind --selftest` on a fresh tree reported a hard FAIL for a condition
/// the operator did nothing to cause.
///
/// Creation and the gate now agree by construction, in one place, so they cannot
/// drift apart again.
pub fn prepare_state_dirs(root: &Path) -> std::io::Result<()> {
    for sub in ["state/ipc", "state/logs", "state/tmp"] {
        std::fs::create_dir_all(root.join(sub))?;
    }
    // The confined socket dir is owner-only: it holds the command and app sockets.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            root.join("state/ipc"),
            std::fs::Permissions::from_mode(0o700),
        )?;
    }
    Ok(())
}

/// The STRUCTURAL preconditions that MUST hold for the daemon to run safely
/// (vs. the informational legs). At startup, a hard FAIL on any of these aborts
/// with an actionable message rather than limping forward. This deliberately
/// EXCLUDES the inference + cloud-key legs (lazy-connect + local-only are
/// resilient by design) and the venv/binary legs (the running daemon obviously
/// has a binary; the venv is the inference server's concern, gated separately).
pub fn startup_blocking_checks(root: &Path) -> Vec<Check> {
    filesystem_checks(root)
        .into_iter()
        .filter(|c| {
            matches!(
                c.name,
                "root" | "config" | "state/ipc" | "state/logs" | "state/tmp" | "ipc_perms"
            )
        })
        .collect()
}

/// Resolve the DARWIN root EXACTLY like the daemon's `resolve_root` so
/// `--selftest` targets the same tree the running daemon would. Kept here so
/// the selftest path does not depend on main.rs internals.
pub fn resolve_root_like_daemon() -> PathBuf {
    if let Ok(root) = std::env::var("DARWIN_ROOT") {
        return PathBuf::from(root);
    }
    if let Ok(exe) = std::env::current_exe() {
        for ancestor in exe.ancestors().skip(1) {
            if ancestor.join("config").join("darwin.toml").exists() || ancestor.join("state").is_dir()
            {
                return ancestor.to_path_buf();
            }
        }
        if let Some(grandparent) = exe.parent().and_then(Path::parent) {
            return grandparent.to_path_buf();
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("darwin-selfcheck-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// A SHORT root under /tmp for tests that bind a Unix socket: macOS caps a
    /// sockaddr_un path at ~104 bytes (SUN_LEN), and the default temp_dir
    /// (/var/folders/...) plus state/ipc/inference.sock overflows it. /tmp keeps
    /// the bound-socket path comfortably under the limit.
    fn short_socket_root(tag: &str) -> PathBuf {
        let d = PathBuf::from("/tmp").join(format!("jv-sc-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn any_failed_and_tally_are_honest() {
        let checks = vec![
            Check::pass("a", "ok"),
            Check::skip("b", "skipped"),
            Check::pass("c", "ok"),
        ];
        assert!(!any_failed(&checks), "skips alone are NOT a failure");
        assert_eq!(tally(&checks), (2, 1, 0));

        let with_fail = vec![Check::pass("a", "ok"), Check::fail("b", "broken")];
        assert!(any_failed(&with_fail), "a single FAIL fails the board");
        assert_eq!(tally(&with_fail), (1, 0, 1));
    }

    #[test]
    fn cloud_key_absent_is_skip_not_fail() {
        // Honest: local-only is a degraded-OK posture, never a failure.
        assert_eq!(cloud_key_check(false).status, Status::Skip);
        assert_eq!(cloud_key_check(true).status, Status::Pass);
    }

    /// The startup-blocking check FAILs honestly when a structural precondition
    /// is missing (the actionable abort path), and the inference/cloud legs are
    /// excluded from the blocking set (lazy-connect / local-only resilience).
    #[test]
    fn startup_blocking_checks_fail_on_missing_state_dirs() {
        // A bare temp dir with NO config and NO state/ — the missing-dep path.
        let root = tmp_root("missing");
        std::fs::create_dir_all(&root).unwrap();
        let checks = startup_blocking_checks(&root);
        assert!(any_failed(&checks), "missing config + state dirs must hard-FAIL the startup gate");
        // EXACT, so this cannot go on resting on a leg whose meaning changed: on a
        // tree that has NEVER started, the three state subdirs SKIP (see
        // `a_never_started_tree_skips_the_state_subdirs_and_a_started_one_still_fails`)
        // and `ipc_perms` cannot stat, so the FAILs here are root + config. Counting
        // failures instead of naming them would let this pass while proving nothing
        // about the legs in its own name.
        let failed: Vec<&str> = checks
            .iter()
            .filter(|c| c.status == Status::Fail)
            .map(|c| c.name)
            .collect();
        assert_eq!(
            failed,
            vec!["root", "config"],
            "the missing-dep FAILs must be exactly root + config: {checks:?}"
        );
        // The inference + cloud-key legs are NOT in the blocking set.
        assert!(
            !checks.iter().any(|c| c.name == "inference" || c.name == "cloud_key"),
            "inference/cloud-key are non-blocking at startup"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A well-formed tree (config + the three state subdirs at 0700 ipc) passes
    /// the structural gate.
    #[test]
    fn startup_blocking_checks_pass_on_a_well_formed_tree() {
        let root = tmp_root("good");
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::write(root.join("config").join("darwin.toml"), "[telemetry]\nport = 7177\n").unwrap();
        for sub in ["ipc", "logs", "tmp"] {
            std::fs::create_dir_all(root.join("state").join(sub)).unwrap();
        }
        // 0700 on ipc.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root.join("state").join("ipc"), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let checks = startup_blocking_checks(&root);
        assert!(!any_failed(&checks), "a well-formed tree must pass the startup gate; got {checks:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 0700 enforcement: a loose (0755) ipc dir is a FAIL, not a pass — the
    /// confinement invariant the security model relies on.
    #[cfg(unix)]
    #[test]
    fn loose_ipc_perms_are_a_fail() {
        use std::os::unix::fs::PermissionsExt;
        let root = tmp_root("loose");
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::write(root.join("config").join("darwin.toml"), "x=1\n").unwrap();
        for sub in ["ipc", "logs", "tmp"] {
            std::fs::create_dir_all(root.join("state").join(sub)).unwrap();
        }
        std::fs::set_permissions(root.join("state").join("ipc"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let checks = startup_blocking_checks(&root);
        let ipc = checks.iter().find(|c| c.name == "ipc_perms").unwrap();
        assert_eq!(ipc.status, Status::Fail, "0755 ipc must FAIL the confinement check");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The inference leg SKIPs (never FAILs, never fake-PASSes) when the socket
    /// is absent — the dev-tree / server-down honest path.
    #[tokio::test]
    async fn inference_check_skips_when_socket_absent() {
        let root = tmp_root("nosock");
        std::fs::create_dir_all(root.join("state").join("ipc")).unwrap();
        let c = inference_check(&root).await;
        assert_eq!(c.status, Status::Skip, "absent inference.sock must SKIP, never PASS/FAIL");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The inference leg PASSes only against a really-bound socket (mock).
    #[tokio::test]
    async fn inference_check_passes_against_a_bound_socket() {
        let root = short_socket_root("bound");
        let ipc = root.join("state").join("ipc");
        std::fs::create_dir_all(&ipc).unwrap();
        let sock = ipc.join("inference.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let c = inference_check(&root).await;
        assert_eq!(c.status, Status::Pass, "a bound inference.sock must PASS");
        drop(listener);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `daemon.ready` keeps inference_reachable as UNKNOWN (null) when no probe
    /// ran — never silently false.
    #[test]
    fn ready_frame_reports_unknown_reachability_honestly() {
        let root = tmp_root("ready");
        let v = ready_frame(&root, None, false, true, true);
        assert!(v["inference_reachable"].is_null(), "skipped probe => null (unknown), not false");
        assert_eq!(v["cloud_key_present"], false);
        assert_eq!(v["telemetry_bound"], true);
        let v2 = ready_frame(&root, Some(true), true, true, true);
        assert_eq!(v2["inference_reachable"], true);
    }
    /// THE FRESH-INSTALL WRINKLE, probed on BOTH sides of BOTH boundaries.
    ///
    /// `./install.sh` does not create `state/ipc|logs|tmp` and does not run
    /// `scripts/init_memory.py` — the daemon makes them at startup — so before the
    /// first start these three legs reported FAIL with the message "missing (the
    /// daemon creates it at startup)", a verdict that contradicted its own
    /// explanation. They now SKIP, but ONLY on a tree with no start witness AND
    /// none of the three present. Every step below is a boundary this SKIP must NOT
    /// cross, so each is asserted rather than argued.
    #[test]
    fn a_never_started_tree_skips_the_state_subdirs_and_a_started_one_still_fails() {
        use std::fs;
        let root = tmp_root("neverstarted");
        fs::create_dir_all(root.join("config")).unwrap();
        fs::write(root.join("config").join("darwin.toml"), "x=1\n").unwrap();
        // EXACTLY install.sh's shape: `state/` exists because the installer writes
        // `state/env.sh` into it, and nothing else is there.
        fs::create_dir_all(root.join("state")).unwrap();
        fs::write(root.join("state").join("env.sh"), "# env\n").unwrap();

        let legs = |root: &Path| -> Vec<(String, Status)> {
            filesystem_checks(root)
                .into_iter()
                .filter(|c| matches!(c.name, "state/ipc" | "state/logs" | "state/tmp"))
                .map(|c| (c.name.to_string(), c.status))
                .collect()
        };

        // (1) NEVER STARTED -> all three SKIP, and the startup gate does not hard-fail.
        let l = legs(&root);
        assert_eq!(l.len(), 3, "all three legs must still be reported: {l:?}");
        assert!(
            l.iter().all(|(_, s)| *s == Status::Skip),
            "a never-started install must SKIP — not FAIL — the dirs only a START \
             creates: {l:?}"
        );

        // (2) THE WITNESS BOUNDARY, other side: darwin.db proves darwind ran here,
        // so the same three missing dirs are a REAL fault. The SKIP must not
        // generalize to a deployed system.
        fs::write(root.join("state").join("darwin.db"), b"").unwrap();
        let l = legs(&root);
        assert!(
            l.iter().all(|(_, s)| *s == Status::Fail),
            "state/darwin.db proves a start happened here, so a missing subdir is a \
             real FAIL, never \"not started yet\": {l:?}"
        );
        assert!(any_failed(&startup_blocking_checks(&root)), "and it must still block startup");
        fs::remove_file(root.join("state").join("darwin.db")).unwrap();
        // audit.db is the second witness (open_audit, same run path) and must count.
        fs::write(root.join("state").join("audit.db"), b"").unwrap();
        assert!(
            legs(&root).iter().all(|(_, s)| *s == Status::Fail),
            "state/audit.db is opened at startup too, so it is a witness as well"
        );
        fs::remove_file(root.join("state").join("audit.db")).unwrap();

        // (3) THE PARTIAL-SET BOUNDARY, other side: no witness at all, but ONE of the
        // three exists. `prepare_state_dirs` makes all three or errors, so a partial
        // set means one was REMOVED — the two missing ones stay FAIL.
        fs::create_dir_all(root.join("state").join("ipc")).unwrap();
        let l = legs(&root);
        for (name, status) in &l {
            let want = if name == "state/ipc" { Status::Pass } else { Status::Fail };
            assert_eq!(*status, want, "partial set: {name} must be {want:?}; got {l:?}");
        }

        // (4) DEGENERATE: all three present -> PASS, with or without a witness.
        for sub in ["logs", "tmp"] {
            fs::create_dir_all(root.join("state").join(sub)).unwrap();
        }
        assert!(
            legs(&root).iter().all(|(_, s)| *s == Status::Pass),
            "all three present must PASS"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A FRESH TREE MUST BOOT.
    ///
    /// `main.rs` creates `state/ipc` with a plain `create_dir_all` (0777 & ~umask,
    /// so 0755 on a normal box) and then runs `startup_blocking_checks`, which
    /// FAILS `ipc_perms` unless the dir is already 0700 — and `main.rs` bails on a
    /// failed startup check. The only code that ever chmods that dir to 0700 is
    /// `command::bind_socket`, roughly 700 lines LATER in boot.
    ///
    /// So on a clean machine darwind exited non-zero on every launch and launchd
    /// restarted it into the same failure forever: it could never reach the point
    /// where it would perform the chmod that satisfies its own gate.
    ///
    /// `startup_blocking_checks_pass_on_a_well_formed_tree` hides this — it
    /// explicitly chmods 0700 before checking, which is exactly the step a fresh
    /// install does not perform. This test deliberately does NOT chmod.
    #[test]
    fn a_freshly_created_tree_passes_the_startup_gate_without_a_manual_chmod() {
        let root = std::path::PathBuf::from(format!(
            "/private/tmp/jrv-fresh-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                % 1_000_000
        ));
        // EXACTLY what main.rs does on first boot — via the same helper, so this
        // test exercises the real boot path rather than a copy of it.
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::write(root.join("config/darwin.toml"), "[audit]
max_entries = 1000
").unwrap();
        prepare_state_dirs(&root).unwrap();
        // PRECONDITION: the dir really is looser than 0700, or this proves nothing.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // A plain create_dir_all leaves this looser than 0700 — that is the
            // whole defect. Prove the helper is what tightens it, by checking a
            // sibling it does NOT chmod.
            let logs = std::fs::metadata(root.join("state/logs"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_ne!(logs, 0o700, "create_dir_all really does leave 0755 here");
        }
        let checks = startup_blocking_checks(&root);
        let failed: Vec<_> = checks.iter().filter(|c| c.status == Status::Fail).collect();
        let _ = std::fs::remove_dir_all(&root);
        assert!(
            failed.is_empty(),
            "a freshly created tree must boot; a fresh install cannot chmod a dir \
             the daemon has not started to create yet: {failed:?}"
        );
    }

}
