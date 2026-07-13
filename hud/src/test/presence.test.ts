import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import PresencePanel from "../components/PresencePanel";
import { parsePresence, type Presence, type TelemetryEnvelope } from "../core/events";
import { initialState, reduce } from "../core/state";

let counter = 0;
function env(event: string, data: Record<string, unknown>, source = "agent.edith"): TelemetryEnvelope {
  counter += 1;
  return { ts: `2026-07-12T00:00:${String(counter % 60).padStart(2, "0")}Z`, source, event, data };
}
function connected() {
  return reduce(initialState(), { type: "ws.connected", at: 0 });
}

describe("parsePresence (never invents away/focused)", () => {
  it("parses a well-formed focused payload", () => {
    const p = parsePresence({
      state: "focused",
      at_machine: true,
      focus_dnd: false,
      signals: { input: true, speech: false, vision: false },
    });
    expect(p).toEqual({
      state: "focused",
      atMachine: true,
      focusDnd: false,
      signals: { input: true, speech: false, vision: false },
    });
  });

  it("coerces an unknown/absent state to the neutral 'present'", () => {
    expect(parsePresence({}).state).toBe("present");
    expect(parsePresence({ state: "bogus" }).state).toBe("present");
    // a garbled signals object degrades to all-false, never throws
    expect(parsePresence({ state: "away", signals: 5 }).signals).toEqual({
      input: false,
      speech: false,
      vision: false,
    });
  });
});

describe("presence.state reducer", () => {
  it("starts null and is populated by a presence.state frame", () => {
    const s0 = connected();
    expect(s0.presence).toBeNull();
    const s1 = reduce(s0, {
      type: "telemetry",
      envelope: env("presence.state", { state: "focused", at_machine: true }),
      at: 1000,
    });
    expect(s1.presence?.state).toBe("focused");
  });
});

describe("PresencePanel", () => {
  const render = (p: Presence | null) => renderToStaticMarkup(createElement(PresencePanel, { presence: p }));

  it("renders nothing before the first frame", () => {
    expect(render(null)).toBe("");
  });

  it("shows the FOCUSED pill and the 'holding spoken proactivity' note", () => {
    const html = render(parsePresence({ state: "focused", at_machine: true, signals: { input: true } }));
    expect(html).toContain("FOCUSED · IN FLOW");
    expect(html).toContain("holding spoken proactivity");
    expect(html).toContain("presence-pill focused");
  });

  it("PRESENT does not claim to be holding proactivity", () => {
    const html = render(parsePresence({ state: "present", at_machine: true }));
    expect(html).toContain("PRESENT");
    expect(html).not.toContain("holding spoken proactivity");
  });

  it("dims a signal that is not feeding the fusion", () => {
    const html = render(parsePresence({ state: "present", signals: { input: true, speech: false, vision: false } }));
    expect(html).toContain("presence-signal on"); // input
    expect(html).toContain("presence-signal off"); // speech / vision
  });
});
