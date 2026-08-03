//! #35 WEBHOOK TRIGGERS — the daemon's FIRST inbound INTERNET-adjacent surface,
//! so the most security-sensitive module in this build. An external relay (a CI
//! system, a smart-home hub, a local tunnel) can POST a small signed event that
//! maps to a DARWIN intent — but only under the strongest fences:
//!
//!   1. HMAC AUTH over the RAW body. Every request carries an
//!      `X-Darwin-Signature: sha256=<hex>` header; the receiver recomputes
//!      `HMAC-SHA256(secret, raw_body)` and compares CONSTANT-TIME. A missing,
//!      malformed, forged, or stale signature is REJECTED (401-equivalent) and
//!      NEVER routes — an unauthenticated request reaches no intent at all.
//!   2. EXPLICIT event->intent ALLOWLIST. The authenticated event name is looked
//!      up in the config-defined `[[webhooks.mappings]]`. An event with no
//!      mapping is REJECTED (404-equivalent) — never guessed into some intent.
//!   3. ROUTE through the NORMAL path, and if the mapped intent is CONSEQUENTIAL
//!      ([`crate::confirm::is_consequential_tool`]) the action PARKS for the
//!      user's spoken confirm (or is rejected) — a webhook can NEVER satisfy the
//!      cross-turn spoken confirmation, so it can never auto-execute a
//!      side-effecting action. The existing gate (the armed-by-default
//!      `[integrations].allow_consequential` master switch — ON, but a confirmed
//!      action still needs a fresh confirm — + the per-turn confirm + voice-id +
//!      lockdown + the agent allowlist + policy) is intact and unbypassed; the
//!      webhook PARKS into exactly that gate, it does not route around it.
//!
//! LIVE LISTENER. [`serve`] binds 127.0.0.1 LOOPBACK by default
//! (`[webhooks].bind`) and ships ON (`[webhooks].enabled = true`) but is INERT
//! WITHOUT MAPPINGS + A KEYCHAIN HMAC SECRET — with no secret it refuses to open
//! the port at all (fail-closed: nothing could ever authenticate). The HMAC
//! secret is resolved from the macOS Keychain (account
//! [`WEBHOOK_SECRET_ACCOUNT`]), NEVER from the config TOML / a log / Debug.
//!
//! This doc used to say the bind was "runtime-gated ... not exercised in tests".
//! That was a euphemism: `serve` contained no socket primitive of any kind, so
//! the feature did not exist at runtime, while this comment, the config, docs/,
//! the HUD panel and an INFO log all asserted a working receiver. A correctly
//! signed, correctly mapped delivery got connection-refused forever. The accept
//! loop is now real and [`serve_conn`] IS exercised — over a real loopback socket
//! — for accept, forgery, unmapped events, the body cap, an unterminated head,
//! and a non-POST method.
//!
//! HERMETIC. The whole AUTHORIZE-then-MAP-then-CLASSIFY decision is the PURE
//! [`handle_webhook`] (a valid-HMAC mapped event -> Route; bad/missing HMAC ->
//! Unauthorized; an unmapped event -> Unmapped; a consequential mapping ->
//! ParkForConfirm, NEVER Execute). The unit tests drive it with synthetic signed
//! requests and a fixed secret — no socket, no port, no network. The body and
//! the secret are NEVER logged.

use std::net::IpAddr;
use std::sync::Arc;

use hmac::{Hmac, KeyInit, Mac};
use serde_json::json;
use sha2::Sha256;
use tracing::{debug, info, warn};

use crate::config::{Config, WebhookMapping};
use crate::telemetry;

type HmacSha256 = Hmac<Sha256>;

/// The macOS Keychain account holding the webhook HMAC shared secret. Resolved
/// via the same `integrations::resolve_secret` machinery as the other secrets;
/// the secret VALUE never appears in the config TOML, a log, or `Debug`.
pub const WEBHOOK_SECRET_ACCOUNT: &str = "webhook_hmac_secret";

/// The signature header an inbound request must carry. The value is
/// `sha256=<hex>` where `<hex>` is `HMAC-SHA256(secret, raw_body)`.
pub const SIGNATURE_HEADER: &str = "x-darwin-signature";
/// The event-name header. (A body `event` field is also honored as a fallback,
/// but the header is the canonical channel.)
pub const EVENT_HEADER: &str = "x-darwin-event";

