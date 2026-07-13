import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import SessionRewindPanel from "../components/SessionRewindPanel";
import {
  parseSessionRewind,
  REWIND_ITEMS_CAP,
  type SessionRewind,
  type TelemetryEnvelope,
} from "../core/events";
import { initialState, reduce, type HudState } from "../core/state";

let counter = 0;
function env(event: string, data: Record<string, unknown>, source = "system"): TelemetryEnvelope {
  counter += 1;
  return { ts: `2026-07-13T00:00:${String(counter % 60).padStart(2, "0")}Z`, source, event, data };
}
function connected() {
  return reduce(initialState(), { type: "ws.connected", at: 0 });
}
function tel(state: HudState, e: TelemetryEnvelope) {
  return reduce(state, { type: "telemetry", envelope: e, at: 1000 });
}

/** Mirrors daemon/src/rewind.rs::payload's wire shape. */
const wire = {
  label: "the last hour",
  from: "2026-07-13T19:00:00+00:00",
  to: "2026-07-13T20:00:00+00:00",
  empty: false,
  turn_count: 2,
  action_count: 1,
  counts_floor: false,
  items_omitted: 0,
  items: [
    { ts: "2026-07-13T19:10:00+00:00", kind: "turn", text: "Asked about inflation", detail: "economics" },
    { ts: "2026-07-13T19:20:00+00:00", kind: "action", text: "gmail_send — parked", detail: "an email to [redacted]" },
    { ts: "2026-07-13T19:40:00+00:00", kind: "turn", text: "Checked the weather", detail: "weather" },
  ],
};

describe("parseSessionRewind (never fabricates a window)", () => {
  it("parses the daemon's wire shape", () => {
    const r = parseSessionRewind(wire);
    expect(r).not.toBeNull();
    expect(r?.label).toBe("the last hour");
    expect(r?.turnCount).toBe(2);
    expect(r?.actionCount).toBe(1);
    expect(r?.countsFloor).toBe(false);
    expect(r?.itemsOmitted).toBe(0);
    expect(r?.items).toHaveLength(3);
    expect(r?.items[1]).toEqual({
      ts: "2026-07-13T19:20:00+00:00",
      kind: "action",
      text: "gmail_send — parked",
      detail: "an email to [redacted]",
    });
  });

  it("drops a frame without a window label", () => {
    expect(parseSessionRewind({})).toBeNull();
    expect(parseSessionRewind({ label: "  ", items: [] })).toBeNull();
  });

  it("caps items, bounds strings, and never invents an action kind", () => {
    const bloated = {
      label: "today",
      items: Array.from({ length: 100 }, (_, i) => ({
        ts: "2026-07-13T19:00:00+00:00",
        kind: i % 2 === 0 ? "detonate" : "action",
        text: `item ${i} ${"z".repeat(5000)}`,
        detail: "d".repeat(5000),
      })),
    };
    const r = parseSessionRewind(bloated);
    expect(r?.items).toHaveLength(REWIND_ITEMS_CAP);
    for (const item of r?.items ?? []) {
      expect(item.text.length).toBeLessThanOrEqual(240);
      expect(item.detail.length).toBeLessThanOrEqual(240);
      // Unknown kinds coerce to the neutral "turn", never a fabricated action.
      expect(["turn", "action"]).toContain(item.kind);
    }
    expect(r?.items[0].kind).toBe("turn");
    // Malformed rows are dropped, not fatal.
    const partial = parseSessionRewind({
      label: "today",
      items: [{ kind: "turn" }, "junk", { ts: "t", kind: "turn", text: "ok" }],
    });
    expect(partial?.items).toHaveLength(1);
  });
});

describe("session.rewind reducer", () => {
  it("is null until the first rewind, then replaces wholesale", () => {
    let s = connected();
    expect(s.sessionRewind).toBeNull();
    s = tel(s, env("session.rewind", wire));
    expect(s.sessionRewind?.label).toBe("the last hour");
    s = tel(s, env("session.rewind", { ...wire, label: "this morning" }));
    expect(s.sessionRewind?.label).toBe("this morning");
  });

  it("drops a malformed frame (same reference)", () => {
    let s = connected();
    s = tel(s, env("session.rewind", wire));
    const before = s.sessionRewind;
    s = tel(s, env("session.rewind", { junk: true }));
    expect(s.sessionRewind).toBe(before);
  });
});

describe("SessionRewindPanel", () => {
  const render = (rewind: SessionRewind | null) =>
    renderToStaticMarkup(createElement(SessionRewindPanel, { rewind }));

  it("renders nothing before the first rewind", () => {
    expect(render(null)).toBe("");
  });

  it("shows the timeline with kind markers and the review-only footnote", () => {
    const html = render(parseSessionRewind(wire) as SessionRewind);
    expect(html).toContain("REWIND // SESSION TIMELINE");
    expect(html).toContain("THE LAST HOUR");
    expect(html).toContain("2 turns");
    expect(html).toContain("1 gated action");
    expect(html).toContain("Asked about inflation");
    expect(html).toContain("gmail_send — parked");
    expect(html).toContain("ACTION");
    expect(html).toContain("nothing is re-executed");
  });

  it("discloses omitted items, floors, and renders the honest empty state", () => {
    const withOmitted = parseSessionRewind({ ...wire, items_omitted: 5 }) as SessionRewind;
    expect(render(withOmitted)).toContain("5 earlier not shown");
    // A saturated store read shows counts as a floor, never exact.
    const floored = parseSessionRewind({ ...wire, counts_floor: true }) as SessionRewind;
    expect(render(floored)).toContain("≥2 turns");

    const empty = parseSessionRewind({
      ...wire,
      empty: true,
      turn_count: 0,
      action_count: 0,
      items: [],
    }) as SessionRewind;
    const html = render(empty);
    expect(html).toContain("nothing recorded in this window");
    expect(html).toContain("non-transient");
  });
});
