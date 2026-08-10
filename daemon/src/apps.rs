//! Micro-app runtime substrate — the host side of docs/SANDBOX.md.
//!
//! Each micro-app is a SEPARATE process launched by darwind, never run in the
//! daemon's address space. At launch the host:
//!   1. parses `apps/<name>/manifest.toml` into a typed [`AppManifest`],
//!   2. generates a macOS `sandbox-exec` (seatbelt / SBPL) profile to
//!      `state/apps/<name>/<name>.sb` — DEFAULT-DENY, granting only what the
//!      manifest declares (see [`generate_sbpl`]),
//!   3. mints a per-launch HMAC-SHA256 capability token bound to the app's
//!      name + permission set + a session nonce ([`AppRegistry::mint_token`]),
//!   4. spawns `/usr/bin/sandbox-exec -p <profile-string> <interp> <entry...>`
//!      (the profile passed INLINE, immune to on-disk tampering) with
//!      the token + socket path handed to the app via the launch env, and
//!   5. accepts the app's connection on a per-app Unix socket
//!      (`state/ipc/apps/<name>.sock`, JSONL), VERIFIES the token on every
//!      inbound line, and relays accepted data onto the 7177 telemetry WS so
//!      the HUD panel renders without its own socket.
//!
//! sandbox-exec is DEPRECATED-BUT-FUNCTIONAL on macOS (the CLI prints a
//! deprecation notice yet the seatbelt kernel enforcement is fully live).
//! Phase-4 may move to a sandboxd profile or App Sandbox entitlements; the
//! manifest -> profile derivation here is the stable part.
//!
//! Reuses the actions.rs discipline: args-only `Command` (never a shell
//! string), `kill_on_drop(true)`, bounded waits. The session HMAC key lives in
//! a process-lifetime `OnceLock` and is NEVER logged, NEVER put on telemetry,
//! and NEVER handed to an app — only the derived per-app token reaches the
//! app's environment.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::Sha256;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, warn};

use crate::telemetry;

type HmacSha256 = Hmac<Sha256>;

/// macOS seatbelt wrapper — deprecated CLI, live kernel enforcement.
pub(crate) const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
/// Apple's baseline BSD seatbelt profile. Imported by every generated profile
/// so the sandboxed process can actually boot (dyld, frameworks, the syscalls
/// every macOS binary needs) WITHOUT opening the filesystem, network, mic, or
/// GPU — those stay default-deny and are granted only per the manifest. This
/// is the same base Apple's own daemon profiles import.
pub(crate) const BSD_BASE_PROFILE: &str = "/System/Library/Sandbox/Profiles/bsd.sb";
/// The project venv interpreter a `runtime = "python"` app launches under.
/// Relative to the project root; resolved per-launch.
const PYTHON_INTERP_REL: &str = ".venv/bin/python3";

const MAX_APP_LINE_BYTES: usize = 1024 * 1024; // 1 MiB: app items/status/log relay lines; bounds a malicious/compromised app from OOMing the daemon (mirrors command.rs MAX_LINE_BYTES).

/// Restart governor: at most this many restarts within the window before the
/// host gives up on an app and emits app.crashed (see [`RestartGovernor`]).
const MAX_RESTARTS: u32 = 3;
const RESTART_WINDOW: Duration = Duration::from_secs(5 * 60);

/// Cap on concurrently in-flight [`request_op`] waiters per app. The agent tool
/// loop issues at most a handful per turn and every waiter is evicted on
/// timeout/teardown; the cap is belt-and-braces so a bug can never grow the
/// pending map without bound.
const MAX_PENDING_REQUESTS: usize = 32;

/// Default wall-clock budget for one [`request_op`] round trip. Generous enough
/// to cover a cold app launch (spawn + sandbox + socket connect + start
/// handshake) ahead of the compute itself; steady-state answers are ms.
pub const APP_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

// ===========================================================================
// Manifest
// ===========================================================================

/// A parsed `apps/<name>/manifest.toml` (docs/SANDBOX.md schema). Unknown keys
/// are rejected (`deny_unknown_fields`) so a typo'd permission can never
/// silently widen or narrow the sandbox.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppManifest {
    pub app: AppSection,
    #[serde(default)]
    pub permissions: PermissionsSection,
    #[serde(default)]
    pub ui: UiSection,
    /// #36 PLUGIN SDK — the OPTIONAL capability-module contract block: the
    /// intents this plugin answers and the tools it exposes. `#[serde(default)]`
    /// (=> empty) so EVERY existing manifest (global-scan, vision, …) that omits
    /// it still parses unchanged. The block is VALIDATED by
    /// `crate::plugin_sdk::validate_manifest` (required fields, well-formed
    /// intent/tool names, requested capability scopes within the allowed set);
    /// the daemon's launcher continues to derive the SBPL profile + token from
    /// `[permissions]` exactly as before — declaring an intent grants nothing.
    #[serde(default)]
    pub intents: IntentsSection,
    #[serde(default)]
    pub tools: ToolsSection,
}

/// #36 — the `[intents]` block: the intent names this plugin claims to answer.
/// EMPTY by default (a plugin need not declare any). Validated by the plugin SDK.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct IntentsSection {
    /// The intent names the plugin answers (e.g. "fab.status"). Each must be a
    /// well-formed dotted lowercase identifier (validated in plugin_sdk.rs).
    pub provides: Vec<String>,
}

/// #36 — the `[tools]` block: the tools this plugin exposes, each with the
/// capability scopes it requests. EMPTY by default. Validated by the plugin SDK:
/// a requested scope outside the allowed set, or a scope the sandbox forbids,
/// is rejected; an exposed tool the SDK marks consequential still rides the gate.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ToolsSection {
    /// The tools the plugin exposes (array-of-tables: `[[tools.exposes]]`).
    pub exposes: Vec<ToolDecl>,
}

/// #36 — one exposed tool's declaration. `deny_unknown_fields` so a typo'd tool
/// key is a parse error, never a silently-dropped scope.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ToolDecl {
    /// Tool name (e.g. "fab.read_status"). Well-formed dotted lowercase id.
    pub name: String,
    /// The capability scopes this tool requests (e.g. "net", "fs_read"). Each
    /// must be within the allowed scope set AND consistent with what the
    /// `[permissions]` block / sandbox actually grants (validated in plugin_sdk).
    pub scopes: Vec<String>,
    /// Whether the tool is side-effecting. A consequential tool still PARKS
    /// behind the cross-turn confirmation gate when invoked — declaring it here
    /// only makes the contract auditable, it never bypasses the gate. The agent
    /// tool loop exposes ONLY consequential=false tools (pure local compute);
    /// a consequential app tool is never auto-invocable.
    pub consequential: bool,
    /// WHEN the model should call this tool — becomes the agent tool def's
    /// description verbatim. Empty = the tool is declared but not self-
    /// describing; the agent loop falls back to the app's description.
    pub description: String,
    /// The tool's input fields (`[[tools.exposes.params]]`) — becomes the agent
    /// tool def's JSON input_schema. Empty = the tool takes no arguments.
    /// Fields ride the op line as TOP-LEVEL siblings of `type`/`id` (the
    /// micro-app compute contract), so `type`, `id`, `op`, and `token` are
    /// reserved and rejected as param names (validated in plugin_sdk).
    pub params: Vec<ToolParam>,
}

/// One declared input field of an exposed tool. `kind` is a JSON-Schema
/// primitive type name (validated in plugin_sdk against an allowed set).
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ToolParam {
    /// Field name as compute() reads it from the op object.
    pub name: String,
    /// JSON-Schema type: "string" | "number" | "integer" | "boolean" |
    /// "object" | "array".
    pub kind: String,
    /// Whether the model must supply it.
    pub required: bool,
    /// What the field means (shown to the model in the input_schema).
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppSection {
    pub name: String,
    pub version: String,
    pub description: String,
    /// Command the app runs. For python/node this is the entry script
    /// (relative to the project root); for a binary it is the executable.
    pub entry: String,
    pub runtime: Runtime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Runtime {
    Python,
    Binary,
    Node,
}

/// THE ONE REFUSAL MESSAGE for a direct-egress net scope, shared verbatim by
/// every gate that can see one: the runtime capability ceiling
/// (`validate_capability_ceiling`), the forge author-time gate
/// (`forge::validate_permissions`), and the plugin scope cross-check
/// (`plugin_sdk::scope_backed_by_permissions`). ONE implementation, three
/// callers — the gates cannot drift into saying different things.
///
/// WHY A NET SCOPE IS REFUSED, NOT SHAPED: macOS SBPL has NO host or IP
/// filtering primitive. `(remote tcp (host-name "x"))` and `(remote ip
/// "1.2.3.4:443")` are both rejected by the compiler ("host must be * or
/// localhost"), so a NON-EMPTY `net_hosts` produced a profile `sandbox-exec`
/// refused (exit 65) and the app never launched at all. There is therefore no
/// hostname list an author can write that works: empty means no network, and
/// non-empty meant a dead app. The declaration has no satisfying state, so the
/// honest gate is to refuse it at validation and name the route that does work.
pub const NET_SCOPE_REFUSAL: &str = "permission not grantable: `net_hosts` (direct outbound network) \
     cannot be enforced on this platform — macOS SBPL has no host or IP filtering primitive, so a \
     non-empty list produces a sandbox profile the OS refuses to compile and the app never launches. \
     Remove `net_hosts` and route the request through the daemon-mediated fetch proxy instead: declare \
     the hostnames in `fetch_hosts` and fetch over `state/ipc/apps/fetch.sock` (https-only, exact-host, \
     SSRF-guarded). See docs/SANDBOX.md.";

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PermissionsSection {
    pub audio: bool,
    /// ALWAYS EMPTY IN A VALIDATED MANIFEST — a direct-egress net scope is
    /// refused at validation (see [`NET_SCOPE_REFUSAL`]). The field is retained
    /// so a manifest that still declares it parses and reaches that refusal with
    /// a precise diagnostic, rather than dying on an `unknown field` error that
    /// names no remedy.
    pub net_hosts: Vec<String>,
    /// Hostnames the app may fetch THROUGH the daemon-mediated fetch proxy
    /// (`fetchproxy.rs`), which the app reaches over `state/ipc/apps/fetch.sock`.
    /// UNLIKE `net_hosts`, this grants NO direct network or DNS: the app declares
    /// the hosts it needs, the daemon fetches each URL (https-only, exact-host,
    /// SSRF/rebind-guarded, redirect-bounded, body-capped) and returns the body.
    /// An app can therefore keep a flat `(deny network*)` SBPL profile while still
    /// reaching declared hosts. It is the ONLY filtered egress DARWIN has: the
    /// sandbox has none to offer, because SBPL cannot filter network at all (the
    /// two "INHERENT caveats" this doc used to cite -- coarse host filtering and
    /// a DNS side channel -- described rules that never compiled; docs/SANDBOX.md
    /// -> "A net scope is not grantable"). The allow-listing, SSRF guard and
    /// redirect re-authorization live in Rust, where they actually run. Ceiling-
    /// checked as a bare DNS name with a bounded count (`MAX_APP_FETCH_HOSTS` --
    /// `net_hosts` has no ceiling any more because no non-empty value is
    /// accepted), and bound into the capability token (see
    /// `canonical_permissions`). `#[serde(default)]` (=>
    /// empty) so EVERY existing manifest that omits the key parses unchanged and
    /// stays fetch-denied for everything.
    pub fetch_hosts: Vec<String>,
    pub fs_read: Vec<String>,
    pub fs_write: Vec<String>,
    pub gpu: bool,
    /// AVFoundation capture from the user's OWN camera (Vision micro-app).
    ///
    /// IMPORTANT — TCC IS THE REAL GATE: this key only *declares* that the app
    /// needs camera access so the daemon can surface it in the launch UI /
    /// status and so the manifest's intent is auditable. It grants NOTHING by
    /// itself. macOS TCC (the Camera privacy permission) requires runtime USER
    /// CONSENT and is NOT grantable by an SBPL/seatbelt profile — consent
    /// happens on-device at first capture. `#[serde(default)]` (=> false) so
    /// EVERY existing manifest (global-scan, silicon-canvas, …) that omits the
    /// key still parses unchanged and stays camera-denied.
    pub camera: bool,
    /// ScreenCaptureKit capture of the user's OWN screen (Vision micro-app).
    /// Same TCC caveat as `camera` (the Screen Recording privacy permission):
    /// a DECLARATION only, never a grant; TCC consent is the on-device gate and
    /// is not SBPL-grantable. `#[serde(default)]` (=> false) keeps all existing
    /// manifests parsing and screen-denied.
    pub screen: bool,
    /// Dynamic code generation (JIT / writable-then-executable memory).
    ///
    /// DEFENSE-IN-DEPTH + AUDITABLE INTENT — NOT the primary gate. On Apple
    /// Silicon a DARWIN micro-app already cannot obtain RWX / `MAP_JIT` memory:
    /// the profile is `(deny default)` and the app runs under an unsigned/ad-hoc
    /// interpreter (python3/node) with NO `com.apple.security.cs.allow-jit`
    /// code-signing entitlement, and arm64e hardware W^X never maps a page
    /// writable-and-executable at once (`pthread_jit_write_protect_np` toggles a
    /// MAP_JIT region between `rw-` and `r-x`, never both). So `jit` here does
    /// three things the platform deny does not: it makes the intent DECLARED and
    /// auditable, it lets `generate_sbpl` emit an EXPLICIT `dynamic-code-generation`
    /// deny/allow (reorder-safe, like `gpu`), and it BINDS the bit into the
    /// per-launch HMAC token (see `canonical_permissions`) so a manifest that
    /// flips `jit` after a token was minted fails verification. `#[serde(default)]`
    /// (=> false) so every existing manifest parses unchanged and stays JIT-denied.
    ///
    /// HONESTY: `jit = true` is NECESSARY-BUT-NOT-SUFFICIENT — the seatbelt
    /// `(allow dynamic-code-generation)` does not grant the hardened-runtime
    /// entitlement, so under the current unsigned-interpreter launch it still does
    /// not enable RWX. Treating `jit = true` as a CONSEQUENTIAL capability
    /// declaration (an authored manifest edit, never a runtime auto-grant) is the
    /// project rule; auto-promotion must ride confirm + voice-id + policy + lockdown.
    pub jit: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct UiSection {
    pub surface: String,
    pub telemetry_topics: Vec<String>,
}

impl Default for UiSection {
    fn default() -> Self {
        Self {
            surface: "panel".to_string(),
            telemetry_topics: Vec::new(),
        }
    }
}

impl AppManifest {
    /// Parse a manifest from its TOML text and validate the invariants the
    /// launcher relies on (non-empty name/version/entry, name = directory).
    /// `dir_name` is the on-disk app directory the manifest was read from;
    /// SANDBOX.md requires `[app].name` to match it (it keys the socket and
    /// the token, so a mismatch would mint a token for the wrong identity).
    pub fn parse(raw: &str, dir_name: &str) -> Result<Self> {
        let manifest: AppManifest =
            toml::from_str(raw).context("manifest is not valid TOML for the SANDBOX.md schema")?;
        manifest.validate(dir_name)?;
        Ok(manifest)
    }

    /// Read and parse `<app_dir>/manifest.toml`.
    pub fn load(app_dir: &Path) -> Result<Self> {
        let dir_name = app_dir
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow!("app dir has no readable name: {}", app_dir.display()))?;
        let path = app_dir.join("manifest.toml");
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&raw, dir_name)
    }

    fn validate(&self, dir_name: &str) -> Result<()> {
        if self.app.name.trim().is_empty() {
            bail!("manifest [app].name is empty");
        }
        if self.app.name != dir_name {
            bail!(
                "manifest [app].name ({:?}) must match its directory name ({:?})",
                self.app.name,
                dir_name
            );
        }
        if self.app.version.trim().is_empty() {
            bail!("manifest [app].version is empty");
        }
        if self.app.entry.trim().is_empty() {
            bail!("manifest [app].entry is empty");
        }
        self.validate_capability_ceiling()?;
        Ok(())
    }

    /// CAPABILITY CEILING (Wave A): bound the STRUCTURAL shape of `[permissions]`
    /// at discover time, so an over-broad or malformed manifest is rejected
    /// (fail-closed, surfaced as an install error) BEFORE the app is ever
    /// registered or launched — the runtime discover/launch path previously had
    /// no permission bound at all.
    ///
    /// Deliberately NOT the forge author-time ban on audio/gpu/camera/screen:
    /// those are legitimate for first-party apps (vision needs camera/screen,
    /// nexus needs audio). This bounds the invariants EVERY app must honor:
    ///   - fs_write / fs_read are CONFINED in-project relative paths (no
    ///     absolute path, no `..`/root escape) — a manifest can never declare
    ///     write access to `/` or read access to `../../etc`;
    ///   - `net_hosts` is EMPTY — a direct-egress net scope is NOT GRANTABLE on
    ///     this OS at all (see [`NET_SCOPE_REFUSAL`]); egress rides `fetch_hosts`.
    ///
    /// Every shipped manifest already satisfies this; the ceiling exists to stop
    /// a NEW/edited manifest from widening the sandbox beyond these invariants.
    fn validate_capability_ceiling(&self) -> Result<()> {
        // Named for the surface it actually bounds. It used to be
        // MAX_APP_NET_HOSTS and was left pointing at `fetch_hosts` when the net
        // scope was refused -- a name that implied a net_hosts ceiling still
        // existed, when in fact NO net_hosts value other than [] is accepted.
        const MAX_APP_FETCH_HOSTS: usize = 16;
        let p = &self.permissions;
        for w in &p.fs_write {
            if !crate::forge::is_confined_relpath(w) {
                bail!("over-broad permission: fs_write {w:?} is not a confined in-project relative path");
            }
        }
        for r in &p.fs_read {
            if !crate::forge::is_confined_relpath(r) {
                bail!("over-broad permission: fs_read {r:?} is not a confined in-project relative path");
            }
        }
        // A DIRECT-EGRESS NET SCOPE IS NOT GRANTABLE — refuse the declaration
        // outright rather than shaping it. There is no bare-hostname/count
        // ceiling to apply any more, because there is no value of `net_hosts`
        // other than `[]` that this OS can enforce. See `NET_SCOPE_REFUSAL`.
        if !p.net_hosts.is_empty() {
            bail!("{NET_SCOPE_REFUSAL}");
        }
        // fetch_hosts (the daemon-mediated fetch proxy allow-list) is ceiling-
        // checked IDENTICALLY to net_hosts: bounded count, each a bare DNS name
        // (no scheme / path / port / whitespace / `..`). The proxy re-validates
        // each URL at fetch time, but this stops a NEW/edited manifest from
        // declaring a malformed or over-broad fetch allow-list at discovery.
        if p.fetch_hosts.len() > MAX_APP_FETCH_HOSTS {
            bail!(
                "over-broad permission: fetch_hosts declares {} hosts (max {MAX_APP_FETCH_HOSTS})",
                p.fetch_hosts.len()
            );
        }
        for h in &p.fetch_hosts {
            let h = h.trim();
            if h.is_empty() || h.contains('/') || h.contains(':') || h.contains(' ') || h.contains("..") {
                bail!("over-broad permission: fetch_hosts entry {h:?} is not a bare hostname");
            }
        }
        Ok(())
    }

    pub fn name(&self) -> &str {
        &self.app.name
    }
}

// ===========================================================================
// Capability token (HMAC-SHA256 over name || perms || nonce)
// ===========================================================================

/// Canonical, stable string of the manifest's permission set. The token binds
/// to THIS exact set, so a manifest that widens its permissions after a token
/// was minted (or a token lifted from another app with a different set) fails
/// verification. Sorting every list makes the canonical form independent of
/// declaration order — two manifests that grant the same thing in a different
/// order produce the same token, a reordered-but-identical manifest is not a
/// new identity.
pub fn canonical_permissions(p: &PermissionsSection) -> String {
    fn joined(label: &str, items: &[String]) -> String {
        let mut v: Vec<&str> = items.iter().map(String::as_str).collect();
        v.sort_unstable();
        format!("{label}=[{}]", v.join(","))
    }
    // camera/screen/jit are part of the bound permission set: a manifest that
    // flips any of them after a token was minted must fail verification (same
    // discipline as audio/gpu — see the token_is_bound_to_* tests). Appended
    // AFTER the original fields so the canonical form stays a stable, readable
    // suffix. The session HMAC key is regenerated every daemon boot and tokens
    // are minted per launch from THIS function, so widening the canonical string
    // does not strand any persisted token — there are none across a restart.
    // fetch_hosts (the fetch-proxy allow-list) is bound too, appended LAST for the
    // same reason: a manifest that widens the hosts it can proxy-fetch after a
    // token was minted must fail verification.
    format!(
        "audio={};gpu={};{};{};{};camera={};screen={};jit={};{}",
        p.audio,
        p.gpu,
        joined("net_hosts", &p.net_hosts),
        joined("fs_read", &p.fs_read),
        joined("fs_write", &p.fs_write),
        p.camera,
        p.screen,
        p.jit,
        joined("fetch_hosts", &p.fetch_hosts),
    )
}

/// A compact, SECRET-FREE, human-readable summary of what a micro-app is DECLARED
/// to be able to do (its granted capabilities from `[permissions]`) — the static
/// "what can this app do" audit that complements the runtime introspection's "what
/// is it doing". Lists ONLY granted capabilities (counts for the list-valued ones,
/// never the paths/hosts themselves), so a locked-down app reads short. Pure.
pub fn capability_summary(p: &PermissionsSection) -> String {
    let mut parts: Vec<String> = Vec::new();
    if p.audio {
        parts.push("audio".to_string());
    }
    if p.gpu {
        parts.push("gpu".to_string());
    }
    if p.camera {
        parts.push("camera".to_string());
    }
    if p.screen {
        parts.push("screen".to_string());
    }
    if p.jit {
        parts.push("jit".to_string());
    }
    if !p.net_hosts.is_empty() {
        parts.push(format!("net({})", p.net_hosts.len()));
    }
    if !p.fetch_hosts.is_empty() {
        parts.push(format!("fetch({})", p.fetch_hosts.len()));
    }
    if !p.fs_read.is_empty() {
        parts.push(format!("fs_read({})", p.fs_read.len()));
    }
    if !p.fs_write.is_empty() {
        parts.push(format!("fs_write({})", p.fs_write.len()));
    }
    if parts.is_empty() {
        "sandboxed (no extra capabilities)".to_string()
    } else {
        parts.join(", ")
    }
}

/// The message the HMAC is computed over: `name || canonical(perms) || nonce`,
/// joined with NUL so no field can bleed into the next (a name ending in the
/// next field's prefix can never collide).
fn token_message(name: &str, perms: &PermissionsSection, nonce: &str) -> Vec<u8> {
    let mut msg = Vec::new();
    msg.extend_from_slice(name.as_bytes());
    msg.push(0);
    msg.extend_from_slice(canonical_permissions(perms).as_bytes());
    msg.push(0);
    msg.extend_from_slice(nonce.as_bytes());
    msg
}

/// Compute the hex-encoded HMAC-SHA256 token. Pure given the key — the unit
/// tests drive it directly with a fixed key to prove forgery/tamper/cross-app
/// rejection without a live daemon.
pub fn compute_token(key: &[u8], name: &str, perms: &PermissionsSection, nonce: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(&token_message(name, perms, nonce));
    hex::encode(mac.finalize().into_bytes())
}

/// Constant-time verification: recompute and compare with the MAC's own
/// constant-time `verify_slice` (never a `==` on the hex string).
pub fn verify_token_with_key(
    key: &[u8],
    name: &str,
    perms: &PermissionsSection,
    nonce: &str,
    presented: &str,
) -> bool {
    let Ok(presented_bytes) = hex::decode(presented) else {
        return false;
    };
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(&token_message(name, perms, nonce));
    mac.verify_slice(&presented_bytes).is_ok()
}

/// The daemon-local session HMAC key, generated once at startup and never
/// after. NEVER logged, NEVER on telemetry, NEVER in an app's env — only the
/// derived per-app token leaves this module. A fresh key every boot means a
/// token leaked from a previous run is dead after a restart.
static SESSION_KEY: OnceLock<[u8; 32]> = OnceLock::new();

fn session_key() -> &'static [u8; 32] {
    SESSION_KEY.get_or_init(|| {
        // 32 bytes of OS entropy. getrandom via a fresh, unseeded source: we
        // pull from /dev/urandom directly to avoid adding an RNG dependency
        // and to keep the key off any logged code path.
        let mut key = [0u8; 32];
        match std::fs::File::open("/dev/urandom")
            .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut key))
        {
            Ok(()) => key,
            Err(e) => {
                // /dev/urandom is effectively always present on macOS; if it
                // is not, fail loud rather than minting predictable tokens.
                panic!("cannot read /dev/urandom to seed the app session key: {e}");
            }
        }
    })
}

// ===========================================================================
// Command-channel capability token (HUD -> daemon command socket)
// ===========================================================================
//
// The HUD command channel (command.rs) is JUST ANOTHER authenticated local
// caller, so it reuses the SAME HMAC-SHA256 machinery as the per-app relay and
// the generate proxy — no parallel token scheme. The principal is a RESERVED
// pseudo-app name (never a real micro-app, so it can never collide with a
// manifest) with an EMPTY permission set, bound to a per-BOOT nonce minted
// once at startup. A fresh session key + fresh nonce every boot means a token
// captured from a previous run is dead after a restart, exactly like an app
// token. The token is the daemon's authority to ACCEPT commands; it is handed
// to the Tauri backend out-of-band (the same keychain/handshake path the HUD
// already uses for verify_dispatch) and NEVER logged or put on telemetry.

/// The reserved capability principal for the HUD command channel. Prefixed with
/// a character no manifest name can use (manifest names are the on-disk app
/// directory name) so it can never collide with a real micro-app identity.
pub const COMMAND_PRINCIPAL: &str = "@hud-command";

/// The command principal's bound permission set: EMPTY. The command token grants
/// no filesystem/network/device capability of its own — it only authenticates
/// the caller to the command socket, whose allowed actions are a fixed structural
/// allowlist (command.rs), each routing into the EXISTING gated pipeline. Binding
/// to a constant empty set means the token is over exactly `name || "" || nonce`,
/// matching the per-app token shape without granting any app permission.
fn command_perms() -> PermissionsSection {
    PermissionsSection::default()
}

/// The per-boot nonce for the command principal, minted once at startup from OS
/// entropy. A leaked command token dies when the daemon restarts (new nonce).
static COMMAND_NONCE: OnceLock<String> = OnceLock::new();

fn command_nonce() -> &'static str {
    COMMAND_NONCE.get_or_init(fresh_nonce)
}

/// Mint the HUD command-channel capability token from the CURRENT session key,
/// the reserved principal, its empty permission set, and the per-boot nonce.
/// Called ONCE at daemon startup; the value is handed to the Tauri backend
/// out-of-band and presented on every command line. Reuses [`compute_token`] —
/// the SAME HMAC machinery as the per-app/genproxy tokens.
pub fn mint_command_token() -> String {
    compute_token(
        session_key(),
        COMMAND_PRINCIPAL,
        &command_perms(),
        command_nonce(),
    )
}

/// Constant-time verify of a token presented on the command socket against the
/// CURRENT session key + per-boot nonce. A forged/tampered/stale (pre-restart)
/// token fails closed. Reuses [`verify_token_with_key`] — no new crypto.
pub fn verify_command_token(presented: &str) -> bool {
    if presented.is_empty() {
        return false;
    }
    verify_token_with_key(
        session_key(),
        COMMAND_PRINCIPAL,
        &command_perms(),
        command_nonce(),
        presented,
    )
}

// ===========================================================================
// Process-global registry handle
// ===========================================================================
//
// The router threads the `Arc<AppRegistry>` explicitly into every app handler
// (handle_silicon_canvas/…), which is the primary path. But a MODEL-callable tool
// (`share_guard_scrub`) runs under `anthropic::execute_tool`, which is NOT given
// the registry, so it reaches the app runtime through this process-global handle
// — the SAME pattern `mcp::global()` uses for its manager. Set ONCE at startup
// (next to `AppRegistry::discover`); `None` until then (a pre-startup / unit-test
// caller gets an honest "runtime not up" rather than a panic).

/// The one live app registry, set once at daemon startup. Read via
/// [`global_registry`]; the router's explicit `Arc<AppRegistry>` threading is
/// unchanged and remains the primary path.
static GLOBAL_REGISTRY: OnceLock<Arc<AppRegistry>> = OnceLock::new();

/// Publish the process-global app registry (called ONCE at startup, right after
/// `AppRegistry::discover`). Idempotent: a second call is ignored so a stray
/// re-init can never swap the live registry out from under running apps.
pub fn set_global_registry(registry: Arc<AppRegistry>) {
    let _ = GLOBAL_REGISTRY.set(registry);
}

/// The process-global app registry, or `None` before startup published it (a
/// pre-startup or unit-test caller). Callers answer honestly on `None` — the app
/// runtime simply isn't up yet.
pub fn global_registry() -> Option<Arc<AppRegistry>> {
    GLOBAL_REGISTRY.get().cloned()
}

