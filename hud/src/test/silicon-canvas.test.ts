import { describe, expect, it } from "vitest";
import {
  CANVAS_TOPIC_RENDER_MS,
  CANVAS_TOPIC_SELECTION,
  CANVAS_TOPIC_VIEWPORT,
  parseCanvasRenderMs,
  parseCanvasSelection,
  parseCanvasViewport,
  type TelemetryEnvelope,
} from "../core/events";
import { initialState, reduce, type HudState } from "../core/state";

/* ------------------------------------------------------------------------ *
 * Pure payload parsers (events.ts) — verbatim against the app's wire structs *
 * in apps/silicon-canvas/src/ops.rs. The Metal IOSurface composite runs      *
 * on-device and is NOT exercised here (it cannot be verified headlessly);    *
 * these cover only the telemetry the HUD-side panel renders.                 *
 * ------------------------------------------------------------------------ */

describe("parseCanvasRenderMs (canvas.render_ms)", () => {
  it("parses a full frame-stats payload", () => {
    const r = parseCanvasRenderMs({ p50: 4.2, p95: 9.8, draws: 27, culled_pct: 61.5 });
    expect(r).toEqual({ p50: 4.2, p95: 9.8, draws: 27, culledPct: 61.5 });
  });

  it("returns null when any stat is missing or non-finite", () => {
    expect(parseCanvasRenderMs({ p50: 4, p95: 9, draws: 27 })).toBeNull();
    expect(parseCanvasRenderMs({ p50: 4, p95: 9, draws: 27, culled_pct: "x" })).toBeNull();
    expect(parseCanvasRenderMs({ p50: 4, p95: 9, draws: 27, culled_pct: NaN })).toBeNull();
    expect(parseCanvasRenderMs({})).toBeNull();
  });
});

describe("parseCanvasViewport (canvas.viewport)", () => {
  it("parses pose + PCB layer visibility", () => {
    const v = parseCanvasViewport({
      x: 12.5,
      y: -3.0,
      scale: 18.0,
      layer_visibility: [
        { layer: "F.Cu", visible: true },
        { layer: "B.Cu", visible: false },
      ],
    });
    expect(v).toEqual({
      x: 12.5,
      y: -3.0,
      scale: 18.0,
      layerVisibility: [
        { layer: "F.Cu", visible: true },
        { layer: "B.Cu", visible: false },
      ],
    });
  });

  it("defaults layer_visibility to [] for a schematic (single logical layer)", () => {
    const v = parseCanvasViewport({ x: 0, y: 0, scale: 1 });
    expect(v).not.toBeNull();
    expect(v!.layerVisibility).toEqual([]);
  });

  it("drops malformed layer entries instead of failing the whole payload", () => {
    const v = parseCanvasViewport({
      x: 0,
      y: 0,
      scale: 1,
      layer_visibility: [
        { layer: "F.Cu", visible: true },
        { layer: 5, visible: true }, // bad layer name
        { layer: "B.Cu", visible: "yes" }, // bad visible
        "not-an-object",
      ],
    });
    expect(v!.layerVisibility).toEqual([{ layer: "F.Cu", visible: true }]);
  });

  it("returns null when x/y/scale are not all finite numbers", () => {
    expect(parseCanvasViewport({ y: 0, scale: 1 })).toBeNull();
    expect(parseCanvasViewport({ x: 0, y: 0, scale: "10" })).toBeNull();
    expect(parseCanvasViewport({ x: 0, y: 0, scale: Infinity })).toBeNull();
  });
});