/// The decision [`handle_webhook`] reaches for one inbound request, BEFORE any
/// side effect. PURE and exhaustively unit-tested: every reject path is distinct
/// and an authenticated consequential mapping resolves to [`ParkForConfirm`],
/// never an execute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebhookDecision {
    /// HMAC verified, event mapped, and the intent is NON-consequential: route it
    /// through the normal path. Carries the resolved intent to dispatch.
    Route { event: String, intent: String },
    /// HMAC verified, event mapped, but the intent is CONSEQUENTIAL: PARK for the
    /// user's spoken confirm. A webhook can never satisfy the cross-turn confirm,
    /// so this NEVER becomes an execute — it surfaces a parked action the user
    /// must confirm out-of-band (voice / the authenticated command channel).
    ParkForConfirm { event: String, intent: String },
    /// The signature was missing, malformed, forged, or stale. 401-equivalent:
    /// the request reaches NO intent. (Also covers a secret that is unavailable —
    /// fail-closed: with no secret, nothing authenticates.)
    Unauthorized,
    /// The signature verified but the event name is not in the allowlist.
    /// 404-equivalent: rejected, never guessed into some intent.
    Unmapped { event: String },
    /// The body exceeded the configured cap, or the request was otherwise
    /// unparseable. 400-equivalent.
    BadRequest,
}

/// How many webhook connections may be in flight at once. A loopback relay is
/// trusted to behave; any local process that can reach loopback is not.
const MAX_CONCURRENT_CONNS: usize = 16;

/// Whole-exchange deadline. A client that connects and goes quiet must not hold
/// a concurrency permit indefinitely.
const EXCHANGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Hard cap on the request HEAD. Bounds a client that never sends the blank line.
const MAX_HEAD_BYTES: usize = 8 * 1024;

/// Is `bind` a loopback address? The receiver refuses to bind anything else by
/// default — it is for a LOCAL relay/tunnel, never a public internet listener.
/// Pure + tiny so the bind guard is unit-testable without opening a socket.
pub fn is_loopback_bind(bind: &str) -> bool {
    match bind.parse::<IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        // A hostname is refused: we require an explicit loopback IP literal so the
        // guard can never be fooled by a name that resolves off-host.
        Err(_) => false,
    }
}

/// Parse a `sha256=<hex>` signature header into its raw hex digest. Returns the
/// hex string (lowercased) or `None` for a missing prefix / empty value.
fn parse_signature(header: &str) -> Option<String> {
    let hex = header.trim().strip_prefix("sha256=")?;
    if hex.is_empty() {
        return None;
    }
    Some(hex.to_ascii_lowercase())
}

/// Compute the hex HMAC-SHA256 over the RAW body with `secret`. Pure given the
/// secret — the unit tests sign synthetic bodies with a fixed secret to prove
/// the accept/reject paths without a live daemon or Keychain. Also the public
/// signing primitive a trusted relay/signer mirrors to produce the
/// `X-Darwin-Signature` header (the verify side, [`verify_signature`], is the
/// daemon's; this is the counterpart for documenting/exercising the contract).
#[allow(dead_code)] // public signing primitive; exercised by the hermetic tests.
pub fn sign_body(secret: &[u8], raw_body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(raw_body);
    hex::encode(mac.finalize().into_bytes())
}

/// Constant-time verify of a presented signature over the raw body. Recompute
/// and compare with the MAC's own constant-time `verify_slice` (never a `==` on
/// the hex string), exactly like the per-app capability-token check.
fn verify_signature(secret: &[u8], raw_body: &[u8], presented_hex: &str) -> bool {
    let Ok(presented) = hex::decode(presented_hex) else {
        return false;
    };
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(raw_body);
    mac.verify_slice(&presented).is_ok()
}

/// Resolve the event name for a request: the `X-Darwin-Event` header wins; a
/// top-level `event` string in the JSON body is honored as a fallback.
fn resolve_event(event_header: Option<&str>, raw_body: &[u8]) -> Option<String> {
    if let Some(h) = event_header {
        let h = h.trim();
        if !h.is_empty() {
            return Some(h.to_string());
        }
    }
    serde_json::from_slice::<serde_json::Value>(raw_body)
        .ok()
        .and_then(|v| v.get("event").and_then(|e| e.as_str()).map(str::to_string))
}

/// Look up an event in the explicit allowlist, returning the mapped intent.
fn map_event<'a>(mappings: &'a [WebhookMapping], event: &str) -> Option<&'a str> {
    mappings
        .iter()
        .find(|m| m.event == event)
        .map(|m| m.intent.as_str())
}

