import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import ForgePanel from "../components/ForgePanel";
import { parseForgeProposed, type TelemetryEnvelope } from "../core/events";
import {
  type ForgeAlert,
  type ForgeProposal,
  HudState,
  initialState,
  reduce,
} from "../core/state";

/* helpers ------------------------------------------------------------------ */

let counter = 0;
function env(
  event: string,
  data: Record<string, unknown> = {},
  source = "system",
): TelemetryEnvelope {
  counter += 1;
  return {
    ts: `2026-06-15T12:00:${String(counter % 60).padStart(2, "0")}Z`,
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
 * The defensive parser. A review card must NEVER be surfaced from a payload
 * the apply command cannot be derived from — so both name AND ts are required.
 * ------------------------------------------------------------------------ */
describe("parseForgeProposed (defensive)", () => {
  it("parses a well-formed forge.proposed payload", () => {
    expect(parseForgeProposed({ name: "reverser", ts: 1770000000 })).toEqual({
      name: "reverser",
      ts: 1770000000,
    });
  });

  it("returns null when name is missing, empty, or non-string", () => {
    expect(parseForgeProposed({ ts: 1770000000 })).toBeNull();
    expect(parseForgeProposed({ name: "", ts: 1770000000 })).toBeNull();
    expect(parseForgeProposed({ name: 42, ts: 1770000000 })).toBeNull();
  });

  it("returns null when ts is missing or non-finite", () => {
    expect(parseForgeProposed({ name: "reverser" })).toBeNull();
    expect(parseForgeProposed({ name: "reverser", ts: "soon" })).toBeNull();
    expect(parseForgeProposed({ name: "reverser", ts: Number.NaN })).toBeNull();
  });

  it("never throws on junk", () => {
    expect(() => parseForgeProposed({})).not.toThrow();
    expect(parseForgeProposed({})).toBeNull();
  });
});

/* ------------------------------------------------------------------------ *
 * The reducer arms. A forge.proposed event surfaces a warn-amber REVIEW card
 * (no auto-apply — the panel only shows the manual command). rejected/blocked
 * are error banners; "disabled" is the shipped-OFF state and is NOT an error.
 * ------------------------------------------------------------------------ */
describe("forge.* reducer", () => {
  it("forge.proposed surfaces a review card with name + ts (the apply <ts>)", () => {
    const s = tel(connected(), env("forge.proposed", { name: "reverser", ts: 1770000000 }));
    expect(s.forgeProposal).not.toBeNull();
    expect(s.forgeProposal!.name).toBe("reverser");
    expect(s.forgeProposal!.ts).toBe(1770000000);
    // A proposal is an attention state, NOT an error: no red banner.
    expect(s.forgeAlert).toBeNull();
  });

  it("ignores a malformed forge.proposed (no review card from junk)", () => {
    const s = tel(connected(), env("forge.proposed", { name: "reverser" })); // no ts
    expect(s.forgeProposal).toBeNull();
    const s2 = tel(connected(), env("forge.proposed", {})); // empty
    expect(s2.forgeProposal).toBeNull();
  });

  it("forge.rejected raises a red banner and clears any pending proposal", () => {
    let s = tel(connected(), env("forge.proposed", { name: "reverser", ts: 1 }));
    expect(s.forgeProposal).not.toBeNull();
    s = tel(s, env("forge.rejected", { reason: "test" }));
    expect(s.forgeProposal).toBeNull();
    expect(s.forgeAlert).not.toBeNull();
    expect(s.forgeAlert!.kind).toBe("rejected");
    expect(s.forgeAlert!.detail).toContain("test");
  });

  it("forge.blocked with reason=disabled is the OFF state — NOT an error banner", () => {
    const s = tel(connected(), env("forge.blocked", { reason: "disabled" }));
    expect(s.forgeAlert).toBeNull();
    expect(s.forgeProposal).toBeNull();
  });

  it("forge.blocked with a real reason (no_api_key) raises a red banner", () => {
    const s = tel(connected(), env("forge.blocked", { reason: "no_api_key" }));
    expect(s.forgeAlert).not.toBeNull();
    expect(s.forgeAlert!.kind).toBe("blocked");
    expect(s.forgeAlert!.detail).toBe("no_api_key");
  });

  it("a fresh proposal clears a stale forge error banner", () => {
    let s = tel(connected(), env("forge.blocked", { reason: "no_api_key" }));
    expect(s.forgeAlert).not.toBeNull();
    s = tel(s, env("forge.proposed", { name: "tipcalc", ts: 99 }));
    expect(s.forgeAlert).toBeNull();
    expect(s.forgeProposal!.name).toBe("tipcalc");
  });

  it("alert.dismiss acknowledges the forge proposal and alert", () => {
    let s = tel(connected(), env("forge.proposed", { name: "reverser", ts: 1 }));
    s = reduce(s, { type: "alert.dismiss" });
    expect(s.forgeProposal).toBeNull();
    s = tel(connected(), env("forge.blocked", { reason: "no_api_key" }));
    s = reduce(s, { type: "alert.dismiss" });
    expect(s.forgeAlert).toBeNull();
  });

  it("never carries a secret — only name + ts survive into state", () => {
    const s = tel(
      connected(),
      env("forge.proposed", {
        name: "reverser",
        ts: 7,
        // hostile extra fields must be ignored, never stored/rendered.
        api_key: "sk-SECRET",
        token: "leak",
      }),
    );
    const serialized = JSON.stringify(s.forgeProposal);
    expect(serialized).not.toContain("SECRET");
    expect(serialized).not.toContain("leak");
    expect(serialized).not.toContain("api_key");
  });
});

/* ------------------------------------------------------------------------ *
 * The panel itself (rendered headlessly via renderToStaticMarkup — node env,
 * no jsdom, same pattern as mark-forge.test.ts). THE SAFETY POSTURE: review +
 * the manual command only, with NO button that auto-applies/auto-runs.
 * ------------------------------------------------------------------------ */
describe("ForgePanel (review-only, no auto-apply)", () => {
  const proposal: ForgeProposal = { name: "reverser", ts: 1770000000, at: "2026-06-15T12:00:00Z" };
  const renderProposal = () =>
    renderToStaticMarkup(createElement(ForgePanel, { proposal, alert: null, onDismiss: () => {} }));

  it("renders the pending proposal: app name + the EXACT manual apply command", () => {
    const html = renderProposal();
    expect(html).toContain("reverser");
    expect(html).toContain("scripts/apply_forge.sh 1770000000");
    expect(html).toContain("HUMAN REVIEW REQUIRED");
  });

  it("makes clear NOTHING is installed or running yet", () => {
    const html = renderProposal();
    expect(html).toMatch(/not in apps\//i);
    expect(html).toMatch(/not running/i);
    expect(html).toMatch(/nothing is installed or running yet/i);
  });

  it("has NO apply/deploy/install button — only DISMISS (review-only)", () => {
    const html = renderProposal();
    // Exactly one button, and it is the DISMISS acknowledgement.
    const buttons = html.match(/<button/g) ?? [];
    expect(buttons.length).toBe(1);
    expect(html).toContain("DISMISS");
    // No button label that would imply a one-click apply/deploy/install/run.
    expect(html).not.toMatch(/<button[^>]*>[^<]*(APPLY|DEPLOY|INSTALL|RUN|CONFIRM)/i);
  });

  it("renders nothing when there is no proposal and no alert", () => {
    const html = renderToStaticMarkup(
      createElement(ForgePanel, { proposal: null, alert: null, onDismiss: () => {} }),
    );
    expect(html).toBe("");
  });

  it("shows the red error banner for a rejected/blocked alert (no proposal card)", () => {
    const alert: ForgeAlert = { kind: "blocked", ts: "2026-06-15T12:00:00Z", detail: "no_api_key" };
    const html = renderToStaticMarkup(
      createElement(ForgePanel, { proposal: null, alert, onDismiss: () => {} }),
    );
    expect(html).toContain("SELF-FORGE BLOCKED");
    expect(html).toContain("no_api_key");
    // The error banner has only the ACK button, no apply path.
    expect(html).not.toMatch(/scripts\/apply_forge\.sh/);
  });
});
