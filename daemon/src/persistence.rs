//! Persistence Sentinel — "Autoruns for the Mac": a READ-ONLY inventory of the
//! host's autostart / persistence surfaces (LaunchAgents, LaunchDaemons, login
//! items, cron, third-party kexts), each backed binary's signing/notarization
//! posture, plus the Gatekeeper switch, with a PURE baseline diff that flags what
//! is NEW / REMOVED / newly-UNSIGNED since the last scan.
//!
//! This is the LOCAL-PERSISTENCE vector — orthogonal to Egress Sentinel (network,
//! `egress.rs`), TCC (`tcc.rs`), and machine posture (`posture.rs`). It follows
//! the SAME discipline as those modules and CHANGES NOTHING:
//!
//!   * every read is a FIXED-ARG bounded subprocess (an absolute program path +
//!     fixed args, NEVER a shell string), 5s timeout, kill_on_drop — the SAME
//!     `run_command` shape as posture.rs / egress.rs / actions.rs;
//!   * the command RUNNER is INJECTED (a function value), so the PURE PARSERS —
//!     one per surface — are unit-tested on hand-written canned output and the
//!     real system commands are NEVER spawned under test;
//!   * `codesign -dv` and `spctl --assess` are ASSESSMENT reads only — they
//!     inspect a binary's signature, they NEVER execute it;
//!   * HONESTY: a surface that needs a privilege the no-sudo daemon lacks (login
//!     items need Automation TCC) degrades to an explicit SKIP — never a
//!     fabricated empty list;
//!   * DARWIN's own two launch items (`com.darwin.daemon` / `com.darwin.inference`)
//!     are LABELED as self and are NEVER alarmed on.
//!
//! STRICTLY READ-ONLY: no file writes to user data, no kills, no unloads, no
//! remediation — not even a gated one. It reports where the user's autostart
//! surface stands; acting on a finding would be consequential and is out of scope.

use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use rusqlite::Connection;
use serde_json::json;
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::warn;

// ---------------------------------------------------------------------------
// Fixed read-only command set. Each is an absolute program path + fixed args —
// never a shell string, exactly like posture.rs / egress.rs.
// ---------------------------------------------------------------------------

/// Convert a plist (text OR binary) to JSON on stdout: `plutil -convert json -o - <path>`.
const PLUTIL: &str = "/usr/bin/plutil";
/// Loaded jobs in the invoking (user) domain: `launchctl list`.
const LAUNCHCTL: &str = "/bin/launchctl";
/// The current user's crontab: `crontab -l`.
const CRONTAB: &str = "/usr/bin/crontab";
/// Login items via System Events (Automation-TCC gated): `osascript -e '…'`.
const OSASCRIPT: &str = "/usr/bin/osascript";
/// Loaded kernel extensions: `kmutil showloaded --list-only`.
const KMUTIL: &str = "/usr/bin/kmutil";
/// Gatekeeper assessment switch: `spctl --status`.
const SPCTL: &str = "/usr/sbin/spctl";
/// Code signature display (READ-ONLY, never executes): `codesign -dv --verbose=2 <path>`.
const CODESIGN: &str = "/usr/bin/codesign";

/// Hard ceiling per spawned read — the same 5s discipline as posture.rs.
const PERSIST_TIMEOUT: Duration = Duration::from_secs(5);

/// The AppleScript that reads the login-item names from System Events. It only
/// *gets* names — it never adds, removes, or enables anything.
const LOGIN_ITEMS_SCRIPT: &str =
    "tell application \"System Events\" to get the name of every login item";

/// DARWIN's own launch items — labeled as self, never alarmed on.
const SELF_LABELS: &[&str] = &["com.darwin.daemon", "com.darwin.inference"];

// ---------------------------------------------------------------------------
// Surfaces + records
// ---------------------------------------------------------------------------

/// The captured outcome of one read: either combined stdout+stderr text, or a
/// note that the read itself could not run (missing binary, timed out). Per-read
/// degradation — one unreadable surface never sinks the whole inventory.
enum ReadOutput {
    Text(String),
    Unavailable(String),
}

/// One autostart / persistence item discovered on the host. `surface` + `key`
/// is its stable identity for the baseline diff.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AutostartItem {
    /// Which surface it came from ("LaunchAgent(user)", "LaunchDaemon", "login item", …).
    surface: &'static str,
    /// Stable identity within the surface (Label / plist stem / bundle id / job line / name).
    key: String,
    /// Human label for the report.
    label: String,
    /// Backing binary path when known (drives signing assessment + self-label).
    program: Option<String>,
    /// RunAtLoad disposition, where the surface carries one.
    run_at_load: bool,
    /// Signing / notarization verdict (NotAssessed when no program or assessment off).
    signed: Signedness,
    /// One of DARWIN's OWN launch items — never alarmed on.
    is_self: bool,
}

/// Signing / notarization verdict for an autostart binary. `suspicious()` is the
/// loud set: nothing validly vouches for the code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Signedness {
    /// No program to assess, or assessment disabled — honestly not evaluated.
    NotAssessed,
    /// Assessed, but the verdict could not be determined.
    Unknown,
    /// `codesign` reports the object is not signed at all.
    Unsigned,
    /// Signed ad-hoc (self-signed with no identity) — no chain of trust.
    Adhoc,
    /// `spctl` rejected the assessment (Gatekeeper would block it).
    Rejected,
    /// Validly code-signed (has an Authority chain).
    Signed,
    /// Signed AND notarized (accepted by `spctl` as a Notarized Developer ID).
    Notarized,
}

impl Signedness {
    /// The loud set — a NEW item that is one of these is flagged `[UNSIGNED]`.
    fn suspicious(self) -> bool {
        matches!(self, Signedness::Unsigned | Signedness::Adhoc | Signedness::Rejected)
    }

    /// A verdict that positively vouches for the code (a regression FROM one of
    /// these TO a suspicious verdict is what we flag).
    fn trusted(self) -> bool {
        matches!(self, Signedness::Signed | Signedness::Notarized)
    }

    /// The wire / baseline token.
    fn wire(self) -> &'static str {
        match self {
            Signedness::NotAssessed => "not_assessed",
            Signedness::Unknown => "unknown",
            Signedness::Unsigned => "unsigned",
            Signedness::Adhoc => "adhoc",
            Signedness::Rejected => "rejected",
            Signedness::Signed => "signed",
            Signedness::Notarized => "notarized",
        }
    }

    /// Parse a stored baseline token back to a verdict (unknown token => NotAssessed).
    fn from_wire(w: &str) -> Signedness {
        match w {
            "unknown" => Signedness::Unknown,
            "unsigned" => Signedness::Unsigned,
            "adhoc" => Signedness::Adhoc,
            "rejected" => Signedness::Rejected,
            "signed" => Signedness::Signed,
            "notarized" => Signedness::Notarized,
            _ => Signedness::NotAssessed,
        }
    }
}

/// The Gatekeeper master switch verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gatekeeper {
    Enabled,
    Disabled,
    Unclear,
}

impl Gatekeeper {
    fn wire(self) -> &'static str {
        match self {
            Gatekeeper::Enabled => "enabled",
            Gatekeeper::Disabled => "disabled",
            Gatekeeper::Unclear => "unclear",
        }
    }
}

