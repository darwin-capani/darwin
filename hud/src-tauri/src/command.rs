//! HUD -> daemon COMMAND CHANNEL — the Tauri BACKEND side (the trust boundary).
//!
//! The React layer never speaks to the daemon socket directly and NEVER holds
//! the capability token. It calls the single `send_command` Tauri command with a
//! bounded `{cmd, …}` request; THIS backend:
//!
//!   1. validates the `cmd` against the SAME structural allowlist the daemon
//!      enforces (defense-in-depth — an unknown cmd never even reaches the wire),
//!   2. reads the per-boot capability token from its `0600` handoff file inside
//!      the daemon's confined `state/ipc/` dir (the out-of-band handshake; the
//!      token is never exposed to JS, never logged, never echoed back),
//!   3. opens the local Unix socket `state/ipc/command.sock`, writes ONE JSONL
//!      line carrying the token, reads ONE JSONL reply, and returns the parsed
//!      reply to the UI — token stripped.
//!
//! It can do NOTHING the daemon's command channel cannot: every consequential
//! action STILL parks via the daemon's cross-turn confirmation gate + the
//! OFF-by-default master switch; `confirm {id}` only replays a genuinely-parked
//! action; `dismiss_forge` clears a marker only (apply stays
//! scripts/apply_forge.sh). This module adds NO authority — it is a typed,
//! token-injecting relay over a local socket.
//!
//! SHAPE: [`build_request`] (request assembly + allowlist) and [`parse_reply`]
//! (defensive reply narrowing) are PURE and unit-tested without any socket. The
//! socket round-trip ([`round_trip`]) is the only I/O and is exercised only by
//! the live app, never by a test that binds a daemon.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

/// Cap on a single command line written to the socket (matches the daemon's
/// MAX_LINE_BYTES). A request larger than this is rejected here before any I/O.
const MAX_LINE_BYTES: usize = 8 * 1024;
/// Cap on the free-text payload (ask.text / mission.goal) — matches the daemon's
/// MAX_TEXT_CHARS; we trim here so an oversized field never rides the wire.
const MAX_TEXT_CHARS: usize = 4 * 1024;
/// Read/connect timeout on the socket round-trip. The pipeline bounds its own
/// work; this is the backstop so a hung daemon never wedges the UI thread.
const SOCKET_TIMEOUT: Duration = Duration::from_secs(120);
/// Cap on the reply we read back (a prose reply, never bulk data).
const MAX_REPLY_BYTES: usize = 64 * 1024;

/// The bounded command set the backend will relay — the SAME structural
/// allowlist as daemon/src/command.rs. An unknown `cmd` from JS is rejected here
/// (defense-in-depth) and never reaches the socket.
const ALLOWED_COMMANDS: &[&str] = &[
    "ask", "brief", "mission", "roster", "state", "pending", "confirm", "deny", "dismiss_forge",
    // `policy` is the USER-SET-ONLY consequential-policy write verb: the backend
    // relays the anchored phrase text; the daemon classifies + applies it via the
    // user-only write path (NOT the model tool loop).
    "policy",
    // Task #12 — the panic/lockdown emergency stop. Both are DEDICATED, bare verbs
    // (no fields): the daemon calls lockdown::panic()/unlock() DIRECTLY, never the
    // model. The HUD PANIC button sends `panic`; the deliberate UNLOCK control
    // sends `unlock` (the authenticated-local USER path — there is no model/agent
    // route to unlock). Each reply carries `locked` so the HUD flips its indicator
    // immediately on the button press.
    "panic", "unlock",
];

/// The typed request the React layer hands `send_command`. Every field is
/// optional on the wire so one command shape serves all ten verbs; the
/// per-command requirements are validated in [`build_request`]. There is
/// DELIBERATELY no `token` field — the token is backend-only.
#[derive(Debug, Default, serde::Deserialize)]
pub struct CommandRequest {
    pub cmd: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub goal: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub ts: Option<u64>,
}