/// THE pure webhook decision. AUTHENTICATE (constant-time HMAC over the raw
/// body) -> MAP (explicit event->intent allowlist) -> CLASSIFY (consequential ->
/// park, else route). Takes everything by value/slice so it has no I/O: the unit
/// tests drive it directly with a fixed secret and synthetic signed bodies.
///
/// ORDER MATTERS and is security-load-bearing: authentication is checked FIRST,
/// so an unmapped/over-body/unknown event from an UNAUTHENTICATED caller still
/// returns [`WebhookDecision::Unauthorized`] — a forger learns nothing about the
/// mapping table. Only an authenticated request proceeds to the allowlist.
///
/// `secret` is `None` when the Keychain has no webhook secret configured; with no
/// secret NOTHING authenticates (fail-closed -> [`WebhookDecision::Unauthorized`]).
pub fn handle_webhook(
    raw_body: &[u8],
    signature_header: Option<&str>,
    event_header: Option<&str>,
    secret: Option<&[u8]>,
    cfg: &Config,
) -> WebhookDecision {
    // (0) Body-size bound. An oversized body is rejected before any work.
    if raw_body.len() > cfg.webhooks.max_body_bytes {
        return WebhookDecision::BadRequest;
    }

    // (1) AUTHENTICATE — constant-time HMAC over the RAW body, FIRST. Fail-closed
    // on a missing secret, a missing/malformed signature header, or a bad MAC.
    let Some(secret) = secret else {
        // No secret configured -> nothing can authenticate.
        return WebhookDecision::Unauthorized;
    };
    let Some(sig_header) = signature_header else {
        return WebhookDecision::Unauthorized;
    };
    let Some(presented) = parse_signature(sig_header) else {
        return WebhookDecision::Unauthorized;
    };
    if !verify_signature(secret, raw_body, &presented) {
        return WebhookDecision::Unauthorized;
    }

    // (2) MAP — the event name (header or body) must be in the explicit allowlist.
    let Some(event) = resolve_event(event_header, raw_body) else {
        // Authenticated but no event name at all: there is nothing to map.
        return WebhookDecision::Unmapped {
            event: String::new(),
        };
    };
    let Some(intent) = map_event(&cfg.webhooks.mappings, &event) else {
        return WebhookDecision::Unmapped { event };
    };
    let intent = intent.to_string();

    // (3) CLASSIFY — a consequential intent PARKS for a spoken confirm (a webhook
    // can never satisfy the cross-turn confirm), it is NEVER auto-executed. A
    // non-consequential (read-only) intent routes normally.
    if crate::confirm::is_consequential_tool(&intent) {
        WebhookDecision::ParkForConfirm { event, intent }
    } else {
        WebhookDecision::Route { event, intent }
    }
}

/// Emit the decision onto telemetry for the HUD — event name + intent + outcome
/// ONLY. NEVER the body, NEVER the secret, NEVER the signature. The HUD's
/// webhook panel renders purely from these.
fn emit_decision(decision: &WebhookDecision) {
    let (outcome, event, intent) = match decision {
        WebhookDecision::Route { event, intent } => ("routed", event.as_str(), intent.as_str()),
        WebhookDecision::ParkForConfirm { event, intent } => {
            ("parked", event.as_str(), intent.as_str())
        }
        WebhookDecision::Unauthorized => ("unauthorized", "", ""),
        WebhookDecision::Unmapped { event } => ("unmapped", event.as_str(), ""),
        WebhookDecision::BadRequest => ("bad_request", "", ""),
    };
    telemetry::emit(
        "system",
        "webhook.received",
        json!({"outcome": outcome, "event": event, "intent": intent}),
    );
}

/// Apply a webhook decision: emit the secret-free telemetry, and for a
/// CONSEQUENTIAL mapping PARK the action into the EXISTING cross-turn
/// confirmation gate (so the user confirms it out-of-band by voice / the
/// authenticated command channel) — a webhook NEVER auto-executes it. A
/// non-consequential `Route` is surfaced for the normal pipeline to pick up; a
/// reject path does nothing but record the secret-free outcome. Returns the
/// resolved intent for a Route, so the live accept-loop can hand it to the router.
///
/// This is the LIVE seam that ties the pure [`handle_webhook`] to the gate. It is
/// reached only from [`serve`]; the decision logic it applies is
/// the pure handler the tests prove.
fn apply_decision(decision: &WebhookDecision) -> Option<String> {
    emit_decision(decision);
    match decision {
        WebhookDecision::Route { intent, .. } => Some(intent.clone()),
        WebhookDecision::ParkForConfirm { event, intent } => {
            // Park into the SAME single-slot confirmation gate the router/tool
            // loop use. The action carries a faithful, secret-free preview (the
            // event + intent only — never the body). The user must confirm it on
            // a later turn (voice / the authenticated command channel); a webhook
            // cannot satisfy the cross-turn confirm, so this never executes here.
            let preview = format!(
                "A webhook event '{event}' wants to run the consequential action '{intent}'"
            );
            // DOES NOT REACH THE USER. An inbound webhook parks on its own schedule;
            // nobody was prompted this turn. Arming PROMPTED here would let it steal a
            // "confirm" the operator spoke for something else entirely — exactly the
            // hijack take_live exists to block.
            crate::confirm::park_ctx(false, crate::confirm::PendingConfirmation {
                agent: "orchestrator".to_string(),
                tool: intent.clone(),
                // No replay material from the wire: the body is never trusted to
                // build the action's input. An empty input means a confirm
                // re-derives nothing from the (untrusted) webhook payload.
                input: serde_json::Value::Null,
                allowed: Vec::new(),
                preview,
                created_at: std::time::Instant::now(),
                id: String::new(),
                // A webhook-parked intent carries no replay input, so no structured
                // planner applies -> text preview path, unchanged.
                plan: None,
            });
            None
        }
        // Reject paths: the secret-free outcome was already emitted; nothing routes.
        WebhookDecision::Unauthorized
        | WebhookDecision::Unmapped { .. }
        | WebhookDecision::BadRequest => None,
    }
}