/// The full inventory of one scan: every discovered item, the Gatekeeper switch,
/// and the honest per-surface SKIPs (a surface a privilege-less read couldn't cover).
struct Inventory {
    items: Vec<AutostartItem>,
    gatekeeper: Gatekeeper,
    /// (surface, honest reason) — e.g. login items without Automation TCC.
    skips: Vec<(&'static str, String)>,
}

// ---------------------------------------------------------------------------
// Pure parsers — one per surface, unit-tested on canned output. No I/O.
// ---------------------------------------------------------------------------

/// Facts extracted from a launchd plist (already `plutil`-converted to JSON).
#[derive(Debug, Clone, PartialEq, Eq)]
struct PlistFacts {
    label: Option<String>,
    program: Option<String>,
    run_at_load: bool,
}

/// PURE: parse `plutil -convert json -o -` output for a launchd plist. Pulls the
/// Label, the backing program (`Program`, else the first `ProgramArguments`
/// entry), and RunAtLoad (accepting JSON `true` or a `1`). Returns `None` only
/// when the text is not a JSON object at all (a corrupt / non-plist file).
fn parse_plist_json(text: &str) -> Option<PlistFacts> {
    let v: serde_json::Value = serde_json::from_str(text.trim()).ok()?;
    let obj = v.as_object()?;
    let label = obj.get("Label").and_then(|x| x.as_str()).map(str::to_string);
    let program = obj
        .get("Program")
        .and_then(|x| x.as_str())
        .map(str::to_string)
        .or_else(|| {
            obj.get("ProgramArguments")
                .and_then(|x| x.as_array())
                .and_then(|a| a.first())
                .and_then(|x| x.as_str())
                .map(str::to_string)
        });
    let run_at_load = match obj.get("RunAtLoad") {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::Number(n)) => n.as_i64().unwrap_or(0) != 0,
        _ => false,
    };
    Some(PlistFacts { label, program, run_at_load })
}

/// PURE: parse `launchctl list` output — a `PID\tStatus\tLabel` table — into
/// (pid, status, label) rows. The header line and malformed rows are skipped.
fn parse_launchctl_list(text: &str) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 3 {
            continue;
        }
        if f[0] == "PID" && f[1] == "Status" {
            continue; // header
        }
        out.push((f[0].to_string(), f[1].to_string(), f[2].to_string()));
    }
    out
}

/// PURE: whether `crontab -l` reported the user simply has no crontab (so an
/// empty inventory is HONEST, not a fabrication).
fn crontab_is_empty(text: &str) -> bool {
    let low = text.to_lowercase();
    low.contains("no crontab for") || text.trim().is_empty()
}

/// PURE: parse `crontab -l` into its scheduled-job lines. Comments (`#…`), blank
/// lines, and `NAME=value` environment assignments are dropped — only the actual
/// schedule entries remain.
fn parse_crontab(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if is_cron_env_assignment(line) {
            continue;
        }
        out.push(line.to_string());
    }
    out
}

/// A `NAME=value` env line (no whitespace before the `=`, a bare identifier key)
/// as opposed to a real schedule row (which begins with time fields).
fn is_cron_env_assignment(line: &str) -> bool {
    let Some(eq) = line.find('=') else {
        return false;
    };
    let key = &line[..eq];
    !key.is_empty()
        && !key.contains(char::is_whitespace)
        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// PURE: did the System-Events login-items read fail for lack of Automation TCC
/// (or any authorization error)? Drives an HONEST SKIP instead of a fake empty list.
fn login_items_denied(text: &str) -> bool {
    let low = text.to_lowercase();
    low.contains("-1743")
        || low.contains("not authorized")
        || low.contains("not allowed")
        || low.contains("execution error")
        || low.contains("erraeeventnotpermitted")
}

/// PURE: parse the comma-separated login-item names AppleScript returns. Empty
/// input (a genuinely empty list, ALREADY known to be authorized by the caller)
/// yields no names.
fn parse_login_items(text: &str) -> Vec<String> {
    let t = text.trim();
    if t.is_empty() {
        return Vec::new();
    }
    t.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// One loaded kernel extension.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Kext {
    bundle_id: String,
    version: String,
}

/// PURE: parse `kmutil showloaded` output and return the THIRD-PARTY kexts only
/// (Apple's own `com.apple.*` are filtered — the signal is non-Apple code in the
/// kernel). The bundle id is the reverse-DNS token; the version rides in the
/// following `(x.y.z)`.
fn parse_kexts(text: &str) -> Vec<Kext> {
    let mut out = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // The bundle id is a reverse-DNS token (has a dot, starts with a letter,
        // is not a hex address / count column).
        let Some(pos) = fields.iter().position(|f| looks_like_bundle_id(f)) else {
            continue;
        };
        let bundle_id = fields[pos].to_string();
        if bundle_id.starts_with("com.apple.") {
            continue; // Apple's own — not the signal.
        }
        let version = fields
            .get(pos + 1)
            .filter(|f| f.starts_with('(') && f.ends_with(')'))
            .map(|f| f.trim_matches(|c| c == '(' || c == ')').to_string())
            .unwrap_or_default();
        out.push(Kext { bundle_id, version });
    }
    out
}

/// A reverse-DNS bundle id heuristic: contains a dot, starts with a letter, and
/// is not a `0x…` address column.
fn looks_like_bundle_id(f: &str) -> bool {
    f.contains('.')
        && !f.starts_with("0x")
        && f.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && f.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

/// PURE: classify `spctl --status` output.
fn classify_gatekeeper(text: &str) -> Gatekeeper {
    let low = text.to_lowercase();
    if low.contains("assessments enabled") {
        Gatekeeper::Enabled
    } else if low.contains("assessments disabled") {
        Gatekeeper::Disabled
    } else {
        Gatekeeper::Unclear
    }
}

/// PURE: classify `codesign -dv --verbose=2` output (which prints to stderr).
fn parse_codesign(text: &str) -> Signedness {
    let low = text.to_lowercase();
    if low.contains("not signed at all") {
        Signedness::Unsigned
    } else if low.contains("signature=adhoc") {
        Signedness::Adhoc
    } else if text.contains("Authority=") {
        Signedness::Signed
    } else {
        Signedness::Unknown
    }
}

/// The Gatekeeper assessment verdict for one binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Assessment {
    Notarized,
    Accepted,
    Rejected,
    Unknown,
}

/// PURE: classify `spctl --assess --type execute -vv` output. The assessment is
/// READ-ONLY — it never runs the binary.
fn parse_spctl_assess(text: &str) -> Assessment {
    let low = text.to_lowercase();
    if low.contains("rejected") {
        Assessment::Rejected
    } else if low.contains("accepted") {
        if low.contains("notarized") {
            Assessment::Notarized
        } else {
            Assessment::Accepted
        }
    } else {
        Assessment::Unknown
    }
}

/// PURE: fold a `codesign` verdict and an `spctl` assessment into one signedness.
/// Ad-hoc and Gatekeeper-rejected both stay suspicious; a valid signature that
/// `spctl` accepts as notarized is the strongest verdict.
fn combine_signedness(cs: Signedness, assess: Assessment) -> Signedness {
    match (cs, assess) {
        (Signedness::Unsigned, _) => Signedness::Unsigned,
        (Signedness::Adhoc, _) => Signedness::Adhoc,
        (_, Assessment::Rejected) => Signedness::Rejected,
        (Signedness::Signed, Assessment::Notarized) => Signedness::Notarized,
        (Signedness::Signed, _) => Signedness::Signed,
        (Signedness::Unknown, Assessment::Notarized) => Signedness::Notarized,
        (Signedness::Unknown, Assessment::Accepted) => Signedness::Signed,
        (other, _) => other,
    }
}