/// The reply surfaced to the UI. `ok` mirrors the daemon's `{ok}`; `reply` is the
/// prose line (ask/brief/mission/roster/state/confirm/deny/dismiss_forge);
/// `pending` carries the replay-free pending listing (pending command only);
/// `error` is the daemon's rejection vocabulary or a backend-local error. NO
/// token or secret is ever present.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct CommandReply {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending: Option<PendingSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Task #12 — the lockdown verdict the daemon attaches to the panic/unlock
    /// replies (`{"locked": is_locked_down()}`), so the HUD can flip its LOCKED
    /// DOWN / NORMAL indicator IMMEDIATELY on the button press without waiting for
    /// the next startup snapshot. Present only when the daemon sends it (every
    /// other verb omits it); narrowed by name like every other field, so no extra
    /// material is forwarded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locked: Option<bool>,
}

impl CommandReply {
    fn err(error: impl Into<String>) -> Self {
        Self { ok: false, reply: None, pending: None, error: Some(error.into()), locked: None }
    }
}

/// The replay-FREE pending listing (the `pending` command's payload). Ids +
/// previews only — no input args ever cross the wire, so nothing here can fire
/// an action; only an explicit `confirm {id}` does. Mirrors the daemon's
/// `{confirmation:{id,agent,tool,preview}|null, forge_pending_ts}`.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct PendingSnapshot {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirmation: Option<PendingConfirmation>,
    /// The forge proposal ts (string), or None. The deck shows the manual apply
    /// command for it and offers Dismiss only — never an apply.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forge_pending_ts: Option<String>,
}

/// One genuinely-parked confirmation: id + agent + tool + a faithful preview.
/// NEVER the input args (those stay daemon-side until an explicit confirm).
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct PendingConfirmation {
    pub id: String,
    pub agent: String,
    pub tool: String,
    pub preview: String,
}

/* --------------------------------------------------------- request assembly */

/// Trim a free-text field to [`MAX_TEXT_CHARS`] chars (char-boundary safe), the
/// same clamp the daemon applies — so an oversized field never rides the wire.
fn clamp_text(s: &str) -> String {
    if s.chars().count() <= MAX_TEXT_CHARS {
        return s.to_string();
    }
    s.chars().take(MAX_TEXT_CHARS).collect()
}

/// Build the JSONL request OBJECT for the socket from a typed UI request + the
/// capability token, validating against the structural allowlist and the
/// per-command required fields. PURE — unit-tested. Returns a structured error
/// string (the UI-facing rejection) when the request is not well-formed; the
/// token is injected here and ONLY here (it is never part of the UI request).
pub fn build_request(req: &CommandRequest, token: &str) -> Result<Value, String> {
    if !ALLOWED_COMMANDS.contains(&req.cmd.as_str()) {
        return Err("unknown_command".to_string());
    }
    let mut obj = serde_json::Map::new();
    obj.insert("token".to_string(), json!(token));
    obj.insert("cmd".to_string(), json!(req.cmd));

    match req.cmd.as_str() {
        "ask" => {
            let text = req.text.as_deref().map(clamp_text).unwrap_or_default();
            if text.trim().is_empty() {
                return Err("ask requires non-empty text".to_string());
            }
            obj.insert("text".to_string(), json!(text));
            // An agent ref, when present and non-empty, selects the handling
            // agent (ITS allowlist applies daemon-side).
            if let Some(agent) = req.agent.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
                obj.insert("agent".to_string(), json!(agent));
            }
        }
        "mission" => {
            let goal = req.goal.as_deref().map(clamp_text).unwrap_or_default();
            if goal.trim().is_empty() {
                return Err("mission requires a non-empty goal".to_string());
            }
            obj.insert("goal".to_string(), json!(goal));
        }
        "confirm" | "deny" => {
            let id = req.id.as_deref().map(str::trim).unwrap_or("");
            if id.is_empty() {
                return Err(format!("{} requires an id", req.cmd));
            }
            obj.insert("id".to_string(), json!(id));
        }
        "dismiss_forge" => {
            let ts = req.ts.ok_or_else(|| "dismiss_forge requires a ts".to_string())?;
            obj.insert("ts".to_string(), json!(ts));
        }
        "policy" => {
            let text = req.text.as_deref().map(clamp_text).unwrap_or_default();
            if text.trim().is_empty() {
                return Err("policy requires the phrase text".to_string());
            }
            obj.insert("text".to_string(), json!(text));
        }
        // brief / roster / state / pending carry no extra fields. The task #12
        // emergency-stop verbs `panic` / `unlock` are also bare (the daemon calls
        // lockdown::panic()/unlock() directly — no payload to validate), so they
        // fall through here intentionally: just `{token, cmd}` rides the wire.
        _ => {}
    }
    Ok(Value::Object(obj))
}