// ===========================================================================
// SBPL (seatbelt) profile generation
// ===========================================================================

/// Generate the macOS `sandbox-exec` (seatbelt / SBPL) profile text for an
/// app. DEFAULT-DENY: the profile opens with `(deny default)` and then grants
/// ONLY what the manifest declares.
///
/// `project_root` is the absolute project root; `interp` is the absolute
/// interpreter/runtime path the app launches under (the venv python for a
/// python app, the binary itself for a binary app); `app_dir` is the app's own
/// absolute directory. All allow-paths are emitted absolute — SBPL path
/// filters are not relative to a cwd.
///
/// Grants:
///   - process-exec* of the interpreter + the entry/app dir (start the child),
///   - file-read* of: the app's own dir, the interpreter & its runtime libs
///     (for python: the project .venv tree + the system framework prefixes the
///     stdlib loads from), each manifest `fs_read` path, plus dyld/dylib search
///     roots so the runtime can actually start,
///   - file-write* of each manifest `fs_write` path + the app's own per-app
///     socket dir (state/ipc/apps) so it can connect,
///   - network: NOTHING. `(deny network*)` unconditionally — a direct-egress
///     net scope is not grantable on this OS (see `NET_SCOPE_REFUSAL`), so no
///     app gets an IP stack or a resolver. The only outbound grants are AF_UNIX
///     literals for the app's own socket and any `.sock` it declares (the
///     fetch/generate proxies), which is how declared egress reaches the daemon,
///   - mach lookups the loader needs (dyld, the system framework registry).
///     Everything else — other filesystem, other network, the microphone, GPU,
///     the window server, the memory DB, secrets — stays denied by the opener.
pub fn generate_sbpl(
    manifest: &AppManifest,
    project_root: &Path,
    interp: &Path,
    app_dir: &Path,
    socket_path: &Path,
) -> String {
    let p = &manifest.permissions;
    let root = project_root;
    let mut s = String::new();

    // --- header --------------------------------------------------------
    s.push_str("(version 1)\n");
    s.push_str(&format!(
        ";; Generated by darwind for micro-app {:?} — docs/SANDBOX.md.\n",
        manifest.name()
    ));
    s.push_str(";; sandbox-exec is deprecated-but-functional on macOS; the\n");
    s.push_str(";; kernel seatbelt enforcement is live. Phase-4 may migrate to\n");
    s.push_str(";; a sandboxd profile or App Sandbox entitlements.\n");
    s.push_str(";; DEFAULT-DENY: everything below is the complete grant set.\n");
    s.push_str("(deny default)\n");
    // Import Apple's baseline BSD profile: it grants ONLY the syscalls, dyld /
    // framework boot reads, and timezone/encoding files that EVERY macOS
    // process needs to start — it does NOT open the filesystem, the network,
    // the mic, or the GPU (reading ~/.ssh or the memory DB is still denied).
    // Without this base, even /bin/sleep aborts on launch under (deny default);
    // with it, file/network/device access remains exactly the manifest grants
    // added below. system.sb is pulled in transitively by bsd.sb.
    if Path::new(BSD_BASE_PROFILE).exists() {
        s.push_str(&format!("(import {})\n", sbpl_str(Path::new(BSD_BASE_PROFILE))));
    }

    // --- explicit denies the manifest's booleans map to -----------------
    // These are already covered by (deny default); stated explicitly so the
    // profile reads as the SANDBOX.md derivation table and so a future
    // allow-rule reordering can't accidentally open them.
    if !p.audio {
        s.push_str("\n;; audio = false -> no microphone / audio device access.\n");
        s.push_str("(deny device-microphone)\n");
    }
    if !p.gpu {
        s.push_str("\n;; gpu = false -> no Metal / IOKit GPU client.\n");
        s.push_str("(deny iokit-open (iokit-user-client-class \"IOAccelerator\"))\n");
        s.push_str("(deny iokit-open (iokit-user-client-class \"AGXDeviceUserClient\"))\n");
    }

    // --- camera / screen (TCC-gated; SBPL is best-effort only) -------------
    // CRITICAL HONESTY: on macOS, CAMERA (AVFoundation) and SCREEN RECORDING
    // (ScreenCaptureKit) are gated by TCC — the privacy-consent subsystem.
    // TCC requires a RUNTIME USER-CONSENT prompt and is NOT grantable by an
    // SBPL/seatbelt profile: there is no `(allow camera)` / `(allow screen)`
    // operation, and even with everything below allowed the kernel+TCC still
    // block capture until the user consents on-device at first use. So the
    // manifest's `camera`/`screen = true` only DECLARES the need (surfaced in
    // the launch UI / status); the profile cannot and does not pretend to
    // enable capture. We keep DEFAULT-DENY and, at most, grant the mach-lookup
    // /device plumbing the capture frameworks need to even REACH the consent
    // prompt (best effort) — never the capture grant itself.
    if p.camera {
        s.push_str("\n;; camera = true -> DECLARED need for AVFoundation capture of\n");
        s.push_str(";; the user's OWN camera. macOS TCC (Camera) is the REAL gate:\n");
        s.push_str(";; it needs runtime user consent and is NOT SBPL-grantable, so\n");
        s.push_str(";; the lines below are BEST EFFORT plumbing only (reach the\n");
        s.push_str(";; capture stack + its consent prompt) — they do NOT enable\n");
        s.push_str(";; capture. No consent -> no frames, profile notwithstanding.\n");
        s.push_str("(allow iokit-open (iokit-user-client-class \"IOVideoDeviceUserClient\"))\n");
        s.push_str("(allow mach-lookup (global-name \"com.apple.cmio.AppleCameraAssistant\"))\n");
        s.push_str("(allow mach-lookup (global-name \"com.apple.tccd\"))\n");
    } else {
        s.push_str("\n;; camera = false -> no camera. (deny default) already blocks\n");
        s.push_str(";; it; stated explicitly so a future allow-reorder can't open it.\n");
        s.push_str("(deny iokit-open (iokit-user-client-class \"IOVideoDeviceUserClient\"))\n");
    }
    if p.screen {
        s.push_str("\n;; screen = true -> DECLARED need for ScreenCaptureKit capture\n");
        s.push_str(";; of the user's OWN screen. macOS TCC (Screen Recording) is the\n");
        s.push_str(";; REAL gate: runtime user consent, NOT SBPL-grantable. The lines\n");
        s.push_str(";; below are BEST EFFORT plumbing (reach the window/capture\n");
        s.push_str(";; server + its consent prompt) — they do NOT enable capture.\n");
        s.push_str(";; No consent -> no frames, profile notwithstanding.\n");
        s.push_str("(allow mach-lookup (global-name \"com.apple.windowserver.active\"))\n");
        s.push_str("(allow mach-lookup (global-name \"com.apple.tccd\"))\n");
    } else {
        s.push_str("\n;; screen = false -> no screen capture. (deny default) already\n");
        s.push_str(";; blocks the window server; stated explicitly for clarity.\n");
        s.push_str("(deny mach-lookup (global-name \"com.apple.windowserver.active\"))\n");
    }

    // --- jit / dynamic code generation (defense-in-depth; NOT the sole gate) ---
    // Only `dynamic-code-generation` is a current seatbelt operation — the
    // legacy `dynamic-signature` op is NOT emitted (it is not a live operation on
    // current macOS and would risk a profile-compile error, the class of failure
    // deny_unknown_fields guards elsewhere). On Apple Silicon the RWX/MAP_JIT deny
    // is PRIMARILY enforced by the platform (no com.apple.security.cs.allow-jit
    // entitlement on the unsigned/ad-hoc interpreter + arm64e hardware W^X), so
    // this line is defense-in-depth and auditable intent, not the primary barrier.
    if !p.jit {
        s.push_str("\n;; jit = false -> no dynamic code generation (JIT / RWX).\n");
        s.push_str(";; Already denied by (deny default) AND, on Apple Silicon, by the\n");
        s.push_str(";; platform (no allow-jit entitlement + arm64e W^X). Stated\n");
        s.push_str(";; explicitly so a future allow-reorder can't open it.\n");
        s.push_str("(deny dynamic-code-generation)\n");
    } else {
        s.push_str("\n;; jit = true -> DECLARED need for dynamic code generation (JIT).\n");
        s.push_str(";; HONESTY: NECESSARY-BUT-NOT-SUFFICIENT. On a hardened/notarized\n");
        s.push_str(";; build the PROCESS also needs the com.apple.security.cs.allow-jit\n");
        s.push_str(";; code-signing entitlement (SBPL cannot grant it) and must use\n");
        s.push_str(";; MAP_JIT + pthread_jit_write_protect_np to keep W^X. Under the\n");
        s.push_str(";; current unsigned-interpreter launch this grant alone does NOT\n");
        s.push_str(";; enable RWX — same best-effort caveat as camera/screen.\n");
        s.push_str("(allow dynamic-code-generation)\n");
    }

    // Resolve the interpreter's REAL path once. The venv python3 is a SYMLINK
    // (.venv/bin/python3 -> the Homebrew Cellar python) and seatbelt checks
    // exec against the RESOLVED target, so we must grant exec on the canonical
    // path too — but as a LITERAL on the resolved file, NOT a subpath over the
    // whole Homebrew/usr-local tree (a broad prefix would let the app exec any
    // bash/curl/git/compiler planted there, and those prefixes are user-
    // writable on Homebrew installs). canonicalize() is best-effort: if it
    // fails (path not yet materialized in a test root) we fall back to the
    // configured path, which the literal below already covers.
    let interp_abs = abs(root, interp);
    let interp_real = std::fs::canonicalize(&interp_abs).unwrap_or_else(|_| interp_abs.clone());

    // Read prefixes: the directory trees the interpreter + its standard
    // libraries live under. The app still needs to READ its stdlib/site-
    // packages to import anything — for a venv those live under .venv and under
    // the resolved interpreter's own INSTALL PREFIX (the Cellar version dir
    // that holds lib/pythonX.Y), which we derive tightly from the resolved
    // interpreter path rather than opening all of /opt/homebrew. Read is a far
    // weaker grant than exec, but we still scope it to just what boots.
    let runtime_read_prefixes: Vec<PathBuf> = match manifest.app.runtime {
        Runtime::Python => {
            let mut v = vec![
                // The interpreter + site-packages read root. SUBSTRATE LOCK
                // (envlock.rs) NARROWING SEAM: when the interpreter lives inside a
                // pinned content-addressed closure (state/envstore/<hash>/…) this is
                // that CLOSURE dir — app-specific, read-only, exactly the pinned
                // files — replacing the shared project .venv and closing the
                // shared-.venv reach + venv-drift caveats. For an UNPINNED app
                // (interpreter under .venv, the legacy path) it returns the project
                // .venv, byte-for-byte the prior behavior.
                crate::envlock::python_runtime_read_root(root, &interp_abs),
                // The system Python framework, when used directly.
                PathBuf::from("/Library/Frameworks/Python.framework"),
            ];
            // The resolved interpreter's install prefix: <prefix>/bin/python3
            // -> <prefix> holds lib/pythonX.Y (the stdlib). Grant read on that
            // prefix only, not the whole Cellar/Homebrew root.
            if let Some(prefix) = interpreter_install_prefix(&interp_real) {
                v.push(prefix);
            }
            v
        }
        Runtime::Node => {
            let mut v = Vec::new();
            if let Some(prefix) = interpreter_install_prefix(&interp_real) {
                v.push(prefix);
            }
            v
        }
        // A prebuilt binary IS its own interpreter; nothing extra to read.
        Runtime::Binary => Vec::new(),
    };

    // --- process exec ---------------------------------------------------
    s.push_str("\n;; Start the child: exec the runtime interpreter (or, for a\n");
    s.push_str(";; binary app, the entry itself) and the app dir's own scripts.\n");
    s.push_str(";; Exec is granted ONLY on the configured interpreter path and\n");
    s.push_str(";; its canonicalized target — never a broad Homebrew/usr-local\n");
    s.push_str(";; subpath — so the app cannot exec other binaries planted there.\n");
    s.push_str("(allow process-fork)\n");
    match manifest.app.runtime {
        Runtime::Python | Runtime::Node => {
            // The configured interpreter path (the venv symlink) AND its
            // canonicalized target (what seatbelt actually checks exec against).
            s.push_str(&format!(
                "(allow process-exec* (literal {}))\n",
                sbpl_str(&interp_abs)
            ));
            if interp_real != interp_abs {
                s.push_str(&format!(
                    "(allow process-exec* (literal {}))\n",
                    sbpl_str(&interp_real)
                ));
            }
            // THE SECOND EXEC. A FRAMEWORK CPython's `bin/pythonX.Y` is a stub
            // that immediately posix_spawns the real interpreter inside the
            // framework bundle:
            //
            //   <prefix>/bin/python3.11
            //     -> <prefix>/Resources/Python.app/Contents/MacOS/Python
            //
            // Both literals above describe only the FIRST exec — `canonicalize`
            // fully resolves the venv symlink chain, so they are two spellings of
            // the same file. The stub's own spawn was never granted, seatbelt
            // denied it, and the child exited 1 before ever reaching its socket:
            // `start()` still returned Ok (it only probes that the entry .py
            // exists), the restart governor burned its budget, and the caller's
            // `request_op` timed out after 15s with nothing to show for it.
            //
            // HOST-CONDITIONAL, which is why it survived review: a venv over
            // /usr/bin/python3 or any non-framework build performs no second exec
            // and works fine. Homebrew python@3.x and the python.org installers
            // are framework builds — and both this dev tree and the deployed
            // install resolve to one, where all 34 runtime="python" apps are dead.
            // (34, counted from apps/*/manifest.toml. It read 36 while `fab-link`
            // and `algo-core` were still in the tree -- and was already wrong
            // then, since both were refused at validation and never registered.
            // Deleting them is what makes a count of shipped apps a count of apps
            // that can run; a stale count here would have kept that untrue.)
            //
            // EXEC ONLY, and a LITERAL. `interpreter_install_prefix` already emits
            // a read subpath covering this file; what was missing is permission to
            // execute exactly it. A subpath grant here would hand the app exec over
            // the whole framework.
            if let Some(stub) = framework_python_stub(&interp_real) {
                s.push_str(&format!(
                    "(allow process-exec* (literal {}))\n",
                    sbpl_str(&stub)
                ));
            }
        }
        Runtime::Binary => {
            // The entry binary itself (the interp == entry for a binary app).
            s.push_str(&format!(
                "(allow process-exec* (literal {}))\n",
                sbpl_str(&interp_abs)
            ));
        }
    }
    // Scripts/helpers inside the app's own dir.
    s.push_str(&format!(
        "(allow process-exec* (subpath {}))\n",
        sbpl_str(&abs(root, app_dir))
    ));

    // --- file reads -----------------------------------------------------
    s.push_str("\n;; Reads: the app's own dir, the runtime libs needed to start,\n");
    s.push_str(";; and each manifest fs_read path. Nothing else is readable.\n");
    let mut read_subpaths: Vec<PathBuf> = Vec::new();
    // The app's own directory is implicitly readable (SANDBOX.md).
    read_subpaths.push(abs(root, app_dir));
    // The runtime read prefixes (interpreter install prefix + venv + libs).
    read_subpaths.extend(runtime_read_prefixes.iter().cloned());
    // System dyld/dylib search roots every macOS process loads from.
    read_subpaths.push(PathBuf::from("/usr/lib"));
    read_subpaths.push(PathBuf::from("/System/Library"));
    read_subpaths.push(PathBuf::from("/Library/Apple"));
    // The configured interpreter path AND its canonical target.
    read_subpaths.push(interp_abs.clone());
    if interp_real != interp_abs {
        read_subpaths.push(interp_real.clone());
    }
    // Manifest fs_read grants (resolved relative to the project root).
    for r in &p.fs_read {
        read_subpaths.push(abs(root, Path::new(r)));
    }
    for path in &read_subpaths {
        s.push_str(&format!("(allow file-read* (subpath {}))\n", sbpl_str(path)));
    }
    // file-read-metadata is SCOPED to the same granted roots — never a blanket
    // grant. A bare `(allow file-read-metadata)` (no path filter) would let the
    // app stat/test-existence on the ENTIRE filesystem — probing whether
    // ~/.ssh/id_rsa or another app's state exists and its size/mtime — an
    // info-leak side channel even though the contents stay denied. file-read*
    // already implies metadata for these subpaths; emitting the scoped
    // metadata rule explicitly documents the boundary and survives a future
    // rule reorder. dyld's startup stats of "/" and the firmlink ancestors are
    // already covered by the bsd.sb/system.sb import, so no blanket grant is
    // needed to boot.
    for path in &read_subpaths {
        s.push_str(&format!(
            "(allow file-read-metadata (subpath {}))\n",
            sbpl_str(path)
        ));
    }

    // --- file writes ----------------------------------------------------
    s.push_str("\n;; Writes: each manifest fs_write path + the app's own socket.\n");
    for w in &p.fs_write {
        s.push_str(&format!(
            "(allow file-write* (subpath {}))\n",
            sbpl_str(&abs(root, Path::new(w)))
        ));
    }
    // The per-app socket the daemon owns: the app connects (read+write) to its
    // own socket path only. The socket dir is under state/ipc/apps.
    let sock_abs = abs(root, socket_path);
    s.push_str(&format!(
        "(allow file-read* file-write* (literal {}))\n",
        sbpl_str(&sock_abs)
    ));

    // --- network --------------------------------------------------------
    // SBPL is last-match-wins, so the IP-network deny/allow rules go FIRST and
    // the Unix-socket connect grant goes LAST — otherwise a (deny network*)
    // would clobber the socket grant and the app could never reach its host.
    // A DIRECT-EGRESS NET SCOPE IS NOT GRANTABLE, so there is exactly ONE
    // network shape: deny it all. `net_hosts` is refused at validation
    // (`NET_SCOPE_REFUSAL`), so no validated manifest can reach here carrying
    // hosts — and this branch is unconditional so that even a manifest built
    // in-process, bypassing validation, still yields a profile that COMPILES
    // and grants nothing. The old host-list branch emitted
    // `(remote tcp (host-name …))`, which macOS rejects outright ("host must be
    // * or localhost"), so it could only ever produce a profile sandbox-exec
    // refused with exit 65 — a dead app, never a filtered one. Removing it
    // deletes a code path that had no correct outcome.
    //
    // All app egress now rides the daemon-mediated fetch proxy: the app declares
    // `fetch_hosts`, connects to state/ipc/apps/fetch.sock (an AF_UNIX literal
    // granted below), and the DAEMON makes the request. This branch used to
    // document two "inherent" SBPL network caveats — coarse host filtering (no IP
    // pinning / CDN co-tenant bleed) and a DNS-label exfil channel — as costs the
    // proxy collapsed. It collapsed neither, because neither ever ran: the rules
    // that would have opened them did not compile, so no app ever held an IP
    // stack or a resolver. See docs/SANDBOX.md.
    s.push_str("\n;; A net scope is not grantable on this OS -> no outbound IP\n");
    s.push_str(";; network, and no DNS, for any app. Declared egress rides the\n");
    s.push_str(";; daemon-mediated fetch proxy over fetch.sock instead.\n");
    s.push_str("(deny network*)\n");
    // The app's OWN Unix socket — granted LAST so neither network branch above
    // can clobber it. Connecting to a Unix-domain socket is network-outbound to
    // the socket path.
    s.push_str(";; The app's own per-app Unix socket (never clobbered above).\n");
    s.push_str(&format!(
        "(allow network-outbound (literal {}))\n",
        sbpl_str(&sock_abs)
    ));
    // A declared fs_read entry that IS a Unix socket (path ends in .sock) needs
    // an AF_UNIX `network-outbound` literal grant IN ADDITION to its file-read*
    // subpath above: on this macOS, file-read alone does NOT permit connect() to
    // a Unix-domain socket (connect is a network operation, not a file read).
    // Emitted here, AFTER the (deny network*) branch, so last-match-wins keeps
    // the connect grant alive. This is how a micro-app reaches the daemon-
    // mediated generate proxy at state/ipc/apps/generate.sock — and ONLY that
    // proxy, since the manifest no longer lists the raw inference.sock at all.
    for r in &p.fs_read {
        if Path::new(r).extension().and_then(|e| e.to_str()) == Some("sock") {
            let r_abs = abs(root, Path::new(r));
            // The app's own socket already has this grant; don't double-emit.
            if r_abs != sock_abs {
                s.push_str(";; fs_read Unix socket -> AF_UNIX connect() grant.\n");
                s.push_str(&format!(
                    "(allow network-outbound (literal {}))\n",
                    sbpl_str(&r_abs)
                ));
            }
        }
    }

    // --- mach / loader services the runtime needs -----------------------
    s.push_str("\n;; Mach lookups the dynamic loader and runtime require.\n");
    s.push_str("(allow mach-lookup (global-name \"com.apple.system.opendirectoryd.libinfo\"))\n");
    s.push_str("(allow mach-lookup (global-name \"com.apple.system.notification_center\"))\n");
    s.push_str("(allow mach-lookup (global-name \"com.apple.coreservices.launchservicesd\"))\n");
    s.push_str("(allow sysctl-read)\n");

    s
}

/// Quote a path/string as an SBPL string literal. SBPL strings are
/// double-quoted with backslash escaping; app paths never contain quotes in
/// practice, but escape defensively so a path with a quote or backslash can
/// never break out of the literal and widen the profile.
pub(crate) fn sbpl_str(p: &Path) -> String {
    let raw = p.to_string_lossy();
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('"');
    for c in raw.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// Resolve a possibly-relative manifest path against the project root; absolute
/// paths pass through unchanged.
fn abs(root: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    }
}

/// The framework CPython stub `<prefix>/bin/pythonX.Y` re-execs, or `None`.
///
/// A framework build ships `bin/pythonX.Y` as a small stub that posix_spawns
/// `<prefix>/Resources/Python.app/Contents/MacOS/Python`. That is a SECOND exec
/// event and needs its own seatbelt grant; without it every python micro-app
/// dies at launch on a framework host (Homebrew python@3.x, python.org).
///
/// Returns the path only when it EXISTS, so a non-framework interpreter emits no
/// grant at all rather than a dangling literal.
fn framework_python_stub(interp_real: &Path) -> Option<PathBuf> {
    let stub = interpreter_install_prefix(interp_real)?
        .join("Resources/Python.app/Contents/MacOS/Python");
    stub.is_file().then_some(stub)
}

/// The install prefix an interpreter's standard library lives under, derived
/// tightly from the RESOLVED interpreter path so the read grant covers the
/// stdlib without opening the whole Homebrew/usr-local tree. A CPython install
/// is laid out as `<prefix>/bin/python3.11` with the stdlib under
/// `<prefix>/lib/pythonX.Y`, so the prefix is the interpreter's grandparent
/// (`bin/`'s parent). Returns None when the path has no such structure (e.g. a
/// bare `/usr/bin/python3`), in which case the per-file interpreter read grant
/// and the system dyld roots already cover the boot.
fn interpreter_install_prefix(interp_real: &Path) -> Option<PathBuf> {
    let bin_dir = interp_real.parent()?; // <prefix>/bin
    // Only treat it as an install prefix when the interpreter sits in a `bin`
    // directory — otherwise we would grant read on an arbitrary ancestor.
    if bin_dir.file_name().and_then(|n| n.to_str()) != Some("bin") {
        return None;
    }
    let prefix = bin_dir.parent()?; // <prefix>
    // Guard against pathological prefixes ("/", "/usr") that would re-open a
    // broad tree — require at least two path components beyond the root.
    if prefix.components().count() < 3 {
        return None;
    }
    Some(prefix.to_path_buf())
}

// ===========================================================================
// Restart governor (pure rate math)
// ===========================================================================

/// Bounded-restart bookkeeping for one app: at most [`MAX_RESTARTS`] restarts
/// within [`RESTART_WINDOW`], after which the host gives up. Pure and tested:
/// the lifecycle loop only calls `should_restart` / `record_restart`.
#[derive(Debug)]
pub struct RestartGovernor {
    window: Duration,
    max: u32,
    /// Restart instants within the rolling window (oldest first).
    restarts: Vec<Instant>,
}

impl RestartGovernor {
    pub fn new() -> Self {
        Self {
            window: RESTART_WINDOW,
            max: MAX_RESTARTS,
            restarts: Vec::new(),
        }
    }

    #[cfg(test)]
    fn with_limits(window: Duration, max: u32) -> Self {
        Self {
            window,
            max,
            restarts: Vec::new(),
        }
    }

    /// Drop restart marks older than the window relative to `now`.
    fn evict(&mut self, now: Instant) {
        let window = self.window;
        self.restarts
            .retain(|t| now.duration_since(*t) <= window);
    }

    /// Would a restart right now stay within the budget? Counts the restarts
    /// still inside the window; true iff fewer than `max` remain.
    pub fn should_restart(&mut self, now: Instant) -> bool {
        self.evict(now);
        (self.restarts.len() as u32) < self.max
    }

    /// Record that a restart happened at `now` (call after `should_restart`).
    pub fn record_restart(&mut self, now: Instant) {
        self.evict(now);
        self.restarts.push(now);
    }

    /// Restarts counted within the window as of `now` — for telemetry.
    pub fn count(&mut self, now: Instant) -> u32 {
        self.evict(now);
        self.restarts.len() as u32
    }
}

impl Default for RestartGovernor {
    fn default() -> Self {
        Self::new()
    }
}

// ===========================================================================
// App registry + lifecycle
// ===========================================================================

/// A micro-app known to the host: its manifest, its session nonce (rotated per
/// launch), its minted token, and the paths the launcher needs.
struct AppEntry {
    manifest: AppManifest,
    app_dir: PathBuf,
    socket_path: PathBuf,
    profile_path: PathBuf,
    /// Rotated on every (re)launch — a leaked token dies when the nonce moves.
    nonce: String,
    token: String,
    /// Set while the app is supposed to be running; the lifecycle task owns it.
    running: bool,
    /// Fired by stop()/restart give-up to WAKE the lifecycle task out of its
    /// blocking select! on read_line/child.wait — otherwise a quiet, well-
    /// behaved app (one that sends a line then idles) would not be torn down
    /// until it happened to exit. Cloned into the lifecycle task at launch.
    stop_notify: Arc<tokio::sync::Notify>,
    /// HOST -> APP op queue. The router pushes a structured op line here via
    /// [`send_op`]; the live connection handler drains it and writes the line
    /// to the app's socket (alongside the start/refresh/stop control verbs).
    /// Unbounded because op lines are tiny and rare (one per spoken command)
    /// and the drain is always live while the app is connected. The receiver
    /// is `take()`n into the connection handler at accept; a line queued while
    /// the app is between connections is held until the next accept drains it.
    /// `Mutex<Option<...>>` so the lifecycle task can move the receiver out for
    /// the duration of a connection and return it on reconnect.
    op_tx: mpsc::UnboundedSender<String>,
    op_rx: Arc<Mutex<Option<mpsc::UnboundedReceiver<String>>>>,
    /// In-flight [`request_op`] waiters keyed by request id. A token-verified
    /// `type:"result"` line whose `id` matches hands its `data` to the waiting
    /// oneshot; every app-terminal path (stop, crash, launch-fail) DRAINS this
    /// map so waiters fail fast with an honest error instead of dangling until
    /// timeout. Shared (Arc) because relay_line resolves it under its own
    /// registry lock while request_op holds a clone across its await.
    pending: Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<Value>>>>,
    /// Op lines to send EVERY time this app connects, right after `start`.
    ///
    /// For an app whose host-side feature must be armed per-connection rather
    /// than once at daemon startup. Continuous screen context is the case that
    /// forced this: it was armed by a single `screen.context.start` sent during
    /// `main()`, which can only reach an ALREADY-RUNNING Vision app — and the
    /// shipped config autostarts nothing, so on a normal boot the op was dropped
    /// with "vision is not running" and NOTHING re-armed when the user later
    /// opened Vision. The ring was empty on every real boot while the config and
    /// README said the only missing piece was a TCC grant.
    ///
    /// Safe to repeat: the Vision side cancels any prior capture task before
    /// starting a new one, so a reconnect re-arms rather than doubling up.
    on_connect_ops: Vec<String>,
}

/// Arm an op to be re-sent to `app` on EVERY connection, right after `start`.
///
/// Called once at startup for a feature whose loop lives inside an app but whose
/// lifetime must follow the CONNECTION, not the daemon. See
/// [`AppEntry::on_connect_ops`] for why a one-shot send at boot is not enough.
pub async fn arm_on_connect(registry: &Arc<AppRegistry>, app: &str, op_line: &str) -> bool {
    let mut apps = registry.apps.lock().await;
    match apps.get_mut(app) {
        Some(e) => {
            if !e.on_connect_ops.iter().any(|o| o == op_line) {
                e.on_connect_ops.push(op_line.to_string());
            }
            true
        }
        None => false,
    }
}