// ---------------------------------------------------------------------------
// Self-label + item construction (pure)
// ---------------------------------------------------------------------------

/// Whether a Label is one of DARWIN's OWN launch items (never alarmed on).
fn is_self_label(label: &str) -> bool {
    SELF_LABELS.contains(&label)
}

/// The plist filename stem (`com.foo.bar.plist` -> `com.foo.bar`), used as the
/// fallback key/label when a plist carries no explicit Label.
fn plist_stem(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("(unnamed)")
        .to_string()
}

// ---------------------------------------------------------------------------
// Per-surface collectors — each drives the INJECTED runner and folds the result
// through its pure parser. Testable with a canned runner (no real subprocess).
// ---------------------------------------------------------------------------

/// The injected runner's shape: an absolute program + OWNED args (dynamic paths)
/// + a timeout, yielding the captured `ReadOutput`.
async fn run_real(program: &'static str, args: Vec<String>, timeout: Duration) -> ReadOutput {
    let mut cmd = Command::new(program);
    cmd.args(&args).kill_on_drop(true);
    match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(out)) => {
            // Several of these tools (codesign, spctl, osascript, crontab) write
            // their meaningful output — or their authorization error — to stderr;
            // combine both so the parser sees the full text and can classify it.
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
            warn!(program, error = %e, "persistence: command could not run");
            ReadOutput::Unavailable("not available on this machine".to_string())
        }
        Err(_) => {
            warn!(program, secs = timeout.as_secs(), "persistence: command timed out");
            ReadOutput::Unavailable("the check timed out".to_string())
        }
    }
}

