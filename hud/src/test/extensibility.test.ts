import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import ExtensibilityPanel from "../components/ExtensibilityPanel";
import {
  parseWebhookEvent,
  applyWebhookEvent,
  webhookSurfaceInitial,
  parsePluginHandshake,
  applyPluginHandshake,
  type PluginSurface,
  type WebhookSurface,
  type TelemetryEnvelope,
} from "../core/events";
import { HudState, initialState, reduce } from "../core/state";

/* helpers ------------------------------------------------------------------ */

let counter = 0;
function env(
  event: string,
  data: Record<string, unknown> = {},
  source = "system",
): TelemetryEnvelope {
  counter += 1;
  return {
    ts: `2026-06-16T12:00:${String(counter % 60).padStart(2, "0")}Z`,
    source,
    event,
    data,
  };
}

function tel(state: HudState, e: TelemetryEnvelope, at = 1000): HudState {
  return reduce(state, { type: "telemetry", envelope: e, at });
}

function connected(at = 0): HudState {
  return reduce(initialState(), { type: "ws.connected", at });
}

/* ------------------------------------------------------------------------ *
 * #35 WEBHOOKS — the defensive parser. Drops a frame with a missing/unknown   *
 * outcome; surfaces ONLY outcome/event/intent; never a body/secret.          *
 * ------------------------------------------------------------------------ */
describe("parseWebhookEvent (defensive, secret-free)", () => {
  it("parses each valid outcome", () => {
    expect(parseWebhookEvent({ outcome: "routed", event: "ci.done", intent: "report_ci" })).toEqual(
      { outcome: "routed", event: "ci.done", intent: "report_ci" },
    );
    expect(
      parseWebhookEvent({ outcome: "parked", event: "deploy.req", intent: "deploy" }),
    ).toEqual({ outcome: "parked", event: "deploy.req", intent: "deploy" });
  });

  it("defaults event/intent to '' on a reject path", () => {
    expect(parseWebhookEvent({ outcome: "unauthorized" })).toEqual({
      outcome: "unauthorized",
      event: "",
      intent: "",
    });
    expect(parseWebhookEvent({ outcome: "unmapped", event: "weird.evt" })).toEqual({
      outcome: "unmapped",
      event: "weird.evt",
      intent: "",
    });
    expect(parseWebhookEvent({ outcome: "bad_request" })).toEqual({
      outcome: "bad_request",
      event: "",
      intent: "",
    });
  });

  it("drops a frame with a missing/unrecognized outcome (un-actionable)", () => {
    expect(parseWebhookEvent({})).toBeNull();
    expect(parseWebhookEvent({ outcome: "executed" })).toBeNull(); // not a real outcome
    expect(parseWebhookEvent({ outcome: 42 })).toBeNull();
  });

  it("NEVER surfaces a body/secret/signature field", () => {
    const ev = parseWebhookEvent({
      outcome: "routed",
      event: "ci.done",
      intent: "report_ci",
      // Hostile extras a malformed payload might carry — the parser surfaces
      // ONLY the three known labels.
      body: "SECRET-PAYLOAD",
      secret: "sk-LEAK",
      signature: "sha256=DEADBEEF",
    });
    const blob = JSON.stringify(ev);
    expect(blob).not.toContain("SECRET-PAYLOAD");
    expect(blob).not.toContain("LEAK");
    expect(blob).not.toContain("DEADBEEF");
    expect(blob).not.toContain("signature");
    expect(blob).not.toContain("body");
  });

  it("never throws on junk", () => {
    expect(() => parseWebhookEvent({ outcome: null as unknown as string })).not.toThrow();
  });
});

describe("applyWebhookEvent / webhookSurfaceInitial (accumulating)", () => {
  it("starts at received=0, last=null", () => {
    expect(webhookSurfaceInitial()).toEqual({ received: 0, last: null });
  });

  it("bumps the count and replaces last", () => {
    let s: WebhookSurface = webhookSurfaceInitial();
    s = applyWebhookEvent(s, { outcome: "routed", event: "a", intent: "ia" });
    s = applyWebhookEvent(s, { outcome: "parked", event: "b", intent: "ib" });
    expect(s.received).toBe(2);
    expect(s.last).toEqual({ outcome: "parked", event: "b", intent: "ib" });
  });
});

/* ------------------------------------------------------------------------ *
 * #36 PLUGINS — the defensive parser. Drops an unnamed/unknown-status frame;  *
 * surfaces ONLY name/status/detail; never the capability token.              *
 * ------------------------------------------------------------------------ */
