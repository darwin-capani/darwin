import type {
  PluginRecord,
  PluginSurface,
  WebhookOutcome,
  WebhookSurface,
} from "../core/events";
import Frame from "./Frame";

/**
 * EXTENSIBILITY — the review/visibility surface for the two opt-in INBOUND /
 * MODULE extension points: #35 WEBHOOK TRIGGERS and #36 the PLUGIN SDK. Both
 * ship OFF; this panel only SHOWS what is wired + gated, read-only.
 *
 * WEBHOOKS (#35; daemon/src/webhooks.rs): the loopback receiver maps an
 * AUTHENTICATED (HMAC) inbound event to an intent via an explicit allowlist and
 * routes it through the NORMAL router — and a CONSEQUENTIAL mapping PARKS for
 * the user's spoken confirm (a webhook NEVER auto-runs it). The panel shows the
 * listener posture (OFF until an event arrives; bound-loopback 127.0.0.1 once
 * one does), the running events-received count, and the LAST event's
 * intent/outcome — NEVER the payload, secret, or signature (the daemon emits
 * only outcome/event/intent; nothing else can reach the wire or this panel).
 *
 * PLUGINS (#36; daemon/src/main.rs register-on-launch): each capability module
 * presents its manifest + a per-launch capability token at launch; the daemon
 * RE-VALIDATES the manifest and verifies the token constant-time, then admits
 * the SCOPED intents. The panel lists the latest handshake per module — admitted
 * (validated manifest, SBPL-sandboxed, declared-intent count) or rejected
 * (invalid_manifest / unauthorized, with the precise reason) — NEVER the
 * capability token (the daemon never puts it on the wire).
 *
 * SAFETY CONTRACT (do not regress):
 *   - REVIEW-ONLY. No button here receives a webhook, runs an intent, admits a
 *     plugin, or changes a setting. The inbound network surface (#35) is the
 *     headline risk; this panel only mirrors the secret-free decisions.
 *   - NEVER renders a payload / secret / signature / capability token. The wire
 *     carries none of those; the parser surfaces only labels + counts + outcomes.
 *   - SHIPPED-OFF honest. Both subsystems opt-in; with the flags off NO event
 *     arrives, so the panel shows explicit OFF / nothing-installed states, not a
 *     blank or a fake.
 *
 * The reducer only ever folds defensively-parsed webhook.received /
 * plugin.handshake frames (malformed fields dropped, no secret), so this
 * component can trust the fields it is handed.
 */
export default function ExtensibilityPanel({
  webhooks,
  plugins,
}: {
  webhooks: WebhookSurface;
  plugins: PluginSurface | null;
}) {
  // An event having arrived at all means the loopback listener is bound this
  // session (the bind is OFF by default + runtime-gated; we never claim it is up
  // until a decision actually flows through it).
  const listenerLive = webhooks.received > 0;

  return (
    <div className="ext-panel">
      <Frame title="EXTENSIBILITY // WEBHOOKS + PLUGINS" tag="REVIEW ONLY">
        <div className="ext-body">
          {/* ---------------------------------------------- #35 WEBHOOKS */}
          <div className="ext-section">
            <div className="ext-section-head">
              <span className="ext-section-title">WEBHOOKS</span>
              <span className={`ext-pill ${listenerLive ? "ok" : "off"}`}>
                {listenerLive ? "BOUND-LOOPBACK 127.0.0.1" : "OFF"}
              </span>
            </div>

            <div className="ext-rows">
              <div className="ext-row">
                <span className="ext-row-label">EVENTS RECEIVED</span>
                <span className="ext-row-value">{webhooks.received}</span>
              </div>
              <div className="ext-row">
                <span className="ext-row-label">LAST INTENT</span>
                <span className="ext-row-value">
                  {webhooks.last && webhooks.last.intent.length > 0 ? (
                    <>
                      <span className="ext-intent">{webhooks.last.intent}</span>
                      <span
                        className={`ext-outcome ${webhooks.last.outcome}`}
                        title={webhookOutcomeTitle(webhooks.last.outcome)}
                      >
                        {webhooks.last.outcome.replace("_", " ").toUpperCase()}
                      </span>
                    </>
                  ) : webhooks.last ? (
                    // A reject (unauthorized / unmapped / bad_request) carries no
                    // intent — show the outcome honestly, never a guessed intent.
                    <span
                      className={`ext-outcome ${webhooks.last.outcome}`}
                      title={webhookOutcomeTitle(webhooks.last.outcome)}
                    >
                      {webhooks.last.outcome.replace("_", " ").toUpperCase()}
                    </span>
                  ) : (
                    <span className="ext-empty dim-note">none yet</span>
                  )}
                </span>
              </div>
            </div>

            <div className="ext-note dim-note">
              Auth required (HMAC); loopback only. A webhook never auto-runs a
              consequential action — it parks for your confirm. The payload,
              secret, and signature are never shown here.
            </div>
          </div>

          {/* ---------------------------------------------- #36 PLUGINS */}
          <div className="ext-section">
            <div className="ext-section-head">
              <span className="ext-section-title">PLUGINS</span>
              <span className="ext-pill off">SANDBOXED</span>
            </div>

            {plugins === null || plugins.modules.length === 0 ? (
              <div className="ext-empty dim-note">
                No capability module installed. A plugin registers on launch by
                presenting its validated manifest + capability token; enable{" "}
                <code>[plugin_sdk].enabled</code> and install a module to turn it
                on.
              </div>
            ) : (
              <div className="ext-plugins">
                {plugins.modules.map((p) => (
                  <PluginRow key={p.name} plugin={p} />
                ))}
              </div>
            )}

            <div className="ext-note dim-note">
              Validated manifest; SBPL-sandboxed. A plugin cannot request a
              capability outside the allowed set, cannot escape the default-deny
              profile, and its consequential tools still ride the confirmation
              gate. The capability token is never shown here.
            </div>
          </div>
        </div>
      </Frame>
    </div>
  );
}