/* ----------------------------------------------------------- reply parsing */

/// Defensively narrow one daemon reply line into a [`CommandReply`]. NEVER
/// throws; a malformed/empty reply becomes a structured backend error rather
/// than a panic. Strips everything except the contracted fields, so even if a
/// future daemon echoed extra material, no stray field (and certainly no token)
/// is forwarded to the UI. PURE — unit-tested.
pub fn parse_reply(raw: &str) -> CommandReply {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return CommandReply::err("empty reply from the command channel");
    }
    let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
        return CommandReply::err("malformed reply from the command channel");
    };
    let ok = value.get("ok").and_then(Value::as_bool).unwrap_or(false);
    if !ok {
        let error = value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("command_failed")
            .to_string();
        return CommandReply::err(error);
    }
    // ok == true: surface reply OR the pending snapshot (only one is present per
    // command). We re-build each field by name so nothing extra is forwarded.
    let reply = value.get("reply").and_then(Value::as_str).map(str::to_string);
    let pending = value.get("pending").map(parse_pending);
    // Task #12: the panic/unlock replies carry `locked`; every other verb omits it
    // (so it stays None). Read by name so nothing else is forwarded.
    let locked = value.get("locked").and_then(Value::as_bool);
    CommandReply { ok: true, reply, pending, error: None, locked }
}

/// Narrow the `pending` object into a [`PendingSnapshot`]. Defensive: a missing/
/// malformed confirmation becomes None (no card), and the forge ts is coerced to
/// a string whether the daemon sent a number or a string. No input args are read
/// (the daemon never sends them on this path).
fn parse_pending(v: &Value) -> PendingSnapshot {
    let confirmation = v.get("confirmation").and_then(|c| {
        let id = c.get("id").and_then(Value::as_str)?.to_string();
        if id.is_empty() {
            return None;
        }
        Some(PendingConfirmation {
            id,
            agent: c.get("agent").and_then(Value::as_str).unwrap_or("").to_string(),
            tool: c.get("tool").and_then(Value::as_str).unwrap_or("").to_string(),
            preview: c.get("preview").and_then(Value::as_str).unwrap_or("").to_string(),
        })
    });
    let forge_pending_ts = v.get("forge_pending_ts").and_then(|t| match t {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    });
    PendingSnapshot { confirmation, forge_pending_ts }
}

/* ------------------------------------------------------------- token + I/O */

/// Resolve the JARVIS repo root, reusing the self-heal resolver (JARVIS_ROOT env,
/// else the exe/cwd upward walk to the scripts/apply_heal.sh + config/jarvis.toml
/// markers). The command socket + token file both live under `<root>/state/ipc/`.
fn jarvis_root() -> Result<PathBuf, String> {
    crate::heal::resolve_root_for_command()
}

/// Read the per-boot capability token from its `0600` handoff file inside the
/// daemon's confined `state/ipc/` dir. The token is read ONLY here, held ONLY on
/// the stack for the round-trip, and is NEVER logged, returned to JS, or put in
/// any error string. A missing file means the daemon is not running (or has not
/// finished its handoff) — a structured, secret-free error.
fn read_token(root: &Path) -> Result<String, String> {
    let path = root.join("state").join("ipc").join("command.token");
    let token = std::fs::read_to_string(&path)
        .map_err(|_| "command channel unavailable (is jarvisd running?)".to_string())?;
    let token = token.trim().to_string();
    if token.is_empty() {
        return Err("command channel token is empty".to_string());
    }
    Ok(token)
}