describe("parseCanvasSelection (canvas.selection)", () => {
  it("parses a net selection with no ERC (erc stays null — not an ERC result)", () => {
    const s = parseCanvasSelection({
      net: { net: 7, name: "3V3", entity_count: 42, pin_count: 9 },
    });
    expect(s.net).toEqual({ net: 7, name: "3V3", entityCount: 42, pinCount: 9 });
    expect(s.component).toBeNull();
    expect(s.erc).toBeNull();
    expect(s.trace).toBeNull();
  });

  it("parses a component selection", () => {
    const s = parseCanvasSelection({
      component: { component: 3, reference: "U3", value: "STM32F405", pin_count: 64 },
    });
    expect(s.component).toEqual({
      component: 3,
      reference: "U3",
      value: "STM32F405",
      pinCount: 64,
    });
  });

  it("distinguishes a clean ERC drop ([] findings) from a plain selection (null)", () => {
    const clean = parseCanvasSelection({ erc: [] });
    expect(clean.erc).toEqual([]); // ERC ran, no findings
    const plain = parseCanvasSelection({ net: { net: 1, name: "GND", entity_count: 5, pin_count: 3 } });
    expect(plain.erc).toBeNull(); // not an ERC result line
  });

  it("parses ERC findings with warning/error severities and fault coords", () => {
    const s = parseCanvasSelection({
      erc: [
        {
          code: "unconnected_pin",
          severity: "warning",
          at: { x: 10, y: 20 },
          message: "Pin 3 of U1 is unconnected",
        },
        {
          code: "output_conflict",
          severity: "error",
          at: { x: 5, y: 5 },
          message: "Two drivers on net N$4",
        },
      ],
    });
    expect(s.erc).toHaveLength(2);
    expect(s.erc![0]).toEqual({
      code: "unconnected_pin",
      severity: "warning",
      at: { x: 10, y: 20 },
      message: "Pin 3 of U1 is unconnected",
    });
    expect(s.erc![1].severity).toBe("error");
  });

  it("drops ERC markers with an unknown/missing severity or code; defaults at to origin", () => {
    const s = parseCanvasSelection({
      erc: [
        { code: "label_typo", severity: "info" }, // unknown severity -> dropped
        { severity: "error", message: "no code" }, // missing code -> dropped
        { code: "dangling_wire", severity: "warning" }, // no `at` / message
        "junk",
      ],
    });
    expect(s.erc).toHaveLength(1);
    expect(s.erc![0]).toEqual({
      code: "dangling_wire",
      severity: "warning",
      at: { x: 0, y: 0 },
      message: "",
    });
  });

  it("never throws on a malformed net/component object (yields null for that slice)", () => {
    const s = parseCanvasSelection({ net: { entity_count: 3 }, component: "nope" });
    expect(s.net).toBeNull();
    expect(s.component).toBeNull();
    expect(s.erc).toBeNull();
    expect(s.trace).toBeNull();
  });
});

/* ------------------------------------------------------------------------ *
 * canvas.selection.trace sub-payload (ops.rs TraceStep) — present ONLY while  *
 * a net trace is walking; absent on plain selections / ERC drops / after     *
 * trace.stop. The on-device GPU does the via-flash; this is the telemetry.    *
 * Wire shape (locked by ops.rs selection_trace_subpayload_wire_shape):        *
 *   "trace":{"at":{"kind":"via","index":7},"distance":2,"crosses_layer":true,*
 *            "step":3,"of":5,"at_end":false}                                  *
 * ------------------------------------------------------------------------ */