/// The host's registry of installed micro-apps, keyed by name. One per daemon.
pub struct AppRegistry {
    project_root: PathBuf,
    /// name -> entry. Mutex (async) because the router and the lifecycle task
    /// both touch it; held only briefly.
    apps: Mutex<HashMap<String, AppEntry>>,
    /// Test-only: override the resolved interpreter for python/node apps so the
    /// hermetic integration test can point at a real interpreter without a
    /// project .venv in its tempdir. Never set in production.
    #[cfg(test)]
    interpreter_override: Option<PathBuf>,
}

/// Public, read-only view of a registered app for routing/intent matching.
#[derive(Debug, Clone)]
pub struct AppInfo {
    pub name: String,
    pub description: String,
    pub running: bool,
    /// Whether the manifest's entry file actually EXISTS right now. Spec-only
    /// apps (manifest + SPEC.md, no code yet) and unbuilt compiled apps
    /// register (deliberate: visible in the deck, build-state independent) but
    /// are honestly labeled not-runnable instead of failing at spawn time.
    pub entry_present: bool,
    /// The app's first EXPOSED tool name (`[[tools.exposes]]`), or "" when it
    /// declares none. Manifest metadata only — SECRET-FREE. Drives the App Deck.
    pub tool: String,
}

/// One AGENT-INVOCABLE micro-app tool: an app's non-consequential
/// `[[tools.exposes]]` declaration plus the app identity the dispatcher needs.
/// Produced by [`AppRegistry::agent_tools`]; consumed by the agent loop's def
/// assembly + dispatch (anthropic.rs).
#[derive(Debug, Clone)]
pub struct AgentTool {
    /// The app that owns (and answers) the tool.
    pub app: String,
    /// The app's `[app].description` — the def-description fallback when the
    /// tool declares none of its own.
    pub app_description: String,
    /// The manifest declaration: name, description, params, scopes.
    pub decl: ToolDecl,
}

/// The agent-facing tool name for an exposed micro-app tool: `app__` +
/// the dotted tool id with dots flattened to underscores (Claude tool names
/// admit only `[a-zA-Z0-9_-]`). E.g. `jwtpeek.decode` -> `app__jwtpeek_decode`.
/// NOT injective ("a.b_c" and "a_b.c" collide) — [`AppRegistry::agent_tools`]
/// dedupes, so a collision drops the later declaration instead of misrouting.
pub fn mangled_tool_name(decl_name: &str) -> String {
    format!("app__{}", decl_name.replace('.', "_"))
}

impl AppRegistry {
    /// Scan `apps/` under the project root, parse every `manifest.toml`, and
    /// build the registry. Apps with a malformed/mismatched manifest are
    /// skipped with a WARN (a bad manifest must not stop the daemon) and
    /// surfaced on telemetry so the HUD can show the install error.
    pub fn discover(project_root: &Path) -> Arc<Self> {
        let apps_dir = project_root.join("apps");
        let mut apps = HashMap::new();
        if let Ok(entries) = std::fs::read_dir(&apps_dir) {
            for entry in entries.flatten() {
                let dir = entry.path();
                if !dir.is_dir() {
                    continue;
                }
                if !dir.join("manifest.toml").exists() {
                    continue;
                }
                match AppManifest::load(&dir) {
                    Ok(manifest) => {
                        let name = manifest.name().to_string();
                        // Entry-resolution guard: child_argv resolves [app].entry as a
                        // SINGLE project-root-relative path (never a shell command), so an
                        // entry that resolves OUTSIDE the app's own directory — the legacy
                        // "python3 main.py" command form (-> <root>/python3 main.py) or a
                        // bare binary name like "vision" (-> <root>/vision) — would fail
                        // SILENTLY at spawn. Report it as an invalid manifest and skip
                        // registration instead. STRUCTURAL (build-state independent): a
                        // not-yet-built binary artifact still resolves inside its app dir,
                        // so it registers and launches once built.
                        let entry_abs = abs(project_root, Path::new(&manifest.app.entry));
                        if !entry_abs.starts_with(&dir) {
                            warn!(
                                dir = %dir.display(),
                                entry = %manifest.app.entry,
                                "skipping micro-app: [app].entry resolves outside the app directory"
                            );
                            telemetry::emit(
                                "system",
                                "app.manifest_invalid",
                                json!({
                                    "name": name,
                                    "error": format!(
                                        "[app].entry {:?} must be a project-root-relative path \
                                         inside the app directory (resolved to {})",
                                        manifest.app.entry,
                                        entry_abs.display()
                                    ),
                                }),
                            );
                            continue;
                        }
                        let socket_path = project_root
                            .join("state/ipc/apps")
                            .join(format!("{name}.sock"));
                        let profile_path = project_root
                            .join("state/apps")
                            .join(&name)
                            .join(format!("{name}.sb"));
                        let (op_tx, op_rx) = mpsc::unbounded_channel::<String>();
                        apps.insert(
                            name.clone(),
                            AppEntry {
                                manifest,
                                app_dir: dir,
                                socket_path,
                                profile_path,
                                nonce: String::new(),
                                token: String::new(),
                                running: false,
                                stop_notify: Arc::new(tokio::sync::Notify::new()),
                                op_tx,
                                op_rx: Arc::new(Mutex::new(Some(op_rx))),
                                pending: Arc::new(Mutex::new(HashMap::new())),
                                on_connect_ops: Vec::new(),
                            },
                        );
                        info!(app = name, "micro-app manifest registered");
                    }
                    Err(e) => {
                        warn!(dir = %dir.display(), error = %e, "skipping invalid micro-app manifest");
                        if let Some(dn) = dir.file_name().and_then(|n| n.to_str()) {
                            telemetry::emit(
                                "system",
                                "app.manifest_invalid",
                                json!({"name": dn, "error": e.to_string()}),
                            );
                        }
                    }
                }
            }
        }
        Arc::new(Self {
            project_root: project_root.to_path_buf(),
            apps: Mutex::new(apps),
            #[cfg(test)]
            interpreter_override: None,
        })
    }

    /// The absolute project root this registry resolves app paths against. Used by
    /// callers that must resolve a manifest-relative sandbox dir (e.g. the Share
    /// Guard bridge staging an image under `state/tmp/share-guard/input`).
    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    /// Read-only listing for the router's intent matcher (sorted by name).
    pub async fn list(&self) -> Vec<AppInfo> {
        let apps = self.apps.lock().await;
        let mut out: Vec<AppInfo> = apps
            .values()
            .map(|e| AppInfo {
                name: e.manifest.name().to_string(),
                description: e.manifest.app.description.clone(),
                running: e.running,
                // Live probe (one stat per app per list): a spec-only or
                // unbuilt entry reads not-runnable; building it flips this
                // honestly without a restart.
                entry_present: abs(&self.project_root, Path::new(&e.manifest.app.entry))
                    .is_file(),
                tool: e.manifest.tools.exposes.first().map(|t| t.name.clone()).unwrap_or_default(),
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// The micro-app tools the AGENT LOOP may invoke: every `[[tools.exposes]]`
    /// across the registry that is `consequential = false` AND whose owning app
    /// grants NO network — neither direct (`net_hosts` empty) NOR proxied
    /// (`fetch_hosts` empty). Pure LOCAL compute only:
    ///   * a consequential (side-effecting) declaration is NEVER agent-invocable
    ///     — it can only ride the confirmation-gated paths;
    ///   * a NETWORK-capable app's tools are withheld too, so the def's promise
    ///     to the model ("no network side effects") is TRUE BY CONSTRUCTION and
    ///     model-supplied args can never reach a tool that could egress them.
    ///     This covers BOTH egress paths: an app with direct `net_hosts` AND an
    ///     app that can reach hosts through the daemon-mediated fetch proxy
    ///     (`fetch_hosts`) — either can move model-supplied args onto the wire.
    ///     (Exposing a net-capable app's tool is a future EXPLICIT opt-in, never
    ///     a silent mislabel.)
    ///
    /// Sorted by (app, tool) and DEDUPED by mangled agent-tool name (first wins,
    /// later collisions dropped with a warning) so the def list is deterministic
    /// and never offers two tools the dispatcher can't tell apart.
    pub async fn agent_tools(&self) -> Vec<AgentTool> {
        let apps = self.apps.lock().await;
        let mut out: Vec<AgentTool> = Vec::new();
        let mut names: Vec<&String> = apps.keys().collect();
        names.sort();
        for name in names {
            let entry = &apps[name];
            // The "no network side effects" promise is enforced here, not merely
            // asserted in the def text: a net-capable app is skipped wholesale.
            // BOTH egress surfaces count — direct net_hosts AND the daemon-
            // mediated fetch proxy allow-list (fetch_hosts) — so an app that can
            // reach hosts through the proxy is withheld exactly like a direct-net
            // one, keeping the promise true by construction.
            if !entry.manifest.permissions.net_hosts.is_empty()
                || !entry.manifest.permissions.fetch_hosts.is_empty()
            {
                continue;
            }
            for decl in &entry.manifest.tools.exposes {
                if decl.consequential {
                    continue;
                }
                out.push(AgentTool {
                    app: name.clone(),
                    app_description: entry.manifest.app.description.clone(),
                    decl: decl.clone(),
                });
            }
        }
        let mut seen = std::collections::HashSet::new();
        out.retain(|t| {
            let mangled = mangled_tool_name(&t.decl.name);
            let fresh = seen.insert(mangled.clone());
            if !fresh {
                warn!(app = %t.app, tool = %t.decl.name, "agent tool name collision; dropping later declaration");
            }
            fresh
        });
        out
    }

    /// Resolve a spoken app reference (e.g. "global scan", "globalscan",
    /// "global-scan") to a registered app name. Compares against each app
    /// name with hyphens/whitespace normalized away, so the classifier's
    /// loosely-spaced transcription still matches the manifest name.
    pub async fn resolve_name(&self, spoken: &str) -> Option<String> {
        let want = normalize_app_ref(spoken);
        if want.is_empty() {
            return None;
        }
        let apps = self.apps.lock().await;
        apps.keys()
            .find(|name| normalize_app_ref(name) == want)
            .cloned()
    }

    /// #36 PLUGIN SDK — the register-on-launch HANDSHAKE for a started plugin.
    /// Re-reads the app's on-disk `manifest.toml`, then drives
    /// [`crate::plugin_sdk::register_plugin`] with the app's CURRENT launch token
    /// + nonce against the live session key — proving the manifest's contract
    ///   block ([intents]/[tools]) validates AND the presented token verifies under
    ///   the SAME HMAC machinery the per-app relay uses. Returns the handshake
    ///   outcome; the caller (main.rs autostart, gated by `[plugin_sdk].enabled`)
    ///   emits secret-free telemetry from it. A not-running / unknown app, or a
    ///   manifest that no longer reads, yields `Unauthorized`/`InvalidManifest` —
    ///   fail-closed. This is the LIVE wiring of the #36 handshake; the pure
    ///   `register_plugin` is what the hermetic tests prove.
    pub async fn register_on_launch(&self, name: &str) -> crate::plugin_sdk::HandshakeOutcome {
        let (manifest_path, token, nonce) = {
            let apps = self.apps.lock().await;
            let Some(entry) = apps.get(name) else {
                return crate::plugin_sdk::HandshakeOutcome::Unauthorized;
            };
            (
                entry.app_dir.join("manifest.toml"),
                entry.token.clone(),
                entry.nonce.clone(),
            )
        };
        let Ok(raw) = std::fs::read_to_string(&manifest_path) else {
            return crate::plugin_sdk::HandshakeOutcome::InvalidManifest(format!(
                "could not read {}",
                manifest_path.display()
            ));
        };
        // The plugin presents its manifest + the launch token; the daemon
        // re-validates + verifies against the live session key + this launch nonce.
        crate::plugin_sdk::register_plugin(&raw, name, &token, session_key(), &nonce)
    }

    /// Mint the capability token for an app from the CURRENT session key, the
    /// app's name + permission set, and its current launch nonce. Pure over
    /// the static session key; the unit tests cover the math via
    /// [`compute_token`] directly.
    fn mint_token(&self, entry: &AppEntry) -> String {
        compute_token(
            session_key(),
            entry.manifest.name(),
            &entry.manifest.permissions,
            &entry.nonce,
        )
    }

    /// Verify a token an app presented on its socket, against that app's
    /// CURRENT nonce + permission set. A bad/forged/stale/cross-app token is
    /// rejected. `name` is the app the connection was accepted for.
    ///
    /// `pub(crate)` so the daemon-mediated generate proxy (genproxy.rs) can
    /// reuse the SAME token machinery as the per-app relay — no duplicate
    /// HMAC/nonce logic lives in the proxy.
    pub(crate) async fn verify_token(&self, name: &str, presented: &str) -> bool {
        let apps = self.apps.lock().await;
        let Some(entry) = apps.get(name) else {
            return false;
        };
        // A token presented before launch (empty nonce) is never valid.
        if entry.nonce.is_empty() || entry.token.is_empty() {
            return false;
        }
        verify_token_with_key(
            session_key(),
            entry.manifest.name(),
            &entry.manifest.permissions,
            &entry.nonce,
            presented,
        )
    }

    /// The hostnames a registered app may fetch THROUGH the daemon-mediated fetch
    /// proxy (`fetchproxy.rs`), read from its manifest `[permissions].fetch_hosts`.
    /// The proxy authorizes every URL against THIS list (exact host, case-
    /// insensitive, no subdomain/wildcard). Returns `None` if the app is not
    /// registered; `Some(vec![])` (or an empty list) means the app may fetch
    /// NOTHING — url_not_permitted for every URL.
    ///
    /// `pub(crate)` so the fetch proxy shares the registry's single source of
    /// truth for the allow-list rather than re-reading the manifest.
    pub(crate) async fn fetch_hosts_for(&self, name: &str) -> Option<Vec<String>> {
        let apps = self.apps.lock().await;
        apps.get(name)
            .map(|e| e.manifest.permissions.fetch_hosts.clone())
    }

    /// Test-only: rotate a registered app's nonce and mint+store a VALID token
    /// for it WITHOUT spawning a sandboxed child. Lets the genproxy unit tests
    /// drive the real `verify_token` path (same HMAC/nonce machinery as a live
    /// launch) without `sandbox-exec`. Returns the minted token, or None if the
    /// app is not registered.
    #[cfg(test)]
    pub(crate) async fn mint_for_test(&self, name: &str) -> Option<String> {
        let mut apps = self.apps.lock().await;
        let entry = apps.get_mut(name)?;
        entry.nonce = fresh_nonce();
        let token = compute_token(
            session_key(),
            entry.manifest.name(),
            &entry.manifest.permissions,
            &entry.nonce,
        );
        entry.token = token.clone();
        Some(token)
    }

    /// Resolve the runtime interpreter path for an app's runtime.
    fn interpreter(&self, manifest: &AppManifest) -> PathBuf {
        #[cfg(test)]
        if let Some(over) = &self.interpreter_override {
            if matches!(manifest.app.runtime, Runtime::Python | Runtime::Node) {
                return over.clone();
            }
        }
        match manifest.app.runtime {
            Runtime::Python => self.project_root.join(PYTHON_INTERP_REL),
            Runtime::Node => PathBuf::from("/usr/local/bin/node"),
            // A binary IS its own interpreter — exec the entry directly.
            Runtime::Binary => abs(&self.project_root, Path::new(&manifest.app.entry)),
        }
    }

    /// The argv the sandboxed child runs (after `sandbox-exec -p <profile-string>`).
    /// For python/node it is `<interp> <entry>`; for a binary it is the binary
    /// alone (the entry IS the interpreter).
    fn child_argv(&self, manifest: &AppManifest, interp: &Path) -> Vec<String> {
        // Test seam: with an interpreter override the entry is irrelevant (the
        // overridden interpreter is a stand-in idle process — /bin/sleep — not
        // a real app); give it a long sleep so the child stays alive while the
        // in-process test plays the app role over the socket, then is reaped by
        // kill_on_drop at stop().
        #[cfg(test)]
        if self.interpreter_override.is_some() {
            return vec![interp.to_string_lossy().into_owned(), "120".to_string()];
        }
        match manifest.app.runtime {
            Runtime::Python | Runtime::Node => vec![
                interp.to_string_lossy().into_owned(),
                abs(&self.project_root, Path::new(&manifest.app.entry))
                    .to_string_lossy()
                    .into_owned(),
            ],
            Runtime::Binary => vec![interp.to_string_lossy().into_owned()],
        }
    }

    /// Read-only snapshot for the introspect sentinel (introspect.rs): one
    /// `(name, profile_path, running)` per registered app. Holds the apps lock
    /// only long enough to clone the tuples — it reads, it changes nothing, and
    /// it exposes no new authority (the paths are already derived at discover).
    pub async fn observed_apps(&self) -> Vec<(String, PathBuf, bool)> {
        let apps = self.apps.lock().await;
        apps.iter()
            .map(|(name, e)| (name.clone(), e.profile_path.clone(), e.running))
            .collect()
    }

    /// Read-only DECLARED-capability inventory: one `(name, capability_summary)`
    /// per registered app, derived purely from each manifest's `[permissions]`.
    /// SECRET-FREE (counts, never paths/hosts). Sorted by name for a stable readout.
    pub async fn capability_inventory(&self) -> Vec<(String, String)> {
        let apps = self.apps.lock().await;
        let mut inv: Vec<(String, String)> = apps
            .iter()
            .map(|(name, e)| (name.clone(), capability_summary(&e.manifest.permissions)))
            .collect();
        inv.sort_by(|a, b| a.0.cmp(&b.0));
        inv
    }
}

/// Normalize an app reference for matching: lowercase, strip everything but
/// alphanumerics (so "global scan", "global-scan", "GlobalScan" all collapse
/// to "globalscan").
fn normalize_app_ref(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

// ===========================================================================
// Launch / lifecycle / socket relay
// ===========================================================================

/// Start a micro-app by name: (re)mint its token, regenerate its seatbelt
/// profile, ensure its dirs/socket, and spawn the supervised lifecycle task.
/// Idempotent — starting an already-running app is a no-op that returns Ok.
pub async fn start(registry: &Arc<AppRegistry>, name: &str) -> Result<()> {
    {
        let mut apps = registry.apps.lock().await;
        let entry = apps
            .get_mut(name)
            .ok_or_else(|| anyhow!("no micro-app named {name:?}"))?;
        if entry.running {
            info!(app = name, "micro-app already running");
            return Ok(());
        }
        // HONEST-LABELING GUARD: a spec-only app (manifest + SPEC.md, no code)
        // or an unbuilt compiled app registers deliberately (visible in the
        // deck) but must refuse to START with a clear reason — not flip
        // `running`, spawn, and die in the lifecycle with a confusing exec
        // error. Skipped under the test interpreter override, where the entry
        // is a stand-in played in-process and need not exist on disk.
        #[cfg(test)]
        let probe_entry = registry.interpreter_override.is_none();
        #[cfg(not(test))]
        let probe_entry = true;
        if probe_entry {
            let entry_abs = abs(&registry.project_root, Path::new(&entry.manifest.app.entry));
            if !entry_abs.is_file() {
                return Err(anyhow!(
                    "micro-app {name:?} isn't runnable yet — its entry {:?} does not exist (spec-only, or not built)",
                    entry.manifest.app.entry
                ));
            }
        }
        // Rotate the nonce + mint a fresh token for this launch.
        entry.nonce = fresh_nonce();
        entry.running = true;
    }
    // Mint after dropping the borrow conflict (mint_token borrows &entry).
    {
        let token = {
            let apps = registry.apps.lock().await;
            let entry = apps.get(name).expect("entry exists; just inserted");
            registry.mint_token(entry)
        };
        let mut apps = registry.apps.lock().await;
        if let Some(entry) = apps.get_mut(name) {
            entry.token = token;
        }
    }

    let reg = registry.clone();
    let name = name.to_string();
    tokio::spawn(async move {
        lifecycle(reg, name).await;
    });
    Ok(())
}

/// Stop a running micro-app: flip its running flag and WAKE the lifecycle task
/// (the notify) so it tears down immediately — kills the child via
/// kill_on_drop, removes the socket — instead of waiting for the child to exit
/// on its own.
pub async fn stop(registry: &Arc<AppRegistry>, name: &str) -> Result<()> {
    let notify = {
        let mut apps = registry.apps.lock().await;
        let entry = apps
            .get_mut(name)
            .ok_or_else(|| anyhow!("no micro-app named {name:?}"))?;
        if !entry.running {
            return Ok(());
        }
        entry.running = false;
        // Invalidate the token immediately so any in-flight line is dropped.
        entry.token.clear();
        entry.nonce.clear();
        entry.stop_notify.clone()
    };
    // Fail in-flight request_op waiters NOW — the token is dead, no answer can
    // ever verify, so waiting out the timeout would just be dishonest latency.
    fail_pending(registry, name).await;
    // Wake the lifecycle task out of its blocking select!.
    notify.notify_waiters();
    Ok(())
}

/// HOST -> APP: forward one already-structured op line to a RUNNING micro-app.
///
/// This is the op-forwarding seam the voice router uses to drive an app after
/// it is launched (e.g. `{"op":"select.net","name":"3V3"}` for Silicon Canvas).
/// `op_line` is the COMPLETE JSON op object as a single line (no trailing
/// newline needed — this adds it); the daemon forwards it VERBATIM and never
/// interprets it, so the contract for what the op means lives entirely in the
/// target app (Silicon Canvas's `src/ops.rs`). The router is responsible for
/// classifying the spoken utterance into the structured op string; the app
/// never parses natural language (SPEC §6).
///
/// Errors when the app is unknown or not running; the line is dropped (never
/// queued for a future launch) so a stale op cannot fire on the next start.
/// Delivery is best-effort once queued: the live connection handler drains the
/// queue and writes the line; a line queued between connections rides the next
/// accepted connection. The op is NOT token-stamped — host->app lines are
/// authenticated by the socket itself (the daemon owns and bound it, 0600), the
/// same trust model the start/refresh/stop control verbs already rely on; the
/// per-app capability token authenticates the REVERSE direction (app->host).
/// Ops that START a CAPTURE — a camera or the screen. Refused while the emergency
/// stop is engaged.
///
/// WHAT WENT WRONG: `lockdown::panic()` stops outward actions, autonomy, parked
/// confirmations, background music, and the MICROPHONE — the mic because audio.rs
/// re-reads the flag per chunk. Capture is a different shape: the Vision app is a
/// SEPARATE PROCESS, so nothing it is told to do is re-checked against the flag,
/// and no gate stood between a capture-start op and the app. `screen_context.rs`
/// and `aperture.rs` contain ZERO `is_locked_down` consults.
///
/// So a user could say "panic", watch the mic go silent and the HUD light turn
/// red, and a subsequent capture-start would still open the lens.
///
/// The check lives HERE, at the one dispatch every op passes through, rather than
/// at each caller — a per-caller gate is exactly the "one gate knew the rule, its
/// twin did not" shape that produced this campaign's other lockdown defect.
fn is_capture_start(op_line: &str) -> bool {
    // Match on the op NAME, not a substring of the whole line: a `read.screen`
    // QUERY carries arbitrary user text and must not be caught by it.
    let op = serde_json::from_str::<Value>(op_line)
        .ok()
        .and_then(|v| v.get("op").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default();
    matches!(op.as_str(), "watch.start" | "screen.context.start" | "describe.capture")
}

/// Stop every capture in flight. Called by the emergency stop.
///
/// `send_op`'s lockdown gate stops a capture from STARTING. This ends one already
/// running — the Vision app is a separate process, so a capture begun before the
/// panic keeps going until it is told otherwise, and nothing was telling it.
///
/// The stops are sent DIRECTLY on the op queue rather than through `send_op`:
/// routing them through the gated path would be fine today (the gate only refuses
/// STARTS), but it would make ending a capture depend on the same predicate that
/// refuses one — and a future edit to that predicate could silently strand a live
/// capture the emergency stop exists to end.
///
/// Best-effort and never fatal: an app that is not running, or whose queue is
/// closed, is simply skipped. A panic must not fail because a lens was already
/// shut.
pub async fn stop_all_captures(registry: &Arc<AppRegistry>) {
    const STOPS: &[&str] = &[r#"{"op":"watch.stop"}"#, r#"{"op":"screen.context.stop"}"#];
    let apps = registry.apps.lock().await;
    let Some(entry) = apps.get("vision") else {
        return;
    };
    if !entry.running {
        return;
    }
    for op in STOPS {
        if entry.op_tx.send((*op).to_string()).is_err() {
            warn!("lockdown: could not queue a capture stop; the app queue is closed");
            return;
        }
    }
    info!("lockdown: sent capture stops to the vision app");
}

pub async fn send_op(registry: &Arc<AppRegistry>, name: &str, op_line: &str) -> Result<()> {
    // EMERGENCY STOP: never open a lens while locked down. Restrict-only — with
    // lockdown off (the shipped default) this is byte-for-byte the old path.
    if crate::lockdown::is_locked_down() && is_capture_start(op_line) {
        bail!("lockdown is engaged; refusing to start a capture");
    }
    let apps = registry.apps.lock().await;
    let entry = apps
        .get(name)
        .ok_or_else(|| anyhow!("no micro-app named {name:?}"))?;
    if !entry.running {
        bail!("micro-app {name:?} is not running; cannot forward op");
    }
    entry
        .op_tx
        .send(op_line.to_string())
        .map_err(|_| anyhow!("micro-app {name:?} op queue is closed"))?;
    Ok(())
}

/// Monotonic request-id source for [`request_op`]. Predictability is fine: the
/// only peer on the app socket is the daemon-launched sandboxed child, already
/// authenticated by socket ownership (host->app) and the HMAC token (app->host);
/// the id exists for CORRELATION, not authentication.
static REQ_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// HOST -> APP -> HOST: send one structured op to a RUNNING micro-app and await
/// its correlated answer — the REQUEST/RESPONSE sibling of the fire-and-forget
/// [`send_op`]. This is the seam that lets the agent tool loop invoke a
/// micro-app's exposed tool and hand the app's ACTUAL result back to the model
/// (send_op can only report "queued").
///
/// Wire contract: the daemon injects `"id":"req-<n>"` into `op` (an object) and
/// forwards it like any op line. A TOOL-INVOCABLE app answers the op with a
/// token-stamped `{"type":"result","id":<same id>,"data":<result>}` line; the
/// relay routes that `data` here by id. Apps that predate the contract (no id
/// echo) simply time out — an honest failure, never a mis-correlated answer.
///
/// Failure semantics (all HONEST, none silent):
///   * app unknown / not running / queue closed -> immediate Err (from send_op's
///     checks — the request is never queued for a future launch);
///   * `timeout` elapses -> Err AND the waiter is evicted, so a late result is
///     dropped by the relay's stale-id path instead of leaking to a later call;
///   * the app stops/crashes/fails to launch while we wait -> the teardown path
///     drains the pending map, our oneshot sender drops, and this returns a
///     fast "app went away" Err instead of dangling until timeout;
///   * more than [`MAX_PENDING_REQUESTS`] already in flight -> immediate Err.
pub async fn request_op(
    registry: &Arc<AppRegistry>,
    name: &str,
    mut op: Value,
    timeout: Duration,
) -> Result<Value> {
    let obj = op
        .as_object_mut()
        .ok_or_else(|| anyhow!("request_op needs a JSON object op"))?;
    let id = format!(
        "req-{}",
        REQ_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    obj.insert("id".to_string(), Value::String(id.clone()));
    let op_line = serde_json::to_string(&op)?;

    // Register the waiter and queue the op under ONE registry lock scope so a
    // teardown can never slip between "queued" and "registered" and leave a
    // waiter no terminal path knows about.
    let (rx, pending) = {
        let apps = registry.apps.lock().await;
        let entry = apps
            .get(name)
            .ok_or_else(|| anyhow!("no micro-app named {name:?}"))?;
        if !entry.running {
            bail!("micro-app {name:?} is not running; cannot request");
        }
        let mut pending = entry.pending.lock().await;
        if pending.len() >= MAX_PENDING_REQUESTS {
            bail!("micro-app {name:?} has too many requests in flight");
        }
        let (tx, rx) = tokio::sync::oneshot::channel::<Value>();
        pending.insert(id.clone(), tx);
        drop(pending);
        entry
            .op_tx
            .send(op_line)
            .map_err(|_| anyhow!("micro-app {name:?} op queue is closed"))?;
        (rx, entry.pending.clone())
    };

    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(data)) => Ok(data),
        // Sender dropped: a terminal path drained the pending map.
        Ok(Err(_)) => bail!("micro-app {name:?} went away before answering"),
        Err(_) => {
            // Evict the waiter so a LATE result is dropped as stale instead of
            // being delivered to nobody (or worse, kept forever).
            pending.lock().await.remove(&id);
            bail!(
                "micro-app {name:?} did not answer within {}s",
                timeout.as_secs()
            )
        }
    }
}

/// Drain every in-flight [`request_op`] waiter for `name`, dropping the senders
/// so the waiters resolve to an honest "app went away" error immediately.
/// Called from every app-terminal path (stop, crash give-up, launch failure,
/// restart) — after a restart the token has rotated and any in-flight answer
/// would fail verification anyway, so failing fast is strictly more honest
/// than letting waiters dangle until timeout.
async fn fail_pending(registry: &Arc<AppRegistry>, name: &str) {
    let pending = {
        let apps = registry.apps.lock().await;
        match apps.get(name) {
            Some(entry) => entry.pending.clone(),
            None => return,
        }
    };
    let drained: usize = {
        let mut map = pending.lock().await;
        let n = map.len();
        map.clear();
        n
    };
    if drained > 0 {
        warn!(app = name, drained, "failed in-flight app requests on teardown");
    }
}

/// One app's supervised lifecycle: bind its socket, spawn the sandboxed child,
/// relay its JSONL onto telemetry, and restart on exit within the governor's
/// budget. Returns when the app is stopped or has exhausted its restarts.
async fn lifecycle(registry: Arc<AppRegistry>, name: String) {
    let mut governor = RestartGovernor::new();

    // The stop notifier for this app, cloned once: stop() fires it to wake the
    // blocking select! below.
    let stop_notify = {
        let apps = registry.apps.lock().await;
        match apps.get(&name) {
            Some(entry) => entry.stop_notify.clone(),
            None => return,
        }
    };

    // Prepare paths + the seatbelt profile once (regenerated on each loop pass
    // so a manifest edit between restarts is picked up).
    loop {
        // Read the snapshot needed to launch under a short lock.
        let (manifest, app_dir, socket_path, profile_path, token) = {
            let apps = registry.apps.lock().await;
            let Some(entry) = apps.get(&name) else {
                return;
            };
            if !entry.running {
                cleanup_socket(&entry.socket_path);
                telemetry::emit("system", "app.stopped", json!({"name": name}));
                return;
            }
            (
                entry.manifest.clone(),
                entry.app_dir.clone(),
                entry.socket_path.clone(),
                entry.profile_path.clone(),
                entry.token.clone(),
            )
        };

        match run_once(
            &registry,
            &name,
            &manifest,
            &app_dir,
            &socket_path,
            &profile_path,
            &token,
            &stop_notify,
        )
        .await
        {
            RunResult::StoppedByHost => {
                fail_pending(&registry, &name).await;
                cleanup_socket(&socket_path);
                telemetry::emit("system", "app.stopped", json!({"name": name}));
                return;
            }
            RunResult::ChildExited => {
                // The child died: any in-flight request is unanswerable (an op
                // already written died with it; a late answer fails the rotated
                // token). Fail waiters fast on EVERY exit path, restart or not.
                fail_pending(&registry, &name).await;
                let now = Instant::now();
                if governor.should_restart(now) {
                    governor.record_restart(now);
                    warn!(app = %name, restart = governor.count(now), "micro-app exited; restarting");
                    // Rotate the nonce + re-mint the token for the new launch.
                    let mut apps = registry.apps.lock().await;
                    if let Some(entry) = apps.get_mut(&name) {
                        if !entry.running {
                            // Stopped while we were deciding to restart.
                            drop(apps);
                            cleanup_socket(&socket_path);
                            telemetry::emit("system", "app.stopped", json!({"name": name}));
                            return;
                        }
                        entry.nonce = fresh_nonce();
                        entry.token = registry.mint_token(entry);
                    }
                    continue;
                } else {
                    let restarts = governor.count(now);
                    error!(app = %name, restarts, "micro-app crashed too often; giving up");
                    {
                        let mut apps = registry.apps.lock().await;
                        if let Some(entry) = apps.get_mut(&name) {
                            entry.running = false;
                            entry.token.clear();
                            entry.nonce.clear();
                        }
                    }
                    cleanup_socket(&socket_path);
                    telemetry::emit(
                        "system",
                        "app.crashed",
                        json!({"name": name, "restarts": restarts}),
                    );
                    return;
                }
            }
            RunResult::LaunchFailed(e) => {
                fail_pending(&registry, &name).await;
                error!(app = %name, error = %e, "micro-app launch failed");
                {
                    let mut apps = registry.apps.lock().await;
                    if let Some(entry) = apps.get_mut(&name) {
                        entry.running = false;
                        entry.token.clear();
                        entry.nonce.clear();
                    }
                }
                cleanup_socket(&socket_path);
                telemetry::emit(
                    "system",
                    "app.crashed",
                    json!({"name": name, "restarts": 0, "error": e.to_string()}),
                );
                return;
            }
        }
    }
}

enum RunResult {
    /// The host flipped running=false; tear down cleanly.
    StoppedByHost,
    /// The child process exited on its own; the governor decides on restart.
    ChildExited,
    /// Could not even launch (profile write / bind / spawn failed).
    LaunchFailed(anyhow::Error),
}

/// One launch: write the profile, bind the socket, spawn the sandboxed child,
/// accept its connection, verify+relay its JSONL until it exits or the host
/// stops it. The child is held with kill_on_drop so every early return reaps
/// it (actions.rs discipline).
#[allow(clippy::too_many_arguments)]
async fn run_once(
    registry: &Arc<AppRegistry>,
    name: &str,
    manifest: &AppManifest,
    app_dir: &Path,
    socket_path: &Path,
    profile_path: &Path,
    token: &str,
    stop_notify: &Arc<tokio::sync::Notify>,
) -> RunResult {
    // SUBSTRATE LOCK (envlock.rs) spawn gate. Armed-by-default ([envlock].enabled).
    // If this app is PINNED (has apps/<name>/env.lock), re-hash its materialized
    // closure under state/envstore/<hash>/ and verify it against the lock
    // FAIL-CLOSED: a mismatch REFUSES to spawn (never a silent fall-back to the
    // shared .venv). On a verified pin the interpreter is the one INSIDE the pinned
    // closure, so generate_sbpl narrows exec/read to that closure instead of the
    // shared .venv. An UNPINNED app (no env.lock — every app that ships today)
    // resolves to the legacy interpreter unchanged.
    let legacy_interp = registry.interpreter(manifest);
    let interp = if crate::envlock::verify_enabled() {
        let pin = crate::envlock::pin_state(&registry.project_root, app_dir);
        crate::envlock::emit_verdict(name, &pin);
        if let crate::envlock::PinState::Pinned {
            verdict: crate::envlock::SpawnVerdict::Refused { reason, .. },
            ..
        } = &pin
        {
            return RunResult::LaunchFailed(anyhow!(
                "envlock: refusing to spawn {name}: pinned dependency closure failed verification ({})",
                reason.as_str()
            ));
        }
        crate::envlock::effective_interpreter(&pin, &legacy_interp, manifest.app.runtime)
    } else {
        legacy_interp
    };

    // The HOST -> APP op queue handle for this app (shared across reconnects):
    // handle_conn moves the receiver out for the life of a connection and puts
    // it back on exit, so a line queued between connections is not lost.
    let op_rx = {
        let apps = registry.apps.lock().await;
        match apps.get(name) {
            Some(entry) => entry.op_rx.clone(),
            None => return RunResult::StoppedByHost,
        }
    };

    // Generate the seatbelt profile (also writes the on-disk AUDIT copy). The
    // returned string is the EXEC source, passed inline to `sandbox-exec -p`
    // below so no on-disk file is re-read at exec time (closes the write->exec
    // TOCTOU — a same-UID swap of the audit copy can't alter the running sandbox).
    let profile = match write_profile(manifest, &registry.project_root, &interp, app_dir, socket_path, profile_path) {
        Ok(p) => p,
        Err(e) => return RunResult::LaunchFailed(e),
    };
    // Ensure the fs_write dirs exist (the app's own state dir) so first write
    // does not fail inside the sandbox.
    ensure_write_dirs(&registry.project_root, manifest);

    // Bind the per-app socket (host owns it). Remove any stale one first.
    cleanup_socket(socket_path);
    if let Some(parent) = socket_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return RunResult::LaunchFailed(anyhow!("creating socket dir: {e}"));
        }
        // Tighten the socket DIR to 0700: only the daemon's UID may even
        // traverse into it. Same-UID is the trust boundary either way, but this
        // removes the casual cross-process connect a 0755 dir would permit and
        // matches SANDBOX.md's "the daemon creates and owns the socket" claim.
        restrict_dir_perms(parent);
    }
    let listener = match UnixListener::bind(socket_path) {
        Ok(l) => l,
        Err(e) => return RunResult::LaunchFailed(anyhow!("binding {}: {e}", socket_path.display())),
    };
    // Tighten the socket itself to 0600: defense-in-depth so an unrelated
    // same-UID process cannot connect() and read the host's start/refresh/stop
    // command stream or wedge the accept/reconnect path (a local DoS). Token
    // verification already blocks INJECTION (a connector can't forge the
    // per-launch HMAC), but 0600 closes the casual-connect leak. This does not
    // stop a same-UID attacker who can chmod — that is outside the trust model.
    restrict_socket_perms(socket_path);

    // Spawn the sandboxed child: sandbox-exec -p <profile-string> <interp> <entry...>.
    // The profile is passed INLINE (not `-f <file>`) so the compiled policy is the
    // daemon's in-memory string — a same-UID edit of the on-disk audit copy cannot
    // widen the running sandbox (no file is re-read at exec time). The SBPL names
    // paths only (no secret), so it is safe in argv.
    let argv = registry.child_argv(manifest, &interp);
    let mut cmd = Command::new(SANDBOX_EXEC);
    cmd.arg("-p").arg(&profile);
    for a in &argv {
        cmd.arg(a);
    }
    // SECURITY: clear the INHERITED environment so no daemon secret crosses into a
    // sandboxed micro-app. The SBPL profile filters files/mach/network — NOT env
    // vars — so an inherited ANTHROPIC_API_KEY / ELEVENLABS_API_KEY / HF_TOKEN would
    // sail past the default-deny sandbox and be readable by a malicious app via
    // getenv(). We re-add ONLY a minimal, non-secret allowlist (mirrors shell.rs's
    // sandboxed-shell spawn, which already env_clear()s). The app learns its socket +
    // token from the env ONLY — never argv (argv is world-readable via ps). The
    // session key never appears here, only the derived token.
    cmd.env_clear();
    cmd.env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
    cmd.env("HOME", &registry.project_root);
    // Forward the vision app's non-secret capability DECLARATIONS if the operator set
    // them in the daemon env — these grant nothing (macOS TCC is the real gate).
    for var in ["DARWIN_VISION_CAMERA", "DARWIN_VISION_SCREEN"] {
        if let Ok(v) = std::env::var(var) {
            cmd.env(var, v);
        }
    }
    // THE CALLER'S BUDGET, so an app cannot wait longer than the daemon will listen.
    // The eight LLM-backed apps each hard-coded a 30 s wait on the generate proxy while
    // APP_REQUEST_TIMEOUT is 15 s — the ordering was INVERTED, so under a busy
    // inference server the daemon gave up first, discarded a reply that was still
    // coming, and reported "the app did not answer" for a tool call that worked.
    // Deriving the app's deadline from this one makes the ordering unbreakable.
    cmd.env(
        "DARWIN_APP_DEADLINE_MS",
        APP_REQUEST_TIMEOUT.as_millis().to_string(),
    );
    cmd.env("DARWIN_APP_TOKEN", token);
    cmd.env("DARWIN_APP_SOCKET", abs(&registry.project_root, socket_path));
    cmd.env("DARWIN_APP_NAME", name);
    cmd.current_dir(&registry.project_root);
    cmd.kill_on_drop(true);
    // Capture stdout/stderr so app logs become telemetry instead of polluting
    // the daemon's own stdio.
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child: Child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return RunResult::LaunchFailed(anyhow!("spawning sandbox-exec: {e}")),
    };
    info!(app = name, "micro-app launched under sandbox-exec");
    telemetry::emit("system", "app.started", json!({"name": name}));
    // Record the child pid for the introspect sentinel to sample (read-only).
    // The guard clears it on EVERY return path from here (StoppedByHost,
    // ChildExited, or any early error), so a dead/reused pid is never sampled —
    // same kill_on_drop discipline that reaps `child` itself.
    let _pid_guard = crate::introspect::record_child(name, child.id());
    // Fresh trust anchor per launch: drop any prior dyld module baseline so this
    // launch's first `modules` report re-seeds (trust-on-first-use). A legitimately
    // updated app loads a different module set; persisting the old baseline across
    // the relaunch would false-flag every changed module as an injection.
    crate::introspect::reset_module_baseline(name);
    // Record the app's declared jit bit so the (feature-gated) ES front-end can
    // tell an EXPECTED executable mapping (jit=true) from a W^X violation.
    crate::introspect::record_app_jit(name, manifest.permissions.jit);

    // Relay the child's stderr/stdout as app.log lines.
    if let Some(out) = child.stdout.take() {
        spawn_log_relay(name.to_string(), out);
    }
    if let Some(err) = child.stderr.take() {
        spawn_log_relay(name.to_string(), err);
    }

    // Accept the app's connection (bounded — a sandboxed app that never
    // connects must not hang the supervisor forever; we still watch the child
    // and the stop flag concurrently).
    let topic = default_topic(manifest);

    loop {
        tokio::select! {
            // The host asked us to stop — tear down now (child reaped by
            // kill_on_drop when this fn returns and `child` drops).
            _ = stop_notify.notified() => {
                info!(app = name, "stop requested; tearing down micro-app");
                return RunResult::StoppedByHost;
            }
            // The child exited on its own.
            status = child.wait() => {
                match status {
                    Ok(s) => info!(app = name, code = s.code(), "micro-app process exited"),
                    Err(e) => warn!(app = name, error = %e, "waiting on micro-app failed"),
                }
                return RunResult::ChildExited;
            }
            // A new connection from the app.
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _peer)) => {
                        // Serve this connection until it closes, the child
                        // exits, or the host stops the app. handle_conn returns
                        // the reason so the outer loop reacts correctly.
                        match handle_conn(registry, name, &topic, manifest, stream, &mut child, stop_notify, &op_rx).await {
                            ConnEnd::HostStopped => return RunResult::StoppedByHost,
                            ConnEnd::ChildExited => return RunResult::ChildExited,
                            // The connection dropped but the child is alive and
                            // the host still wants it: loop to accept a
                            // reconnect (the app may reconnect after a hiccup).
                            ConnEnd::ConnClosed => continue,
                        }
                    }
                    Err(e) => {
                        warn!(app = name, error = %e, "accept on app socket failed");
                        // Fall through to re-check the child / stop flag.
                        if !host_wants_running(registry, name).await {
                            return RunResult::StoppedByHost;
                        }
                    }
                }
            }
        }
    }
}