/// The socket path: `<root>/state/ipc/command.sock`.
fn socket_path(root: &Path) -> PathBuf {
    root.join("state").join("ipc").join("command.sock")
}

/// ONE blocking JSONL round-trip over the local Unix socket: connect, write the
/// request line, read the reply line (bounded). The ONLY I/O in this module. The
/// token is already embedded in `line` (built by [`build_request`]); this fn
/// never logs `line`. Returns the raw reply string for [`parse_reply`].
fn round_trip(sock: &Path, line: &str) -> Result<String, String> {
    if line.len() > MAX_LINE_BYTES {
        return Err("oversized".to_string());
    }
    let mut stream = UnixStream::connect(sock)
        .map_err(|_| "command channel unavailable (is jarvisd running?)".to_string())?;
    stream
        .set_read_timeout(Some(SOCKET_TIMEOUT))
        .and_then(|_| stream.set_write_timeout(Some(SOCKET_TIMEOUT)))
        .map_err(|e| format!("socket timeout setup failed: {e}"))?;

    let mut out = line.as_bytes().to_vec();
    if !out.ends_with(b"\n") {
        out.push(b'\n');
    }
    stream
        .write_all(&out)
        .and_then(|_| stream.flush())
        .map_err(|_| "failed to send the command".to_string())?;

    // Read up to the first newline (one JSONL reply), bounded so a misbehaving
    // peer cannot stream unbounded bytes into the UI thread.
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => break, // peer closed
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                buf.push(byte[0]);
                if buf.len() > MAX_REPLY_BYTES {
                    return Err("reply exceeded the size cap".to_string());
                }
            }
            Err(_) => return Err("failed to read the command reply".to_string()),
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/* ----------------------------------------------------------------- command */

/// The single Tauri command the React deck calls. It is the trust boundary to
/// the daemon: it validates the request (structural allowlist + required
/// fields), injects the backend-only capability token, performs ONE local
/// socket round-trip off the async runtime (blocking I/O on a worker), and
/// returns the defensively-parsed reply — token stripped, no secret echoed.
///
/// Errors are surfaced as a `CommandReply` with `ok:false` + a secret-free
/// `error` (so the UI renders a clean failure state) rather than a thrown Tauri
/// error, EXCEPT for the not-well-formed-request case which is a programmer/UI
/// bug worth a hard error.
#[tauri::command]
pub async fn send_command(request: CommandRequest) -> Result<CommandReply, String> {
    // Validate + resolve cheaply (no token, no I/O) before touching the socket.
    if !ALLOWED_COMMANDS.contains(&request.cmd.as_str()) {
        return Ok(CommandReply::err("unknown_command"));
    }

    // Run the token read + blocking socket round-trip off the async runtime.
    tauri::async_runtime::spawn_blocking(move || {
        let root = match jarvis_root() {
            Ok(r) => r,
            Err(e) => return Ok(CommandReply::err(e)),
        };
        let token = match read_token(&root) {
            Ok(t) => t,
            Err(e) => return Ok(CommandReply::err(e)),
        };
        let line = match build_request(&request, &token) {
            Ok(v) => v.to_string(),
            Err(e) => return Ok(CommandReply::err(e)),
        };
        // `token` is dropped at the end of this scope; it never leaves the stack.
        let raw = match round_trip(&socket_path(&root), &line) {
            Ok(r) => r,
            Err(e) => return Ok(CommandReply::err(e)),
        };
        Ok(parse_reply(&raw))
    })
    .await
    .map_err(|e| format!("command task failed: {e}"))?
}

/* --------------------------------------------------------------------- tests */

#[cfg(test)]
mod tests {
    use super::*;

    fn req(cmd: &str) -> CommandRequest {
        CommandRequest { cmd: cmd.to_string(), ..Default::default() }
    }