/// LIVE LISTENER — RUNTIME-GATED. Bind the loopback receiver and accept signed
/// requests, dispatching each through the PURE [`handle_webhook`] and then
/// [`apply_decision`] (which parks a consequential mapping into the existing
/// gate). This is wired behind `[webhooks].enabled` and is NEVER reached in tests
/// (no port is opened): the precedent is the always-listening mic loop and the
/// live ScreenCaptureKit capture, both device/runtime-gated while their pure
/// cores are proven hermetically. The security-load-bearing decision
/// ([`handle_webhook`]) is the part the tests exercise.
///
/// HONESTY: this function is the wired-but-not-test-exercised leg. It refuses to
/// bind a non-loopback address and returns immediately when the flag is off or no
/// secret is configured (fail-closed). The full HTTP request framing of the live
/// accept-loop is intentionally out of the hermetic scope; the moment a request
/// IS framed, it flows through exactly the pure handler + [`apply_decision`] seam
/// below — so the live path can never auth/map/execute differently from the
/// proven core.
#[allow(dead_code)] // wired behind the OFF-default flag; the bind is runtime-gated, not tested.
pub async fn serve(cfg: Arc<Config>) {
    if !cfg.webhooks.enabled {
        info!("webhook receiver disabled ([webhooks].enabled = false); not binding");
        return;
    }
    if !is_loopback_bind(&cfg.webhooks.bind) {
        warn!(
            bind = %cfg.webhooks.bind,
            "webhook receiver refuses a non-loopback bind; not binding (loopback only)"
        );
        return;
    }
    // Fail-closed: with no secret, nothing could ever authenticate, so do not
    // even open the port.
    let Some(secret) = crate::integrations::resolve_secret(WEBHOOK_SECRET_ACCOUNT).await else {
        warn!(
            account = WEBHOOK_SECRET_ACCOUNT,
            "webhook receiver has no HMAC secret in the Keychain; not binding (fail-closed)"
        );
        return;
    };
    let addr = format!("{}:{}", cfg.webhooks.bind, cfg.webhooks.port);
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            warn!(%addr, error = %e, "webhook receiver could NOT bind; the receiver is DOWN");
            return;
        }
    };
    info!(
        %addr,
        mappings = cfg.webhooks.mappings.len(),
        "webhook receiver LISTENING (loopback, HMAC-authenticated)"
    );

    // BOUNDED CONCURRENCY. A loopback relay is trusted to be well-behaved; a
    // process that can reach loopback is not necessarily. Without a cap, a client
    // opening connections faster than they are served would grow tasks without
    // limit inside the daemon.
    let sem = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_CONNS));
    loop {
        let (sock, peer) = match listener.accept().await {
            Ok(v) => v,
            // A transient accept error must not kill the receiver for the rest of
            // the process's life.
            Err(e) => {
                warn!(error = %e, "webhook accept failed; continuing");
                continue;
            }
        };
        let Ok(permit) = sem.clone().acquire_owned().await else { return };
        let cfg = cfg.clone();
        let secret = secret.clone();
        tokio::spawn(async move {
            let _permit = permit;
            // WHOLE-EXCHANGE DEADLINE. A client that connects and then goes silent
            // must not hold a permit forever.
            match tokio::time::timeout(EXCHANGE_TIMEOUT, serve_conn(sock, &cfg, &secret)).await {
                Ok(Err(e)) => debug!(?peer, error = %e, "webhook connection ended early"),
                Err(_) => debug!(?peer, "webhook connection timed out"),
                Ok(Ok(())) => {}
            }
        });
    }
}