enum ConnEnd {
    HostStopped,
    ChildExited,
    ConnClosed,
}

/// Serve one accepted app connection: send the initial `start` command, then
/// read JSONL lines. Every inbound line's token is VERIFIED against the app's
/// current nonce+perms; a bad/missing token drops the line and emits
/// app.auth_failed. Accepted items/status lines relay onto telemetry as
/// app.data; log lines as app.log.
#[allow(clippy::too_many_arguments)]
async fn handle_conn(
    registry: &Arc<AppRegistry>,
    name: &str,
    topic: &str,
    manifest: &AppManifest,
    stream: UnixStream,
    child: &mut Child,
    stop_notify: &Arc<tokio::sync::Notify>,
    op_rx: &Arc<Mutex<Option<mpsc::UnboundedReceiver<String>>>>,
) -> ConnEnd {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    // Host -> app: kick it off.
    let _ = send_command(&mut write_half, "start").await;

    // ...then RE-ARM anything whose lifetime follows this connection rather than
    // the daemon's. Sent on every connect, so a feature stays armed across an app
    // restart and — the case this exists for — becomes armed the FIRST time the
    // user opens the app, long after the daemon booted.
    let on_connect: Vec<String> = {
        let apps = registry.apps.lock().await;
        apps.get(name).map(|e| e.on_connect_ops.clone()).unwrap_or_default()
    };
    for op_line in &on_connect {
        if send_op_line(&mut write_half, op_line).await.is_err() {
            warn!(app = name, "failed to re-arm an on-connect op");
            break;
        }
        debug!(app = name, "re-armed an on-connect op");
    }

    // Take the op receiver for the life of THIS connection. None should never
    // happen (run_once serves one connection at a time per app), but if it
    // does we still serve the connection without op forwarding rather than
    // panicking. The receiver is put back below on every exit path so a
    // reconnect resumes draining the same queue.
    let mut op_rx_guard = op_rx.lock().await.take();

    let end = serve_conn(
        registry,
        name,
        topic,
        manifest,
        &mut reader,
        &mut write_half,
        child,
        stop_notify,
        op_rx_guard.as_mut(),
    )
    .await;

    // Return the receiver so the next connection (or send_op between
    // connections, via the still-live op_tx) keeps the same queue.
    if let Some(rx) = op_rx_guard {
        *op_rx.lock().await = Some(rx);
    }
    end
}

/// Read one newline-terminated line into `line`, buffering AT MOST `max` bytes.
/// The stdlib/tokio `read_line` grows the target String without limit until it
/// sees a newline, so a malicious/compromised micro-app that sends a huge line
/// with no `\n` would OOM the daemon BEFORE any post-hoc `line.len()` check could
/// run. This caps the buffer as it fills: `Ok(0)` on EOF, the byte count on a
/// complete line, or an `InvalidData` error once `max` bytes arrive without a
/// newline (the caller then drops the connection — a well-behaved app never sends
/// a line anywhere near `MAX_APP_LINE_BYTES`).
///
/// CANCELLATION SAFETY: this future is used in a `tokio::select!` arm, so it can
/// be DROPPED mid-read whenever another arm (a queued host->app op, a stop, a
/// child exit) wins. The accumulator therefore lives in the CALLER's `pending`
/// buffer, NOT a local one: bytes already pulled off the reader survive the drop
/// and the next call resumes exactly where it left off. The single `.await` is
/// `fill_buf`, which is itself cancellation-safe (it consumes nothing until we
/// call `consume`), so no byte is ever read-and-lost. `pending` is cleared only
/// when a complete line (or EOF/oversize) is returned.
///
/// `pub(crate)` so the generate proxy (`genproxy.rs`), which reads the SAME
/// untrusted-micro-app socket line protocol, shares this one audited bounded
/// reader instead of an unbounded `read_line` that a hostile app could OOM.
pub(crate) async fn read_line_bounded(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    pending: &mut Vec<u8>,
    line: &mut String,
    max: usize,
) -> std::io::Result<usize> {
    use tokio::io::AsyncBufReadExt;
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            if pending.is_empty() {
                return Ok(0); // clean EOF
            }
            *line = String::from_utf8_lossy(pending).into_owned();
            let n = pending.len();
            pending.clear();
            return Ok(n); // trailing line without newline at EOF
        }
        if let Some(i) = chunk.iter().position(|&b| b == b'\n') {
            // The newline branch is subject to the SAME cap: a line whose bytes up to
            // the newline (already-buffered `pending` + `i` in this chunk) exceed
            // `max` is rejected EXACTLY, not returned. Without this, a line that ends
            // in a newline within one fill_buf chunk could overshoot the cap by up to
            // a buffer's worth — tightened because this reader is shared with the
            // pre-auth generate proxy.
            if pending.len().saturating_add(i) > max {
                reader.consume(i + 1);
                pending.clear();
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "app line exceeds MAX_APP_LINE_BYTES",
                ));
            }
            let take = i + 1;
            pending.extend_from_slice(&chunk[..take]);
            reader.consume(take);
            *line = String::from_utf8_lossy(pending).into_owned();
            let n = pending.len();
            pending.clear();
            return Ok(n);
        }
        let take = chunk.len();
        pending.extend_from_slice(chunk);
        reader.consume(take);
        if pending.len() > max {
            pending.clear(); // drop the oversized accumulation; caller closes the conn
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "app line exceeds MAX_APP_LINE_BYTES with no newline",
            ));
        }
    }
}

/// The connection service loop, factored out so [`handle_conn`] can put the op
/// receiver back on every exit path without repeating it at each `return`.
#[allow(clippy::too_many_arguments)]
async fn serve_conn(
    registry: &Arc<AppRegistry>,
    name: &str,
    topic: &str,
    manifest: &AppManifest,
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    child: &mut Child,
    stop_notify: &Arc<tokio::sync::Notify>,
    mut op_rx: Option<&mut mpsc::UnboundedReceiver<String>>,
) -> ConnEnd {
    let mut line = String::new();
    // Persists ACROSS loop iterations so a partial line survives a select!
    // cancellation (a queued op firing mid-read) — see read_line_bounded's
    // cancellation-safety contract.
    let mut pending: Vec<u8> = Vec::new();
    loop {
        line.clear();
        // A future that resolves to the next queued op line, or never resolves
        // when there is no receiver — so the select! arm is simply inert in
        // that case rather than spinning.
        let next_op = async {
            match op_rx.as_mut() {
                Some(rx) => rx.recv().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            // Host stop: wake out of the blocking read so a quiet, idling app
            // is torn down immediately rather than at its next line / exit.
            _ = stop_notify.notified() => {
                info!(app = name, "stop requested mid-connection; tearing down");
                return ConnEnd::HostStopped;
            }
            status = child.wait() => {
                match status {
                    Ok(s) => info!(app = name, code = s.code(), "micro-app process exited"),
                    Err(e) => warn!(app = name, error = %e, "waiting on micro-app failed"),
                }
                return ConnEnd::ChildExited;
            }
            // HOST -> APP: a structured op line the router queued via send_op.
            // Forward it VERBATIM (the daemon never interprets the op body) on
            // the same socket as the control verbs. A write failure means the
            // connection is gone; loop will pick up the close/exit next.
            op = next_op => {
                // The sender is dropped only when the registry is torn down;
                // treat as nothing more to forward (do not exit the conn).
                if let Some(op_line) = op {
                    if let Err(e) = send_op_line(write_half, &op_line).await {
                        warn!(app = name, error = %e, "forwarding op to app failed");
                    }
                }
            }
            read = read_line_bounded(reader, &mut pending, &mut line, MAX_APP_LINE_BYTES) => {
                match read {
                    Ok(0) => return ConnEnd::ConnClosed, // app closed the socket
                    Ok(_) => {
                        if line.len() > MAX_APP_LINE_BYTES {
                            warn!(app = name, len = line.len(), "oversized line from app; dropping");
                            continue;
                        }
                        if !host_wants_running(registry, name).await {
                            return ConnEnd::HostStopped;
                        }
                        relay_line(registry, name, topic, manifest, line.trim()).await;
                    }
                    Err(e) => {
                        warn!(app = name, error = %e, "reading app socket failed");
                        return ConnEnd::ConnClosed;
                    }
                }
            }
        }
    }
}

/// What an authenticated App->host line resolves to, decided purely so it can
/// be unit-tested without telemetry side effects.
#[derive(Debug, PartialEq)]
enum RelayDecision {
    /// items/status: relay as app.data on this topic with this payload.
    Data { topic: String, payload: Value },
    /// log: relay as app.log with this line.
    Log { line: String },
    /// modules: an app's in-proc dyld loaded-module report — attested against a
    /// trust-on-first-use baseline in introspect.rs (defensive, observability-only).
    Modules { modules: Vec<crate::introspect::Module> },
    /// result: the correlated answer to a [`request_op`] — routed to the waiting
    /// oneshot by id, NOT relayed to telemetry (the payload goes to the
    /// requester; telemetry gets only a secret-free breadcrumb). A result line
    /// with a missing/empty/non-string id is malformed -> Drop.
    ToolResult { id: String, payload: Value },
    /// Malformed JSON, an unknown message type, or an empty line — drop it.
    Drop,
}

/// PURE classification of an already-token-verified line. The token check lives
/// in [`relay_line`] (it needs the async registry); everything after it —
/// JSON parse, type dispatch, topic resolution — is decided here so the unit
/// tests can prove an app cannot publish to an undeclared topic and that junk
/// is dropped, with no socket and no telemetry.
fn classify_inbound_line(manifest: &AppManifest, default_topic: &str, raw: &str) -> RelayDecision {
    if raw.trim().is_empty() {
        return RelayDecision::Drop;
    }
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return RelayDecision::Drop;
    };
    let msg_type = value.get("type").and_then(Value::as_str).unwrap_or("");
    let data = value.get("data").cloned().unwrap_or(Value::Null);
    match msg_type {
        "items" | "status" => RelayDecision::Data {
            topic: resolve_topic(manifest, default_topic, &data),
            payload: data,
        },
        "log" => {
            // Apps ship logs as data={"line":str} per the app contract; accept
            // that first, then a bare string, then any other JSON as-is.
            let line = data
                .get("line")
                .and_then(Value::as_str)
                .or_else(|| data.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| data.to_string());
            RelayDecision::Log { line }
        }
        "modules" => RelayDecision::Modules {
            modules: crate::introspect::parse_module_report(&data),
        },
        "result" => match value.get("id").and_then(Value::as_str) {
            Some(id) if !id.is_empty() => RelayDecision::ToolResult {
                id: id.to_string(),
                payload: data,
            },
            _ => RelayDecision::Drop,
        },
        _ => RelayDecision::Drop,
    }
}