    #[test]
    fn build_request_admits_only_the_allowlist() {
        // Every allowlisted verb with its required fields builds a valid object.
        let ok = [
            { let mut r = req("ask"); r.text = Some("hi".into()); r },
            req("brief"),
            { let mut r = req("mission"); r.goal = Some("do x".into()); r },
            req("roster"),
            req("state"),
            req("pending"),
            { let mut r = req("confirm"); r.id = Some("abc".into()); r },
            { let mut r = req("deny"); r.id = Some("abc".into()); r },
            { let mut r = req("dismiss_forge"); r.ts = Some(42); r },
            { let mut r = req("policy"); r.text = Some("always allow the gmail_send action".into()); r },
            // Task #12 — the bare emergency-stop verbs build with just {token, cmd}.
            req("panic"),
            req("unlock"),
        ];
        for r in ok {
            let v = build_request(&r, "TOK").expect("known verb builds");
            assert_eq!(v["cmd"], r.cmd);
            assert_eq!(v["token"], "TOK", "token is injected by the backend");
        }
        // Unknown / privileged-sounding verbs are rejected before any I/O.
        for cmd in ["apply_forge", "deploy", "exec", "", "shutdown", "set_switch"] {
            assert_eq!(build_request(&req(cmd), "TOK"), Err("unknown_command".into()));
        }
    }

    #[test]
    fn build_request_enforces_required_fields() {
        assert!(build_request(&req("ask"), "T").is_err()); // no text
        let mut blank = req("ask");
        blank.text = Some("   ".into());
        assert!(build_request(&blank, "T").is_err()); // whitespace text
        assert!(build_request(&req("mission"), "T").is_err()); // no goal
        assert!(build_request(&req("confirm"), "T").is_err()); // no id
        assert!(build_request(&req("deny"), "T").is_err()); // no id
        assert!(build_request(&req("dismiss_forge"), "T").is_err()); // no ts
        assert!(build_request(&req("policy"), "T").is_err()); // no phrase text
        let mut blank_policy = req("policy");
        blank_policy.text = Some("   ".into());
        assert!(build_request(&blank_policy, "T").is_err()); // whitespace phrase
    }

    #[test]
    fn build_request_carries_the_policy_phrase_verbatim() {
        let mut r = req("policy");
        r.text = Some("never allow the x_post action".into());
        let v = build_request(&r, "T").unwrap();
        assert_eq!(v["cmd"], "policy");
        assert_eq!(v["text"], "never allow the x_post action");
        assert_eq!(v["token"], "T", "token injected by the backend");
    }

    #[test]
    fn build_request_carries_the_agent_only_when_present() {
        let mut with = req("ask");
        with.text = Some("status".into());
        with.agent = Some("edith".into());
        let v = build_request(&with, "T").unwrap();
        assert_eq!(v["agent"], "edith");

        // A blank agent is dropped (routes to the orchestrator daemon-side).
        let mut blank = req("ask");
        blank.text = Some("status".into());
        blank.agent = Some("   ".into());
        let v = build_request(&blank, "T").unwrap();
        assert!(v.get("agent").is_none(), "blank agent omitted");
    }

    #[test]
    fn build_request_clamps_oversized_text() {
        let mut r = req("ask");
        r.text = Some("a".repeat(MAX_TEXT_CHARS + 500));
        let v = build_request(&r, "T").unwrap();
        assert_eq!(
            v["text"].as_str().unwrap().chars().count(),
            MAX_TEXT_CHARS,
            "text clamped to the cap before the wire"
        );
    }