describe("parseCanvasSelection — trace sub-payload", () => {
  it("parses a well-formed trace front (verbatim wire shape)", () => {
    const s = parseCanvasSelection({
      net: { net: 7, name: "3V3", entity_count: 42, pin_count: 9 },
      trace: {
        at: { kind: "via", index: 7 },
        distance: 2,
        crosses_layer: true,
        step: 3,
        of: 5,
        at_end: false,
      },
    });
    expect(s.trace).toEqual({
      at: { kind: "via", index: 7 },
      distance: 2,
      crossesLayer: true,
      step: 3,
      of: 5,
      atEnd: false,
    });
    // sibling fields still apply
    expect(s.net!.name).toBe("3V3");
  });

  it("parses the at_end re-report of the last node", () => {
    const s = parseCanvasSelection({
      trace: {
        at: { kind: "pad", index: 12 },
        distance: 4,
        crosses_layer: false,
        step: 5,
        of: 5,
        at_end: true,
      },
    });
    expect(s.trace).toEqual({
      at: { kind: "pad", index: 12 },
      distance: 4,
      crossesLayer: false,
      step: 5,
      of: 5,
      atEnd: true,
    });
  });

  it("keeps an unknown future entity kind opaque rather than dropping the front", () => {
    const s = parseCanvasSelection({
      trace: {
        at: { kind: "mystery_kind", index: 1 },
        distance: 0,
        crosses_layer: false,
        step: 1,
        of: 1,
        at_end: true,
      },
    });
    expect(s.trace!.at.kind).toBe("mystery_kind");
    expect(s.trace!.at.index).toBe(1);
  });

  it("is null when absent — a plain net selection / ERC drop is unaffected", () => {
    const plain = parseCanvasSelection({
      net: { net: 1, name: "GND", entity_count: 5, pin_count: 3 },
    });
    expect(plain.trace).toBeNull();
    expect(plain.net!.name).toBe("GND"); // sibling intact

    const ercDrop = parseCanvasSelection({ erc: [] });
    expect(ercDrop.trace).toBeNull();
    expect(ercDrop.erc).toEqual([]); // ERC drop intact
  });

  it("yields null (never throws) on a malformed/partial trace and leaves siblings intact", () => {
    // trace is not an object
    expect(parseCanvasSelection({ trace: "nope" }).trace).toBeNull();
    expect(parseCanvasSelection({ trace: 42 }).trace).toBeNull();
    expect(parseCanvasSelection({ trace: null }).trace).toBeNull();
    expect(parseCanvasSelection({ trace: [] }).trace).toBeNull();
    // missing/invalid `at` ref (the one required slice) -> null, no throw
    expect(parseCanvasSelection({ trace: { distance: 2, step: 1, of: 3 } }).trace).toBeNull();
    expect(parseCanvasSelection({ trace: { at: "via" } }).trace).toBeNull();
    expect(parseCanvasSelection({ trace: { at: { kind: "via" } } }).trace).toBeNull(); // no index
    expect(parseCanvasSelection({ trace: { at: { index: 7 } } }).trace).toBeNull(); // no kind
    // a malformed trace must not disturb a valid sibling net selection
    const s = parseCanvasSelection({
      net: { net: 2, name: "VBUS", entity_count: 8, pin_count: 4 },
      trace: { at: { kind: 9, index: 7 } }, // kind not a string
    });
    expect(s.trace).toBeNull();
    expect(s.net!.name).toBe("VBUS");
  });

  it("defaults the numeric/flag progress fields when only `at` is present", () => {
    const s = parseCanvasSelection({
      trace: { at: { kind: "track", index: 3 } },
    });
    expect(s.trace).toEqual({
      at: { kind: "track", index: 3 },
      distance: 0,
      crossesLayer: false,
      step: 0,
      of: 0,
      atEnd: false,
    });
  });
});

/* ------------------------------------------------------------------------ *
 * Reducer: app.data stashes each canvas topic under feed.topics, keyed by    *
 * the relay topic — additive, and must not disturb the global-scan feed view.*
 * ------------------------------------------------------------------------ */

const SC = "silicon-canvas";

function env(event: string, data: Record<string, unknown>): TelemetryEnvelope {
  return { ts: "2026-06-13T12:00:00.000Z", source: "system", event, data };
}

function tel(state: HudState, e: TelemetryEnvelope, at = 1000): HudState {
  return reduce(state, { type: "telemetry", envelope: e, at });
}

function connected(): HudState {
  return reduce(initialState(), { type: "ws.connected", at: 0 });
}