/// One HTTP/1.1 request/response exchange.
///
/// Deliberately a minimal reader rather than an HTTP crate: this endpoint accepts
/// exactly one shape — a POST with two headers and a bounded body — and every
/// byte it reads is attacker-influenced, so a small surface that refuses anything
/// unexpected is worth more here than generality.
///
/// The decision itself is NOT made here. Headers are lowercased and handed to
/// [`handle_webhook`], the same pure, hermetically-tested core the unit tests
/// drive, so the live path cannot diverge from the proven one.
async fn serve_conn(
    mut sock: tokio::net::TcpStream,
    cfg: &Config,
    secret: &str,
) -> std::io::Result<()> {
    use tokio::io::AsyncReadExt;

    // -- head, with a hard byte cap so a client that never sends \r\n\r\n cannot
    //    make the daemon buffer without bound.
    let mut head = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= MAX_HEAD_BYTES {
            return respond(&mut sock, 431, "Request Header Fields Too Large").await;
        }
        if sock.read(&mut byte).await? == 0 {
            return Ok(()); // client hung up mid-head
        }
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).to_string();
    let mut lines = head.lines();
    let Some(request_line) = lines.next() else {
        return respond(&mut sock, 400, "Bad Request").await;
    };
    if !request_line.starts_with("POST ") {
        // Only POST carries a webhook. Anything else is a probe, not a delivery.
        return respond(&mut sock, 405, "Method Not Allowed").await;
    }

    // HTTP header names are case-insensitive; the pure core looks them up by their
    // canonical lowercase names, so normalize here — at the framing edge — rather
    // than teaching the core about casing.
    let mut headers: std::collections::HashMap<String, String> = Default::default();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    // -- body. REJECT ON THE DECLARED LENGTH, BEFORE READING A BYTE OF IT. The
    //    config promises "a larger body is rejected rather than buffered"; checking
    //    after the read would make that promise false in exactly the case it exists
    //    for.
    let declared: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    if declared > cfg.webhooks.max_body_bytes {
        warn!(declared, cap = cfg.webhooks.max_body_bytes, "webhook body over cap; rejected unread");
        return respond(&mut sock, 413, "Payload Too Large").await;
    }
    let mut body = vec![0u8; declared];
    if declared > 0 {
        sock.read_exact(&mut body).await?;
    }

    let sig = headers.get(SIGNATURE_HEADER).map(String::as_str);
    let evt = headers.get(EVENT_HEADER).map(String::as_str);
    let decision = handle_webhook(&body, sig, evt, Some(secret.as_bytes()), cfg);
    let (code, reason) = status_for(&decision);
    apply_decision(&decision);
    respond(&mut sock, code, reason).await
}

/// The HTTP status each decision answers with. Split out so the mapping is
/// assertable without a socket — an Unauthorized that answered 200 would tell a
/// prober their forged signature worked.
fn status_for(d: &WebhookDecision) -> (u16, &'static str) {
    match d {
        WebhookDecision::Route { .. } | WebhookDecision::ParkForConfirm { .. } => (200, "OK"),
        WebhookDecision::Unauthorized => (401, "Unauthorized"),
        WebhookDecision::Unmapped { .. } => (404, "Not Found"),
        WebhookDecision::BadRequest => (400, "Bad Request"),
    }
}