describe("parsePluginHandshake (defensive, secret-free)", () => {
  it("parses an admitted handshake and derives the intent count", () => {
    expect(parsePluginHandshake({ name: "vision", status: "admitted", detail: "3 intents" })).toEqual(
      { name: "vision", status: "admitted", detail: "3 intents", intents: 3 },
    );
  });

  it("parses a rejected handshake (precise reason, no count)", () => {
    expect(
      parsePluginHandshake({
        name: "evil",
        status: "invalid_manifest",
        detail: "unknown capability scope: net.raw",
      }),
    ).toEqual({
      name: "evil",
      status: "invalid_manifest",
      detail: "unknown capability scope: net.raw",
      intents: null,
    });
    expect(parsePluginHandshake({ name: "forged", status: "unauthorized", detail: "" })).toEqual({
      name: "forged",
      status: "unauthorized",
      detail: "",
      intents: null,
    });
  });

  it("drops an unnamed module or an unrecognized status", () => {
    expect(parsePluginHandshake({ status: "admitted" })).toBeNull();
    expect(parsePluginHandshake({ name: "", status: "admitted" })).toBeNull();
    expect(parsePluginHandshake({ name: "x", status: "loaded" })).toBeNull();
  });

  it("defaults a malformed/absent intent count to null", () => {
    expect(
      parsePluginHandshake({ name: "x", status: "admitted", detail: "lots of intents" })!.intents,
    ).toBeNull();
  });

  it("NEVER surfaces a capability token", () => {
    const rec = parsePluginHandshake({
      name: "vision",
      status: "admitted",
      detail: "3 intents",
      token: "cap-SECRET-TOKEN",
      capability_token: "leak",
    });
    const blob = JSON.stringify(rec);
    expect(blob).not.toContain("SECRET-TOKEN");
    expect(blob).not.toContain("leak");
    expect(blob).not.toContain("token");
  });

  it("never throws on junk", () => {
    expect(() => parsePluginHandshake({ name: 7 as unknown as string })).not.toThrow();
  });
});

describe("applyPluginHandshake (accumulating, latest-per-name)", () => {
  it("appends new modules and updates one in place on re-launch", () => {
    let s: PluginSurface | null = null;
    s = applyPluginHandshake(s, { name: "a", status: "admitted", detail: "2 intents", intents: 2 });
    s = applyPluginHandshake(s, { name: "b", status: "unauthorized", detail: "", intents: null });
    expect(s.modules.map((m) => m.name)).toEqual(["a", "b"]);
    // re-launch of "a" with a new outcome updates in place (stable order).
    s = applyPluginHandshake(s, {
      name: "a",
      status: "admitted",
      detail: "5 intents",
      intents: 5,
    });
    expect(s.modules.map((m) => m.name)).toEqual(["a", "b"]);
    expect(s.modules[0].intents).toBe(5);
  });
});

/* ------------------------------------------------------------------------ *
 * The reducer arms — accumulate across the two event streams; both flags off  *
 * by default (no event => honest idle/empty surface). Drop malformed frames.  *
 * ------------------------------------------------------------------------ */
describe("webhook.received reducer", () => {
  it("is idle by default (no event)", () => {
    const s = connected();
    expect(s.webhooks).toEqual({ received: 0, last: null });
  });

  it("accumulates count + last across events", () => {
    let s = connected();
    s = tel(s, env("webhook.received", { outcome: "routed", event: "ci.done", intent: "report" }));
    s = tel(s, env("webhook.received", { outcome: "unauthorized" }));
    expect(s.webhooks.received).toBe(2);
    expect(s.webhooks.last).toEqual({ outcome: "unauthorized", event: "", intent: "" });
  });

  it("a consequential mapping parks (never executes)", () => {
    const base = connected();
    const s = tel(base, env("webhook.received", { outcome: "parked", event: "deploy.req", intent: "deploy" }));
    expect(s.webhooks.last!.outcome).toBe("parked");
    // The frame ONLY records the parked decision onto the webhook surface — it
    // does not synthesize any executed/forwarded action (no op rides this path;
    // the user must confirm out-of-band). The action surface is untouched.
    expect(s.actionSurface).toEqual(base.actionSurface);
  });

  it("drops a malformed frame (no count bump)", () => {
    let s = connected();
    s = tel(s, env("webhook.received", { outcome: "nope" }));
    expect(s.webhooks.received).toBe(0);
  });

  it("never stores a body/secret in state", () => {
    let s = connected();
    s = tel(
      s,
      env("webhook.received", {
        outcome: "routed",
        event: "e",
        intent: "i",
        body: "SECRET-PAYLOAD",
        secret: "sk-LEAK",
      }),
    );
    const blob = JSON.stringify(s.webhooks);
    expect(blob).not.toContain("SECRET-PAYLOAD");
    expect(blob).not.toContain("LEAK");
  });
});

describe("plugin.handshake reducer", () => {
  it("is null by default (no handshake; SDK ships OFF)", () => {
    expect(connected().plugins).toBeNull();
  });

  it("accumulates admitted + rejected modules", () => {
    let s = connected();
    s = tel(s, env("plugin.handshake", { name: "vision", status: "admitted", detail: "3 intents" }));
    s = tel(
      s,
      env("plugin.handshake", { name: "evil", status: "invalid_manifest", detail: "over-privileged" }),
    );
    expect(s.plugins!.modules.length).toBe(2);
    expect(s.plugins!.modules[0]).toEqual({
      name: "vision",
      status: "admitted",
      detail: "3 intents",
      intents: 3,
    });
  });

  it("drops a malformed handshake", () => {
    let s = connected();
    s = tel(s, env("plugin.handshake", { status: "admitted" })); // no name
    expect(s.plugins).toBeNull();
  });

  it("never stores a capability token", () => {
    let s = connected();
    s = tel(
      s,
      env("plugin.handshake", {
        name: "vision",
        status: "admitted",
        detail: "3 intents",
        token: "cap-SECRET-TOKEN",
      }),
    );
    expect(JSON.stringify(s.plugins)).not.toContain("SECRET-TOKEN");
  });
});

