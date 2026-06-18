import { describe, expect, it } from "vitest";
import {
  parseUiActuateBlocked,
  parseUiActuateRefused,
  parseUiActuateActionEvent,
  type TelemetryEnvelope,
} from "../core/events";
import {
  type UiActuateSurface,
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
    ts: `2026-06-17T12:00:${String(counter % 60).padStart(2, "0")}Z`,
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
 * GATED UI AUTOMATION (#44, the CAPSTONE) — the HUD's read-only view of the
 * SINGLE MOST DANGEROUS feature (physically actuating the UI). It is fed ONLY
 * by ui_actuate.* events, ships OFF, parks PER-ACTION (one confirm = one
 * actuation), and never fabricates a result. These tests pin the parsers + the
 * reducer to that honest contract.
 * ------------------------------------------------------------------------ */

describe("parseUiActuateBlocked (OFF/locked vs device-gated)", () => {
  it("reason=disabled is the OFF/LOCKED gate — no action, no reason surfaced", () => {
    const out = parseUiActuateBlocked({ reason: "disabled" }, "T");
    expect(out.kind).toBe("blocked-off");
    expect(out.action).toBe("");
    expect(out.target).toBe("");
    expect(out.reason).toBe("");
  });

  it("reason=device_gated is the Accessibility-TCC seam refusing/failing", () => {
    const out = parseUiActuateBlocked({ reason: "device_gated" }, "T");
    expect(out.kind).toBe("blocked-device");
    expect(out.reason).toBe("device_gated");
  });

  it("never throws on a malformed payload", () => {
    expect(() => parseUiActuateBlocked({}, "T")).not.toThrow();
    const out = parseUiActuateBlocked({}, "T");
    // No reason => not "disabled" => treated as the device-gated arm, honestly.
    expect(out.kind).toBe("blocked-device");
  });
});

describe("parseUiActuateRefused (planner refusal — pre-actuation)", () => {
  it("surfaces the planner's honest reason; never parked, never acted", () => {
    const out = parseUiActuateRefused(
      { reason: "the target (5000, 5000) is off-screen" },
      "T",
    );
    expect(out.kind).toBe("refused");
    expect(out.reason).toContain("off-screen");
    // A refusal carries NO action/target (nothing was planned).
    expect(out.action).toBe("");
  });

  it("defaults to 'unknown' when no reason is present (never throws)", () => {
    const out = parseUiActuateRefused({}, "T");
    expect(out.kind).toBe("refused");
    expect(out.reason).toBe("unknown");
  });
});

describe("parseUiActuateActionEvent (parked / actuating / actuated)", () => {
  it("parses a parked preview carrying the action class + faithful target", () => {
    const out = parseUiActuateActionEvent(
      "parked",
      { action: "click", target: "the Send button" },
      "T",
    );
    expect(out).not.toBeNull();
    expect(out?.kind).toBe("parked");
    expect(out?.action).toBe("click");
    expect(out?.target).toBe("the Send button");
  });

  it("parses the faithful single-action result (actuated)", () => {
    const out = parseUiActuateActionEvent(
      "actuated",
      { action: "key", target: "the document window" },
      "T",
    );
    expect(out?.kind).toBe("actuated");
    expect(out?.action).toBe("key");
  });

  it("drops a phantom event with no action (the panel never shows one)", () => {
    expect(parseUiActuateActionEvent("parked", {}, "T")).toBeNull();
    expect(parseUiActuateActionEvent("actuating", { action: "" }, "T")).toBeNull();
  });

  it("NEVER carries typed text or coordinates on the wire (secret-free)", () => {
    // Even if a (hypothetical) payload smuggled text/x/y, the parser only reads
    // action + target — nothing else can leak through this surface.
    const out = parseUiActuateActionEvent(
      "actuating",
      { action: "type", target: "a field", text: "secret", x: 10, y: 20 },
      "T",
    );
    expect(out?.action).toBe("type");
    expect(out?.target).toBe("a field");
    expect(out).not.toHaveProperty("text");
    expect(JSON.stringify(out)).not.toContain("secret");
  });
});

/* ------------------------------------------------------------------------ *
 * The reducer folds ui_actuate.* into the uiActuate surface — and the HONEST
 * per-action, never-auto-run, never-fabricate contract holds end-to-end.
 * ------------------------------------------------------------------------ */
describe("reduce(ui_actuate.*) — the honest per-action surface", () => {
  it("ships OFF: the surface is null until the first event", () => {
    expect(connected().uiActuate).toBeNull();
  });

  it("the OFF/locked gate lands as blocked-off (the inert default, not an error)", () => {
    const s = tel(connected(), env("ui_actuate.blocked", { reason: "disabled" }));
    expect((s.uiActuate as UiActuateSurface).last?.kind).toBe("blocked-off");
  });

  it("a degenerate/off-screen instruction is refused PRE-actuation (never parked)", () => {
    const s = tel(
      connected(),
      env("ui_actuate.refused", { reason: "the instruction named no target" }),
    );
    expect((s.uiActuate as UiActuateSurface).last?.kind).toBe("refused");
  });

  it("a preview PARKS (never auto-runs) — the act only follows a separate actuating", () => {
    let s = tel(connected(), env("ui_actuate.preview", { action: "click", target: "OK" }));
    expect((s.uiActuate as UiActuateSurface).last?.kind).toBe("parked");
    // The act only follows AFTER the full gate, as a SEPARATE event.
    s = tel(s, env("ui_actuate.actuating", { action: "click", target: "OK" }));
    expect((s.uiActuate as UiActuateSurface).last?.kind).toBe("actuating");
    s = tel(s, env("ui_actuate.actuated", { action: "click", target: "OK" }));
    expect((s.uiActuate as UiActuateSurface).last?.kind).toBe("actuated");
  });

  it("PER-ACTION: a second actuation re-parks (one confirm = one actuation)", () => {
    // Action A is actuated…
    let s = tel(connected(), env("ui_actuate.preview", { action: "click", target: "A" }));
    s = tel(s, env("ui_actuate.actuated", { action: "click", target: "A" }));
    expect((s.uiActuate as UiActuateSurface).last?.kind).toBe("actuated");
    // …then action B PARKS AGAIN — it needs its OWN confirm; the surface shows
    // the fresh parked preview, never an auto-run carried over from A.
    s = tel(s, env("ui_actuate.preview", { action: "click", target: "B" }));
    const last = (s.uiActuate as UiActuateSurface).last;
    expect(last?.kind).toBe("parked");
    expect(last?.target).toBe("B");
  });

  it("a phantom preview (no action) is dropped — the prior outcome is kept", () => {
    let s = tel(connected(), env("ui_actuate.preview", { action: "click", target: "A" }));
    const before = (s.uiActuate as UiActuateSurface).last;
    s = tel(s, env("ui_actuate.preview", {})); // no action -> dropped
    expect((s.uiActuate as UiActuateSurface).last).toBe(before);
  });

  it("device-gated absence of consent lands as blocked-device, never a fake success", () => {
    const s = tel(connected(), env("ui_actuate.blocked", { reason: "device_gated" }));
    const last = (s.uiActuate as UiActuateSurface).last;
    expect(last?.kind).toBe("blocked-device");
    expect(last?.reason).toBe("device_gated");
  });
});