/// Parse, authenticate, and relay one App->host JSONL line.
///   {"token":str,"type":"items"|"status"|"log","data":obj}
async fn relay_line(
    registry: &Arc<AppRegistry>,
    name: &str,
    topic: &str,
    manifest: &AppManifest,
    raw: &str,
) {
    if raw.is_empty() {
        return;
    }
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        warn!(app = name, "dropping non-JSON line from app");
        return;
    };
    // Token check FIRST — a line without a valid token never reaches relay.
    let presented = value.get("token").and_then(Value::as_str).unwrap_or("");
    if !registry.verify_token(name, presented).await {
        warn!(app = name, "app line failed token verification; dropping");
        telemetry::emit("system", "app.auth_failed", json!({"name": name}));
        return;
    }
    match classify_inbound_line(manifest, topic, raw) {
        RelayDecision::Data { topic, payload } => {
            // CONTINUOUS SCREEN CONTEXT (#42): a vision.screen readout tagged
            // `read_kind=context` is a snapshot from the Vision app's DEVICE-gated
            // continuous capture loop — route its recognized text into the daemon's
            // bounded/redacted/transient context ring (the redaction + bounding
            // happen inside `ingest_continuous_snapshot`, which is itself GATED on
            // [screen_context].enabled — ships ON but INERT WITHOUT Screen-Recording
            // TCC consent (and a no-op when disabled, the ring never grows). The raw
            // text is NOT echoed to telemetry; only the honest
            // WATCHING indicator (the loop is active) rides, so the HUD can show the
            // prominent watching state without the sensitive glyphs. A one-shot
            // read (read_kind=screen/handwriting/document) is left UNTOUCHED — it is
            // the transient on-request read, never the continuous ring.
            if topic == "vision.screen"
                && payload.get("read_kind").and_then(Value::as_str) == Some("context")
            {
                let text = payload.get("text").and_then(Value::as_str).unwrap_or("");
                let ts = payload
                    .get("ts")
                    .and_then(Value::as_f64)
                    .map(|t| t as u64)
                    .unwrap_or(0);
                let src = payload
                    .get("source")
                    .and_then(Value::as_str)
                    .unwrap_or("screen");
                // SCREEN GROUNDING: which frontmost app/window the snapshot came
                // from (attributed AX-free by the vision app; absent keys = an
                // honestly unattributed snapshot — an older app build or a
                // headless read). The window title is redacted + bounded at push.
                let src_app = payload.get("source_app").and_then(Value::as_str);
                let src_window = payload.get("source_window").and_then(Value::as_str);
                let ingested = crate::screen_context::ingest_continuous_snapshot_attributed(
                    ts, text, src, src_app, src_window,
                );
                telemetry::emit(
                    "system",
                    "screen_context.watching",
                    // SECRET-FREE: never the recognized text — only that the loop is
                    // active (watching) and whether THIS snapshot was ingested
                    // (false when the loop is OFF, so this honestly reflects the
                    // OFF-default gate). The HUD reads this for the WATCHING badge.
                    json!({
                        "name": name,
                        "watching": crate::screen_context::is_enabled(),
                        "ingested": ingested,
                        // A bounded, secret-free count of how much recent context is
                        // held (never the glyphs) plus the hard cap — for the HUD
                        // WATCHING badge ("held N / cap M").
                        "held": crate::screen_context::global_len(),
                        "cap": crate::screen_context::global_cap(),
                    }),
                );
                // Do NOT relay the sensitive glyphs onward as app.data; the
                // continuous context lives only in the transient ring.
                return;
            }
            // LUMEN voice-navigation: a one-shot on-request screen read
            // (read_kind=screen) is the readout Lumen consults to resolve a
            // voice-named UI action — cache its controls so "read me the buttons,
            // then click the third" can select a target. READ-ONLY: the cache is
            // only ever consulted by the per-action-gated `ui_actuate` path (which
            // still PARKS for a spoken confirm), never an autonomous click; parse is
            // bounded. The readout still relays to the HUD below.
            if topic == "vision.screen"
                && payload.get("read_kind").and_then(Value::as_str) == Some("screen")
            {
                crate::lumen::remember_readout(&payload);
            }
            telemetry::emit(
                "system",
                "app.data",
                json!({"name": name, "topic": topic, "payload": payload}),
            );
        }
        RelayDecision::Log { line } => {
            telemetry::emit("system", "app.log", json!({"name": name, "line": line}));
        }
        RelayDecision::Modules { modules } => {
            // Cooperative dyld attestation: seed on first report, then flag any
            // module the baseline never had (injection / unexpected dlopen). The
            // token was already verified above, so a different process can't forge
            // this. READ-ONLY: it reports, it never unloads/blocks anything.
            let total = modules.len();
            // Envelopes come from introspect.rs's telemetry-contract builders (the
            // single source of truth for the field names the HUD reads), which key
            // the app on "app" — NOT the "name" of the app.data/app.log relay.
            match crate::introspect::attest_or_seed(name, &modules) {
                None => {
                    // First report — baseline seeded silently.
                    let (event, payload) =
                        crate::introspect::ev_modattest(name, total, 0, 0, true);
                    telemetry::emit("system", event, payload);
                }
                Some(att) => {
                    let (event, payload) = crate::introspect::ev_modattest(
                        name,
                        att.total,
                        att.unexpected.len(),
                        att.missing_count,
                        false,
                    );
                    telemetry::emit("system", event, payload);
                    // Bound the per-report fan-out: a malicious app could report up
                    // to MAX_MODULES unexpected entries, which unthrottled would be
                    // MAX_MODULES telemetry emits + findings-ring evictions per line
                    // (a telemetry-flood DoS + a way to evict real findings). The
                    // aggregate `unexpected` count already rides the single
                    // ev_modattest envelope above, so emit only the first K here and
                    // summarize the rest.
                    const MAX_VIOLATION_EMITS: usize = 16;
                    for module in att.unexpected.iter().take(MAX_VIOLATION_EMITS) {
                        // Finding ring is user/cloud-facing -> redact the home
                        // prefix; the telemetry envelope below keeps the full path.
                        crate::introspect::record_finding(crate::introspect::redact_home(&format!(
                            "module: {name} loaded unexpected {}",
                            module.path
                        )));
                        let (event, payload) =
                            crate::introspect::ev_module_violation(name, &module.path, &module.uuid);
                        telemetry::emit("system", event, payload);
                    }
                    if att.unexpected.len() > MAX_VIOLATION_EMITS {
                        crate::introspect::record_finding(format!(
                            "module: {name} +{} more unexpected modules (per-report cap)",
                            att.unexpected.len() - MAX_VIOLATION_EMITS
                        ));
                    }
                }
            }
        }
        RelayDecision::ToolResult { id, payload } => {
            // Deliver the correlated answer to its request_op waiter. The
            // payload rides ONLY the oneshot (it is the tool result, returned
            // to the requester); telemetry gets a secret-free breadcrumb.
            //
            // DIAGNOSTIC. That breadcrumb line used to end "so the HUD/audit can
            // see THAT a tool answered without echoing WHAT", and NEITHER half is
            // true: `applyEnvelope` is an exact-match switch with no `app.result`
            // case, so the frame reaches no pixel; and this arm calls
            // `telemetry::emit` only — nothing on this path writes the audit ring
            // (audit.rs emits telemetry, it never ingests it). The breadcrumb is
            // for the operator's live stream. The requester learns the outcome
            // from the oneshot itself, and a result with no live waiter is
            // reported by the `warn!` in the `None` arm below. Pinned by
            // `hud/src/test/silent-drops.test.ts`.
            let waiter = {
                let apps = registry.apps.lock().await;
                match apps.get(name) {
                    Some(entry) => entry.pending.lock().await.remove(&id),
                    None => None,
                }
            };
            let delivered = match waiter {
                // send fails only if the requester gave up (timeout evicted +
                // dropped the receiver between our remove and this send) —
                // dropping the result then is exactly right.
                Some(tx) => tx.send(payload).is_ok(),
                None => {
                    // Stale/unknown id: the waiter timed out and was evicted,
                    // or the app answered an id it invented. Drop the payload.
                    warn!(app = name, id = %id, "app result for no live request; dropping");
                    false
                }
            };
            telemetry::emit(
                "system",
                "app.result",
                json!({"name": name, "id": id, "delivered": delivered}),
            );
        }
        RelayDecision::Drop => {
            warn!(app = name, "app sent an unhandled/empty line; dropping");
        }
    }
}

/// Topic for an app.data relay: a topic the app names in its data IF it is one
/// the manifest declared, else the manifest's first declared topic, else
/// "feed". Apps can never publish to a topic they did not declare.
fn resolve_topic(manifest: &AppManifest, default: &str, data: &Value) -> String {
    if let Some(requested) = data.get("topic").and_then(Value::as_str) {
        if manifest
            .ui
            .telemetry_topics
            .iter()
            .any(|t| t == requested)
        {
            return requested.to_string();
        }
    }
    default.to_string()
}

/// The default telemetry topic for an app's data: its first declared topic, or
/// "feed" when it declared none (the contract default).
fn default_topic(manifest: &AppManifest) -> String {
    manifest
        .ui
        .telemetry_topics
        .first()
        .cloned()
        .unwrap_or_else(|| "feed".to_string())
}

/// Host -> app command line: {"type":"start"|"refresh"|"stop"}.
async fn send_command(
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    command: &str,
) -> std::io::Result<()> {
    let mut line = json!({"type": command}).to_string();
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await
}

/// Host -> app: write one already-structured op line VERBATIM, JSONL-framed.
/// The daemon never interprets the body — the op contract lives in the target
/// app — so this writes exactly what the router queued, trimming any trailing
/// newline and re-appending a single one so the framing is well-formed.
async fn send_op_line(
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    op_line: &str,
) -> std::io::Result<()> {
    let mut line = op_line.trim_end_matches('\n').to_string();
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await
}

/// Is the app still supposed to be running?
async fn host_wants_running(registry: &Arc<AppRegistry>, name: &str) -> bool {
    let apps = registry.apps.lock().await;
    apps.get(name).map(|e| e.running).unwrap_or(false)
}

/// Read ONE line from a buffered stream, capping the retained bytes at `max`. If a
/// line exceeds `max` before a newline, the first `max` bytes are kept and the rest
/// (up to the next newline or EOF) is DRAINED WITHOUT BUFFERING — so a hostile
/// micro-app streaming a newline-free flood on its stdout cannot grow the daemon's
/// memory without bound, while logging RESYNCS on the next line. Returns `Ok(None)`
/// at clean EOF, `Ok(Some(()))` when `out` holds a (possibly truncated) line. The
/// sole `.await` is `fill_buf` (cancellation-safe), and peak memory is `max` + one
/// fill_buf chunk. Generic over any buffered reader, so it bounds the stdout/stderr
/// relay just as `read_line_bounded` bounds the socket relay.
async fn read_capped_log_line<R>(
    reader: &mut R,
    out: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<Option<()>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;
    out.clear();
    let mut saw_any = false;
    let mut overflowed = false; // cap reached: keep draining to the newline, stop buffering
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            return if saw_any { Ok(Some(())) } else { Ok(None) };
        }
        saw_any = true;
        if let Some(i) = chunk.iter().position(|&b| b == b'\n') {
            if !overflowed {
                let room = max.saturating_sub(out.len());
                out.extend_from_slice(&chunk[..room.min(i)]); // exclude the '\n'
            }
            reader.consume(i + 1);
            return Ok(Some(()));
        }
        if !overflowed {
            let room = max.saturating_sub(out.len());
            if chunk.len() <= room {
                out.extend_from_slice(chunk);
            } else {
                out.extend_from_slice(&chunk[..room]);
                overflowed = true;
            }
        }
        let n = chunk.len();
        reader.consume(n);
    }
}

/// Relay one of the child's stdio streams as app.log telemetry, line by line.
/// BOUNDED per line (see [`read_capped_log_line`]): a micro-app fully controls its
/// own stdout, and this relay is attached to EVERY launched app, so an unbounded
/// `next_line()` would let a hostile app OOM the daemon with a newline-free flood.
fn spawn_log_relay<R>(name: String, stream: R)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut reader = BufReader::new(stream);
        let mut buf: Vec<u8> = Vec::new();
        while let Ok(Some(())) = read_capped_log_line(&mut reader, &mut buf, MAX_APP_LINE_BYTES).await {
            let line = String::from_utf8_lossy(&buf);
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            telemetry::emit("system", "app.log", json!({"name": name, "line": line}));
        }
    });
}

/// Write the seatbelt profile to disk (creating its dir).
/// Sequence counter for unique temp-profile names (so a same-UID pre-plant can
/// never sit at the exact temp path we `create_new`).
static PROFILE_TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Generate the seatbelt profile, RETURN it (the exec source — see below), and
/// write an on-disk AUDIT COPY that the introspect sentinel monitors for
/// integrity. Returns the profile string so the launcher can pass it to
/// `sandbox-exec -p` INLINE.
///
/// TOCTOU: the EXECUTED policy is the returned in-memory string, handed to
/// `sandbox-exec -p <profile>` on the command line — so a same-UID edit of the
/// on-disk copy between this write and the exec CANNOT widen (or alter) the
/// running sandbox (there is no file for the launcher to re-read at exec time).
/// The on-disk copy at `profile_path` is therefore an AUDIT ARTIFACT, not the
/// exec source: it is written atomically to an owner-only (0600) unique temp via
/// `create_new` (so a pre-planted symlink or looser-mode file at the temp path
/// cannot hijack the write) and renamed into place, and its fingerprint is
/// recorded so the introspect drift sentinel can flag any later tampering of the
/// record. (The SBPL is not secret — it names paths, no token/key — so passing
/// it in argv is fine; argv carries no secret, per the launch's env-only rule.)
fn write_profile(
    manifest: &AppManifest,
    project_root: &Path,
    interp: &Path,
    app_dir: &Path,
    socket_path: &Path,
    profile_path: &Path,
) -> Result<String> {
    let profile = generate_sbpl(manifest, project_root, interp, app_dir, socket_path);
    let parent = profile_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("profile path has no parent dir"))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating profile dir {}", parent.display()))?;
    // Owner-only atomic write of the audit copy via a UNIQUE temp + create_new
    // (O_EXCL: never follows a symlink, fails on any pre-existing path) so no
    // same-UID pre-plant can redirect or loosen it; then rename into place.
    let seq = PROFILE_TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp_path = parent.join(format!(".{}.{}.{}.sb.tmp", manifest.name(), std::process::id(), seq));
    {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts
            .open(&tmp_path)
            .with_context(|| format!("creating temp profile {}", tmp_path.display()))?;
        f.write_all(profile.as_bytes())
            .with_context(|| format!("writing temp profile {}", tmp_path.display()))?;
        f.flush().ok();
    }
    std::fs::rename(&tmp_path, profile_path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path); // don't leak the temp on failure
        anyhow::anyhow!("installing audit profile {}: {e}", profile_path.display())
    })?;
    // Fingerprint the audit copy so the introspect sentinel can flag later
    // tampering of the record (the executed policy is the returned string, so
    // this is an integrity signal on the audit artifact, not the exec source).
    crate::introspect::record_profile(manifest.name(), &profile);
    Ok(profile)
}

/// Create the app's declared fs_write directories so the first write inside
/// the sandbox does not fail on a missing parent.
fn ensure_write_dirs(project_root: &Path, manifest: &AppManifest) {
    for w in &manifest.permissions.fs_write {
        let dir = abs(project_root, Path::new(w));
        if let Err(e) = std::fs::create_dir_all(&dir) {
            warn!(dir = %dir.display(), error = %e, "could not pre-create app write dir");
        }
    }
}

/// Remove an app's socket file (missing is fine).
fn cleanup_socket(socket_path: &Path) {
    if let Err(e) = std::fs::remove_file(socket_path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!(path = %socket_path.display(), error = %e, "failed to remove app socket");
        }
    }
}

/// Set a path's permission bits, warning (not failing) on error — these are
/// defense-in-depth tightenings, not load-bearing for correctness.
fn set_mode(path: &Path, mode: u32, what: &str) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)) {
        warn!(path = %path.display(), error = %e, "could not tighten {what} permissions");
    }
}

/// Restrict the bound per-app socket to 0600 (owner read/write only).
fn restrict_socket_perms(socket_path: &Path) {
    set_mode(socket_path, 0o600, "app socket");
}

/// Restrict the per-app socket directory to 0700 (owner-only traversal).
fn restrict_dir_perms(dir: &Path) {
    set_mode(dir, 0o700, "app socket dir");
}