    #[test]
    fn parse_reply_narrows_ok_prose() {
        let r = parse_reply(r#"{"ok":true,"reply":"Roll call complete."}"#);
        assert!(r.ok);
        assert_eq!(r.reply.as_deref(), Some("Roll call complete."));
        assert!(r.error.is_none());
        assert!(r.pending.is_none());
    }

    #[test]
    fn parse_reply_narrows_pending_snapshot() {
        let r = parse_reply(
            r#"{"ok":true,"pending":{"confirmation":{"id":"deadbeef","agent":"agent.pepper","tool":"gmail_send","preview":"Would email Alice"},"forge_pending_ts":"1770000000"}}"#,
        );
        assert!(r.ok);
        let p = r.pending.expect("pending present");
        let c = p.confirmation.expect("confirmation present");
        assert_eq!(c.id, "deadbeef");
        assert_eq!(c.tool, "gmail_send");
        assert_eq!(c.preview, "Would email Alice");
        assert_eq!(p.forge_pending_ts.as_deref(), Some("1770000000"));
    }

    #[test]
    fn parse_reply_coerces_a_numeric_forge_ts() {
        let r = parse_reply(r#"{"ok":true,"pending":{"confirmation":null,"forge_pending_ts":1770000000}}"#);
        let p = r.pending.unwrap();
        assert!(p.confirmation.is_none(), "null confirmation -> no card");
        assert_eq!(p.forge_pending_ts.as_deref(), Some("1770000000"));
    }

    #[test]
    fn parse_reply_surfaces_daemon_rejections() {
        for err in ["unauthorized", "unknown_command", "rate_limited", "oversized", "malformed"] {
            let line = format!(r#"{{"ok":false,"error":"{err}"}}"#);
            let r = parse_reply(&line);
            assert!(!r.ok);
            assert_eq!(r.error.as_deref(), Some(err));
        }
    }

    #[test]
    fn build_request_admits_the_bare_panic_and_unlock_verbs() {
        // Task #12: panic/unlock are DEDICATED bare verbs — they build with just
        // {token, cmd} and carry NO payload (the daemon calls lockdown directly).
        for cmd in ["panic", "unlock"] {
            let v = build_request(&req(cmd), "TOK").expect("bare verb builds");
            assert_eq!(v["cmd"], cmd);
            assert_eq!(v["token"], "TOK");
            // No stray fields beyond token + cmd.
            assert_eq!(v.as_object().unwrap().len(), 2, "{cmd} carries only token+cmd");
        }
    }

    #[test]
    fn parse_reply_surfaces_the_lockdown_locked_flag() {
        // Task #12: the panic reply flips the indicator -> locked true; the unlock
        // reply -> locked false. Every other verb omits the field (stays None).
        let panic = parse_reply(r#"{"ok":true,"reply":"Lockdown engaged.","locked":true}"#);
        assert!(panic.ok);
        assert_eq!(panic.locked, Some(true));
        let unlock = parse_reply(r#"{"ok":true,"reply":"Lockdown lifted.","locked":false}"#);
        assert_eq!(unlock.locked, Some(false));
        // A plain reply (no locked field) leaves it absent so it never falsely flips.
        let plain = parse_reply(r#"{"ok":true,"reply":"Roll call complete."}"#);
        assert_eq!(plain.locked, None);
    }

    #[test]
    fn parse_reply_never_throws_on_junk() {
        for junk in ["", "   ", "not json", "[1,2,3]", "{", "null", "42"] {
            let r = parse_reply(junk);
            assert!(!r.ok, "junk yields a clean error for {junk:?}");
            assert!(r.error.is_some());
        }
    }

    #[test]
    fn parse_reply_drops_a_confirmation_with_no_id() {
        // A confirmation object lacking a usable id is not surfaced as a card.
        let r = parse_reply(r#"{"ok":true,"pending":{"confirmation":{"id":"","tool":"x"}}}"#);
        let p = r.pending.unwrap();
        assert!(p.confirmation.is_none());
    }

    #[test]
    fn reply_serialization_never_carries_a_token_or_extra_fields() {
        // Even if the daemon echoed a token (it must not), parse_reply rebuilds
        // by name, so the serialized reply has no token/secret-shaped field.
        let r = parse_reply(r#"{"ok":true,"reply":"hi","token":"LEAK","secret":"sk-XXX"}"#);
        let s = serde_json::to_string(&r).unwrap();
        assert!(!s.contains("LEAK"));
        assert!(!s.contains("sk-XXX"));
        assert!(!s.contains("token"));
    }
}