/// A bare, bodyless response. Deliberately says nothing about WHY beyond the
/// status: a detailed error is a probing oracle for an unauthenticated caller.
async fn respond(
    sock: &mut tokio::net::TcpStream,
    code: u16,
    reason: &str,
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let msg = format!("HTTP/1.1 {code} {reason}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
    sock.write_all(msg.as_bytes()).await?;
    sock.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    /// A fixed secret + a config with a known mapping set, for synthetic signing.
    const SECRET: &[u8] = b"hermetic-test-shared-secret-not-a-real-one";

    fn cfg_with_mappings(pairs: &[(&str, &str)]) -> Config {
        let mut cfg = Config::default();
        cfg.webhooks.mappings = pairs
            .iter()
            .map(|(event, intent)| WebhookMapping {
                event: event.to_string(),
                intent: intent.to_string(),
            })
            .collect();
        cfg
    }

    /// Build a `sha256=<hex>` header that correctly signs `body` with `SECRET`.
    fn valid_sig(body: &[u8]) -> String {
        format!("sha256={}", sign_body(SECRET, body))
    }

    // -- loopback bind guard -------------------------------------------------

    #[test]
    fn loopback_bind_guard_accepts_only_loopback_literals() {
        assert!(is_loopback_bind("127.0.0.1"), "ipv4 loopback");
        assert!(is_loopback_bind("::1"), "ipv6 loopback");
        assert!(!is_loopback_bind("0.0.0.0"), "wildcard is NOT loopback");
        assert!(!is_loopback_bind("192.168.1.10"), "LAN address is NOT loopback");
        assert!(!is_loopback_bind("example.com"), "a hostname is refused");
        assert!(!is_loopback_bind(""), "empty is refused");
    }

    // -- (1) authentication --------------------------------------------------

    /// A valid HMAC over the raw body, an allowlisted event, a read-only intent:
    /// ROUTE. The happy path.
    #[test]
    fn valid_hmac_mapped_readonly_event_routes() {
        let cfg = cfg_with_mappings(&[("ci.failed", "system.query")]);
        let body = br#"{"event":"ci.failed","detail":"build #42 red"}"#;
        let decision = handle_webhook(
            body,
            Some(&valid_sig(body)),
            Some("ci.failed"),
            Some(SECRET),
            &cfg,
        );
        assert_eq!(
            decision,
            WebhookDecision::Route {
                event: "ci.failed".to_string(),
                intent: "system.query".to_string()
            }
        );
    }

    /// A FORGED signature (right shape, wrong MAC) is Unauthorized and reaches no
    /// intent — even though the event WOULD map.
    #[test]
    fn forged_signature_is_unauthorized_and_never_routes() {
        let cfg = cfg_with_mappings(&[("ci.failed", "system.query")]);
        let body = br#"{"event":"ci.failed"}"#;
        // A hex string of the right length but not the real MAC.
        let forged = format!("sha256={}", "a".repeat(64));
        let decision = handle_webhook(body, Some(&forged), Some("ci.failed"), Some(SECRET), &cfg);
        assert_eq!(decision, WebhookDecision::Unauthorized);
    }

    /// A signature over a DIFFERENT body (replay/tamper) fails: the MAC is over
    /// the raw body, so flipping one byte of the body invalidates the signature.
    #[test]
    fn signature_is_bound_to_the_exact_body() {
        let cfg = cfg_with_mappings(&[("ci.failed", "system.query")]);
        let signed_body = br#"{"event":"ci.failed","amount":1}"#;
        let sig = valid_sig(signed_body);
        // Present the valid signature but a tampered body.
        let tampered_body = br#"{"event":"ci.failed","amount":999}"#;
        let decision =
            handle_webhook(tampered_body, Some(&sig), Some("ci.failed"), Some(SECRET), &cfg);
        assert_eq!(decision, WebhookDecision::Unauthorized, "tampered body breaks the MAC");
    }

    /// A MISSING signature header is Unauthorized.
    #[test]
    fn missing_signature_is_unauthorized() {
        let cfg = cfg_with_mappings(&[("ci.failed", "system.query")]);
        let body = br#"{"event":"ci.failed"}"#;
        let decision = handle_webhook(body, None, Some("ci.failed"), Some(SECRET), &cfg);
        assert_eq!(decision, WebhookDecision::Unauthorized);
    }

    /// A malformed signature header (no `sha256=` prefix / empty) is Unauthorized.
    #[test]
    fn malformed_signature_header_is_unauthorized() {
        let cfg = cfg_with_mappings(&[("ci.failed", "system.query")]);
        let body = br#"{"event":"ci.failed"}"#;
        for bad in ["not-a-sig", "sha256=", "md5=abcd", ""] {
            let decision =
                handle_webhook(body, Some(bad), Some("ci.failed"), Some(SECRET), &cfg);
            assert_eq!(decision, WebhookDecision::Unauthorized, "header {bad:?}");
        }
    }

    /// With NO secret configured, NOTHING authenticates — fail-closed, even with a
    /// well-formed (but unverifiable) signature.
    #[test]
    fn no_secret_is_fail_closed_unauthorized() {
        let cfg = cfg_with_mappings(&[("ci.failed", "system.query")]);
        let body = br#"{"event":"ci.failed"}"#;
        // Sign with SOME secret, but the receiver has None.
        let decision = handle_webhook(body, Some(&valid_sig(body)), Some("ci.failed"), None, &cfg);
        assert_eq!(decision, WebhookDecision::Unauthorized);
    }

    /// Authentication is checked BEFORE the mapping lookup: an UNAUTHENTICATED
    /// request for an UNMAPPED event still returns Unauthorized (a forger cannot
    /// probe the mapping table).
    #[test]
    fn auth_precedes_mapping_so_a_forger_cannot_probe() {
        let cfg = cfg_with_mappings(&[("ci.failed", "system.query")]);
        let body = br#"{"event":"totally.unknown"}"#;
        let decision = handle_webhook(body, None, Some("totally.unknown"), Some(SECRET), &cfg);
        assert_eq!(
            decision,
            WebhookDecision::Unauthorized,
            "unauthenticated probe must not reveal it is unmapped"
        );
    }

    // -- (2) explicit allowlist ----------------------------------------------

    /// An authenticated event with NO mapping is Unmapped (rejected, not guessed).
    #[test]
    fn authenticated_unmapped_event_is_rejected() {
        let cfg = cfg_with_mappings(&[("ci.failed", "system.query")]);
        let body = br#"{"event":"deploy.succeeded"}"#;
        let decision = handle_webhook(
            body,
            Some(&valid_sig(body)),
            Some("deploy.succeeded"),
            Some(SECRET),
            &cfg,
        );
        assert_eq!(
            decision,
            WebhookDecision::Unmapped {
                event: "deploy.succeeded".to_string()
            }
        );
    }

    /// The event name can come from the BODY when no header is present (header
    /// still wins when both are set).
    #[test]
    fn event_resolves_from_body_when_no_header() {
        let cfg = cfg_with_mappings(&[("ci.failed", "system.query")]);
        let body = br#"{"event":"ci.failed"}"#;
        let decision = handle_webhook(body, Some(&valid_sig(body)), None, Some(SECRET), &cfg);
        assert_eq!(
            decision,
            WebhookDecision::Route {
                event: "ci.failed".to_string(),
                intent: "system.query".to_string()
            }
        );
    }

    // -- (3) consequential PARKS, never auto-executes ------------------------

    /// A mapping whose intent is CONSEQUENTIAL (e.g. gmail_send) PARKS for a
    /// spoken confirm — it is NEVER routed-to-execute. This is THE headline
    /// security property: a webhook cannot auto-execute a consequential action.
    #[test]
    fn consequential_mapping_parks_and_never_executes() {
        // gmail_send is in confirm::CONSEQUENTIAL_TOOLS.
        let cfg = cfg_with_mappings(&[("inbox.urgent", "gmail_send")]);
        let body = br#"{"event":"inbox.urgent"}"#;
        let decision = handle_webhook(
            body,
            Some(&valid_sig(body)),
            Some("inbox.urgent"),
            Some(SECRET),
            &cfg,
        );
        assert_eq!(
            decision,
            WebhookDecision::ParkForConfirm {
                event: "inbox.urgent".to_string(),
                intent: "gmail_send".to_string()
            },
            "a consequential intent must PARK, never route-to-execute"
        );
        // And it must NOT be a Route — assert the negative explicitly.
        assert!(
            !matches!(decision, WebhookDecision::Route { .. }),
            "a webhook can never auto-execute a consequential action"
        );
    }

    /// EVERY consequential tool, when mapped, parks — none ever routes. Pins the
    /// property against the whole CONSEQUENTIAL_TOOLS set so a newly-gated tool
    /// can never silently become webhook-auto-executable.
    #[test]
    fn every_consequential_tool_parks_when_mapped() {
        for tool in crate::confirm::CONSEQUENTIAL_TOOLS {
            let cfg = cfg_with_mappings(&[("evt", tool)]);
            let body = br#"{"event":"evt"}"#;
            let decision =
                handle_webhook(body, Some(&valid_sig(body)), Some("evt"), Some(SECRET), &cfg);
            assert!(
                matches!(decision, WebhookDecision::ParkForConfirm { .. }),
                "consequential tool {tool} mapped from a webhook must PARK, got {decision:?}"
            );
        }
    }

    // -- (0) body bound ------------------------------------------------------

    /// A body larger than the cap is a BadRequest (rejected before any auth work).
    #[test]
    fn oversized_body_is_bad_request() {
        let mut cfg = cfg_with_mappings(&[("ci.failed", "system.query")]);
        cfg.webhooks.max_body_bytes = 16;
        let body = vec![b'x'; 64];
        let decision = handle_webhook(
            &body,
            Some(&valid_sig(&body)),
            Some("ci.failed"),
            Some(SECRET),
            &cfg,
        );
        assert_eq!(decision, WebhookDecision::BadRequest);
    }

    // -- sign/verify round-trip ----------------------------------------------

    #[test]
    fn sign_then_verify_round_trips_and_rejects_a_flipped_bit() {
        let body = b"the exact bytes that were signed";
        let sig = sign_body(SECRET, body);
        assert!(verify_signature(SECRET, body, &sig), "the real signature verifies");
        // A wrong secret fails.
        assert!(!verify_signature(b"other-secret", body, &sig));
        // A non-hex signature fails cleanly (no panic).
        assert!(!verify_signature(SECRET, body, "not-hex-zzzz"));
    }
    // =====================================================================
    // THE LIVE LISTENER — it binds, and it answers
    // =====================================================================

    /// Drive `serve_conn` over a REAL loopback socket.
    ///
    /// `serve` itself resolves the Keychain secret and loops forever, so the test
    /// binds its own listener and hands one accepted connection to the same
    /// per-connection handler the accept loop uses. Everything under test — the
    /// framing, the cap, the status mapping, and the dispatch into the pure core —
    /// is exercised for real.
    async fn round_trip(cfg: Config, request: &[u8]) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind loopback");
        let addr = listener.local_addr().unwrap();
        // BOUNDED. If a change makes the handler wait for bytes a client never
        // sends — e.g. reading a declared Content-Length before checking it against
        // the cap — this fails the test instead of hanging the whole suite.
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.expect("accept");
            let secret = String::from_utf8(SECRET.to_vec()).unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(3), serve_conn(sock, &cfg, &secret))
                .await
                .expect("the handler must not block on bytes the client never sent")
                .ok();
        });
        let mut client = tokio::net::TcpStream::connect(addr).await.expect("connect");
        client.write_all(request).await.expect("write request");
        client.flush().await.unwrap();
        let mut resp = Vec::new();
        let _ = client.read_to_end(&mut resp).await;
        let _ = server.await;
        String::from_utf8_lossy(&resp).to_string()
    }

    fn post(headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
        let mut r = format!("POST / HTTP/1.1\r\nhost: 127.0.0.1\r\ncontent-length: {}\r\n", body.len());
        for (k, v) in headers {
            r.push_str(&format!("{k}: {v}\r\n"));
        }
        r.push_str("\r\n");
        let mut out = r.into_bytes();
        out.extend_from_slice(body);
        out
    }

    /// THE FEATURE EXISTS AT RUNTIME.
    ///
    /// `serve` used to resolve the secret, log "webhook receiver configured", and
    /// return — it contained no socket primitive of any kind. Nothing ever listened
    /// on the port, under any configuration, while config, docs, the HUD panel and
    /// that log line all asserted a working loopback receiver. A correctly signed,
    /// correctly mapped delivery got connection-refused forever.
    #[tokio::test]
    async fn a_signed_mapped_delivery_is_accepted() {
        let cfg = cfg_with_mappings(&[("ci.failed", "system.query")]);
        let body = br#"{"event":"ci.failed"}"#;
        let resp = round_trip(
            cfg,
            &post(&[(SIGNATURE_HEADER, &valid_sig(body)), (EVENT_HEADER, "ci.failed")], body),
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "expected 200, got: {resp}");
    }

    /// A FORGED SIGNATURE IS REFUSED — and says so with 401, not 200. Answering
    /// 200 to a bad signature tells a prober their forgery worked.
    #[tokio::test]
    async fn a_forged_signature_is_rejected_over_the_wire() {
        let cfg = cfg_with_mappings(&[("ci.failed", "system.query")]);
        let body = br#"{"event":"ci.failed"}"#;
        let resp = round_trip(
            cfg,
            &post(
                &[(SIGNATURE_HEADER, "sha256=00000000deadbeef"), (EVENT_HEADER, "ci.failed")],
                body,
            ),
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 401"), "expected 401, got: {resp}");
    }

    /// An authenticated but UNMAPPED event is 404 — rejected, never guessed into
    /// some intent.
    #[tokio::test]
    async fn an_unmapped_event_is_404_over_the_wire() {
        let cfg = cfg_with_mappings(&[("ci.failed", "system.query")]);
        let body = br#"{"event":"something.else"}"#;
        let resp = round_trip(
            cfg,
            &post(&[(SIGNATURE_HEADER, &valid_sig(body)), (EVENT_HEADER, "something.else")], body),
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 404"), "expected 404, got: {resp}");
    }

    /// AN OVER-CAP BODY IS REJECTED UNREAD. The config promises "a larger body is
    /// rejected rather than buffered"; the declared Content-Length is checked
    /// BEFORE a byte of body is read, so the promise holds in the case it exists
    /// for. The body here is never sent — only its length is claimed.
    #[tokio::test]
    async fn an_over_cap_body_is_rejected_before_it_is_read() {
        let mut cfg = cfg_with_mappings(&[("ci.failed", "system.query")]);
        cfg.webhooks.max_body_bytes = 64;
        let mut req =
            b"POST / HTTP/1.1\r\nhost: 127.0.0.1\r\ncontent-length: 100000\r\n\r\n".to_vec();
        req.extend_from_slice(b"only a few bytes actually follow");
        let resp = round_trip(cfg, &req).await;
        assert!(resp.starts_with("HTTP/1.1 413"), "expected 413, got: {resp}");
    }

    /// A HEAD that never terminates is bounded rather than buffered without limit.
    #[tokio::test]
    async fn an_unterminated_head_is_bounded() {
        let cfg = cfg_with_mappings(&[("ci.failed", "system.query")]);
        let mut req = b"POST / HTTP/1.1\r\n".to_vec();
        // Well past MAX_HEAD_BYTES, and never the blank line.
        for i in 0..600 {
            req.extend_from_slice(format!("x-pad-{i}: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n").as_bytes());
        }
        let resp = round_trip(cfg, &req).await;
        assert!(resp.starts_with("HTTP/1.1 431"), "expected 431, got: {resp}");
    }

    /// GET is not a delivery. Only POST carries a webhook.
    #[tokio::test]
    async fn a_get_is_not_a_delivery() {
        let cfg = cfg_with_mappings(&[("ci.failed", "system.query")]);
        let resp = round_trip(cfg, b"GET / HTTP/1.1\r\nhost: 127.0.0.1\r\n\r\n").await;
        assert!(resp.starts_with("HTTP/1.1 405"), "expected 405, got: {resp}");
    }

    /// Every decision maps to a distinct, honest status. Pure — no socket.
    #[test]
    fn every_decision_has_an_honest_status() {
        assert_eq!(status_for(&WebhookDecision::Route { event: "e".into(), intent: "i".into() }).0, 200);
        assert_eq!(
            status_for(&WebhookDecision::ParkForConfirm { event: "e".into(), intent: "i".into() }).0,
            200
        );
        assert_eq!(status_for(&WebhookDecision::Unauthorized).0, 401);
        assert_eq!(status_for(&WebhookDecision::Unmapped { event: "e".into() }).0, 404);
        assert_eq!(status_for(&WebhookDecision::BadRequest).0, 400);
    }

}