/* ------------------------------------------------------------------------ *
 * The panel (rendered headlessly, node env). REVIEW-ONLY + secret-free +      *
 * honest OFF/empty + honest safety copy.                                     *
 * ------------------------------------------------------------------------ */
describe("ExtensibilityPanel (review-only, secret-free, honest)", () => {
  const render = (webhooks: WebhookSurface, plugins: PluginSurface | null) =>
    renderToStaticMarkup(createElement(ExtensibilityPanel, { webhooks, plugins }));

  it("shows the honest OFF / nothing-installed state before any event", () => {
    const html = render(webhookSurfaceInitial(), null);
    expect(html).toContain("REVIEW ONLY");
    expect(html).toContain("WEBHOOKS");
    expect(html).toContain("PLUGINS");
    expect(html).toContain(">OFF<"); // listener OFF pill
    expect(html).toMatch(/none yet/i); // last intent
    expect(html).toMatch(/No capability module installed/i);
  });

  it("carries the honest safety copy", () => {
    const html = render(webhookSurfaceInitial(), null);
    expect(html).toMatch(/loopback only/i);
    expect(html).toMatch(/never auto-runs? a\s+consequential action/i);
    expect(html).toMatch(/parks for your confirm/i);
    expect(html).toMatch(/SBPL-sandboxed/i);
    expect(html).toMatch(/consequential tools still ride the confirmation\s+gate/i);
  });

  it("shows bound-loopback + count + last intent once an event arrives", () => {
    const html = render(
      { received: 4, last: { outcome: "routed", event: "ci.done", intent: "report_ci" } },
      null,
    );
    expect(html).toContain("BOUND-LOOPBACK 127.0.0.1");
    expect(html).toContain("4");
    expect(html).toContain("report_ci");
    expect(html).toContain("ROUTED");
  });

  it("shows a parked consequential webhook honestly (parked, not executed)", () => {
    const html = render(
      { received: 1, last: { outcome: "parked", event: "deploy.req", intent: "deploy" } },
      null,
    );
    expect(html).toContain("deploy");
    expect(html).toContain("PARKED");
    expect(html).not.toMatch(/executed|auto-ran|ran/i);
  });

  it("shows a reject (no intent) as the outcome only, never a guessed intent", () => {
    const html = render(
      { received: 1, last: { outcome: "unauthorized", event: "", intent: "" } },
      null,
    );
    expect(html).toContain("UNAUTHORIZED");
    expect(html).not.toMatch(/none yet/i); // we DID receive one; just no intent
  });

  it("lists installed modules with intent count + sandboxed badge", () => {
    const html = render(webhookSurfaceInitial(), {
      modules: [
        { name: "vision", status: "admitted", detail: "3 intents", intents: 3 },
        { name: "nexus", status: "admitted", detail: "1 intents", intents: 1 },
      ],
    });
    expect(html).toContain("vision");
    expect(html).toContain("3 INTENTS");
    expect(html).toContain("nexus");
    expect(html).toContain("1 INTENT"); // singular
    expect(html).toContain("SANDBOXED");
  });

  it("surfaces a rejected module with its precise reason", () => {
    const html = render(webhookSurfaceInitial(), {
      modules: [
        {
          name: "evil",
          status: "invalid_manifest",
          detail: "unknown capability scope: net.raw",
          intents: null,
        },
      ],
    });
    expect(html).toContain("evil");
    expect(html).toContain("INVALID MANIFEST");
    expect(html).toContain("unknown capability scope: net.raw");
  });

  it("has NO action button — it is review-only", () => {
    const html = render(
      { received: 2, last: { outcome: "routed", event: "e", intent: "i" } },
      { modules: [{ name: "a", status: "admitted", detail: "2 intents", intents: 2 }] },
    );
    expect(html).not.toContain("<button");
  });

  it("never renders a planted secret/payload/token VALUE", () => {
    // The honest safety copy DOES name "payload", "secret", "signature", and
    // "token" (as the things it promises never to show) — so we don't forbid
    // those words. We forbid any actual secret-shaped VALUE: the parser drops
    // such fields before state, and the panel only reads the secret-free shape,
    // so a planted value can never reach the markup.
    const html = render(
      { received: 1, last: { outcome: "routed", event: "ci.done", intent: "report_ci" } },
      { modules: [{ name: "vision", status: "admitted", detail: "3 intents", intents: 3 }] },
    );
    expect(html).not.toMatch(/sk-|cap-|sha256=|user:pw|SECRET|LEAK|DEADBEEF/);
  });
});