/** One installed capability module: name + its latest handshake. An admitted
 *  module shows its declared-intent count + a sandboxed badge; a rejected one
 *  shows the outcome + the precise reason (never a token). */
function PluginRow({ plugin }: { plugin: PluginRecord }) {
  const admitted = plugin.status === "admitted";
  return (
    <div className="ext-plugin">
      <div className="ext-plugin-head">
        <span className="ext-plugin-name">{plugin.name}</span>
        {admitted ? (
          <>
            <span
              className="ext-pill count"
              title="declared intents/tools the validated manifest exposes"
            >
              {plugin.intents === null
                ? "INTENTS —"
                : `${plugin.intents} INTENT${plugin.intents === 1 ? "" : "S"}`}
            </span>
            <span
              className="ext-pill sandboxed"
              title="SBPL default-deny sandboxed; consequential tools still gated"
            >
              SANDBOXED
            </span>
          </>
        ) : (
          <span
            className={`ext-pill reject ${plugin.status}`}
            title={pluginStatusTitle(plugin.status)}
          >
            {plugin.status === "invalid_manifest" ? "INVALID MANIFEST" : "UNAUTHORIZED"}
          </span>
        )}
      </div>
      {/* The precise reason on a rejected module — the manifest error string the
          daemon emitted (never a token). Hidden for an admitted module (its
          detail is just the intent count already badged above). */}
      {!admitted && plugin.detail.length > 0 ? (
        <div className="ext-plugin-detail dim-note">{plugin.detail}</div>
      ) : null}
    </div>
  );
}

/** The hover copy for a webhook outcome — honest about what each decision means. */
function webhookOutcomeTitle(outcome: WebhookOutcome): string {
  switch (outcome) {
    case "routed":
      return "authenticated + mapped to a non-consequential intent; routed through the normal pipeline";
    case "parked":
      return "authenticated + mapped to a CONSEQUENTIAL intent; PARKED for your spoken confirm — a webhook never auto-runs it";
    case "unauthorized":
      return "missing/forged/expired signature — rejected, never routed";
    case "unmapped":
      return "authenticated but the event is not in the allowlist mapping — rejected, not guessed";
    case "bad_request":
      return "malformed request — rejected";
  }
}

/** The hover copy for a plugin handshake reject. */
function pluginStatusTitle(status: PluginRecord["status"]): string {
  switch (status) {
    case "invalid_manifest":
      return "the presented manifest failed validation (malformed or over-privileged) — not admitted";
    case "unauthorized":
      return "the manifest was valid but the capability token failed verification — not admitted";
    case "admitted":
      return "validated manifest + verified token — admitted with its scoped intents";
  }
}
