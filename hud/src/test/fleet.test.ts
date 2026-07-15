import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import FleetPanel from "../components/FleetPanel";
import { parseFleetStatus, type FleetStatus, type TelemetryEnvelope } from "../core/events";
import { initialState, reduce, type HudState } from "../core/state";

let counter = 0;
function env(event: string, data: Record<string, unknown>, source = "system"): TelemetryEnvelope {
  counter += 1;
  return { ts: `2026-07-15T00:00:${String(counter % 60).padStart(2, "0")}Z`, source, event, data };
}
function connected() {
  return reduce(initialState(), { type: "ws.connected", at: 0 });
}
function tel(state: HudState, e: TelemetryEnvelope) {
  return reduce(state, { type: "telemetry", envelope: e, at: 1000 });
}

/** Mirrors daemon/src/fleet.rs::status_payload (OFF). */
const offWire = {
  enabled: false,
  baseline_active: false,
  authored_by: "",
  created: "",
  rule_count: 0,
  rules: [],
  hardens_only: true,
  transport_inert: true,
};

/** Mirrors an ACTIVE baseline authored by "mac-studio" with two ceilings. */
const activeWire = {
  enabled: true,
  baseline_active: true,
  authored_by: "mac-studio",
  created: "2026-07-15T10:00:00Z",
  rule_count: 2,
  rules: [
    { tool: "gmail_send", decision: "ask" },
    { tool: "x_post", decision: "never" },
  ],
  hardens_only: true,
  transport_inert: true,
};

describe("parseFleetStatus (pins the honest invariants)", () => {
  it("parses the off state", () => {
    expect(parseFleetStatus(offWire)).toEqual({
      enabled: false,
      baselineActive: false,
      authoredBy: "",
      created: "",
      ruleCount: 0,
      rules: [],
      hardensOnly: true,
    });
  });

  it("parses an active baseline with its authoring device + per-tool ceilings", () => {
    const f = parseFleetStatus(activeWire);
    expect(f.enabled).toBe(true);
    expect(f.baselineActive).toBe(true);
    expect(f.authoredBy).toBe("mac-studio");
    expect(f.created).toBe("2026-07-15T10:00:00Z");
    expect(f.ruleCount).toBe(2);
    expect(f.rules).toEqual([
      { tool: "gmail_send", decision: "ask" },
      { tool: "x_post", decision: "never" },
    ]);
  });

  it("pins hardensOnly true and never lets a payload claim it grants", () => {
    const spoofed = parseFleetStatus({ ...activeWire, hardens_only: false });
    expect(spoofed.hardensOnly).toBe(true);
  });

  it("reads baseline_active as a literal true only (a garbled frame is never active)", () => {
    expect(parseFleetStatus({ ...offWire, baseline_active: "yes" }).baselineActive).toBe(false);
    expect(parseFleetStatus({ ...offWire, enabled: 1 }).enabled).toBe(false);
  });

  it("coerces a junk hardening to the strictest 'never' (never a looser display)", () => {
    const f = parseFleetStatus({
      ...activeWire,
      rules: [
        { tool: "gmail_send", decision: "always" }, // not a valid hardening -> never
        { tool: "x_post", decision: "garbage" }, // junk -> never
      ],
    });
    expect(f.rules).toEqual([
      { tool: "gmail_send", decision: "never" },
      { tool: "x_post", decision: "never" },
    ]);
  });

  it("drops a rule with no usable tool id, and bounds the list", () => {
    const f = parseFleetStatus({
      ...activeWire,
      rules: [{ decision: "never" }, { tool: "", decision: "ask" }, { tool: "ok", decision: "ask" }],
    });
    expect(f.rules).toEqual([{ tool: "ok", decision: "ask" }]);
  });

  it("degrades a malformed frame to the honest off state", () => {
    const d = parseFleetStatus({});
    expect(d.enabled).toBe(false);
    expect(d.baselineActive).toBe(false);
    expect(d.rules).toEqual([]);
    expect(d.hardensOnly).toBe(true);
  });
});

describe("fleet.status reducer", () => {
  it("is null until the first frame, then set", () => {
    let s = connected();
    expect(s.fleet).toBeNull();
    s = tel(s, env("fleet.status", activeWire));
    expect(s.fleet?.enabled).toBe(true);
    expect(s.fleet?.baselineActive).toBe(true);
    expect(s.fleet?.authoredBy).toBe("mac-studio");
  });
});

describe("FleetPanel", () => {
  const render = (fleet: FleetStatus | null) =>
    renderToStaticMarkup(createElement(FleetPanel, { fleet }));

  it("renders nothing before the first frame", () => {
    expect(render(null)).toBe("");
  });

  it("shows OFF and the hardens-only / floor-of-strictness footnote", () => {
    const html = render(parseFleetStatus(offWire));
    expect(html).toContain("FLEET // POLICY");
    expect(html).toContain("OFF");
    expect(html).toContain("floor of");
    expect(html).toContain("never loosen");
  });

  it("shows ARMED · AWAITING BASELINE when enabled but no baseline yet", () => {
    const html = render(parseFleetStatus({ ...offWire, enabled: true }));
    expect(html).toContain("ARMED · AWAITING BASELINE");
  });

  it("shows ACTIVE, the authoring device, and each ceiling (NEVER / ALWAYS ASK)", () => {
    const html = render(parseFleetStatus(activeWire));
    expect(html).toContain("ACTIVE");
    expect(html).toContain("authored by mac-studio");
    expect(html).toContain("gmail_send");
    expect(html).toContain("ALWAYS ASK");
    expect(html).toContain("x_post");
    expect(html).toContain("NEVER");
  });
});