/// Assess ONE binary's signing/notarization via `codesign -dv` then (only when
/// codesign didn't already say "unsigned") `spctl --assess`. BOTH are READ-ONLY —
/// they inspect the signature, they never execute the binary.
async fn assess_program<F, Fut>(run: &F, path: &str) -> Signedness
where
    F: Fn(&'static str, Vec<String>, Duration) -> Fut,
    Fut: Future<Output = ReadOutput>,
{
    let cs = match run(
        CODESIGN,
        vec!["-dv".into(), "--verbose=2".into(), path.to_string()],
        PERSIST_TIMEOUT,
    )
    .await
    {
        ReadOutput::Text(t) => parse_codesign(&t),
        ReadOutput::Unavailable(_) => return Signedness::Unknown,
    };
    if cs == Signedness::Unsigned {
        return Signedness::Unsigned;
    }
    let assess = match run(
        SPCTL,
        vec![
            "--assess".into(),
            "--type".into(),
            "execute".into(),
            "-vv".into(),
            path.to_string(),
        ],
        PERSIST_TIMEOUT,
    )
    .await
    {
        ReadOutput::Text(t) => parse_spctl_assess(&t),
        ReadOutput::Unavailable(_) => Assessment::Unknown,
    };
    combine_signedness(cs, assess)
}

/// Collect the launchd jobs from an explicit list of (surface, plist path). The
/// path list is produced by `launch_plist_paths` at runtime (a directory read);
/// this fold is driven by the injected runner so it is CI-tested on canned plutil
/// output. Each job's backing binary is signing-assessed while `budget` remains.
async fn collect_launch_jobs<F, Fut>(
    run: &F,
    paths: &[(&'static str, PathBuf)],
    assess_signing: bool,
    budget: &mut usize,
) -> Vec<AutostartItem>
where
    F: Fn(&'static str, Vec<String>, Duration) -> Fut,
    Fut: Future<Output = ReadOutput>,
{
    let mut out = Vec::new();
    for (surface, path) in paths {
        let facts = match run(
            PLUTIL,
            vec![
                "-convert".into(),
                "json".into(),
                "-o".into(),
                "-".into(),
                path.display().to_string(),
            ],
            PERSIST_TIMEOUT,
        )
        .await
        {
            ReadOutput::Text(t) => parse_plist_json(&t),
            ReadOutput::Unavailable(_) => None,
        };
        let Some(facts) = facts else { continue };
        // Not a launchd job unless it names something to run or carries a label.
        if facts.label.is_none() && facts.program.is_none() {
            continue;
        }
        let label = facts.label.clone().unwrap_or_else(|| plist_stem(path));
        let is_self = is_self_label(&label) || is_self_label(&plist_stem(path));
        let mut signed = Signedness::NotAssessed;
        if assess_signing && !is_self {
            if let Some(prog) = facts.program.as_deref() {
                if *budget > 0 {
                    signed = assess_program(run, prog).await;
                    *budget -= 1;
                }
            }
        }
        out.push(AutostartItem {
            surface,
            key: label.clone(),
            label,
            program: facts.program,
            run_at_load: facts.run_at_load,
            signed,
            is_self,
        });
    }
    out
}

/// Collect loaded jobs from the user domain (`launchctl list`), keeping the
/// THIRD-PARTY ones (Apple's own `com.apple.*` are filtered as noise, mirroring
/// the kext filter) and labeling DARWIN's own as self.
async fn collect_launchctl<F, Fut>(run: &F) -> Vec<AutostartItem>
where
    F: Fn(&'static str, Vec<String>, Duration) -> Fut,
    Fut: Future<Output = ReadOutput>,
{
    let text = match run(LAUNCHCTL, vec!["list".into()], PERSIST_TIMEOUT).await {
        ReadOutput::Text(t) => t,
        ReadOutput::Unavailable(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for (pid, status, label) in parse_launchctl_list(&text) {
        if label.starts_with("com.apple.") {
            continue;
        }
        let is_self = is_self_label(&label);
        out.push(AutostartItem {
            surface: "launchctl",
            key: label.clone(),
            label,
            program: None,
            run_at_load: false,
            signed: Signedness::NotAssessed,
            is_self,
            // status is informational; the running/loaded fact is what matters.
        });
        let _ = (pid, status);
    }
    out
}

/// Collect the current user's cron jobs (`crontab -l`). An empty crontab is an
/// HONEST empty result (never fabricated).
async fn collect_crontab<F, Fut>(run: &F) -> Vec<AutostartItem>
where
    F: Fn(&'static str, Vec<String>, Duration) -> Fut,
    Fut: Future<Output = ReadOutput>,
{
    let text = match run(CRONTAB, vec!["-l".into()], PERSIST_TIMEOUT).await {
        ReadOutput::Text(t) => t,
        ReadOutput::Unavailable(_) => return Vec::new(),
    };
    if crontab_is_empty(&text) {
        return Vec::new();
    }
    parse_crontab(&text)
        .into_iter()
        .map(|line| AutostartItem {
            surface: "crontab",
            key: line.clone(),
            label: line,
            program: None,
            run_at_load: true,
            signed: Signedness::NotAssessed,
            is_self: false,
        })
        .collect()
}

/// Collect login items via System Events. Returns the items on success, or a
/// `(surface, reason)` SKIP when Automation TCC is not granted — NEVER a
/// fabricated empty list masquerading as "no login items".
async fn collect_login_items<F, Fut>(
    run: &F,
) -> (Vec<AutostartItem>, Option<(&'static str, String)>)
where
    F: Fn(&'static str, Vec<String>, Duration) -> Fut,
    Fut: Future<Output = ReadOutput>,
{
    let text = match run(
        OSASCRIPT,
        vec!["-e".into(), LOGIN_ITEMS_SCRIPT.to_string()],
        PERSIST_TIMEOUT,
    )
    .await
    {
        ReadOutput::Text(t) => t,
        ReadOutput::Unavailable(why) => {
            return (Vec::new(), Some(("login item", why)));
        }
    };
    if login_items_denied(&text) {
        return (
            Vec::new(),
            Some((
                "login item",
                "needs Automation consent (System Events) — grant DARWIN Automation \
                 in System Settings › Privacy & Security › Automation to inventory login items"
                    .to_string(),
            )),
        );
    }
    let items = parse_login_items(&text)
        .into_iter()
        .map(|name| AutostartItem {
            surface: "login item",
            key: name.clone(),
            label: name,
            program: None,
            run_at_load: true,
            signed: Signedness::NotAssessed,
            is_self: false,
        })
        .collect();
    (items, None)
}

/// Collect the THIRD-PARTY loaded kexts (`kmutil showloaded`). Non-Apple code in
/// the kernel is inherently notable; the presence of a NEW one is the alert.
async fn collect_kexts<F, Fut>(run: &F) -> Vec<AutostartItem>
where
    F: Fn(&'static str, Vec<String>, Duration) -> Fut,
    Fut: Future<Output = ReadOutput>,
{
    let text = match run(KMUTIL, vec!["showloaded".into(), "--list-only".into()], PERSIST_TIMEOUT)
        .await
    {
        ReadOutput::Text(t) => t,
        ReadOutput::Unavailable(_) => return Vec::new(),
    };
    parse_kexts(&text)
        .into_iter()
        .map(|k| {
            let label = if k.version.is_empty() {
                k.bundle_id.clone()
            } else {
                format!("{} ({})", k.bundle_id, k.version)
            };
            AutostartItem {
                surface: "kext",
                key: k.bundle_id,
                label,
                program: None,
                run_at_load: true,
                signed: Signedness::NotAssessed,
                is_self: false,
            }
        })
        .collect()
}

/// Read the Gatekeeper switch (`spctl --status`).
async fn collect_gatekeeper<F, Fut>(run: &F) -> Gatekeeper
where
    F: Fn(&'static str, Vec<String>, Duration) -> Fut,
    Fut: Future<Output = ReadOutput>,
{
    match run(SPCTL, vec!["--status".into()], PERSIST_TIMEOUT).await {
        ReadOutput::Text(t) => classify_gatekeeper(&t),
        ReadOutput::Unavailable(_) => Gatekeeper::Unclear,
    }
}

/// The three launch directories to scan, most-accessible first. The user domain
/// is skipped when `$HOME` is unset.
fn launch_dirs() -> Vec<(&'static str, PathBuf)> {
    let mut dirs = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        dirs.push(("LaunchAgent(user)", Path::new(&home).join("Library/LaunchAgents")));
    }
    dirs.push(("LaunchAgent(global)", PathBuf::from("/Library/LaunchAgents")));
    dirs.push(("LaunchDaemon", PathBuf::from("/Library/LaunchDaemons")));
    dirs
}

/// Enumerate the `.plist` files under each launch directory (a runtime directory
/// read; the JSON *parse* of each is what the tests exercise). Missing /
/// unreadable directories are silently skipped — that surface simply contributes
/// nothing this scan.
fn launch_plist_paths() -> Vec<(&'static str, PathBuf)> {
    let mut paths = Vec::new();
    for (surface, dir) in launch_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("plist") {
                paths.push((surface, p));
            }
        }
    }
    paths
}

/// Runtime: build the full inventory by driving the REAL runner across every
/// surface. Bounded signing assessment (`max_assess` binaries per scan). This is
/// the only caller that reads real directories / spawns real subprocesses; its
/// sub-collectors + parsers are all tested via canned runners above.
async fn build_inventory(assess_signing: bool, max_assess: usize) -> Inventory {
    let mut items = Vec::new();
    let mut skips = Vec::new();
    let mut budget = max_assess;

    let paths = launch_plist_paths();
    items.extend(collect_launch_jobs(&run_real, &paths, assess_signing, &mut budget).await);
    items.extend(collect_launchctl(&run_real).await);
    items.extend(collect_crontab(&run_real).await);
    let (login, login_skip) = collect_login_items(&run_real).await;
    items.extend(login);
    if let Some(skip) = login_skip {
        skips.push(skip);
    }
    items.extend(collect_kexts(&run_real).await);
    let gatekeeper = collect_gatekeeper(&run_real).await;

    Inventory { items, gatekeeper, skips }
}

// ---------------------------------------------------------------------------
// Pure baseline diff (the detection core) + summary counts
// ---------------------------------------------------------------------------

/// A previously-recorded item: (surface, key, signed-token).
type BaselineRow = (String, String, String);

/// PURE: compare a fresh inventory against the recorded baseline and return the
/// human-readable anomaly lines — a NEW autostart item (flagged `[UNSIGNED]` when
/// its binary isn't validly signed), a REMOVED item, and a signed→unsigned
/// regression on an existing one. DARWIN's own items are excluded on BOTH sides,
/// so `com.darwin.*` never appears as an anomaly.
fn baseline_diff(baseline: &[BaselineRow], live: &[AutostartItem]) -> Vec<String> {
    let live_non_self: Vec<&AutostartItem> = live.iter().filter(|i| !i.is_self).collect();
    let live_keys: HashSet<(&str, &str)> = live_non_self
        .iter()
        .map(|i| (i.surface, i.key.as_str()))
        .collect();
    let mut anomalies = Vec::new();

    for i in &live_non_self {
        let prior = baseline
            .iter()
            .find(|(s, k, _)| s == i.surface && k == &i.key);
        match prior {
            None => {
                let tag = if i.signed.suspicious() { " [UNSIGNED]" } else { "" };
                anomalies.push(format!("NEW autostart: {} → {}{tag}", i.surface, i.label));
            }
            Some((_, _, prior_signed)) => {
                if Signedness::from_wire(prior_signed).trusted() && i.signed.suspicious() {
                    anomalies.push(format!(
                        "UNSIGNED-NOW: {} → {} was {}, now {}",
                        i.surface,
                        i.label,
                        prior_signed,
                        i.signed.wire()
                    ));
                }
            }
        }
    }

    for (s, k, _) in baseline {
        if !live_keys.contains(&(s.as_str(), k.as_str())) {
            anomalies.push(format!("REMOVED autostart: {s} → {k}"));
        }
    }

    anomalies
}

/// PURE: the headline counts folded into telemetry + the posture readout.
/// `(total, self, unsigned)` where `unsigned` counts NON-self items whose binary
/// is not validly signed.
fn summarize(items: &[AutostartItem]) -> (usize, usize, usize) {
    let total = items.len();
    let self_n = items.iter().filter(|i| i.is_self).count();
    let unsigned = items
        .iter()
        .filter(|i| !i.is_self && i.signed.suspicious())
        .count();
    (total, self_n, unsigned)
}

/// PURE: per-surface item counts (secret-free), for the telemetry frame.
fn by_surface(items: &[AutostartItem]) -> serde_json::Value {
    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for i in items {
        *counts.entry(i.surface).or_default() += 1;
    }
    json!(counts)
}

// ---------------------------------------------------------------------------
// Durable baseline store — mirrors tcc.rs::TccBaseline (plaintext or SQLCipher).
// ---------------------------------------------------------------------------

/// The durable persistence baseline (`state/persistence_baseline.db`). Its OWN
/// dedicated SQLite file, plaintext or SQLCipher-encrypted exactly like
/// `audit.db` / `tcc_baseline.db`. An async Mutex serializes access. It records
/// the autostart items DARWIN has already seen (secret-free: surface + key +
/// signedness token) so a later scan can flag what is NEW / REMOVED / newly
/// unsigned. DARWIN's own items are excluded before write.
pub struct PersistenceBaseline {
    conn: Mutex<Connection>,
}

impl PersistenceBaseline {
    /// Open (or create) the baseline DB PLAINTEXT (the default).
    pub fn open(path: &Path) -> Result<Self> {
        Self::init_conn(Connection::open(path)?)
    }

    /// Open (or create) the baseline DB ENCRYPTED (SQLCipher). `key` is applied
    /// via `PRAGMA key` before any other statement — the same seam as AuditLog /
    /// TccBaseline.
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
            "CREATE TABLE IF NOT EXISTS persistence_baseline(
                surface TEXT NOT NULL,
                key TEXT NOT NULL,
                signed TEXT NOT NULL,
                first_seen INTEGER NOT NULL,
                last_seen INTEGER NOT NULL,
                PRIMARY KEY(surface, key)
            );",
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// In-memory baseline for tests (no disk). Same schema.
    #[cfg(test)]
    fn in_memory() -> Result<Self> {
        Self::init_conn(Connection::open_in_memory()?)
    }

    /// True when no item has ever been recorded (drives the silent cold-start
    /// seed, so a first run does not alert on every pre-existing autostart item).
    async fn is_empty(&self) -> Result<bool> {
        let conn = self.conn.lock().await;
        let n: i64 =
            conn.query_row("SELECT COUNT(*) FROM persistence_baseline", [], |r| r.get(0))?;
        Ok(n == 0)
    }

    /// Load the recorded (surface, key, signed) rows.
    async fn load(&self) -> Result<Vec<BaselineRow>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare("SELECT surface, key, signed FROM persistence_baseline")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Replace the baseline with EXACTLY the current non-self live set: upsert
    /// each live item and DELETE any recorded (surface, key) no longer present.
    /// Set-replacement (not accumulation) is what lets a later diff report a
    /// genuine REMOVED delta tick-over-tick without re-alarming forever. Call
    /// AFTER diffing against the prior baseline.
    async fn replace_with(&self, items: &[AutostartItem], now: i64) -> Result<()> {
        let live: Vec<&AutostartItem> = items.iter().filter(|i| !i.is_self).collect();
        let conn = self.conn.lock().await;
        for i in &live {
            conn.execute(
                "INSERT INTO persistence_baseline(surface, key, signed, first_seen, last_seen)
                 VALUES(?1, ?2, ?3, ?4, ?4)
                 ON CONFLICT(surface, key) DO UPDATE SET signed = ?3, last_seen = ?4",
                rusqlite::params![i.surface, i.key, i.signed.wire(), now],
            )?;
        }
        // Delete rows no longer present in the live set.
        let mut stmt = conn.prepare("SELECT surface, key FROM persistence_baseline")?;
        let recorded: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);
        let live_keys: HashSet<(&str, &str)> =
            live.iter().map(|i| (i.surface, i.key.as_str())).collect();
        for (s, k) in &recorded {
            if !live_keys.contains(&(s.as_str(), k.as_str())) {
                conn.execute(
                    "DELETE FROM persistence_baseline WHERE surface = ?1 AND key = ?2",
                    rusqlite::params![s, k],
                )?;
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Posture fold — a cached one-line summary for posture.rs's read-only readout.
// ---------------------------------------------------------------------------

/// The last summary the sentinel computed, for the posture readout.
#[derive(Debug, Clone, Copy)]
struct LastSnapshot {
    total: usize,
    self_n: usize,
    unsigned: usize,
    gatekeeper: Gatekeeper,
}

static LAST_SNAPSHOT: StdMutex<Option<LastSnapshot>> = StdMutex::new(None);

fn set_last_snapshot(snap: LastSnapshot) {
    if let Ok(mut g) = LAST_SNAPSHOT.lock() {
        *g = Some(snap);
    }
}

/// A one-line persistence summary for `posture.rs`'s read-only report, or `None`
/// if the sentinel has not ticked yet (so posture shows nothing stale).
/// SECRET-FREE — counts + the Gatekeeper token only.
pub fn posture_line() -> Option<String> {
    let s = (*LAST_SNAPSHOT.lock().ok()?)?;
    Some(format!(
        "Persistence sentinel: {} autostart item(s) ({} DARWIN self, {} unsigned/untrusted) · Gatekeeper {} — read-only",
        s.total, s.self_n, s.unsigned, s.gatekeeper.wire()
    ))
}

// ---------------------------------------------------------------------------
// Sentinel tick + loop (the live reads are runtime-only; the cores are tested).
// ---------------------------------------------------------------------------

/// Generous startup delay (keep housekeeping out of the first exchanges) + a slow
/// tick (the autostart surface moves on the order of installs, not seconds).
const DEFAULT_STARTUP_DELAY_SECS: u64 = 45;
const DEFAULT_INTERVAL_SECS: u64 = 300;

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// One sentinel tick: inventory the autostart surfaces, cache the posture
/// summary, emit the `security.persistence` frame, and — once past the silent
/// cold-start seed — diff against the baseline and fold any NEW / REMOVED /
/// newly-UNSIGNED anomalies into the SAME frame. READ-ONLY over every OS surface;
/// only the daemon's own baseline store is written. Runtime-only (the live reads
/// make this inspection-verified; its collectors + diff cores are tested).
async fn sentinel_tick(store: &PersistenceBaseline, assess_signing: bool, max_assess: usize) {
    let inv = build_inventory(assess_signing, max_assess).await;
    let (total, self_n, unsigned) = summarize(&inv.items);
    set_last_snapshot(LastSnapshot { total, self_n, unsigned, gatekeeper: inv.gatekeeper });

    let now = now_secs();
    // Cold start: seed silently so a first run does not alert on every existing item.
    let anomalies: Vec<String> = match store.is_empty().await {
        Ok(true) => {
            let _ = store.replace_with(&inv.items, now).await;
            Vec::new()
        }
        Ok(false) => {
            let baseline = store.load().await.unwrap_or_default();
            let anomalies = baseline_diff(&baseline, &inv.items);
            let _ = store.replace_with(&inv.items, now).await;
            anomalies
        }
        Err(_) => Vec::new(),
    };

    let skips: Vec<String> = inv
        .skips
        .iter()
        .map(|(surface, why)| crate::introspect::redact_home(&format!("{surface}: {why}")))
        .collect();
    let anomalies_redacted: Vec<String> =
        anomalies.iter().map(|a| crate::introspect::redact_home(a)).collect();

    crate::telemetry::emit(
        "system",
        "security.persistence",
        json!({
            "available": true,
            "total": total,
            "self": self_n,
            "unsigned": unsigned,
            "gatekeeper": inv.gatekeeper.wire(),
            "by_surface": by_surface(&inv.items),
            "skips": skips,
            "anomalies": anomalies_redacted,
        }),
    );
}

/// The ambient persistence sentinel loop (runtime-only; never run in tests).
/// Mirrors `tcc::sentinel_task`: a startup delay, then a slow periodic
/// `sentinel_tick`. READ-ONLY throughout.
pub async fn sentinel_task(
    store: Arc<PersistenceBaseline>,
    startup_delay_secs: u64,
    interval_secs: u64,
    assess_signing: bool,
    max_assess: usize,
) {
    let startup = if startup_delay_secs == 0 { DEFAULT_STARTUP_DELAY_SECS } else { startup_delay_secs };
    let interval = if interval_secs == 0 { DEFAULT_INTERVAL_SECS } else { interval_secs };
    tokio::time::sleep(Duration::from_secs(startup)).await;
    loop {
        sentinel_tick(&store, assess_signing, max_assess).await;
        tokio::time::sleep(Duration::from_secs(interval)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- helpers -------------------------------------------------------------

    fn item(surface: &'static str, key: &str, signed: Signedness, is_self: bool) -> AutostartItem {
        AutostartItem {
            surface,
            key: key.to_string(),
            label: key.to_string(),
            program: None,
            run_at_load: true,
            signed,
            is_self,
        }
    }

    // -- plist parser --------------------------------------------------------

    #[test]
    fn parse_plist_reads_label_program_and_run_at_load() {
        // Program via ProgramArguments, RunAtLoad true.
        let j = r#"{"Label":"com.example.agent","ProgramArguments":["/usr/local/bin/foo","--x"],"RunAtLoad":true}"#;
        let f = parse_plist_json(j).unwrap();
        assert_eq!(f.label.as_deref(), Some("com.example.agent"));
        assert_eq!(f.program.as_deref(), Some("/usr/local/bin/foo"));
        assert!(f.run_at_load);

        // Program via the explicit Program key, RunAtLoad as an integer 1.
        let j2 = r#"{"Label":"com.example.daemon","Program":"/opt/bar","RunAtLoad":1}"#;
        let f2 = parse_plist_json(j2).unwrap();
        assert_eq!(f2.program.as_deref(), Some("/opt/bar"));
        assert!(f2.run_at_load, "RunAtLoad=1 must read as true");

        // No RunAtLoad => false; no program is fine.
        let j3 = r#"{"Label":"com.example.watch"}"#;
        let f3 = parse_plist_json(j3).unwrap();
        assert!(!f3.run_at_load);
        assert!(f3.program.is_none());

        // A non-object / corrupt file => None (a launch job is never fabricated).
        assert!(parse_plist_json("not json at all").is_none());
        assert!(parse_plist_json("[1,2,3]").is_none());
    }

    // -- launchctl parser ----------------------------------------------------

    #[test]
    fn parse_launchctl_skips_header_and_malformed() {
        let out = "PID\tStatus\tLabel\n1234\t0\tcom.apple.something\n-\t0\tcom.example.agent\nbadrow";
        let rows = parse_launchctl_list(out);
        assert_eq!(rows.len(), 2, "header + the 1-field junk row are skipped");
        assert_eq!(rows[0], ("1234".into(), "0".into(), "com.apple.something".into()));
        assert_eq!(rows[1].2, "com.example.agent");
    }

    // -- crontab parser ------------------------------------------------------

    #[test]
    fn parse_crontab_keeps_only_schedule_lines() {
        let cron = "# a comment\nPATH=/usr/bin\nSHELL=/bin/sh\n\n0 9 * * * /usr/local/bin/backup\n*/5 * * * * ping";
        let jobs = parse_crontab(cron);
        assert_eq!(jobs.len(), 2, "comments/env/blank dropped: {jobs:?}");
        assert!(jobs[0].contains("/usr/local/bin/backup"));
        assert!(jobs[1].starts_with("*/5"));
        // A schedule line with an '=' inside the command is NOT an env assignment.
        assert!(!is_cron_env_assignment("0 9 * * * FOO=1 run"), "has whitespace before =");
        assert!(is_cron_env_assignment("MAILTO=root"));
    }

    #[test]
    fn crontab_empty_is_detected_honestly() {
        assert!(crontab_is_empty("no crontab for user"));
        assert!(crontab_is_empty("   \n"));
        assert!(!crontab_is_empty("0 9 * * * job"));
    }

    // -- login items parser --------------------------------------------------

    #[test]
    fn login_items_parse_and_denial() {
        assert_eq!(parse_login_items("Dropbox, Rectangle, Docker"), vec!["Dropbox", "Rectangle", "Docker"]);
        assert!(parse_login_items("").is_empty(), "an authorized empty list is genuinely empty");
        // The TCC-denied error must be detected as a SKIP, never parsed as data.
        assert!(login_items_denied("execution error: Not authorized to send Apple events to System Events. (-1743)"));
        assert!(!login_items_denied("Dropbox, Rectangle"));
    }

    // -- kext parser ---------------------------------------------------------

    #[test]
    fn parse_kexts_filters_apple_and_extracts_version() {
        let out = "  Index Refs Address Size Wired Name (Version) UUID\n\
                   1 0 0xffffff8012 0x1000 0x1000 com.apple.kpi.bsd (8.0.0) ABC\n\
                   99 0 0xffffff8099 0x2000 0x2000 com.thirdparty.driver (1.2.3) DEF\n\
                   100 0 0xffffff80aa 0x3000 0x3000 org.virtualbox.kext.VBoxDrv (7.0.14) GHI";
        let kexts = parse_kexts(out);
        assert_eq!(kexts.len(), 2, "com.apple.* filtered out: {kexts:?}");
        assert_eq!(kexts[0].bundle_id, "com.thirdparty.driver");
        assert_eq!(kexts[0].version, "1.2.3");
        assert_eq!(kexts[1].bundle_id, "org.virtualbox.kext.VBoxDrv");
    }

    // -- gatekeeper classifier ----------------------------------------------

    #[test]
    fn classify_gatekeeper_reads_enabled_disabled_unclear() {
        assert_eq!(classify_gatekeeper("assessments enabled"), Gatekeeper::Enabled);
        assert_eq!(classify_gatekeeper("assessments disabled"), Gatekeeper::Disabled);
        assert_eq!(classify_gatekeeper("mystery"), Gatekeeper::Unclear);
    }

    // -- signing parsers -----------------------------------------------------

    #[test]
    fn parse_codesign_reads_the_four_states() {
        assert_eq!(parse_codesign("test: code object is not signed at all"), Signedness::Unsigned);
        assert_eq!(parse_codesign("Executable=/x\nSignature=adhoc\nfoo"), Signedness::Adhoc);
        assert_eq!(
            parse_codesign("Authority=Developer ID Application: Acme (TEAM123)\nTeamIdentifier=TEAM123"),
            Signedness::Signed
        );
        assert_eq!(parse_codesign("something unexpected"), Signedness::Unknown);
    }

    #[test]
    fn parse_spctl_assess_reads_verdicts() {
        assert_eq!(parse_spctl_assess("/x: accepted\nsource=Notarized Developer ID"), Assessment::Notarized);
        assert_eq!(parse_spctl_assess("/x: accepted\nsource=Developer ID"), Assessment::Accepted);
        assert_eq!(parse_spctl_assess("/x: rejected"), Assessment::Rejected);
        assert_eq!(parse_spctl_assess("no idea"), Assessment::Unknown);
    }

    #[test]
    fn combine_signedness_prefers_suspicion_and_notarization() {
        assert_eq!(combine_signedness(Signedness::Unsigned, Assessment::Accepted), Signedness::Unsigned);
        assert_eq!(combine_signedness(Signedness::Adhoc, Assessment::Notarized), Signedness::Adhoc);
        assert_eq!(combine_signedness(Signedness::Signed, Assessment::Rejected), Signedness::Rejected);
        assert_eq!(combine_signedness(Signedness::Signed, Assessment::Notarized), Signedness::Notarized);
        assert_eq!(combine_signedness(Signedness::Signed, Assessment::Unknown), Signedness::Signed);
    }

    // -- self-label logic ----------------------------------------------------

    #[test]
    fn self_label_matches_only_darwin_own_items() {
        assert!(is_self_label("com.darwin.daemon"));
        assert!(is_self_label("com.darwin.inference"));
        assert!(!is_self_label("com.darwin.evil"));
        assert!(!is_self_label("com.example.agent"));
    }

    #[test]
    fn plist_stem_falls_back_for_self_detection() {
        assert_eq!(plist_stem(Path::new("/Library/LaunchAgents/com.darwin.daemon.plist")), "com.darwin.daemon");
    }

    // -- signedness helpers --------------------------------------------------

    #[test]
    fn signedness_suspicious_trusted_and_wire_roundtrip() {
        for s in [Signedness::Unsigned, Signedness::Adhoc, Signedness::Rejected] {
            assert!(s.suspicious(), "{s:?} is suspicious");
            assert!(!s.trusted());
        }
        for s in [Signedness::Signed, Signedness::Notarized] {
            assert!(s.trusted());
            assert!(!s.suspicious());
        }
        for s in [
            Signedness::NotAssessed,
            Signedness::Unknown,
            Signedness::Unsigned,
            Signedness::Adhoc,
            Signedness::Rejected,
            Signedness::Signed,
            Signedness::Notarized,
        ] {
            assert_eq!(Signedness::from_wire(s.wire()), s, "wire round-trips for {s:?}");
        }
    }

    // -- baseline diff: new / removed / unsigned + self exclusion ------------

    #[test]
    fn baseline_diff_flags_new_removed_and_unsigned_only() {
        let baseline = vec![
            ("LaunchAgent(user)".into(), "com.keep.me".into(), "notarized".into()),
            ("LaunchDaemon".into(), "com.went.away".into(), "signed".into()),
            ("LaunchAgent(user)".into(), "com.was.signed".into(), "signed".into()),
        ];
        let live = vec![
            // Unchanged — no anomaly.
            item("LaunchAgent(user)", "com.keep.me", Signedness::Notarized, false),
            // New AND unsigned — NEW + [UNSIGNED].
            item("LaunchAgent(user)", "com.new.tool", Signedness::Unsigned, false),
            // New but validly signed — NEW without the tag.
            item("LaunchDaemon", "com.new.signed", Signedness::Notarized, false),
            // Was signed, now adhoc — UNSIGNED-NOW regression.
            item("LaunchAgent(user)", "com.was.signed", Signedness::Adhoc, false),
            // DARWIN's own — must NEVER appear as an anomaly.
            item("LaunchAgent(user)", "com.darwin.daemon", Signedness::NotAssessed, true),
        ];
        let a = baseline_diff(&baseline, &live);
        assert!(a.iter().any(|x| x.contains("NEW autostart") && x.contains("com.new.tool") && x.contains("[UNSIGNED]")), "{a:?}");
        assert!(a.iter().any(|x| x.contains("NEW autostart") && x.contains("com.new.signed") && !x.contains("[UNSIGNED]")), "{a:?}");
        assert!(a.iter().any(|x| x.contains("REMOVED autostart") && x.contains("com.went.away")), "{a:?}");
        assert!(a.iter().any(|x| x.contains("UNSIGNED-NOW") && x.contains("com.was.signed")), "{a:?}");
        // No anomaly ever names DARWIN's own item.
        assert!(!a.iter().any(|x| x.contains("com.darwin.daemon")), "self must never be alarmed: {a:?}");
        // Exactly 4 anomalies (2 new + 1 removed + 1 regression).
        assert_eq!(a.len(), 4, "{a:?}");
    }

    #[test]
    fn baseline_diff_silent_when_unchanged() {
        let baseline = vec![("kext".into(), "com.thirdparty.driver".into(), "not_assessed".into())];
        let live = vec![item("kext", "com.thirdparty.driver", Signedness::NotAssessed, false)];
        assert!(baseline_diff(&baseline, &live).is_empty());
    }

    #[test]
    fn summarize_counts_self_and_unsigned() {
        let items = vec![
            item("LaunchAgent(user)", "com.darwin.daemon", Signedness::NotAssessed, true),
            item("LaunchAgent(user)", "com.a.signed", Signedness::Notarized, false),
            item("LaunchDaemon", "com.b.unsigned", Signedness::Unsigned, false),
            item("kext", "com.c.adhoc", Signedness::Adhoc, false),
        ];
        assert_eq!(summarize(&items), (4, 1, 2), "4 total, 1 self, 2 unsigned/untrusted");
    }

    // -- durable store round-trip incl. set-replacement (removed) ------------

    #[tokio::test]
    async fn store_replaces_set_and_drops_removed() {
        let store = PersistenceBaseline::in_memory().unwrap();
        assert!(store.is_empty().await.unwrap());
        let first = vec![
            item("LaunchAgent(user)", "com.a", Signedness::Signed, false),
            item("LaunchDaemon", "com.b", Signedness::Notarized, false),
            // self excluded from the store entirely.
            item("LaunchAgent(user)", "com.darwin.daemon", Signedness::NotAssessed, true),
        ];
        store.replace_with(&first, 1000).await.unwrap();
        assert!(!store.is_empty().await.unwrap());
        let rows = store.load().await.unwrap();
        assert_eq!(rows.len(), 2, "self is excluded: {rows:?}");
        assert!(rows.iter().all(|(_, k, _)| k != "com.darwin.daemon"));

        // Second scan drops com.b, keeps com.a (now adhoc), adds com.c.
        let second = vec![
            item("LaunchAgent(user)", "com.a", Signedness::Adhoc, false),
            item("kext", "com.c", Signedness::NotAssessed, false),
        ];
        store.replace_with(&second, 2000).await.unwrap();
        let rows = store.load().await.unwrap();
        assert_eq!(rows.len(), 2, "set-replacement: com.b removed, com.c added: {rows:?}");
        assert!(rows.iter().any(|(s, k, sig)| s == "LaunchAgent(user)" && k == "com.a" && sig == "adhoc"));
        assert!(rows.iter().any(|(_, k, _)| k == "com.c"));
        assert!(!rows.iter().any(|(_, k, _)| k == "com.b"), "removed item must be gone from the baseline");
    }

    // -- injected-runner drives each surface on canned output ----------------

    /// A canned runner keyed by program (and, for plutil/codesign/spctl, by the
    /// LAST arg — the path). The real system commands are NEVER spawned.
    fn canned(
        map: std::collections::HashMap<String, ReadStub>,
    ) -> impl Fn(&'static str, Vec<String>, Duration) -> std::future::Ready<ReadOutput> {
        move |program: &'static str, args: Vec<String>, _to| {
            let path_key = format!("{program}::{}", args.last().cloned().unwrap_or_default());
            let stub = map.get(&path_key).or_else(|| map.get(program));
            let out = match stub {
                Some(ReadStub::Text(t)) => ReadOutput::Text(t.clone()),
                Some(ReadStub::Unavail(w)) => ReadOutput::Unavailable(w.clone()),
                None => ReadOutput::Unavailable("no stub".to_string()),
            };
            std::future::ready(out)
        }
    }

    #[derive(Clone)]
    enum ReadStub {
        Text(String),
        Unavail(String),
    }

    fn stub_text(m: &mut std::collections::HashMap<String, ReadStub>, key: &str, text: &str) {
        m.insert(key.to_string(), ReadStub::Text(text.to_string()));
    }

    fn stub_unavail(m: &mut std::collections::HashMap<String, ReadStub>, key: &str, why: &str) {
        m.insert(key.to_string(), ReadStub::Unavail(why.to_string()));
    }

    #[tokio::test]
    async fn collectors_degrade_honestly_when_a_read_is_unavailable() {
        // A missing/timed-out binary must never fabricate data: gatekeeper reads
        // Unclear, and the list surfaces yield an honest empty (not a fake list).
        let mut m = std::collections::HashMap::new();
        stub_unavail(&mut m, SPCTL, "not available on this machine");
        stub_unavail(&mut m, CRONTAB, "the check timed out");
        stub_unavail(&mut m, KMUTIL, "not available on this machine");
        let run = canned(m);
        assert_eq!(collect_gatekeeper(&run).await, Gatekeeper::Unclear);
        assert!(collect_crontab(&run).await.is_empty());
        assert!(collect_kexts(&run).await.is_empty());
        // A login-items read that can't run SKIPs (records the surface), never
        // silently returns "no login items".
        let (items, skip) = collect_login_items(&run).await;
        assert!(items.is_empty());
        assert_eq!(skip.expect("unavailable osascript must SKIP").0, "login item");
    }

    #[tokio::test]
    async fn collect_launchctl_filters_apple_and_labels_self() {
        let mut m = std::collections::HashMap::new();
        stub_text(
            &mut m,
            LAUNCHCTL,
            "PID\tStatus\tLabel\n1\t0\tcom.apple.foo\n-\t0\tcom.example.agent\n42\t0\tcom.darwin.daemon",
        );
        let run = canned(m);
        let items = collect_launchctl(&run).await;
        assert_eq!(items.len(), 2, "com.apple.* filtered: {items:?}");
        assert!(items.iter().any(|i| i.key == "com.example.agent" && !i.is_self));
        assert!(items.iter().any(|i| i.key == "com.darwin.daemon" && i.is_self), "self labeled");
    }

    #[tokio::test]
    async fn collect_login_items_skips_honestly_when_denied() {
        let mut m = std::collections::HashMap::new();
        stub_text(&mut m, OSASCRIPT, "execution error: Not authorized ... (-1743)");
        let run = canned(m);
        let (items, skip) = collect_login_items(&run).await;
        assert!(items.is_empty(), "must NOT fabricate an empty list");
        let skip = skip.expect("a TCC-denied read must SKIP, not silently succeed");
        assert_eq!(skip.0, "login item");
        assert!(skip.1.to_lowercase().contains("automation"), "actionable reason: {}", skip.1);
    }

    #[tokio::test]
    async fn collect_login_items_parses_when_authorized() {
        let mut m = std::collections::HashMap::new();
        stub_text(&mut m, OSASCRIPT, "Dropbox, Rectangle");
        let run = canned(m);
        let (items, skip) = collect_login_items(&run).await;
        assert!(skip.is_none());
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].surface, "login item");
    }

    #[tokio::test]
    async fn assess_program_folds_codesign_and_spctl() {
        // Unsigned short-circuits (no spctl needed).
        let mut m = std::collections::HashMap::new();
        stub_text(&mut m, &format!("{CODESIGN}::/bad"), "code object is not signed at all");
        let run = canned(m);
        assert_eq!(assess_program(&run, "/bad").await, Signedness::Unsigned);

        // Signed + notarized => Notarized.
        let mut m2 = std::collections::HashMap::new();
        stub_text(&mut m2, &format!("{CODESIGN}::/good"), "Authority=Developer ID Application: Acme (T1)");
        stub_text(&mut m2, &format!("{SPCTL}::/good"), "/good: accepted\nsource=Notarized Developer ID");
        let run2 = canned(m2);
        assert_eq!(assess_program(&run2, "/good").await, Signedness::Notarized);
    }

    #[tokio::test]
    async fn collect_launch_jobs_parses_assesses_and_self_labels() {
        let user_plist = PathBuf::from("/Users/x/Library/LaunchAgents/com.example.agent.plist");
        let self_plist = PathBuf::from("/Library/LaunchAgents/com.darwin.daemon.plist");
        let mut m = std::collections::HashMap::new();
        // plutil output per plist path.
        stub_text(
            &mut m,
            &format!("{PLUTIL}::{}", user_plist.display()),
            r#"{"Label":"com.example.agent","ProgramArguments":["/opt/agent"],"RunAtLoad":true}"#,
        );
        stub_text(
            &mut m,
            &format!("{PLUTIL}::{}", self_plist.display()),
            r#"{"Label":"com.darwin.daemon","Program":"/opt/darwind","RunAtLoad":true}"#,
        );
        // signing assessment for the non-self agent's binary.
        stub_text(&mut m, &format!("{CODESIGN}::/opt/agent"), "code object is not signed at all");
        let run = canned(m);
        let paths = vec![
            ("LaunchAgent(user)", user_plist),
            ("LaunchAgent(global)", self_plist),
        ];
        let mut budget = 10;
        let items = collect_launch_jobs(&run, &paths, true, &mut budget).await;
        assert_eq!(items.len(), 2);
        let agent = items.iter().find(|i| i.key == "com.example.agent").unwrap();
        assert!(!agent.is_self);
        assert_eq!(agent.signed, Signedness::Unsigned, "the non-self binary got assessed");
        let me = items.iter().find(|i| i.key == "com.darwin.daemon").unwrap();
        assert!(me.is_self, "DARWIN's own agent labeled self");
        assert_eq!(me.signed, Signedness::NotAssessed, "self is never assessed/alarmed");
        assert_eq!(budget, 9, "exactly one assessment consumed the budget");
    }

    #[test]
    fn by_surface_counts_are_secret_free_and_grouped() {
        let items = vec![
            item("LaunchAgent(user)", "a", Signedness::Signed, false),
            item("LaunchAgent(user)", "b", Signedness::Signed, false),
            item("kext", "c", Signedness::NotAssessed, false),
        ];
        let v = by_surface(&items);
        assert_eq!(v["LaunchAgent(user)"], 2);
        assert_eq!(v["kext"], 1);
    }
}