describe("reducer: app.data canvas topic storage", () => {
  it("stores each canvas topic payload verbatim under feed.topics[topic]", () => {
    let s = connected();
    s = tel(s, env("app.data", {
      name: SC,
      topic: CANVAS_TOPIC_RENDER_MS,
      payload: { p50: 4.2, p95: 9.8, draws: 27, culled_pct: 60 },
    }));
    s = tel(s, env("app.data", {
      name: SC,
      topic: CANVAS_TOPIC_VIEWPORT,
      payload: { x: 1, y: 2, scale: 12, layer_visibility: [] },
    }));
    s = tel(s, env("app.data", {
      name: SC,
      topic: CANVAS_TOPIC_SELECTION,
      payload: { net: { net: 7, name: "3V3", entity_count: 42, pin_count: 9 } },
    }));

    const feed = s.appFeeds[SC];
    expect(feed.running).toBe(true);
    expect(s.runningApps.has(SC)).toBe(true);

    // Each topic slice round-trips through the matching parser.
    expect(parseCanvasRenderMs(feed.topics[CANVAS_TOPIC_RENDER_MS])).toEqual({
      p50: 4.2,
      p95: 9.8,
      draws: 27,
      culledPct: 60,
    });
    expect(parseCanvasViewport(feed.topics[CANVAS_TOPIC_VIEWPORT])!.scale).toBe(12);
    expect(parseCanvasSelection(feed.topics[CANVAS_TOPIC_SELECTION]).net!.name).toBe("3V3");
  });

  it("a newer payload on the SAME topic replaces it; other topics are retained", () => {
    let s = connected();
    s = tel(s, env("app.data", {
      name: SC,
      topic: CANVAS_TOPIC_RENDER_MS,
      payload: { p50: 4, p95: 9, draws: 27, culled_pct: 60 },
    }));
    s = tel(s, env("app.data", {
      name: SC,
      topic: CANVAS_TOPIC_SELECTION,
      payload: { erc: [] },
    }));
    // Newer render_ms on the same topic.
    s = tel(s, env("app.data", {
      name: SC,
      topic: CANVAS_TOPIC_RENDER_MS,
      payload: { p50: 5, p95: 11, draws: 30, culled_pct: 55 },
    }));

    const feed = s.appFeeds[SC];
    expect(parseCanvasRenderMs(feed.topics[CANVAS_TOPIC_RENDER_MS])!.draws).toBe(30);
    // The selection topic stored earlier survives the render_ms update.
    expect(parseCanvasSelection(feed.topics[CANVAS_TOPIC_SELECTION]).erc).toEqual([]);
  });

  it("does not mutate the prior topics map in place (immutable update)", () => {
    let s = connected();
    s = tel(s, env("app.data", {
      name: SC,
      topic: CANVAS_TOPIC_RENDER_MS,
      payload: { p50: 4, p95: 9, draws: 27, culled_pct: 60 },
    }));
    const beforeTopics = s.appFeeds[SC].topics;
    s = tel(s, env("app.data", {
      name: SC,
      topic: CANVAS_TOPIC_VIEWPORT,
      payload: { x: 0, y: 0, scale: 1, layer_visibility: [] },
    }));
    // The earlier snapshot's topics map is unchanged (no viewport key added).
    expect(CANVAS_TOPIC_VIEWPORT in beforeTopics).toBe(false);
    expect(CANVAS_TOPIC_VIEWPORT in s.appFeeds[SC].topics).toBe(true);
  });

  it("the global-scan 'feed' topic still populates the feed-shaped fields", () => {
    // Topic storage is additive: the legacy feed view is untouched.
    let s = connected();
    s = tel(s, env("app.data", {
      name: "global-scan",
      topic: "feed",
      payload: {
        brief: "B",
        items: [
          {
            title: "T",
            source: "NPR",
            url: "https://x",
            published: "",
            category: "world",
            summary: "S",
          },
        ],
      },
    }));
    const feed = s.appFeeds["global-scan"];
    expect(feed.brief).toBe("B");
    expect(feed.items.map((i) => i.title)).toEqual(["T"]);
    // And the same payload is mirrored under the "feed" topic key.
    expect(feed.topics["feed"]).toBeDefined();
  });
});
