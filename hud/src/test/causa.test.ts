import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import CausaTracePanel from "../components/CausaTracePanel";
import {
  parseCausaTrace,
  CAUSA_STEPS_CAP,
  type CausaTrace,
  type TelemetryEnvelope,
} from "../core/events";
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

/** Mirrors daemon/src/explain.rs::payload's wire shape. */
const wire = {
  query: "last",
  agent_query: "",
  empty: false,
  turn_ref: 7,
  ts: "2026-07-15T12:00:00Z",
  agent: "darwin",
  utterance: "pause the ads campaign",
  steps: [
    { stage: "intent", chosen: "action", why: "I classified the request as \"action\" (72% confident)", alternatives: [] },
    { stage: "selector", chosen: "act", why: "the selector read the phrasing as \"act\"", alternatives: [] },
    { stage: "agent", chosen: "darwin", why: "a \"action\" request routed to the darwin agent (agent.darwin)", alternatives: [] },
    { stage: "route", chosen: "local", why: "I answered on-device (the local model), no cloud call", alternatives: [] },
    { stage: "outcome", chosen: "answered", why: "Paused the campaign, sir.", alternatives: [] },
  ],
};

/** The honest-empty frame the daemon rides when a turn wasn't recorded. */
const emptyWire = {
  query: "agent",
  agent_query: "nobody",
  empty: true,
  turn_ref: 0,
  ts: "",
  agent: "",
  utterance: "",
  steps: [],
};

describe("parseCausaTrace (never fabricates a trace)", () => {
  it("parses the daemon's wire shape", () => {
    const t = parseCausaTrace(wire);
    expect(t).not.toBeNull();
    expect(t?.query).toBe("last");
    expect(t?.empty).toBe(false);
    expect(t?.turnRef).toBe(7);
    expect(t?.agent).toBe("darwin");
    expect(t?.steps).toHaveLength(5);
    expect(t?.steps[2]).toEqual({
      stage: "agent",
      chosen: "darwin",
      why: 'a "action" request routed to the darwin agent (agent.darwin)',
      alternatives: [],
    });
  });

  it("parses the honest-empty frame (no steps, keeps the query)", () => {
    const t = parseCausaTrace(emptyWire);
    expect(t).not.toBeNull();
    expect(t?.empty).toBe(true);
    expect(t?.query).toBe("agent");
    expect(t?.agentQuery).toBe("nobody");
    expect(t?.steps).toHaveLength(0);
  });

  it("drops a frame with no valid query", () => {
    expect(parseCausaTrace({})).toBeNull();
    expect(parseCausaTrace({ query: "sideways", steps: [] })).toBeNull();
  });

  it("caps steps, bounds strings, and drops malformed rows", () => {
    const bloated = {
      query: "last",
      steps: Array.from({ length: 100 }, (_, i) => ({
        stage: `stage${i}`,
        chosen: "z".repeat(5000),
        why: "w".repeat(5000),
        alternatives: Array.from({ length: 50 }, (_, j) => `alt${j}`),
      })),
    };
    const t = parseCausaTrace(bloated);
    expect(t?.steps).toHaveLength(CAUSA_STEPS_CAP);
    for (const step of t?.steps ?? []) {
      expect(step.chosen.length).toBeLessThanOrEqual(240);
      expect(step.why.length).toBeLessThanOrEqual(240);
      expect(step.alternatives.length).toBeLessThanOrEqual(6);
    }
    // A step without a stage is meaningless and dropped, not fatal.
    const partial = parseCausaTrace({
      query: "last",
      steps: [{ chosen: "x" }, "junk", { stage: "intent", chosen: "ok" }],
    });
    expect(partial?.steps).toHaveLength(1);
    expect(partial?.steps[0].stage).toBe("intent");
  });
});

describe("causa.trace reducer", () => {
  it("is null until the first ask, then replaces wholesale", () => {
    let s = connected();
    expect(s.causaTrace).toBeNull();
    s = tel(s, env("causa.trace", wire));
    expect(s.causaTrace?.turnRef).toBe(7);
    s = tel(s, env("causa.trace", { ...wire, turn_ref: 9 }));
    expect(s.causaTrace?.turnRef).toBe(9);
  });

  it("drops a malformed frame (same reference)", () => {
    let s = connected();
    s = tel(s, env("causa.trace", wire));
    const before = s.causaTrace;
    s = tel(s, env("causa.trace", { junk: true }));
    expect(s.causaTrace).toBe(before);
  });
});

describe("CausaTracePanel", () => {
  const render = (trace: CausaTrace | null) =>
    renderToStaticMarkup(createElement(CausaTracePanel, { trace }));

  it("renders nothing before the first ask", () => {
    expect(render(null)).toBe("");
  });

  it("shows the ordered decision steps and the review-only footnote", () => {
    const html = render(parseCausaTrace(wire) as CausaTrace);
    expect(html).toContain("CAUSA // DECISION TRACE");
    expect(html).toContain("WHY DID YOU DO THAT");
    expect(html).toContain("turn #7");
    expect(html).toContain("INTENT");
    expect(html).toContain("ROUTE");
    expect(html).toContain("routed to the darwin agent");
    expect(html).toContain("nothing is re-executed");
  });

  it("labels a named-agent ask and renders the honest-empty state", () => {
    const empty = parseCausaTrace(emptyWire) as CausaTrace;
    const html = render(empty);
    expect(html).toContain("WHY · NOBODY");
    expect(html).toContain("no recent trace of the nobody agent");
    // No steps are rendered in the empty state.
    expect(html).not.toContain("INTENT");
  });
});