/// A fresh per-launch nonce: hex of 16 bytes of OS entropy. Distinct from the
/// session key (which is the HMAC secret); the nonce is non-secret and rotates
/// per launch so a leaked token dies on restart.
fn fresh_nonce() -> String {
    let mut buf = [0u8; 16];
    match std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf))
    {
        Ok(()) => hex::encode(buf),
        Err(_) => {
            // Extremely unlikely; fall back to a time+pid mix so a launch still
            // gets a unique-per-launch nonce rather than a fixed string.
            let t = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            format!("{t:x}{:x}", std::process::id())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest() -> AppManifest {
        let raw = r#"
            [app]
            name        = "global-scan"
            version     = "0.1.0"
            description = "Intel feed aggregator."
            entry       = "apps/global-scan/main.py"
            runtime     = "python"

            [permissions]
            audio     = false
            gpu       = false
            net_hosts = []
            fs_read   = ["state/ipc/inference.sock"]
            fs_write  = ["state/apps/global-scan"]

            [ui]
            surface          = "panel"
            telemetry_topics = ["feed"]
        "#;
        AppManifest::parse(raw, "global-scan").expect("sample manifest parses")
    }

    /// A manifest with the given `[permissions]` body, else the sample shape.
    fn manifest_with_perms(perms: &str) -> Result<AppManifest> {
        let raw = format!(
            r#"
            [app]
            name        = "probe"
            version     = "0.1.0"
            description = "ceiling probe."
            entry       = "apps/probe/main.py"
            runtime     = "python"

            [permissions]
            {perms}

            [ui]
            surface          = "panel"
            telemetry_topics = ["feed"]
        "#
        );
        AppManifest::parse(&raw, "probe")
    }

    // -- capability ceiling (Wave A) ------------------------------------
    #[test]
    fn ceiling_rejects_an_escaping_or_absolute_fs_path() {
        // Absolute fs_write is refused.
        assert!(manifest_with_perms(
            "audio=false\ngpu=false\nnet_hosts=[]\nfs_read=[]\nfs_write=[\"/etc\"]"
        )
        .is_err());
        // A `..` escape in fs_read is refused.
        assert!(manifest_with_perms(
            "audio=false\ngpu=false\nnet_hosts=[]\nfs_read=[\"../../etc/passwd\"]\nfs_write=[]"
        )
        .is_err());
        // A confined in-project path is allowed (state/tmp + apps/<x>/data shapes
        // the first-party apps actually use).
        assert!(manifest_with_perms(
            "audio=false\ngpu=false\nnet_hosts=[]\nfs_read=[\"state/ipc/inference.sock\"]\nfs_write=[\"state/tmp/probe\"]"
        )
        .is_ok());
    }

    /// THE CEILING NOW REFUSES *ANY* net_hosts, not just a malformed one.
    ///
    /// This replaces `ceiling_rejects_a_non_bare_or_overlong_net_hosts`, which
    /// asserted the declaration was SHAPED -- bare hostnames, at most 16. Both
    /// rules are gone, because there is no well-shaped value: macOS SBPL cannot
    /// express a host filter at all, so a non-empty list only ever produced an
    /// uncompilable profile. Note the second case below: `octoprint.local` is a
    /// perfectly bare hostname that the OLD ceiling ACCEPTED, and it is exactly
    /// what made the deleted `fab-link` app unlaunchable (docs/BLOCKED_APPS.md).
    /// It must now be refused.
    #[test]
    fn ceiling_refuses_any_net_hosts_declaration_however_well_formed() {
        for host in [
            "https://evil.com", // malformed: was refused before, still refused
            "evil.com/path",
            "host:8080",
            "a b",
            "octoprint.local", // WELL-FORMED: was ACCEPTED before, refused now
            "api.binance.com",
        ] {
            let err = manifest_with_perms(&format!(
                "audio=false\ngpu=false\nnet_hosts=[\"{host}\"]\nfs_read=[]\nfs_write=[]"
            ))
            .expect_err(&format!("net_hosts {host:?} must be refused"));
            let msg = format!("{err:#}");
            assert!(
                msg.contains("not grantable"),
                "the refusal must say the scope is not grantable, not that it is malformed: {msg}"
            );
            assert!(
                msg.contains("fetch_hosts"),
                "the refusal must name the route that works, or it is a dead end: {msg}"
            );
        }
        // An EMPTY list is the only accepted value, and stays accepted.
        assert!(manifest_with_perms(
            "audio=false\ngpu=false\nnet_hosts=[]\nfs_read=[]\nfs_write=[]"
        )
        .is_ok());
    }

    #[test]
    fn ceiling_rejects_a_non_bare_or_overlong_fetch_hosts() {
        // fetch_hosts gets the SAME ceiling as net_hosts: a URL / path / port /
        // space in an entry is refused (must be a bare DNS name).
        for bad in ["https://evil.com", "evil.com/path", "host:8080", "a b"] {
            assert!(
                manifest_with_perms(&format!(
                    "audio=false\ngpu=false\nfetch_hosts=[\"{bad}\"]\nfs_read=[]\nfs_write=[]"
                ))
                .is_err(),
                "fetch_host {bad:?} must be rejected"
            );
        }
        // A bare hostname is fine.
        assert!(manifest_with_perms(
            "audio=false\ngpu=false\nfetch_hosts=[\"feeds.npr.org\"]\nfs_read=[]\nfs_write=[]"
        )
        .is_ok());
        // Over the count ceiling (>16) is refused.
        let many = (0..17).map(|i| format!("\"h{i}.example\"")).collect::<Vec<_>>().join(",");
        assert!(manifest_with_perms(&format!(
            "audio=false\ngpu=false\nfetch_hosts=[{many}]\nfs_read=[]\nfs_write=[]"
        ))
        .is_err());
    }

    #[test]
    fn ceiling_does_not_ban_first_party_elevated_permissions() {
        // audio/gpu/camera are LEGITIMATE for first-party apps (nexus/vision) —
        // the runtime ceiling bounds path/host SHAPE, not these declarations.
        assert!(manifest_with_perms(
            "audio=true\ngpu=true\nnet_hosts=[]\nfs_read=[]\nfs_write=[\"state/tmp/probe\"]"
        )
        .is_ok());
    }

    // -- manifest parse -------------------------------------------------

    #[test]
    fn manifest_parses_full_schema() {
        let m = sample_manifest();
        assert_eq!(m.app.name, "global-scan");
        assert_eq!(m.app.version, "0.1.0");
        assert_eq!(m.app.runtime, Runtime::Python);
        assert_eq!(m.app.entry, "apps/global-scan/main.py");
        assert!(!m.permissions.audio);
        assert!(!m.permissions.gpu);
        assert!(m.permissions.net_hosts.is_empty(), "a validated manifest never carries net_hosts");
        assert_eq!(m.permissions.fs_read, vec!["state/ipc/inference.sock"]);
        assert_eq!(m.permissions.fs_write, vec!["state/apps/global-scan"]);
        assert_eq!(m.ui.surface, "panel");
        assert_eq!(m.ui.telemetry_topics, vec!["feed"]);
    }

    #[test]
    fn manifest_name_must_match_directory() {
        let raw = r#"
            [app]
            name = "global-scan"
            version = "0.1.0"
            description = "x"
            entry = "main.py"
            runtime = "python"
        "#;
        assert!(AppManifest::parse(raw, "global-scan").is_ok());
        let err = AppManifest::parse(raw, "wrong-dir").unwrap_err().to_string();
        assert!(err.contains("must match its directory"), "{err}");
    }

    #[test]
    fn manifest_rejects_unknown_keys_and_unknown_runtime() {
        // Unknown permission key — must not silently widen/narrow the sandbox.
        let raw = r#"
            [app]
            name = "x"
            version = "0.1.0"
            description = "d"
            entry = "main.py"
            runtime = "python"
            [permissions]
            net_hots = ["a.com"]
        "#;
        assert!(AppManifest::parse(raw, "x").is_err(), "typo'd key must be rejected");

        let bad_runtime = r#"
            [app]
            name = "x"
            version = "0.1.0"
            description = "d"
            entry = "main.py"
            runtime = "ruby"
        "#;
        assert!(AppManifest::parse(bad_runtime, "x").is_err(), "unknown runtime rejected");
    }

    #[test]
    fn manifest_defaults_empty_permissions_and_ui() {
        let raw = r#"
            [app]
            name = "bare"
            version = "0.1.0"
            description = "d"
            entry = "bare"
            runtime = "binary"
        "#;
        let m = AppManifest::parse(raw, "bare").unwrap();
        assert!(!m.permissions.audio && !m.permissions.gpu);
        assert!(m.permissions.net_hosts.is_empty());
        assert_eq!(m.ui.surface, "panel"); // default surface
        assert!(m.ui.telemetry_topics.is_empty());
    }

    #[test]
    fn camera_and_screen_default_false_and_omitting_them_still_parses() {
        // The NEW camera/screen keys are #[serde(default)] => false. EVERY
        // existing manifest omits them, so omission must parse and leave both
        // false (camera/screen-denied). This is the invariant that keeps all
        // shipped manifests (global-scan, silicon-canvas) green.
        let m = sample_manifest();
        assert!(!m.permissions.camera, "camera defaults false when omitted");
        assert!(!m.permissions.screen, "screen defaults false when omitted");

        // When a manifest DOES declare them, they parse through.
        let raw = r#"
            [app]
            name = "vision"
            version = "0.1.0"
            description = "d"
            entry = "vision"
            runtime = "binary"
            [permissions]
            gpu = true
            camera = true
            screen = true
        "#;
        let v = AppManifest::parse(raw, "vision").unwrap();
        assert!(v.permissions.camera);
        assert!(v.permissions.screen);
    }

    #[test]
    fn shipped_vision_manifest_parses_with_tcc_keys() {
        // The shipped Vision manifest must parse under the extended schema: it
        // is offline (net_hosts empty), GPU-on (ANE/Core ML), and declares the
        // camera/screen TCC needs. Those two keys are LIVE in the shipped manifest
        // (`camera = true` / `screen = true`), so they are pinned here alongside the
        // offline/gpu invariants — this used to say they were still commented out and
        // skipped asserting them for that (now false) reason.
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("apps")
            .join("vision");
        let m = AppManifest::load(&path).expect("shipped vision manifest must parse");
        assert_eq!(m.name(), "vision");
        assert_eq!(m.app.runtime, Runtime::Binary);
        // Defensive-only + on-device: fully offline.
        assert!(
            m.permissions.net_hosts.is_empty(),
            "Vision must be fully offline (net_hosts = [])"
        );
        assert!(m.permissions.gpu, "Vision uses the ANE/GPU for built-in Vision requests");
        assert!(!m.permissions.audio, "Vision never touches the microphone");
        // The TCC declarations the shipped manifest really makes.
        assert!(m.permissions.camera, "the shipped Vision manifest declares camera = true");
        assert!(m.permissions.screen, "the shipped Vision manifest declares screen = true");
        // Declared topics include the detection + status streams.
        assert!(m.ui.telemetry_topics.iter().any(|t| t == "vision.detections"));
    }

    #[test]
    fn shipped_global_scan_manifest_parses() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("apps")
            .join("global-scan");
        let m = AppManifest::load(&path).expect("shipped global-scan manifest must parse");
        assert_eq!(m.name(), "global-scan");
        assert_eq!(m.app.runtime, Runtime::Python);
        // Global-Scan has NO direct network: net_hosts is empty; it fetches feeds
        // through the daemon-mediated fetch proxy, so the feed hostnames live in
        // fetch_hosts (still lockstep with feeds.toml).
        assert!(m.permissions.net_hosts.is_empty(), "no direct network egress");
        assert!(m.permissions.fetch_hosts.contains(&"feeds.npr.org".to_string()));
        assert!(m.permissions.fetch_hosts.contains(&"hnrss.org".to_string()));
        // It is granted the fetch-proxy socket (its only path to a feed).
        assert!(m.permissions.fs_read.contains(&"state/ipc/apps/fetch.sock".to_string()));
        assert_eq!(m.permissions.fs_write, vec!["state/apps/global-scan"]);
        assert_eq!(m.ui.telemetry_topics, vec!["feed"]);
    }

    /// Lockstep: every hostname in the manifest's fetch_hosts (the fetch-proxy
    /// allow-list) must appear as a URL host in feeds.toml, and vice versa — the
    /// proxy allow-list and the feed list cannot drift. (net_hosts is empty now
    /// that all egress goes through the daemon fetch proxy.)
    #[test]
    fn manifest_fetch_hosts_match_feeds_toml_hosts() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("apps")
            .join("global-scan");
        let m = AppManifest::load(&base).unwrap();
        // No direct network egress — the app fetches ONLY through the proxy.
        assert!(
            m.permissions.net_hosts.is_empty(),
            "global-scan must have no direct net_hosts (egress goes through the fetch proxy)"
        );
        let mut manifest_hosts: Vec<String> = m.permissions.fetch_hosts.clone();
        manifest_hosts.sort();

        let feeds_raw = std::fs::read_to_string(base.join("feeds.toml")).unwrap();
        // Extract every https://HOST/ from the feeds file.
        let mut feed_hosts: Vec<String> = feeds_raw
            .lines()
            .filter_map(|l| {
                let l = l.trim();
                let start = l.find("https://")? + "https://".len();
                let rest = &l[start..];
                let end = rest.find('/').unwrap_or(rest.len());
                Some(rest[..end].to_string())
            })
            .collect();
        feed_hosts.sort();
        feed_hosts.dedup();

        assert_eq!(
            manifest_hosts, feed_hosts,
            "manifest fetch_hosts and feeds.toml hosts must be identical"
        );
    }

    // -- SBPL generation ------------------------------------------------

    fn gen_profile(m: &AppManifest) -> String {
        let root = Path::new("/Users/test/darwin");
        let interp = root.join(".venv/bin/python3");
        let app_dir = root.join("apps/global-scan");
        let sock = root.join("state/ipc/apps/global-scan.sock");
        generate_sbpl(m, root, &interp, &app_dir, &sock)
    }

    #[test]
    fn sbpl_is_default_deny() {
        let p = gen_profile(&sample_manifest());
        assert!(p.starts_with("(version 1)\n"), "must start with version");
        assert!(p.contains("(deny default)"), "must be default-deny");
    }

    #[test]
    fn sbpl_grants_exec_read_write_for_declared_paths() {
        let p = gen_profile(&sample_manifest());
        // Exec the interpreter + the app dir.
        assert!(p.contains("(allow process-exec* (literal \"/Users/test/darwin/.venv/bin/python3\"))"));
        assert!(p.contains("(allow process-exec* (subpath \"/Users/test/darwin/apps/global-scan\"))"));
        // Read the app dir + the venv + the declared fs_read.
        assert!(p.contains("(allow file-read* (subpath \"/Users/test/darwin/apps/global-scan\"))"));
        assert!(p.contains("(allow file-read* (subpath \"/Users/test/darwin/.venv\"))"));
        assert!(p.contains("(allow file-read* (subpath \"/Users/test/darwin/state/ipc/inference.sock\"))"));
        // Write the declared fs_write only.
        assert!(p.contains("(allow file-write* (subpath \"/Users/test/darwin/state/apps/global-scan\"))"));
        // Connect to its own socket.
        assert!(p.contains("(allow network-outbound (literal \"/Users/test/darwin/state/ipc/apps/global-scan.sock\"))"));
    }

    #[test]
    fn sbpl_grants_read_of_the_shared_sdk_harness_when_declared() {
        // Every standard micro-app now imports the shared apps/_sdk harness. That
        // read is only permitted if the manifest grants `apps/_sdk` fs_read AND
        // the generated seatbelt profile turns that grant into a file-read subpath
        // — else `import harness` fails at LAUNCH (uncaught by the Python tests,
        // which don't run under the sandbox). This pins the profile-level grant so
        // the sandbox can actually read the harness.
        let raw = r#"
            [app]
            name = "global-scan"
            version = "0.1.0"
            description = "x"
            entry = "apps/global-scan/main.py"
            runtime = "python"
            [permissions]
            fs_read = ["apps/global-scan", "apps/_sdk"]
            fs_write = ["state/apps/global-scan"]
        "#;
        let m = AppManifest::parse(raw, "global-scan").expect("valid");
        let p = gen_profile(&m);
        assert!(
            p.contains("(allow file-read* (subpath \"/Users/test/darwin/apps/_sdk\"))"),
            "the seatbelt profile must grant reading the shared apps/_sdk harness"
        );
    }

    #[test]
    fn sbpl_fs_read_unix_socket_gets_af_unix_connect_grant() {
        // Finding #4 fix (SBPL side): a declared fs_read entry that IS a Unix
        // socket needs an AF_UNIX network-outbound literal grant IN ADDITION to
        // its file-read* subpath — file-read alone does not permit connect() on
        // this macOS. A NORMAL (non-.sock) fs_read entry must NOT get one.
        let mut m = sample_manifest();
        m.permissions.fs_read = vec![
            "state/ipc/apps/generate.sock".to_string(), // a socket
            "state/shared/config.json".to_string(),     // a normal file
        ];
        let p = gen_profile(&m);
        // Both get the file-read* subpath grant (unchanged behavior).
        assert!(p.contains("(allow file-read* (subpath \"/Users/test/darwin/state/ipc/apps/generate.sock\"))"));
        assert!(p.contains("(allow file-read* (subpath \"/Users/test/darwin/state/shared/config.json\"))"));
        // Only the .sock entry gets the AF_UNIX connect() literal.
        assert!(
            p.contains("(allow network-outbound (literal \"/Users/test/darwin/state/ipc/apps/generate.sock\"))"),
            "a .sock fs_read entry must get an AF_UNIX connect grant"
        );
        assert!(
            !p.contains("(allow network-outbound (literal \"/Users/test/darwin/state/shared/config.json\"))"),
            "a normal file fs_read entry must NOT get a network-outbound grant"
        );
        // And the grant lands AFTER the unconditional (deny network*), because
        // SBPL is last-match-wins and the deny would otherwise clobber the
        // AF_UNIX connect the app needs to reach the daemon.
        let deny_idx = p.find("(deny network*)").expect("deny network present");
        let grant_idx = p
            .find("(allow network-outbound (literal \"/Users/test/darwin/state/ipc/apps/generate.sock\"))")
            .expect("socket grant present");
        assert!(grant_idx > deny_idx, "the connect grant must come after the network deny");
    }

    /// THE NET SCOPE IS GONE FROM THE SBPL ENTIRELY. This replaces the old
    /// `sbpl_network_is_host_filtered_when_listed` (and the two DNS-pinning
    /// tests that sat beside it), which asserted the literals of a profile macOS
    /// never accepted: `(remote tcp (host-name ...))` is not valid SBPL, so those
    /// rules could only ever produce an uncompilable profile. They passed
    /// because the generator was agreeing with itself.
    ///
    /// The contract now: no matter what a manifest carries -- even one built
    /// in-process that bypassed the validator's refusal -- the profile is a FLAT
    /// deny with no IP stack, no resolver, and no host filter of any kind.
    #[test]
    fn sbpl_never_grants_direct_network_even_for_a_manifest_carrying_hosts() {
        let mut m = sample_manifest();
        // Bypass validation deliberately: this is the belt-and-braces path.
        m.permissions.net_hosts = vec!["feeds.npr.org".into(), "hnrss.org".into()];
        let p = gen_profile(&m);
        assert!(p.contains("(deny network*)"), "the IP network is denied outright");
        assert!(!p.contains("(system-network)"), "no IP network stack is ever granted");
        assert!(!p.contains("host-name"), "SBPL has no host filter; none may be emitted");
        for host in &m.permissions.net_hosts {
            assert!(
                !p.contains(host.as_str()),
                "a declared host ({host}) must not reach the profile at all"
            );
        }
        // ...and no DNS grant survives either -- the resolver grant only ever
        // existed to serve the host allow-list, and it was the exfil channel.
        assert!(!p.contains(":53"), "no DNS grant without a net scope to serve");
    }


    #[test]
    fn sbpl_exec_is_literal_only_never_a_broad_prefix() {
        // Finding #2 fix: exec must be granted ONLY on literal interpreter
        // paths + the app's own dir subpath — NEVER a broad /opt/homebrew or
        // /usr/local subpath that would let the app exec arbitrary binaries.
        let p = gen_profile(&sample_manifest());
        assert!(!p.contains("(allow process-exec* (subpath \"/opt/homebrew\"))"));
        assert!(!p.contains("(allow process-exec* (subpath \"/usr/local\"))"));
        // The only process-exec* subpath is the app's own directory.
        let exec_subpaths: Vec<&str> = p
            .lines()
            .filter(|l| l.contains("process-exec* (subpath"))
            .collect();
        assert_eq!(exec_subpaths.len(), 1, "only the app dir may be an exec subpath: {exec_subpaths:?}");
        assert!(exec_subpaths[0].contains("apps/global-scan"));
    }

    #[test]
    fn sbpl_file_read_metadata_is_scoped_never_blanket() {
        // Finding #1 fix: a bare `(allow file-read-metadata)` (no path filter)
        // is an arbitrary-path stat side channel and must NEVER be emitted.
        let p = gen_profile(&sample_manifest());
        assert!(
            !p.lines().any(|l| l.trim() == "(allow file-read-metadata)"),
            "blanket file-read-metadata must never be emitted"
        );
        // Every metadata grant is subpath-scoped, and to a root we also granted
        // file-read* on (e.g. the app dir).
        assert!(p.contains("(allow file-read-metadata (subpath \"/Users/test/darwin/apps/global-scan\"))"));
    }

    #[test]
    fn interpreter_install_prefix_derivation() {
        // <prefix>/bin/python3.11 -> <prefix>
        assert_eq!(
            interpreter_install_prefix(Path::new(
                "/opt/homebrew/Cellar/python@3.11/3.11.9/bin/python3.11"
            )),
            Some(PathBuf::from("/opt/homebrew/Cellar/python@3.11/3.11.9"))
        );
        // Not in a bin/ dir -> None (no broad-ancestor grant).
        assert_eq!(
            interpreter_install_prefix(Path::new("/opt/homebrew/python3")),
            None
        );
        // Pathologically shallow prefix -> None (would re-open a broad tree).
        assert_eq!(interpreter_install_prefix(Path::new("/usr/bin/python3")), None);
    }

    #[test]
    fn sbpl_fetch_hosts_grant_no_direct_network() {
        // The load-bearing invariant of the fetch proxy: declaring fetch_hosts
        // grants the app NOTHING in the SBPL network layer. With net_hosts empty
        // the profile stays a FLAT (deny network*) — no (system-network), no DNS,
        // and no host-name filter — EVEN THOUGH fetch_hosts is non-empty. All that
        // egress rides the daemon over the fetch.sock AF_UNIX literal instead.
        let mut m = sample_manifest();
        m.permissions.net_hosts.clear();
        m.permissions.fetch_hosts = vec!["feeds.npr.org".into(), "www.nasa.gov".into()];
        let p = gen_profile(&m);
        assert!(p.contains("(deny network*)"), "fetch_hosts must not open direct network");
        assert!(!p.contains("(system-network)"), "no IP network stack for a proxy-only app");
        assert!(!p.contains("host-name"), "a fetch_hosts entry is never an SBPL host-name grant");
    }

    #[test]
    fn sbpl_denies_mic_and_gpu_by_default_and_grants_nothing_stray() {
        let p = gen_profile(&sample_manifest());
        assert!(p.contains("(deny device-microphone)"), "audio=false denies mic");
        assert!(p.contains("AGXDeviceUserClient"), "gpu=false denies the GPU client");
        // No stray write grant outside the declared path: the only file-write*
        // subpath is the declared one (state/apps/global-scan); the socket is
        // a literal, not a subpath.
        let write_subpaths: Vec<&str> = p
            .lines()
            .filter(|l| l.contains("file-write* (subpath"))
            .collect();
        assert_eq!(write_subpaths.len(), 1, "exactly one write subpath: {write_subpaths:?}");
        assert!(write_subpaths[0].contains("state/apps/global-scan"));
    }

    #[test]
    fn sbpl_gpu_true_omits_the_gpu_deny() {
        let mut m = sample_manifest();
        m.permissions.gpu = true;
        let p = gen_profile(&m);
        assert!(!p.contains("AGXDeviceUserClient"), "gpu=true must not deny the GPU client");
    }

    #[test]
    fn sbpl_jit_defaults_denied_and_never_emits_legacy_dynamic_signature() {
        // Every existing manifest omits `jit` -> jit=false -> explicit deny of the
        // ONE current operation (dynamic-code-generation). The legacy
        // `dynamic-signature` op must NEVER be emitted (not a live operation).
        let p = gen_profile(&sample_manifest());
        assert!(
            p.contains("(deny dynamic-code-generation)"),
            "jit=false must explicitly deny dynamic-code-generation"
        );
        assert!(
            !p.contains("dynamic-signature"),
            "the non-current dynamic-signature op must never be emitted"
        );
        assert!(
            !p.contains("(allow dynamic-code-generation)"),
            "jit=false must not allow dynamic-code-generation"
        );
    }

    #[test]
    fn sbpl_jit_true_allows_dynamic_code_generation_and_documents_the_entitlement_caveat() {
        let mut m = sample_manifest();
        m.permissions.jit = true;
        let p = gen_profile(&m);
        assert!(
            p.contains("(allow dynamic-code-generation)"),
            "jit=true must allow dynamic-code-generation"
        );
        assert!(
            !p.contains("(deny dynamic-code-generation)"),
            "jit=true must not also deny it"
        );
        // The best-effort honesty note (the process still needs the allow-jit
        // entitlement) must be present so the profile never pretends SBPL alone
        // enables JIT — same discipline as the camera/screen TCC caveat.
        assert!(
            p.contains("allow-jit"),
            "jit=true must document that the process also needs cs.allow-jit"
        );
        // Still never the legacy op.
        assert!(!p.contains("dynamic-signature"));
    }

    #[test]
    fn sbpl_camera_and_screen_default_deny_when_unset() {
        // An app that does NOT declare camera/screen (every existing one) must
        // get the explicit camera/screen denies and NONE of the best-effort
        // plumbing allows.
        let p = gen_profile(&sample_manifest()); // camera=false, screen=false
        assert!(
            p.contains("(deny iokit-open (iokit-user-client-class \"IOVideoDeviceUserClient\"))"),
            "camera=false must explicitly deny the camera device client"
        );
        assert!(
            p.contains("(deny mach-lookup (global-name \"com.apple.windowserver.active\"))"),
            "screen=false must explicitly deny the window-server lookup"
        );
        // No best-effort capture plumbing leaks in when both are false.
        assert!(!p.contains("(allow iokit-open (iokit-user-client-class \"IOVideoDeviceUserClient\"))"));
        assert!(!p.contains("AppleCameraAssistant"));
    }

    #[test]
    fn sbpl_camera_screen_grant_is_best_effort_and_documents_tcc_is_the_real_gate() {
        // With camera/screen declared, the profile grants ONLY best-effort
        // plumbing AND must DOCUMENT that TCC — not SBPL — is the real gate, so
        // the profile never pretends to enable capture on its own.
        let mut m = sample_manifest();
        m.permissions.camera = true;
        m.permissions.screen = true;
        let p = gen_profile(&m);

        // Best-effort plumbing present (reaches the capture stack + consent
        // prompt) but NOT a capture grant — there is no such SBPL op.
        assert!(p.contains("(allow iokit-open (iokit-user-client-class \"IOVideoDeviceUserClient\"))"));
        assert!(p.contains("(allow mach-lookup (global-name \"com.apple.windowserver.active\"))"));
        assert!(p.contains("com.apple.tccd"), "must allow reaching tccd for the consent prompt");
        // The explicit denies are gone now that the keys are true.
        assert!(!p.contains("(deny iokit-open (iokit-user-client-class \"IOVideoDeviceUserClient\"))"));
        assert!(!p.contains("(deny mach-lookup (global-name \"com.apple.windowserver.active\"))"));

        // Honesty requirement: the profile DOCUMENTS that TCC is the real gate
        // and is NOT SBPL-grantable — for BOTH camera and screen.
        assert!(
            p.contains("macOS TCC (Camera) is the REAL gate"),
            "camera block must document TCC as the real gate"
        );
        assert!(
            p.contains("macOS TCC (Screen Recording) is the\n;; REAL gate"),
            "screen block must document TCC as the real gate"
        );
        assert!(
            p.contains("NOT SBPL-grantable") || p.contains("NOT\n;; SBPL-grantable"),
            "must state TCC is not SBPL-grantable"
        );
        // Still default-deny overall.
        assert!(p.contains("(deny default)"));
    }

    #[test]
    fn sbpl_string_escaping_neutralizes_quotes() {
        // A path with a quote must not break out of the SBPL string literal.
        let escaped = sbpl_str(Path::new("/tmp/a\"b\\c"));
        assert_eq!(escaped, "\"/tmp/a\\\"b\\\\c\"");
    }

    /// Regression-lock the PRODUCTION profile: generate it from the shipped
    /// global-scan manifest with a realistic project root and assert the
    /// invariants the app actually depends on to launch and stay contained.
    #[test]
    fn sbpl_for_shipped_global_scan_manifest_is_correct() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("apps")
            .join("global-scan");
        let m = AppManifest::load(&base).unwrap();
        let root = Path::new("/Users/op/darwin");
        let interp = root.join(".venv/bin/python3");
        let app_dir = root.join("apps/global-scan");
        let sock = root.join("state/ipc/apps/global-scan.sock");
        let p = generate_sbpl(&m, root, &interp, &app_dir, &sock);

        // Boots: default-deny + the Apple base profile import (so python can
        // actually start) + exec on the configured interpreter literal (the
        // symlinked venv python). Exec is LITERAL-only — never a broad
        // Homebrew/usr-local subpath (finding #2).
        assert!(p.contains("(deny default)"));
        // The bsd.sb import is emitted whenever that stock macOS profile exists
        // (it does on the M-series targets); the generator gates on it, so the
        // test gates the same way to stay portable to a stripped CI image.
        if Path::new(BSD_BASE_PROFILE).exists() {
            assert!(p.contains("(import \"/System/Library/Sandbox/Profiles/bsd.sb\")"));
        }
        assert!(p.contains("(allow process-exec* (literal \"/Users/op/darwin/.venv/bin/python3\"))"));
        assert!(!p.contains("(allow process-exec* (subpath \"/opt/homebrew\"))"));
        assert!(!p.contains("(allow process-exec* (subpath \"/usr/local\"))"));
        // Reads: the app dir, the venv (read prefix), and its declared fs_read —
        // the daemon-mediated FETCH proxy socket AND the generate PROXY socket,
        // NOT the raw inference.sock (finding #4 fix); writes: only its own app
        // state dir.
        assert!(p.contains("(allow file-read* (subpath \"/Users/op/darwin/.venv\"))"));
        assert!(p.contains("(allow file-read* (subpath \"/Users/op/darwin/apps/global-scan\"))"));
        assert!(p.contains("(allow file-read* (subpath \"/Users/op/darwin/state/ipc/apps/fetch.sock\"))"));
        assert!(p.contains("(allow file-read* (subpath \"/Users/op/darwin/state/ipc/apps/generate.sock\"))"));
        // The raw inference socket is NO LONGER reachable by the app.
        assert!(
            !p.contains("inference.sock"),
            "the app must have no grant of any kind to the raw inference.sock"
        );
        assert!(p.contains("(allow file-write* (subpath \"/Users/op/darwin/state/apps/global-scan\"))"));
        // Connects to its own host socket...
        assert!(p.contains("(allow network-outbound (literal \"/Users/op/darwin/state/ipc/apps/global-scan.sock\"))"));
        // ...and gets the AF_UNIX connect() grant for BOTH .sock fs_read entries
        // (file-read alone does not permit connect() on this macOS).
        assert!(p.contains("(allow network-outbound (literal \"/Users/op/darwin/state/ipc/apps/fetch.sock\"))"));
        assert!(p.contains("(allow network-outbound (literal \"/Users/op/darwin/state/ipc/apps/generate.sock\"))"));
        // NO direct network: net_hosts is empty, so the profile is a FLAT
        // (deny network*) with NO (system-network) and NO host-name filters at
        // all — all feed egress now flows through the fetch proxy, which is the
        // ONLY filtered egress there is (the two "inherent SBPL caveats" this
        // comment used to invoke described rules that never compiled).
        assert!(m.permissions.net_hosts.is_empty(), "shipped global-scan must declare no direct net_hosts");
        assert!(p.contains("(deny network*)"));
        assert!(!p.contains("(system-network)"), "no direct IP network stack");
        assert!(!p.contains("host-name"), "no host-name filter survives an empty net_hosts");
        // Not even the declared FETCH hosts leak into the SBPL as host filters —
        // they are the proxy's allow-list, never a seatbelt network grant.
        for host in &m.permissions.fetch_hosts {
            assert!(
                !p.contains(&format!("host-name \"{host}\"")),
                "a fetch_hosts entry ({host}) must never become an SBPL host-name grant"
            );
        }
        // No write grant outside the declared app dir.
        let write_subpaths: Vec<&str> = p.lines().filter(|l| l.contains("file-write* (subpath")).collect();
        assert_eq!(write_subpaths.len(), 1, "exactly one write subpath: {write_subpaths:?}");
        // Mic + GPU denied (audio=false, gpu=false).
        assert!(p.contains("(deny device-microphone)"));
        assert!(p.contains("AGXDeviceUserClient"));
    }

    // -- token mint / verify --------------------------------------------

    const TEST_KEY: &[u8] = b"unit-test-session-key-not-the-real-one";

    fn perms(net: &[&str]) -> PermissionsSection {
        PermissionsSection {
            audio: false,
            gpu: false,
            net_hosts: net.iter().map(|s| s.to_string()).collect(),
            fs_read: vec!["state/ipc/inference.sock".to_string()],
            fs_write: vec!["state/apps/global-scan".to_string()],
            // camera/screen default false (Default) — these token tests model an
            // existing app that declares neither, so the canonical form keeps the
            // camera=false;screen=false suffix.
            ..Default::default()
        }
    }

    #[test]
    fn token_roundtrips_and_is_deterministic() {
        let p = perms(&["feeds.npr.org"]);
        let t1 = compute_token(TEST_KEY, "global-scan", &p, "nonce-A");
        let t2 = compute_token(TEST_KEY, "global-scan", &p, "nonce-A");
        assert_eq!(t1, t2, "same inputs -> same token");
        assert!(verify_token_with_key(TEST_KEY, "global-scan", &p, "nonce-A", &t1));
    }

    #[test]
    fn token_forgery_is_rejected() {
        let p = perms(&["feeds.npr.org"]);
        // A made-up token never verifies.
        assert!(!verify_token_with_key(TEST_KEY, "global-scan", &p, "nonce-A", "deadbeef"));
        // A valid token under a DIFFERENT key fails (the secret is the gate).
        let other = compute_token(b"some-other-key", "global-scan", &p, "nonce-A");
        assert!(!verify_token_with_key(TEST_KEY, "global-scan", &p, "nonce-A", &other));
    }

    #[test]
    fn token_is_bound_to_nonce_name_and_permissions() {
        let p = perms(&["feeds.npr.org"]);
        let t = compute_token(TEST_KEY, "global-scan", &p, "nonce-A");
        // Stale nonce (a leaked token after a restart rotated the nonce).
        assert!(!verify_token_with_key(TEST_KEY, "global-scan", &p, "nonce-B", &t));
        // Cross-app: another app presenting global-scan's token.
        assert!(!verify_token_with_key(TEST_KEY, "other-app", &p, "nonce-A", &t));
        // Tampered permission set (a manifest that widened net_hosts after the
        // token was minted).
        let widened = perms(&["feeds.npr.org", "evil.com"]);
        assert!(!verify_token_with_key(TEST_KEY, "global-scan", &widened, "nonce-A", &t));
    }

    #[test]
    fn token_is_bound_to_camera_and_screen_flags() {
        // camera/screen join the bound set: a token minted for a camera-less
        // app must NOT verify for the same app after it flips camera (or screen)
        // on — the same anti-privilege-escalation discipline as net_hosts.
        let base = perms(&["feeds.npr.org"]);
        let t = compute_token(TEST_KEY, "vision", &base, "nonce-A");
        assert!(verify_token_with_key(TEST_KEY, "vision", &base, "nonce-A", &t));

        let mut cam = base.clone();
        cam.camera = true;
        assert!(
            !verify_token_with_key(TEST_KEY, "vision", &cam, "nonce-A", &t),
            "flipping camera on must invalidate a token minted without it"
        );
        let mut scr = base.clone();
        scr.screen = true;
        assert!(
            !verify_token_with_key(TEST_KEY, "vision", &scr, "nonce-A", &t),
            "flipping screen on must invalidate a token minted without it"
        );
    }

    #[test]
    fn capability_summary_lists_only_granted_caps_with_counts() {
        // A locked-down app reads short.
        let bare = PermissionsSection::default();
        assert_eq!(capability_summary(&bare), "sandboxed (no extra capabilities)");

        // A grant set lists only what's granted, counts for the list-valued ones,
        // and never the paths/hosts themselves (secret-free).
        let p = PermissionsSection {
            audio: true,
            gpu: false,
            camera: true,
            screen: false,
            jit: true,
            net_hosts: vec!["a.com".into(), "b.com".into()],
            fetch_hosts: vec![],
            fs_read: vec!["state/x".into()],
            fs_write: vec![],
        };
        let s = capability_summary(&p);
        assert_eq!(s, "audio, camera, jit, net(2), fs_read(1)");
        assert!(!s.contains("a.com"), "must not leak the actual hosts");
        assert!(!s.contains("gpu"), "an ungranted cap is omitted");
        assert!(!s.contains("fs_write"), "an empty list is omitted");

        // fetch_hosts surfaces as a secret-free fetch(N) count, right after net().
        let f = PermissionsSection {
            net_hosts: vec![],
            fetch_hosts: vec!["feeds.npr.org".into(), "hnrss.org".into(), "www.nasa.gov".into()],
            fs_read: vec!["state/ipc/apps/fetch.sock".into()],
            ..Default::default()
        };
        let fs = capability_summary(&f);
        assert_eq!(fs, "fetch(3), fs_read(1)");
        assert!(!fs.contains("feeds.npr.org"), "must not leak the actual fetch hosts");
    }

    #[test]
    fn token_is_bound_to_jit_flag() {
        // jit joins the bound set: a token minted for a non-JIT app must NOT
        // verify after the manifest flips jit on — same anti-privilege-escalation
        // discipline as camera/screen/net_hosts. This is what makes auto-promoting
        // an app to jit=true detectable rather than silent.
        let base = perms(&["feeds.npr.org"]);
        let t = compute_token(TEST_KEY, "jit-probe-app", &base, "nonce-A");
        assert!(verify_token_with_key(TEST_KEY, "jit-probe-app", &base, "nonce-A", &t));
        let mut jit = base.clone();
        jit.jit = true;
        assert!(
            !verify_token_with_key(TEST_KEY, "jit-probe-app", &jit, "nonce-A", &t),
            "flipping jit on must invalidate a token minted without it"
        );
    }

    #[test]
    fn token_is_bound_to_fetch_hosts() {
        // fetch_hosts joins the bound set exactly like net_hosts: a manifest that
        // WIDENS the hosts it can proxy-fetch after a token was minted must fail
        // verification — so a silent widening of the fetch-proxy allow-list is
        // detectable, not free.
        let base = perms(&["feeds.npr.org"]);
        let t = compute_token(TEST_KEY, "global-scan", &base, "nonce-A");
        assert!(verify_token_with_key(TEST_KEY, "global-scan", &base, "nonce-A", &t));
        let mut widened = base.clone();
        widened.fetch_hosts = vec!["evil.example".into()];
        assert!(
            !verify_token_with_key(TEST_KEY, "global-scan", &widened, "nonce-A", &t),
            "widening fetch_hosts must invalidate a token minted without it"
        );
    }

    #[test]
    fn canonical_permissions_is_order_independent() {
        let a = perms(&["b.com", "a.com"]);
        let b = perms(&["a.com", "b.com"]);
        assert_eq!(canonical_permissions(&a), canonical_permissions(&b));
        // ...so the token is identical regardless of declaration order.
        assert_eq!(
            compute_token(TEST_KEY, "x", &a, "n"),
            compute_token(TEST_KEY, "x", &b, "n")
        );
        // But a genuinely different set differs.
        let c = perms(&["a.com"]);
        assert_ne!(canonical_permissions(&a), canonical_permissions(&c));
    }

    #[test]
    fn token_rejects_non_hex_input() {
        let p = perms(&["feeds.npr.org"]);
        // Garbage that is not even hex must be rejected before the MAC compare.
        assert!(!verify_token_with_key(TEST_KEY, "global-scan", &p, "not-hex-zz", &compute_token(TEST_KEY, "global-scan", &p, "n")[..1]));
        assert!(!verify_token_with_key(TEST_KEY, "global-scan", &p, "n", "zzzz"));
    }

    // -- restart governor math ------------------------------------------

    #[test]
    fn governor_allows_up_to_max_then_gives_up() {
        let mut g = RestartGovernor::with_limits(Duration::from_secs(300), 3);
        let t0 = Instant::now();
        // 3 restarts allowed within the window.
        assert!(g.should_restart(t0));
        g.record_restart(t0);
        assert!(g.should_restart(t0));
        g.record_restart(t0);
        assert!(g.should_restart(t0));
        g.record_restart(t0);
        // The 4th is refused.
        assert!(!g.should_restart(t0), "4th restart within the window is refused");
        assert_eq!(g.count(t0), 3);
    }

    #[test]
    fn governor_forgets_restarts_outside_the_window() {
        let window = Duration::from_secs(300);
        let t0 = Instant::now();

        // Just past the window: all three have aged out, budget is full again.
        let mut g = RestartGovernor::with_limits(window, 3);
        g.record_restart(t0);
        g.record_restart(t0);
        g.record_restart(t0);
        let later = t0 + window + Duration::from_secs(1);
        assert!(g.should_restart(later), "restarts outside the window are forgotten");
        assert_eq!(g.count(later), 0);

        // At exactly the window boundary they are still counted (the retain
        // keeps marks whose age is <= window). Fresh governor: count() mutates
        // (it evicts), so this must not run after the past-window eviction
        // above.
        let mut g = RestartGovernor::with_limits(window, 3);
        g.record_restart(t0);
        g.record_restart(t0);
        g.record_restart(t0);
        let boundary = t0 + window;
        assert_eq!(g.count(boundary), 3, "marks exactly at the window edge still count");
    }

    // -- name normalization / resolution --------------------------------

    #[test]
    fn app_ref_normalization_collapses_spacing_and_case() {
        assert_eq!(normalize_app_ref("global scan"), "globalscan");
        assert_eq!(normalize_app_ref("Global-Scan"), "globalscan");
        assert_eq!(normalize_app_ref("  GLOBAL  SCAN  "), "globalscan");
        assert_eq!(normalize_app_ref("global-scan"), normalize_app_ref("global scan"));
        assert_eq!(normalize_app_ref(""), "");
    }

    // -- inbound line classification (post-auth, pure) ------------------

    #[test]
    fn inbound_items_relay_as_data_on_the_default_topic() {
        let m = sample_manifest(); // telemetry_topics = ["feed"]
        let line = r#"{"token":"x","type":"items","data":{"brief":"b","items":[]}}"#;
        match classify_inbound_line(&m, "feed", line) {
            RelayDecision::Data { topic, payload } => {
                assert_eq!(topic, "feed");
                assert_eq!(payload["brief"], "b");
            }
            other => panic!("expected Data, got {other:?}"),
        }
    }

    #[test]
    fn inbound_cannot_publish_to_an_undeclared_topic() {
        let m = sample_manifest(); // only "feed" is declared
        // The app asks for a topic it never declared -> falls back to default.
        let line = r#"{"token":"x","type":"status","data":{"topic":"secrets","feeds_ok":3}}"#;
        match classify_inbound_line(&m, "feed", line) {
            RelayDecision::Data { topic, .. } => {
                assert_eq!(topic, "feed", "undeclared topic must not be honored");
            }
            other => panic!("expected Data, got {other:?}"),
        }
        // A DECLARED topic the app names is honored.
        let mut m2 = m.clone();
        m2.ui.telemetry_topics = vec!["feed".into(), "alerts".into()];
        let line = r#"{"token":"x","type":"items","data":{"topic":"alerts"}}"#;
        match classify_inbound_line(&m2, "feed", line) {
            RelayDecision::Data { topic, .. } => assert_eq!(topic, "alerts"),
            other => panic!("expected Data, got {other:?}"),
        }
    }

    #[test]
    fn inbound_log_and_junk_are_classified_correctly() {
        let m = sample_manifest();
        assert_eq!(
            classify_inbound_line(&m, "feed", r#"{"type":"log","data":"hello"}"#),
            RelayDecision::Log { line: "hello".into() }
        );
        // The shape every shipped app actually sends: data={"line":str}.
        assert_eq!(
            classify_inbound_line(&m, "feed", r#"{"type":"log","data":{"line":"hello"}}"#),
            RelayDecision::Log { line: "hello".into() }
        );
        // Empty, non-JSON, and unknown types all drop.
        assert_eq!(classify_inbound_line(&m, "feed", "   "), RelayDecision::Drop);
        assert_eq!(classify_inbound_line(&m, "feed", "not json"), RelayDecision::Drop);
        assert_eq!(
            classify_inbound_line(&m, "feed", r#"{"type":"exec","data":{}}"#),
            RelayDecision::Drop
        );
    }

    #[test]
    fn inbound_modules_report_classifies_as_modules() {
        let m = sample_manifest();
        let line = r#"{"token":"x","type":"modules","data":{"modules":[
            {"path":"/usr/lib/libSystem.B.dylib","uuid":"AAAA"},
            {"path":"/app/main"}
        ]}}"#;
        match classify_inbound_line(&m, "feed", line) {
            RelayDecision::Modules { modules } => {
                assert_eq!(modules.len(), 2);
                assert_eq!(modules[0].path, "/usr/lib/libSystem.B.dylib");
                assert_eq!(modules[0].uuid.as_deref(), Some("AAAA"));
                assert_eq!(modules[1].uuid, None);
            }
            other => panic!("expected Modules, got {other:?}"),
        }
        // A modules report with no usable entries still classifies as Modules
        // (empty) — it is a valid type, just an empty inventory, not a Drop.
        match classify_inbound_line(&m, "feed", r#"{"type":"modules","data":{}}"#) {
            RelayDecision::Modules { modules } => assert!(modules.is_empty()),
            other => panic!("expected empty Modules, got {other:?}"),
        }
    }

    #[test]
    fn inbound_result_classifies_by_id_and_malformed_results_drop() {
        let m = sample_manifest();
        // A well-formed result: id + data route to the ToolResult arm.
        let line = r#"{"token":"x","type":"result","id":"req-7","data":{"answer":42}}"#;
        match classify_inbound_line(&m, "feed", line) {
            RelayDecision::ToolResult { id, payload } => {
                assert_eq!(id, "req-7");
                assert_eq!(payload["answer"], 42);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
        // Missing, empty, and non-string ids are malformed -> Drop (a result
        // that can't be correlated must never be delivered anywhere).
        for bad in [
            r#"{"token":"x","type":"result","data":{"answer":42}}"#,
            r#"{"token":"x","type":"result","id":"","data":{}}"#,
            r#"{"token":"x","type":"result","id":7,"data":{}}"#,
        ] {
            assert_eq!(
                classify_inbound_line(&m, "feed", bad),
                RelayDecision::Drop,
                "should drop: {bad}"
            );
        }
    }

    #[test]
    fn mangled_tool_names_flatten_dots() {
        assert_eq!(mangled_tool_name("jwtpeek.decode"), "app__jwtpeek_decode");
        assert_eq!(mangled_tool_name("subnet.plan"), "app__subnet_plan");
        // Not injective — agent_tools() dedupes; this test just pins the shape.
        assert_eq!(mangled_tool_name("a.b_c"), mangled_tool_name("a_b.c"));
    }

    // -- hermetic socket + token handshake + relay + stop integration ---
    //
    // ONE integration test, hermetic and fast: a tempdir project root and a
    // discovered manifest, the host's REAL per-app socket bound by start(),
    // and a plain in-process UnixStream standing in for the sandboxed app. It
    // exercises the full host path that the seatbelt child would otherwise
    // drive — bind+accept, the "start" command the host sends, token verify on
    // every inbound line, telemetry relay of a VALID line, drop+auth_failed for
    // a FORGED line, and stop() teardown (socket removed, token dead).
    //
    // The APP role (the socket peer) is played in-process for a deterministic
    // relay; the sandboxed child is a stand-in idle /bin/sleep, so we do NOT
    // depend on a real sandboxed Python booting (that bootstrap is environment-
    // coupled and is instead validated by the manual seatbelt probes during
    // development and the pure sbpl_* unit tests above). The test is a macOS
    // seatbelt integration test and skips cleanly where sandbox-exec is absent.
    #[tokio::test]
    async fn socket_token_handshake_relay_and_stop_round_trip() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as TokioBufReader};

        // macOS-only: needs the seatbelt wrapper + Apple's base profile so the
        // stand-in child can launch. Skip cleanly anywhere they are absent.
        if !(Path::new(SANDBOX_EXEC).exists() && Path::new(BSD_BASE_PROFILE).exists()) {
            eprintln!("skipping: sandbox-exec / bsd.sb not present on this host");
            return;
        }

        // A SHORT, NON-SYMLINKED root: AF_UNIX socket paths must fit in SUN_LEN
        // (~104 bytes on macOS) — the default temp dir under /var/folders blows
        // that with the app subpath appended — and /tmp is a symlink to
        // /private/tmp, so seatbelt path filters (which see the resolved path)
        // wouldn't match a /tmp grant. /private/tmp is short and real.
        let root = PathBuf::from(format!(
            "/private/tmp/jrv-it-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                % 1_000_000
        ));
        let app_dir = root.join("apps/echo-app");
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::create_dir_all(root.join("state/ipc/apps")).unwrap();
        std::fs::create_dir_all(root.join("state/apps")).unwrap();

        let manifest = r#"
            [app]
            name = "echo-app"
            version = "0.1.0"
            description = "hermetic test echo app"
            entry = "apps/echo-app/main.py"
            runtime = "python"
            [permissions]
            audio = false
            gpu = false
            net_hosts = []
            fs_read = []
            fs_write = ["state/apps/echo-app"]
            [ui]
            surface = "panel"
            telemetry_topics = ["feed"]
        "#;
        std::fs::write(app_dir.join("manifest.toml"), manifest).unwrap();

        // Subscribe to telemetry BEFORE launch so we catch the relay.
        let mut events = crate::telemetry::subscribe_for_test();

        let mut registry = AppRegistry::discover(&root);
        // Override the interpreter to a stand-in idle child: the host spawns a
        // real sandboxed `/bin/sleep` (proving the live launch path), while the
        // app role over the socket is played in-process below for determinism.
        Arc::get_mut(&mut registry).unwrap().interpreter_override =
            Some(PathBuf::from("/bin/sleep"));
        assert!(registry.resolve_name("echo app").await.is_some(), "app discovered");

        start(&registry, "echo-app").await.unwrap();

        let sock_path = root.join("state/ipc/apps/echo-app.sock");
        let mut waited = 0;
        while !sock_path.exists() && waited < 60 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            waited += 1;
        }
        assert!(sock_path.exists(), "host bound the app socket");

        // The minted token verifies; a forged one is rejected (the exact gate
        // relay_line applies to every inbound line).
        let good_token = {
            let apps = registry.apps.lock().await;
            apps.get("echo-app").unwrap().token.clone()
        };
        assert!(!good_token.is_empty(), "token minted at launch");
        assert!(registry.verify_token("echo-app", &good_token).await);
        assert!(!registry.verify_token("echo-app", "deadbeef").await);

        // Play the app: connect to the host socket, read the host's "start"
        // command, then send a VALID token-stamped items line and a FORGED one.
        let stream = UnixStream::connect(&sock_path).await.expect("connect to host socket");
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = TokioBufReader::new(read_half);

        // The host immediately sends {"type":"start"}.
        let mut start_line = String::new();
        tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut start_line))
            .await
            .expect("host sends a command promptly")
            .expect("read host command");
        let cmd: Value = serde_json::from_str(start_line.trim()).unwrap();
        assert_eq!(cmd["type"], "start", "host kicks the app with a start command");

        // HOST -> APP op forwarding: the router queues a structured op via
        // send_op; the live connection handler must write it VERBATIM to the
        // app socket (after the start command, JSONL-framed). This is the seam
        // the Silicon Canvas voice routing drives.
        send_op(&registry, "echo-app", r#"{"op":"select.net","name":"3V3"}"#)
            .await
            .expect("queue op for a running app");
        let mut op_line = String::new();
        tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut op_line))
            .await
            .expect("host forwards the op promptly")
            .expect("read forwarded op");
        let forwarded: Value = serde_json::from_str(op_line.trim()).unwrap();
        assert_eq!(forwarded["op"], "select.net", "the op tag is forwarded verbatim");
        assert_eq!(forwarded["name"], "3V3", "the op body is forwarded verbatim");

        let good = serde_json::json!({
            "token": good_token, "type": "items",
            "data": {"brief": "hello", "items": [{"title": "t"}]}
        });
        let forged = serde_json::json!({
            "token": "deadbeef", "type": "items", "data": {"brief": "EVIL"}
        });
        write_half
            .write_all(format!("{good}\n{forged}\n").as_bytes())
            .await
            .unwrap();
        write_half.flush().await.unwrap();

        // Drain telemetry: the VALID line relays as app.data on the declared
        // topic; the FORGED line emits app.auth_failed and its payload NEVER
        // appears on the wire.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut saw_data = false;
        let mut saw_auth_failed = false;
        let mut saw_evil = false;
        while Instant::now() < deadline && !(saw_data && saw_auth_failed) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(remaining, events.recv()).await {
                Ok(Ok(line)) => {
                    if line.contains("EVIL") {
                        saw_evil = true;
                    }
                    let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                    if v["event"] == "app.data" && v["data"]["name"] == "echo-app" {
                        saw_data = true;
                        assert_eq!(v["data"]["topic"], "feed", "relayed on the declared topic");
                        assert_eq!(v["data"]["payload"]["brief"], "hello");
                    }
                    if v["event"] == "app.auth_failed" && v["data"]["name"] == "echo-app" {
                        saw_auth_failed = true;
                    }
                }
                _ => break,
            }
        }
        assert!(saw_data, "the valid token-stamped items line was relayed as app.data");
        assert!(saw_auth_failed, "the forged line emitted app.auth_failed");
        assert!(!saw_evil, "a forged line's payload must NEVER be relayed");

        // Stop: the lifecycle task wakes on the notify, reaps the sandboxed
        // child (kill_on_drop) and removes the socket; the token dies with the
        // nonce so a previously-valid token no longer verifies.
        stop(&registry, "echo-app").await.unwrap();
        let mut waited = 0;
        while sock_path.exists() && waited < 80 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            waited += 1;
        }
        assert!(!sock_path.exists(), "socket removed on stop");
        assert!(
            !registry.verify_token("echo-app", &good_token).await,
            "token is dead after stop (nonce cleared)"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The REQUEST/RESPONSE primitive end-to-end over the real host socket:
    /// request_op injects a correlation id and forwards the op; a token-stamped
    /// `result` line with the SAME id resolves the waiter with the payload; a
    /// WRONG-id result is dropped as stale (never mis-delivered); an
    /// unanswered request times out AND evicts its waiter; stop() fails any
    /// still-pending request fast (no dangling until timeout). Same hermetic
    /// harness as the round-trip test above (in-process peer plays the app).
    #[tokio::test]
    async fn request_op_correlates_times_out_and_fails_fast_on_stop() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as TokioBufReader};

        if !(Path::new(SANDBOX_EXEC).exists() && Path::new(BSD_BASE_PROFILE).exists()) {
            eprintln!("skipping: sandbox-exec / bsd.sb not present on this host");
            return;
        }

        let root = PathBuf::from(format!(
            "/private/tmp/jrv-req-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                % 1_000_000
        ));
        let app_dir = root.join("apps/echo-app");
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::create_dir_all(root.join("state/ipc/apps")).unwrap();
        std::fs::create_dir_all(root.join("state/apps")).unwrap();
        std::fs::write(
            app_dir.join("manifest.toml"),
            r#"
            [app]
            name = "echo-app"
            version = "0.1.0"
            description = "hermetic request/response echo app"
            entry = "apps/echo-app/main.py"
            runtime = "python"
            [permissions]
            fs_write = ["state/apps/echo-app"]
            [[tools.exposes]]
            name = "echo.compute"
            scopes = []
            consequential = false
            "#,
        )
        .unwrap();

        let mut registry = AppRegistry::discover(&root);
        Arc::get_mut(&mut registry).unwrap().interpreter_override =
            Some(PathBuf::from("/bin/sleep"));
        start(&registry, "echo-app").await.unwrap();

        let sock_path = root.join("state/ipc/apps/echo-app.sock");
        let mut waited = 0;
        while !sock_path.exists() && waited < 60 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            waited += 1;
        }
        assert!(sock_path.exists(), "host bound the app socket");
        let good_token = {
            let apps = registry.apps.lock().await;
            apps.get("echo-app").unwrap().token.clone()
        };

        let stream = UnixStream::connect(&sock_path).await.expect("connect");
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = TokioBufReader::new(read_half);
        let mut start_line = String::new();
        tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut start_line))
            .await
            .expect("start command promptly")
            .expect("read start");

        // 1) CORRELATION: fire the request, then (as the app) answer a WRONG id
        //    first — it must be dropped as stale — then the REAL id.
        let reg = registry.clone();
        let pending_req = tokio::spawn(async move {
            request_op(
                &reg,
                "echo-app",
                serde_json::json!({"type": "echo.compute", "x": 41}),
                Duration::from_secs(10),
            )
            .await
        });
        let mut op_line = String::new();
        tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut op_line))
            .await
            .expect("op forwarded promptly")
            .expect("read op");
        let op: Value = serde_json::from_str(op_line.trim()).unwrap();
        assert_eq!(op["type"], "echo.compute", "op type forwarded");
        assert_eq!(op["x"], 41, "op args ride top-level");
        let req_id = op["id"].as_str().expect("request_op injected an id").to_string();
        assert!(req_id.starts_with("req-"), "correlation id shape");

        let stale = serde_json::json!({
            "token": good_token, "type": "result", "id": "req-999999", "data": {"answer": -1}
        });
        let real = serde_json::json!({
            "token": good_token, "type": "result", "id": req_id, "data": {"answer": 42}
        });
        write_half
            .write_all(format!("{stale}\n{real}\n").as_bytes())
            .await
            .unwrap();
        write_half.flush().await.unwrap();

        let answer = pending_req.await.unwrap().expect("request answered");
        assert_eq!(answer["answer"], 42, "the CORRELATED payload, not the stale one");

        // 2) TIMEOUT + EVICTION: an unanswered request errs and leaves no waiter.
        let err = request_op(
            &registry,
            "echo-app",
            serde_json::json!({"type": "echo.compute"}),
            Duration::from_millis(200),
        )
        .await
        .expect_err("no answer -> timeout");
        assert!(err.to_string().contains("did not answer"), "honest timeout error: {err}");
        {
            let apps = registry.apps.lock().await;
            let pending = apps.get("echo-app").unwrap().pending.clone();
            assert!(pending.lock().await.is_empty(), "timed-out waiter evicted");
        }
        // Drain the ONE op line the timed-out request produced. (The pending-map
        // assertion above issues no op, and read_line returns after a single newline,
        // so "two" was never right and never mattered.)
        let mut drain = String::new();
        let _ = tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut drain)).await;

        // 3) STOP FAILS PENDING FAST: a request in flight when the app stops
        //    errs immediately with the teardown error, well before its timeout.
        let reg = registry.clone();
        let pending_req = tokio::spawn(async move {
            request_op(
                &reg,
                "echo-app",
                serde_json::json!({"type": "echo.compute"}),
                Duration::from_secs(30),
            )
            .await
        });
        // Let the request register its waiter before stopping.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let t0 = Instant::now();
        stop(&registry, "echo-app").await.unwrap();
        let err = pending_req.await.unwrap().expect_err("stop fails the pending request");
        assert!(
            err.to_string().contains("went away"),
            "honest teardown error: {err}"
        );
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "failed FAST on stop, not at the 30s timeout"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// FLEET LOCKSTEP: every SHIPPED manifest under the repo's real apps/ tree
    /// parses through the actual loader (deny_unknown_fields) AND validates
    /// through the plugin-SDK contract, and every tool-exposing PYTHON app's
    /// main.py both defines the reply_result correlation helper and serves
    /// every non-consequential declared op. This pins the agent-tool contract
    /// repo-wide: a manifest typo, a declared-but-unserved tool (a live
    /// 15s-timeout trap for the model), or a missing id-echo helper fails CI
    /// here instead of surfacing as a runtime timeout.
    #[test]
    fn shipped_manifests_all_validate_and_declared_tools_are_served() {
        let apps_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../apps");
        assert!(apps_root.is_dir(), "repo apps/ tree present");
        let mut tool_decls = 0usize;
        let mut checked_apps = 0usize;
        // (dir_name, manifest text) of one REAL shipped app that validates, used
        // after the loop to build the synthetic net-scope probe.
        let mut net_scope_probe: Option<(String, String)> = None;
        for entry in std::fs::read_dir(&apps_root).unwrap() {
            let dir = entry.unwrap().path();
            let manifest_path = dir.join("manifest.toml");
            if !manifest_path.is_file() {
                continue;
            }
            let dir_name = dir.file_name().unwrap().to_str().unwrap().to_string();
            let raw = std::fs::read_to_string(&manifest_path).unwrap();

            // NO EXEMPTIONS. There used to be an arm here for `fab-link` and
            // `algo-core`: two spec-only apps whose manifests declared a
            // direct-egress `net_hosts` (not grantable on this OS -- see
            // `NET_SCOPE_REFUSAL`), asserted POSITIVELY to be refused so their
            // state could not rot into silence. The owner has since DELETED both
            // apps (docs/BLOCKED_APPS.md carries what they were for, the exact
            // endpoints each needed, why neither can use the fetch proxy, the
            // mechanism each would need, and the git SHA the full source is
            // recoverable from), so the exemption is gone with them and EVERY
            // manifest under apps/ must now validate.
            //
            // The GUARD those two carried is NOT deleted with them -- it is
            // re-pinned below against a SYNTHETIC manifest, so the rule ("a net
            // scope is not grantable") outlives the two examples that happened
            // to trip it.

            // The REAL loader contract (deny_unknown_fields) + the SDK contract.
            let manifest = crate::plugin_sdk::validate_manifest(&raw, &dir_name)
                .unwrap_or_else(|e| panic!("apps/{dir_name}/manifest.toml invalid: {e}"));
            // TAUTOLOGY, KEPT DELIBERATELY AND LABELLED AS ONE. `validate_manifest`
            // refuses a non-empty `net_hosts` outright, so a manifest that reached
            // this line CANNOT carry one and this assertion cannot fail today. It
            // is a structural pin, not evidence: if the refusal is ever narrowed
            // this line becomes load-bearing again. It PROVES NOTHING about the
            // refusal on its own -- `net_scope_probe` below is what proves that.
            assert!(
                manifest.permissions.net_hosts.is_empty(),
                "apps/{dir_name}: a validated manifest can never carry net_hosts"
            );
            // Keep ONE real shipped manifest's text to build the synthetic probe
            // from, after the loop. Built from a manifest that DOES validate, so
            // the only difference between the accepted and the refused form is the
            // net scope itself.
            if net_scope_probe.is_none() {
                net_scope_probe = Some((dir_name.clone(), raw.clone()));
            }
            checked_apps += 1;

            // HARNESS GRANT — checked for EVERY python app that imports the shared
            // apps/_sdk harness, regardless of whether it exposes tools: the
            // import is a HARD launch dependency, and reading apps/_sdk is only
            // permitted if the manifest grants it (else a ModuleNotFoundError
            // crash at launch, uncaught by the Python tests which don't run under
            // the sandbox). Not coupled to the tool-exposition checks below, so a
            // future harness-using app with no tools is still guarded.
            if manifest.app.runtime == Runtime::Python {
                let main_py = dir.join("main.py");
                if let Ok(src) = std::fs::read_to_string(&main_py) {
                    if src.contains("from harness import") {
                        assert!(
                            manifest.permissions.fs_read.iter().any(|p| p == "apps/_sdk"),
                            "apps/{dir_name}: imports the apps/_sdk harness but its manifest fs_read does not grant \"apps/_sdk\" (import would ModuleNotFoundError-crash at launch)"
                        );
                    }
                }
            }

            if manifest.tools.exposes.is_empty() {
                continue;
            }
            if manifest.app.runtime != Runtime::Python {
                continue; // compiled apps serve ops in their own runtimes
            }
            let main_py = dir.join("main.py");
            let src = std::fs::read_to_string(&main_py)
                .unwrap_or_else(|_| panic!("apps/{dir_name}: tool-exposing app has no main.py"));
            for decl in &manifest.tools.exposes {
                if decl.consequential {
                    continue;
                }
                tool_decls += 1;
                // Look for the SERVING COMPARISON (`op == "<name>"`), not just the
                // quoted name anywhere: every app also emits its tool name in the
                // start-status line `{"tool":"<name>",...}`, so a bare-name
                // substring would pass even for a declared-but-UNSERVED tool (the
                // exact 15s-timeout trap this test exists to catch). The `==`
                // comparison is how every app dispatches its op.
                assert!(
                    src.contains(&format!("== \"{}\"", decl.name)),
                    "apps/{dir_name}: declared tool {:?} has no `op == \"{}\"` serving branch \
                     in main.py (a declared-but-unserved tool is offered to the model and times out)",
                    decl.name,
                    decl.name
                );
                // And that branch must answer via the shared harness id-echo
                // helper: the app IMPORTS reply_result from apps/_sdk (the socket
                // loop lives in ONE place now) and CALLS it in a serving branch.
                assert!(
                    src.contains("from harness import") && src.contains("reply_result(conn, msg"),
                    "apps/{dir_name}: main.py must import the apps/_sdk harness and answer via reply_result(conn, msg, ...)"
                );
                // (The apps/_sdk fs_read grant is enforced above for EVERY
                // harness-importing python app, not just tool-exposing ones.)
                assert!(
                    !decl.description.trim().is_empty(),
                    "apps/{dir_name}: tool {:?} needs a model-facing description",
                    decl.name
                );
            }
        }
        assert!(checked_apps >= 30, "the fleet registered ({checked_apps} apps)");
        assert!(tool_decls >= 30, "the agent-tool surface is live ({tool_decls} tools)");

        // THE RULE, PINNED AGAINST A SYNTHETIC MANIFEST -- the guard the two
        // deleted apps used to carry, rewritten so it no longer depends on them
        // existing.
        //
        // "Every shipped manifest validates" and "the validator refuses a net
        // scope" are DIFFERENT CLAIMS, and only the first survives deleting the
        // examples: a validator that had quietly stopped refusing `net_hosts`
        // would pass every assertion above, because no shipped app declares one.
        // So take a manifest that DID validate a moment ago, inject a net scope
        // into it, and require the refusal -- the accepted and the refused text
        // differ by exactly that one permission.
        let (probe_name, probe_raw) = net_scope_probe
            .expect("no shipped manifest was available to build the net-scope probe from");
        assert!(
            crate::plugin_sdk::validate_manifest(&probe_raw, &probe_name).is_ok(),
            "the probe base must be a manifest that VALIDATES, or the refusal below proves nothing"
        );
        // Drop any existing (necessarily EMPTY -- it validated) `net_hosts` line
        // and put a POPULATED one right after `[permissions]`. Line-wise rather
        // than a substring replace: nearly every shipped manifest carries
        // `net_hosts = []` plus a comment mentioning it, and a naive replace
        // would edit the comment and leave the real declaration alone -- a
        // no-op mutation, which is indistinguishable from a surviving one.
        let mut injected = String::new();
        let mut inserted = false;
        for line in probe_raw.lines() {
            if line.trim_start().starts_with("net_hosts") {
                continue;
            }
            injected.push_str(line);
            injected.push('\n');
            if line.trim() == "[permissions]" {
                injected.push_str("net_hosts = [\"voron.local\", \"stream.binance.com\"]\n");
                inserted = true;
            }
        }
        assert!(
            inserted,
            "the injection was a no-op (apps/{probe_name}/manifest.toml has no [permissions] \
             table), so the refusal below would be asserted against an unmodified manifest"
        );
        assert_ne!(
            injected, probe_raw,
            "the injected text is byte-identical to the original -- nothing was mutated"
        );
        assert!(
            injected.contains("net_hosts = [\"voron.local\""),
            "the populated net scope is not in the injected text: {injected}"
        );
        let err = crate::plugin_sdk::validate_manifest(&injected, &probe_name).expect_err(
            "a net scope is NOT GRANTABLE on this OS (macOS SBPL has no host filter): a manifest \
             declaring net_hosts must be refused at validation, whatever hosts it names",
        );
        assert!(
            err.contains("not grantable"),
            "the refusal must name the reason -- an author told the scope is `malformed` will try \
             to reshape it, and there is no shape that works: {err}"
        );
        assert!(
            err.contains("fetch_hosts"),
            "the refusal must name the route that DOES work, or it is a dead end: {err}"
        );
    }

    /// THE SCHEMA DOC'S WORKED EXAMPLE MUST BE AN APP THAT ACTUALLY VALIDATES.
    ///
    /// docs/SANDBOX.md is what an app author copies a manifest from, and its
    /// worked example was `apps/fab-link/manifest.toml` -- a manifest this
    /// daemon REFUSES -- complete with a five-step "at launch, darwind ..."
    /// sequence that never happened on any machine. It was moved to a shipped,
    /// running app; NOTHING made that stick, so nothing stopped the next edit
    /// putting a blocked app back in the one block authors copy. (fab-link and
    /// algo-core are since DELETED -- docs/BLOCKED_APPS.md -- but "the example
    /// names an app whose manifest validates" is the rule, and it is not about
    /// those two.)
    ///
    /// This does. It reads `[app].name` out of the fenced block that follows the
    /// "## Worked example" heading -- bounded at BOTH ends (heading -> opening
    /// fence -> closing fence), so it can neither self-match on the heading nor
    /// run past the block into the rest of the document -- and requires that
    /// app's REAL manifest to pass the SAME validator the loader runs. Point the
    /// doc at any app whose manifest the daemon refuses and this test fails.
    #[test]
    fn the_sandbox_doc_worked_example_names_an_app_whose_manifest_validates() {
        let doc = Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/SANDBOX.md");
        let text = std::fs::read_to_string(&doc).expect("docs/SANDBOX.md is present");
        let after = text
            .split_once("## Worked example")
            .expect("docs/SANDBOX.md still has a Worked example section")
            .1;
        let fenced = after
            .split_once("```toml")
            .expect("the worked example still carries a toml block")
            .1;
        let block = fenced.split_once("```").expect("that toml block is closed").0;
        // The window must not have bound to nothing (a bound-at-both-ends slice
        // can bind so tightly it matches an empty string).
        assert!(
            block.contains("[app]") && block.contains("[permissions]"),
            "the extracted example block is not a manifest -- the window slipped: {block:?}"
        );
        let name = block
            .lines()
            .map(str::trim)
            .find(|l| l.starts_with("name"))
            .and_then(|l| l.split_once('='))
            .map(|(_, v)| v.trim().trim_matches('"').to_string())
            .expect("the worked example declares [app].name");
        assert!(!name.is_empty(), "the example names an app");
        let manifest_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../apps")
            .join(&name)
            .join("manifest.toml");
        let raw = std::fs::read_to_string(&manifest_path).unwrap_or_else(|e| {
            panic!("docs/SANDBOX.md's worked example is apps/{name}, which must be a real shipped app: {e}")
        });
        crate::plugin_sdk::validate_manifest(&raw, &name).unwrap_or_else(|e| {
            panic!(
                "docs/SANDBOX.md's worked example is apps/{name}, whose manifest the daemon REFUSES ({e}) \
                 -- an author copying the schema doc's canonical example would build an app that can never load"
            )
        });
    }

    /// agent_tools() offers ONLY consequential=false declarations, sorted by
    /// app, deduped by mangled name — the exact set the agent loop may invoke.
    #[tokio::test]
    async fn agent_tools_filters_consequential_and_dedupes() {
        let root = PathBuf::from(format!(
            "/private/tmp/jrv-agtools-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                % 1_000_000
        ));
        for (dir, manifest) in [
            (
                "alpha",
                r#"
                [app]
                name = "alpha"
                version = "0.1.0"
                description = "alpha app"
                entry = "apps/alpha/main.py"
                runtime = "python"
                [[tools.exposes]]
                name = "alpha.calc"
                consequential = false
                description = "compute a thing"
                [[tools.exposes.params]]
                name = "x"
                kind = "number"
                required = true
                description = "input"
                [[tools.exposes]]
                name = "alpha.publish"
                consequential = true
                "#,
            ),
            (
                "beta",
                r#"
                [app]
                name = "beta"
                version = "0.1.0"
                description = "beta app"
                entry = "apps/beta/main.py"
                runtime = "python"
                [[tools.exposes]]
                name = "alpha.calc"
                consequential = false
                "#,
            ),
            (
                "delta",
                r#"
                [app]
                name = "delta"
                version = "0.1.0"
                description = "fetch-proxy-capable app"
                entry = "apps/delta/main.py"
                runtime = "python"
                [permissions]
                fetch_hosts = ["example.com"]
                fs_read = ["state/ipc/apps/fetch.sock"]
                [[tools.exposes]]
                name = "delta.pull"
                consequential = false
                "#,
            ),
        ] {
            let d = root.join("apps").join(dir);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("manifest.toml"), manifest).unwrap();
        }
        let registry = AppRegistry::discover(&root);
        let tools = registry.agent_tools().await;
        // alpha.publish (consequential) is filtered; beta's alpha.calc collides
        // with alpha's (same mangled name) and is dropped (alpha sorts first);
        // delta.pull is withheld because delta can egress THROUGH the fetch proxy
        // (non-empty fetch_hosts) -- the "no network side effects" promise stays
        // true by construction.
        //
        // There is no direct-network fixture here any more: a manifest declaring
        // net_hosts is REFUSED at discovery now, so such an app can never reach
        // the registry to be withheld from. The old `gamma` case would still have
        // passed, but vacuously -- skipped as invalid rather than withheld as
        // networked -- which is a test that has stopped testing its own claim.
        assert_eq!(tools.len(), 1, "one invocable tool: {tools:?}");
        assert_eq!(tools[0].app, "alpha");
        assert_eq!(tools[0].decl.name, "alpha.calc");
        assert_eq!(tools[0].decl.params.len(), 1);
        assert_eq!(tools[0].decl.params[0].name, "x");
        assert!(
            !tools.iter().any(|t| t.app == "delta"),
            "a fetch-proxy-capable app's tools are never auto-exposed"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// discover() SKIPS a manifest whose [app].entry resolves outside the app's
    /// own directory (the legacy "python3 main.py" command form / a bare binary
    /// name), reporting it as app.manifest_invalid instead of registering an app
    /// that would fail silently at spawn. A within-dir entry registers normally,
    /// even when the target file is not present (build-state independent — a
    /// binary artifact registers before it is built).
    #[tokio::test]
    async fn discover_rejects_entry_outside_app_dir_and_keeps_valid_ones() {
        let root = PathBuf::from(format!(
            "/private/tmp/jrv-entryguard-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                % 1_000_000
        ));
        // good-app: entry resolves inside its dir (no main.py on disk — the guard
        // is structural, not an existence check).
        let good = root.join("apps/good-app");
        std::fs::create_dir_all(&good).unwrap();
        std::fs::write(
            good.join("manifest.toml"),
            r#"
            [app]
            name = "good-app"
            version = "0.1.0"
            description = "valid entry"
            entry = "apps/good-app/main.py"
            runtime = "python"
            [permissions]
            audio = false
            gpu = false
            net_hosts = []
            fs_read = []
            fs_write = []
            [ui]
            surface = "panel"
            telemetry_topics = ["feed"]
        "#,
        )
        .unwrap();
        // bad-app: the legacy command form resolves to <root>/python3 main.py,
        // OUTSIDE apps/bad-app -> must be skipped.
        let bad = root.join("apps/bad-app");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(
            bad.join("manifest.toml"),
            r#"
            [app]
            name = "bad-app"
            version = "0.1.0"
            description = "entry resolves outside the app dir"
            entry = "python3 main.py"
            runtime = "python"
            [permissions]
            audio = false
            gpu = false
            net_hosts = []
            fs_read = []
            fs_write = []
            [ui]
            surface = "panel"
            telemetry_topics = ["feed"]
        "#,
        )
        .unwrap();

        let mut events = crate::telemetry::subscribe_for_test();
        let registry = AppRegistry::discover(&root);

        assert!(
            registry.resolve_name("good app").await.is_some(),
            "a within-dir entry registers"
        );
        assert!(
            registry.resolve_name("bad app").await.is_none(),
            "an entry resolving outside the app dir is skipped"
        );

        // The skip is REPORTED (not silent): app.manifest_invalid for bad-app.
        let mut saw_invalid = false;
        while let Ok(line) = events.try_recv() {
            if line.contains("app.manifest_invalid") && line.contains("bad-app") {
                saw_invalid = true;
            }
        }
        assert!(saw_invalid, "the skipped manifest is reported as app.manifest_invalid");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// send_op rejects an unknown app and an app that is not running, and drops
    /// the line rather than queueing it for a future launch — a stale op must
    /// never fire on the next start. No socket / no child needed: the gate is
    /// the registry's running flag.
    #[tokio::test]
    async fn send_op_rejects_unknown_and_not_running_apps() {
        let root = PathBuf::from(format!(
            "/private/tmp/jrv-sendop-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                % 1_000_000
        ));
        let app_dir = root.join("apps/echo-app");
        std::fs::create_dir_all(&app_dir).unwrap();
        let manifest = r#"
            [app]
            name = "echo-app"
            version = "0.1.0"
            description = "hermetic test echo app"
            entry = "apps/echo-app/main.py"
            runtime = "python"
            [permissions]
            audio = false
            gpu = false
            net_hosts = []
            fs_read = []
            fs_write = ["state/apps/echo-app"]
            [ui]
            surface = "panel"
            telemetry_topics = ["feed"]
        "#;
        std::fs::write(app_dir.join("manifest.toml"), manifest).unwrap();

        let registry = AppRegistry::discover(&root);

        // Unknown app -> error.
        let err = send_op(&registry, "no-such-app", r#"{"op":"erc.run"}"#)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no micro-app named"), "{err}");

        // Registered but NOT running -> error (the line is dropped, not queued).
        let err = send_op(&registry, "echo-app", r#"{"op":"erc.run"}"#)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not running"), "{err}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Regression (full-OS sweep): a manifest whose entry doesn't exist (a spec-only
    /// app, or an unbuilt compiled one) used to register as fully runnable, then flip
    /// `running` + spawn + die with a confusing exec error. It must register (visible
    /// in the deck), report entry_present false, and refuse to start with a clear
    /// reason.
    #[tokio::test]
    async fn a_spec_only_app_registers_but_is_labeled_not_runnable_and_refuses_to_start() {
        let root = PathBuf::from(format!(
            "/private/tmp/jrv-specapp-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() % 1_000_000
        ));
        let app_dir = root.join("apps/spec-app");
        std::fs::create_dir_all(&app_dir).unwrap();
        // manifest + SPEC.md, but NO main.py at the declared entry.
        std::fs::write(
            app_dir.join("manifest.toml"),
            r#"
            [app]
            name = "spec-app"
            version = "0.1.0"
            description = "spec-only, no code yet"
            entry = "apps/spec-app/main.py"
            runtime = "python"
            [permissions]
            audio = false
            gpu = false
            net_hosts = []
            fs_read = []
            fs_write = []
            "#,
        )
        .unwrap();

        let registry = AppRegistry::discover(&root);
        let info = registry.list().await;
        let spec = info.iter().find(|a| a.name == "spec-app").expect("registers despite no entry");
        assert!(!spec.entry_present, "labeled not-runnable (entry absent)");
        let err = start(&registry, "spec-app").await.expect_err("start refuses a spec-only app");
        assert!(err.to_string().contains("isn't runnable yet"), "honest refusal: {err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// read_line_bounded must be CANCELLATION-SAFE: dropped mid-read (a select!
    /// arm losing the race) it must not lose bytes it already pulled off the
    /// reader. We prove it by driving a partial line, cancelling the read via a
    /// timer that wins a select!, then resuming — the reassembled line must be
    /// WHOLE. (With the accumulator local to the future, as it was before, the
    /// prefix would be consumed-then-dropped and the resumed read would return
    /// only the tail — the exact desync this guards.)
    #[tokio::test]
    async fn read_line_bounded_is_cancellation_safe_across_a_dropped_read() {
        let (mut client, server) = UnixStream::pair().expect("unix socketpair");
        let (read_half, _write_half) = server.into_split();
        let mut reader = BufReader::new(read_half);
        let mut pending: Vec<u8> = Vec::new();
        let mut line = String::new();

        // App sends the FIRST half of a line — no newline yet.
        client.write_all(b"hello wor").await.expect("write prefix");

        // A read races a 50ms timer. read_line_bounded consumes "hello wor" (no
        // newline) then awaits more data; the timer wins, so the read future is
        // DROPPED. The consumed prefix must survive in `pending`.
        tokio::select! {
            _ = read_line_bounded(&mut reader, &mut pending, &mut line, MAX_APP_LINE_BYTES) => {
                panic!("read must not complete before a newline arrives");
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }
        assert_eq!(pending, b"hello wor", "consumed prefix must persist across the cancel");

        // The rest of the line arrives; the next read must return the WHOLE line.
        client.write_all(b"ld\n").await.expect("write suffix");
        let n = read_line_bounded(&mut reader, &mut pending, &mut line, MAX_APP_LINE_BYTES)
            .await
            .expect("read completes");
        assert_eq!(line, "hello world\n", "line reassembled whole after cancellation");
        assert_eq!(n, "hello world\n".len());
        assert!(pending.is_empty(), "pending is cleared once a full line is returned");
    }

    /// DoS DEFENSE (shared by the app-relay socket AND the generate proxy). A line
    /// that exceeds `max` with NO newline must ERROR (so the caller drops the
    /// connection) rather than buffer unboundedly — a hostile app cannot OOM the
    /// daemon by streaming a newline-free flood. We use a tiny `max` and a modest
    /// over-cap write to prove the bound without allocating megabytes.
    #[tokio::test]
    async fn read_line_bounded_errors_on_an_overlong_line_with_no_newline() {
        let (mut client, server) = UnixStream::pair().expect("unix socketpair");
        let (read_half, _write_half) = server.into_split();
        let mut reader = BufReader::new(read_half);
        let mut pending: Vec<u8> = Vec::new();
        let mut line = String::new();
        // 64 bytes, no newline, cap = 16 -> must exceed and error. Keep `client`
        // alive so the reader sees data (not EOF, which would return a trailing line).
        client.write_all(&[b'x'; 64]).await.expect("write flood");
        let err = read_line_bounded(&mut reader, &mut pending, &mut line, 16)
            .await
            .expect_err("an over-cap no-newline line must error, not buffer unboundedly");
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::InvalidData,
            "overflow must be InvalidData so the caller drops the connection"
        );
    }

    /// The cap is EXACT even when the over-long line ends in a newline within one
    /// read: a line whose bytes exceed `max` is rejected, not returned (closes the
    /// one-buffer overshoot on the newline branch).
    #[tokio::test]
    async fn read_line_bounded_errors_on_an_overlong_line_that_ends_in_a_newline() {
        let (mut client, server) = UnixStream::pair().expect("unix socketpair");
        let (read_half, _write_half) = server.into_split();
        let mut reader = BufReader::new(read_half);
        let mut pending: Vec<u8> = Vec::new();
        let mut line = String::new();
        // 40 bytes then a newline, cap = 16 -> the whole line (incl. its terminator)
        // arrives in one chunk; it must ERROR, not return a 40-byte line.
        client.write_all(&[b'x'; 40]).await.expect("write body");
        client.write_all(b"\n").await.expect("write newline");
        let err = read_line_bounded(&mut reader, &mut pending, &mut line, 16)
            .await
            .expect_err("an over-cap line ending in a newline must still error");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    /// DoS DEFENSE for the stdout/stderr LOG relay. An over-long line is TRUNCATED
    /// to the cap (memory bounded), the rest is drained, and logging RESYNCS on the
    /// next line — a hostile app's newline-free flood can't OOM the daemon, and a
    /// normal line after it is still relayed whole.
    #[tokio::test]
    async fn read_capped_log_line_truncates_flood_and_resyncs() {
        // A 100-byte no-newline flood, a newline, then a normal line, then EOF.
        let mut data = vec![b'A'; 100];
        data.push(b'\n');
        data.extend_from_slice(b"next line\n");
        let mut reader = BufReader::new(&data[..]);
        let mut buf: Vec<u8> = Vec::new();

        // First line: capped to 16 bytes (the flood is truncated, not buffered whole).
        read_capped_log_line(&mut reader, &mut buf, 16).await.unwrap().unwrap();
        assert_eq!(buf.len(), 16, "over-long line truncated to the cap");
        assert!(buf.iter().all(|&b| b == b'A'), "kept the leading bytes: {buf:?}");

        // Second line: resynced past the flood, relayed WHOLE.
        read_capped_log_line(&mut reader, &mut buf, 16).await.unwrap().unwrap();
        assert_eq!(buf, b"next line", "logging resyncs on the next line");

        // Clean EOF.
        assert!(
            read_capped_log_line(&mut reader, &mut buf, 16).await.unwrap().is_none(),
            "clean EOF returns None"
        );
    }
    /// AN ON-CONNECT OP IS RE-ARMED ON A CONNECTION THE DAEMON DID NOT START.
    ///
    /// Continuous screen context was armed by ONE `screen.context.start` sent
    /// during `main()`. That can only reach an already-running app, and the
    /// shipped config autostarts nothing — so on a normal boot the op was dropped
    /// ("vision is not running") and NOTHING re-armed when the user opened Vision
    /// later. The ring was empty on every real boot while the config and README
    /// told the user the only missing piece was a Screen Recording grant.
    ///
    /// This drives the real socket: arm the op AFTER the registry exists, start
    /// the app, then play the app and assert the op arrives on THIS connection,
    /// right after `start` — i.e. without anyone calling `send_op`.
    #[tokio::test]
    async fn an_armed_op_is_resent_on_every_connection() {
        use tokio::io::AsyncBufReadExt;
        let root = PathBuf::from(format!(
            "/private/tmp/jrv-arm-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                % 1_000_000
        ));
        let app_dir = root.join("apps/echo-app");
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::create_dir_all(root.join("state/ipc/apps")).unwrap();
        std::fs::create_dir_all(root.join("state/apps")).unwrap();
        std::fs::write(
            app_dir.join("manifest.toml"),
            r#"
            [app]
            name = "echo-app"
            version = "0.1.0"
            description = "hermetic test echo app"
            entry = "apps/echo-app/main.py"
            runtime = "python"
            [permissions]
            audio = false
            gpu = false
            net_hosts = []
            fs_read = []
            fs_write = ["state/apps/echo-app"]
            [ui]
            surface = "panel"
            telemetry_topics = ["feed"]
        "#,
        )
        .unwrap();

        let mut registry = AppRegistry::discover(&root);
        Arc::get_mut(&mut registry).unwrap().interpreter_override =
            Some(PathBuf::from("/bin/sleep"));

        // Arm BEFORE the app has ever connected — the situation at daemon boot.
        let armed = arm_on_connect(&registry, "echo-app", r#"{"op":"screen.context.start"}"#).await;
        assert!(armed, "arming a known app must succeed");
        assert!(
            !arm_on_connect(&registry, "no-such-app", r#"{"op":"x"}"#).await,
            "arming an unknown app must report failure, not silently succeed"
        );

        start(&registry, "echo-app").await.unwrap();
        let sock_path = root.join("state/ipc/apps/echo-app.sock");
        let mut waited = 0;
        while !sock_path.exists() && waited < 60 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            waited += 1;
        }
        assert!(sock_path.exists(), "host bound the app socket");

        // Play the app. Nobody calls send_op here — the arm is the only source.
        let stream = UnixStream::connect(&sock_path).await.expect("connect to host socket");
        let (read_half, _write_half) = stream.into_split();
        let mut reader = tokio::io::BufReader::new(read_half);

        let mut start_line = String::new();
        tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut start_line))
            .await
            .expect("host sends start promptly")
            .expect("read start");
        assert_eq!(
            serde_json::from_str::<Value>(start_line.trim()).unwrap()["type"],
            "start",
            "host kicks the app with start first"
        );

        let mut op_line = String::new();
        tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut op_line))
            .await
            .expect("the armed op must arrive on this connection without a send_op")
            .expect("read armed op");
        let op: Value = serde_json::from_str(op_line.trim()).unwrap();
        assert_eq!(
            op["op"], "screen.context.start",
            "the armed op must be re-sent right after start, on a connection the \
             daemon did not initiate: got {op_line}"
        );

        let _ = stop(&registry, "echo-app").await;
        let _ = std::fs::remove_dir_all(&root);
    }

    /// THE PROFILE MUST ACTUALLY LET A PYTHON APP RUN — proved by running it.
    ///
    /// A framework CPython (Homebrew python@3.x, python.org) ships
    /// `bin/pythonX.Y` as a stub that posix_spawns
    /// `<prefix>/Resources/Python.app/Contents/MacOS/Python`. That SECOND exec had
    /// no grant, so seatbelt denied it and every one of the 36 shipped
    /// `runtime="python"` apps exited 1 before reaching its socket — while
    /// `start()` returned Ok (it only checks the entry .py exists) and the caller
    /// saw a 15s `request_op` timeout, then "crashed too often".
    ///
    /// Two string-matching tests already covered the exec grants and BOTH passed
    /// throughout: they asserted the literals the generator emits, which is the
    /// generator agreeing with itself. This one hands the profile to the real
    /// `sandbox-exec` and makes the interpreter print something.
    ///
    /// macOS-only, and skips itself when the host's interpreter is not a framework
    /// build — where there is no second exec and nothing to prove.
    #[test]
    #[cfg(target_os = "macos")]
    fn a_generated_python_profile_actually_executes_the_interpreter() {
        let interp_real = match std::fs::canonicalize(
            std::process::Command::new("/usr/bin/env")
                .args(["python3", "-c", "import sys; print(sys.executable)"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default(),
        ) {
            Ok(p) => p,
            Err(_) => return, // no usable python3 on this host
        };
        if framework_python_stub(&interp_real).is_none() {
            // Not a framework build: no second exec exists, nothing to regress.
            return;
        }

        let root = PathBuf::from(format!("/private/tmp/jrv-sbx-{}", std::process::id()));
        let app_dir = root.join("apps/probe-app");
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::create_dir_all(root.join("state/ipc/apps")).unwrap();

        let mut m = sample_manifest();
        m.app.name = "probe-app".into();
        m.app.runtime = Runtime::Python;
        // Belt-and-braces: the generator denies all IP network regardless, but
        // keep this explicit so the probe is unambiguously about the interpreter
        // exec chain rather than anything network-shaped.
        m.permissions.net_hosts = Vec::new();
        let profile = generate_sbpl(
            &m,
            &root,
            &interp_real,
            &app_dir,
            &root.join("state/ipc/apps/probe-app.sock"),
        );
        let profile_path = root.join("probe.sb");
        std::fs::write(&profile_path, &profile).unwrap();

        let out = std::process::Command::new(SANDBOX_EXEC)
            .arg("-f")
            .arg(&profile_path)
            .arg(&interp_real)
            .args(["-c", "print('alive')"])
            .output()
            .expect("run sandbox-exec");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let _ = std::fs::remove_dir_all(&root);

        assert!(
            stdout.contains("alive"),
            "the sandboxed interpreter did not run.\nstatus: {:?}\nstderr: {stderr}\n\nprofile:\n{profile}",
            out.status.code()
        );
    }

    /// THE FLIP OF `a_net_hosts_profile_does_not_compile_today` (kept, not
    /// deleted, exactly as that test's own note instructed).
    ///
    /// WAS: a declared `net_hosts` made `generate_sbpl` emit
    /// `(remote tcp (host-name ...))`, which macOS refuses to compile
    /// ("host must be * or localhost"). `sandbox-exec` exited 65, the profile was
    /// rejected, and the app never launched. It failed CLOSED, so there was no
    /// security exposure -- but two shipped apps (fab-link, algo-core, both since
    /// DELETED: docs/BLOCKED_APPS.md) were
    /// unlaunchable and docs/SANDBOX.md described an allow-list this OS never
    /// accepted. The two string-matching tests over those rules passed the whole
    /// time, because they asserted the literals the generator emitted -- the
    /// generator agreeing with itself. Only handing the profile to `sandbox-exec`
    /// ever caught it.
    ///
    /// NOW: the decision has landed. A net scope is refused at validation
    /// (`NET_SCOPE_REFUSAL`), and the generator denies all IP network
    /// unconditionally, so the uncompilable rule can no longer be emitted by any
    /// input. This test asserts BOTH halves of that, and it is the only one that
    /// proves the first half against the real OS compiler rather than against our
    /// own string literals:
    ///   1. a manifest carrying hosts is REFUSED by the validator, and
    ///   2. even if one is built in-process (bypassing the validator), the
    ///      profile it produces COMPILES and grants no network.
    #[test]
    #[cfg(target_os = "macos")]
    fn a_net_hosts_declaration_is_refused_and_can_no_longer_emit_uncompilable_sbpl() {
        // (1) The validator refuses the declaration outright.
        let refused = AppManifest::parse(
            r#"
            [app]
            name        = "probe"
            version     = "0.1.0"
            description = "declares a net scope"
            entry       = "apps/probe/main.py"
            runtime     = "python"

            [permissions]
            net_hosts = ["voron.local"]
            "#,
            "probe",
        );
        let err = format!("{:#}", refused.expect_err("a net scope must be refused"));
        assert!(err.contains("not grantable"), "refusal must name the reason: {err}");
        assert!(err.contains("fetch_hosts"), "refusal must name the route that works: {err}");

        // (2) And the generator can no longer produce the profile macOS rejected,
        //     even when handed a manifest that bypassed (1) entirely.
        let root = PathBuf::from(format!("/private/tmp/jrv-nh-{}", std::process::id()));
        std::fs::create_dir_all(root.join("state/ipc/apps")).unwrap();
        let mut m = sample_manifest();
        m.permissions.net_hosts = vec!["voron.local".into(), "api.binance.com".into()];
        let profile = generate_sbpl(
            &m,
            &root,
            &PathBuf::from("/usr/bin/true"),
            &root.join("apps/probe"),
            &root.join("state/ipc/apps/probe.sock"),
        );
        let path = root.join("nh.sb");
        std::fs::write(&path, &profile).unwrap();
        let out = std::process::Command::new(SANDBOX_EXEC)
            .arg("-f")
            .arg(&path)
            .arg("/usr/bin/true")
            .output()
            .expect("run sandbox-exec");
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        let _ = std::fs::remove_dir_all(&root);
        assert!(
            out.status.success(),
            "the profile must now COMPILE (was exit 65). status: {:?} stderr: {stderr}\n\nprofile:\n{profile}",
            out.status.code()
        );
        assert!(
            !stderr.contains("host must be * or localhost"),
            "the uncompilable host rule must be gone: {stderr}"
        );
        assert!(
            !profile.contains("host-name"),
            "no host filter may reach the profile: {profile}"
        );
    }

    /// A PANIC MUST CLOSE THE LENS, NOT JUST THE MICROPHONE.
    ///
    /// `lockdown::panic()` stops outward actions, autonomy, parked confirmations,
    /// background music, and the mic — the mic because audio.rs re-reads the flag
    /// per chunk. Capture is a different shape: the Vision app is a SEPARATE
    /// PROCESS, so nothing it is told is re-checked, and screen_context.rs and
    /// aperture.rs contain ZERO `is_locked_down` consults. A capture-start after a
    /// panic still opened the lens.
    #[test]
    fn a_capture_start_is_refused_while_locked_down_but_a_read_is_not() {
        // The op NAME is what is matched — never a substring of the line.
        for line in [
            r#"{"op":"watch.start","source":"camera"}"#,
            r#"{"op":"screen.context.start","interval_secs":30}"#,
            r#"{"op":"describe.capture"}"#,
        ] {
            assert!(is_capture_start(line), "{line} starts a capture");
        }
        // Not captures: stopping one, and a read whose QUERY may contain anything.
        for line in [
            r#"{"op":"watch.stop"}"#,
            r#"{"op":"screen.context.stop"}"#,
            r#"{"op":"read.screen"}"#,
            r#"{"op":"select.net","name":"GND"}"#,
        ] {
            assert!(!is_capture_start(line), "{line} is not a capture start");
        }
        // THE TRAP THIS GUARDS: a read whose user-supplied query quotes a capture
        // op name. A substring match would refuse it — and worse, a substring
        // match on "watch.start" inside a query is how a user asking about their
        // own settings would get an unexplained refusal.
        assert!(
            !is_capture_start(r#"{"op":"read.screen","query":"what does watch.start do"}"#),
            "a read whose QUERY mentions a capture op is still a read"
        );
        // Malformed input must not be treated as a capture start (it will fail
        // later on its own terms) — and must not panic.
        assert!(!is_capture_start("not json"));
        assert!(!is_capture_start(""));
    }

    /// ...AND `send_op` ACTUALLY CONSULTS IT.
    ///
    /// The test above pins the predicate. Deleting the check from `send_op` still
    /// compiles and leaves that test green — a correct predicate nobody consults
    /// is exactly the shape of this campaign's other lockdown defect (the MCP
    /// manager's gate was right; it was built before the flag existed). So this
    /// drives the real dispatch with the flag forced on.
    #[tokio::test]
    async fn send_op_refuses_a_capture_start_under_a_live_lockdown() {
        let root = PathBuf::from(format!(
            "/private/tmp/jrv-lockcap-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                % 1_000_000
        ));
        let app_dir = root.join("apps/vision");
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::create_dir_all(root.join("state/ipc/apps")).unwrap();
        std::fs::write(
            app_dir.join("manifest.toml"),
            r#"
            [app]
            name = "vision"
            version = "0.1.0"
            description = "hermetic capture stand-in"
            entry = "apps/vision/main.py"
            runtime = "python"
            [permissions]
            audio = false
            gpu = false
            net_hosts = []
            fs_read = []
            fs_write = ["state/apps/vision"]
            [ui]
            surface = "panel"
            telemetry_topics = ["feed"]
        "#,
        )
        .unwrap();
        let mut registry = AppRegistry::discover(&root);
        Arc::get_mut(&mut registry).unwrap().interpreter_override =
            Some(PathBuf::from("/bin/sleep"));
        start(&registry, "vision").await.unwrap();
        let sock = root.join("state/ipc/apps/vision.sock");
        let mut waited = 0;
        while !sock.exists() && waited < 60 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            waited += 1;
        }

        // PRECONDITION: unlocked, the capture start is accepted — otherwise this
        // would pass against a build that refuses everything.
        {
            let _unlocked = crate::lockdown::LockdownOverride::force(false);
            assert!(
                send_op(&registry, "vision", r#"{"op":"watch.start","source":"camera"}"#)
                    .await
                    .is_ok(),
                "unlocked, a capture start must go through unchanged"
            );
        }

        {
            let _locked = crate::lockdown::LockdownOverride::force(true);
            let err = send_op(&registry, "vision", r#"{"op":"watch.start","source":"camera"}"#)
                .await
                .expect_err("a panic must close the lens, not just the microphone");
            assert!(
                err.to_string().contains("lockdown"),
                "the refusal must name the reason: {err}"
            );
            // ...and STOPPING a capture is still allowed while locked — a gate that
            // blocked the stop would strand a live capture it exists to end.
            assert!(
                send_op(&registry, "vision", r#"{"op":"watch.stop"}"#).await.is_ok(),
                "watch.stop must still be deliverable under lockdown"
            );
        }

        let _ = stop(&registry, "vision").await;
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A PANIC ENDS A CAPTURE ALREADY RUNNING, not just the next one.
    ///
    /// `send_op`'s gate stops a capture from STARTING. The Vision app is a separate
    /// process, so one begun before the panic keeps going until told otherwise —
    /// and nothing was telling it. This drives the real queue and asserts both
    /// stops arrive.
    #[tokio::test]
    async fn a_panic_sends_capture_stops_to_a_running_app() {
        let root = PathBuf::from(format!(
            "/private/tmp/jrv-capstop-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                % 1_000_000
        ));
        let app_dir = root.join("apps/vision");
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::create_dir_all(root.join("state/ipc/apps")).unwrap();
        std::fs::write(
            app_dir.join("manifest.toml"),
            r#"
            [app]
            name = "vision"
            version = "0.1.0"
            description = "hermetic capture stand-in"
            entry = "apps/vision/main.py"
            runtime = "python"
            [permissions]
            audio = false
            gpu = false
            net_hosts = []
            fs_read = []
            fs_write = ["state/apps/vision"]
            [ui]
            surface = "panel"
            telemetry_topics = ["feed"]
        "#,
        )
        .unwrap();
        let mut registry = AppRegistry::discover(&root);
        Arc::get_mut(&mut registry).unwrap().interpreter_override =
            Some(PathBuf::from("/bin/sleep"));
        start(&registry, "vision").await.unwrap();
        let sock = root.join("state/ipc/apps/vision.sock");
        let mut waited = 0;
        while !sock.exists() && waited < 60 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            waited += 1;
        }

        // Play the app and read what the host sends.
        use tokio::io::AsyncBufReadExt;
        let stream = UnixStream::connect(&sock).await.expect("connect");
        let (read_half, _w) = stream.into_split();
        let mut reader = tokio::io::BufReader::new(read_half);
        let mut start_line = String::new();
        tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut start_line))
            .await
            .expect("host sends start")
            .expect("read start");

        stop_all_captures(&registry).await;

        let mut seen: Vec<String> = Vec::new();
        for _ in 0..2 {
            let mut l = String::new();
            tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut l))
                .await
                .expect("a capture stop must arrive")
                .expect("read stop");
            seen.push(l.trim().to_string());
        }
        let joined = seen.join(" ");
        assert!(joined.contains("watch.stop"), "the camera watch must be stopped: {joined}");
        assert!(
            joined.contains("screen.context.stop"),
            "the screen-context loop must be stopped too: {joined}"
        );

        let _ = stop(&registry, "vision").await;
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ...and a panic with nothing running is a no-op, never a failure. An
    /// emergency stop must not depend on a lens being open.
    #[tokio::test]
    async fn stopping_captures_with_no_app_running_is_a_no_op() {
        let root = PathBuf::from(format!("/private/tmp/jrv-capnoop-{}", std::process::id()));
        std::fs::create_dir_all(root.join("state/ipc/apps")).unwrap();
        let registry = AppRegistry::discover(&root);
        stop_all_captures(&registry).await; // must not panic or hang
        let _ = std::fs::remove_dir_all(&root);
    }

}
