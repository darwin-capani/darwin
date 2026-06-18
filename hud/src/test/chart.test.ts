import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import ChartPanel from "../components/ChartPanel";
import {
  parseChartSpec,
  CHART_SERIES_CAP,
  CHART_POINTS_CAP,
  type ChartSpec,
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

function render(spec: ChartSpec | null): string {
  return renderToStaticMarkup(createElement(ChartPanel, { spec }));
}

/* fixtures — mirror daemon/src/chart.rs ChartSpec::to_telemetry wire shape --- */

/** A line chart with three REAL points, exactly as chart.rs emits them. */
const linePayload: Record<string, unknown> = {
  kind: "line",
  title: "system load",
  x_axis: "sample",
  y_axis: "percent",
  empty: false,
  series: [
    {
      label: "cpu",
      points: [
        [0, 12],
        [1, 30.5],
        [2, 18],
      ],
    },
  ],
};

/* parse: exact points, no fabrication -------------------------------------- */

describe("parseChartSpec — the EXACT series, no fabricated point", () => {
  it("parses every emitted point verbatim", () => {
    const spec = parseChartSpec(linePayload);
    expect(spec).not.toBeNull();
    expect(spec!.kind).toBe("line");
    expect(spec!.title).toBe("system load");
    expect(spec!.xAxis).toBe("sample");
    expect(spec!.yAxis).toBe("percent");
    expect(spec!.empty).toBe(false);
    expect(spec!.series).toHaveLength(1);
    expect(spec!.series[0].label).toBe("cpu");
    // EXACTLY the three emitted points, in order, verbatim.
    expect(spec!.series[0].points).toEqual([
      { x: 0, y: 12 },
      { x: 1, y: 30.5 },
      { x: 2, y: 18 },
    ]);
  });

  it("drops an unrecognized kind (never guesses a draw mode)", () => {
    expect(parseChartSpec({ ...linePayload, kind: "pie" })).toBeNull();
    expect(parseChartSpec({ ...linePayload, kind: 7 })).toBeNull();
    expect(parseChartSpec({ ...linePayload })).not.toBeNull();
  });

  it("accepts bar and sparkline kinds", () => {
    expect(parseChartSpec({ ...linePayload, kind: "bar" })!.kind).toBe("bar");
    expect(parseChartSpec({ ...linePayload, kind: "sparkline" })!.kind).toBe(
      "sparkline",
    );
  });

  it("drops a malformed point (half-pair / non-finite) — never zero-fills it", () => {
    const spec = parseChartSpec({
      kind: "line",
      title: "t",
      x_axis: "x",
      y_axis: "y",
      empty: false,
      series: [
        {
          label: "s",
          points: [
            [0, 10], // good
            [1], // half-pair -> dropped
            [2, "nope"], // non-numeric y -> dropped
            [Number.NaN, 5], // non-finite x -> dropped
            "garbage", // not an array -> dropped
            [3, 20], // good
          ],
        },
      ],
    });
    expect(spec).not.toBeNull();
    // Only the two real, complete points survive — never a fabricated/zero point.
    expect(spec!.series[0].points).toEqual([
      { x: 0, y: 10 },
      { x: 3, y: 20 },
    ]);
  });

  it("drops a series with no usable point (never an empty stub)", () => {
    const spec = parseChartSpec({
      kind: "bar",
      title: "t",
      x_axis: "x",
      y_axis: "y",
      empty: false,
      series: [
        { label: "real", points: [[0, 1]] },
        { label: "empty", points: [] },
        { label: "junk", points: ["x", [9]] },
        { label: "noPoints" },
      ],
    });
    expect(spec).not.toBeNull();
    expect(spec!.series).toHaveLength(1);
    expect(spec!.series[0].label).toBe("real");
  });

  it("re-derives empty from the surviving points (never trusts the wire flag)", () => {
    // Wire claims NOT empty but every series is empty -> honest-empty wins.
    const a = parseChartSpec({
      kind: "line",
      title: "t",
      x_axis: "x",
      y_axis: "y",
      empty: false,
      series: [{ label: "s", points: [] }],
    });
    expect(a!.empty).toBe(true);
    // Wire claims empty but a real point exists -> still plotted, not empty.
    const b = parseChartSpec({
      kind: "line",
      title: "t",
      x_axis: "x",
      y_axis: "y",
      empty: true,
      series: [{ label: "s", points: [[0, 1]] }],
    });
    expect(b!.empty).toBe(false);
    expect(b!.series[0].points).toEqual([{ x: 0, y: 1 }]);
  });

  it("handles an empty series array as honest-empty", () => {
    const spec = parseChartSpec({
      kind: "line",
      title: "t",
      x_axis: "x",
      y_axis: "y",
      empty: true,
      series: [],
    });
    expect(spec).not.toBeNull();
    expect(spec!.empty).toBe(true);
    expect(spec!.series).toHaveLength(0);
  });

  it("bounds series + points to the VIEW caps", () => {
    const manySeries = Array.from({ length: CHART_SERIES_CAP + 5 }, (_, i) => ({
      label: `s${i}`,
      points: [[0, i]],
    }));
    const spec = parseChartSpec({
      kind: "bar",
      title: "t",
      x_axis: "x",
      y_axis: "y",
      empty: false,
      series: manySeries,
    });
    expect(spec!.series.length).toBeLessThanOrEqual(CHART_SERIES_CAP);

    const manyPoints = Array.from({ length: CHART_POINTS_CAP + 50 }, (_, i) => [i, i]);
    const spec2 = parseChartSpec({
      kind: "line",
      title: "t",
      x_axis: "x",
      y_axis: "y",
      empty: false,
      series: [{ label: "s", points: manyPoints }],
    });
    expect(spec2!.series[0].points.length).toBeLessThanOrEqual(CHART_POINTS_CAP);
  });
});

/* reducer ------------------------------------------------------------------ */

describe("reduce(chart.data)", () => {
  it("ships OFF by default — no chart until a chart.data arrives", () => {
    expect(initialState().chart).toBeNull();
    expect(connected().chart).toBeNull();
  });

  it("folds a parsed spec onto state.chart", () => {
    const s = tel(connected(), env("chart.data", linePayload));
    expect(s.chart).not.toBeNull();
    expect(s.chart!.series[0].points).toHaveLength(3);
  });

  it("a fresh spec replaces the prior one", () => {
    let s = tel(connected(), env("chart.data", linePayload));
    s = tel(s, env("chart.data", { ...linePayload, title: "newer", series: [{ label: "m", points: [[0, 9]] }] }));
    expect(s.chart!.title).toBe("newer");
    expect(s.chart!.series).toHaveLength(1);
    expect(s.chart!.series[0].points).toEqual([{ x: 0, y: 9 }]);
  });

  it("drops a malformed (unrecognized-kind) payload — keeps the prior reference", () => {
    const s0 = tel(connected(), env("chart.data", linePayload));
    const s1 = tel(s0, env("chart.data", { kind: "doughnut", series: [] }));
    // Same reference — junk never churns the tree.
    expect(s1).toBe(s0);
    expect(s1.chart).toBe(s0.chart);
  });

  it("folds an honest-empty spec (panel shows the empty state)", () => {
    const s = tel(
      connected(),
      env("chart.data", {
        kind: "line",
        title: "nothing",
        x_axis: "x",
        y_axis: "y",
        empty: true,
        series: [],
      }),
    );
    expect(s.chart).not.toBeNull();
    expect(s.chart!.empty).toBe(true);
  });
});

/* render ------------------------------------------------------------------- */

describe("ChartPanel render", () => {
  it("renders nothing until a spec arrives", () => {
    expect(render(null)).toBe("");
  });

  it("plots EXACTLY the emitted points (one polyline vertex per given point)", () => {
    const spec = parseChartSpec(linePayload)!;
    const html = render(spec);
    // The polyline has exactly three vertices (the three emitted points) — no
    // interpolated/invented vertex.
    const m = html.match(/points="([^"]+)"/);
    expect(m).not.toBeNull();
    const verts = m![1].trim().split(/\s+/);
    expect(verts).toHaveLength(3);
    // A marker dot per emitted point on the line variant.
    const dots = html.match(/class="chart-dot"/g) ?? [];
    expect(dots).toHaveLength(3);
  });

  it("labels axes with the data-derived REAL min/max", () => {
    const spec = parseChartSpec(linePayload)!;
    const html = render(spec);
    // y min/max of the data are 12 and 30.5; x min/max are 0 and 2.
    expect(html).toContain("30.5");
    expect(html).toContain("12");
    expect(html).toContain(">0<");
    expect(html).toContain(">2<");
    // the honest axis label strings
    expect(html).toContain("percent");
    expect(html).toContain("sample");
  });

  it("renders one bar per emitted point for a bar chart", () => {
    const spec = parseChartSpec({ ...linePayload, kind: "bar" })!;
    const html = render(spec);
    const bars = html.match(/class="chart-bar"/g) ?? [];
    expect(bars).toHaveLength(3);
  });

  it("renders the honest-empty state for an empty spec — never a fabricated point", () => {
    const spec = parseChartSpec({
      kind: "line",
      title: "nothing yet",
      x_axis: "x",
      y_axis: "y",
      empty: true,
      series: [],
    })!;
    const html = render(spec);
    expect(html.toLowerCase()).toContain("no data");
    // No polyline / bar / dot drawn.
    expect(html).not.toContain("chart-bar");
    expect(html).not.toContain("<polyline");
  });

  it("shows a single point as a marker (no zero-length polyline)", () => {
    const spec = parseChartSpec({
      kind: "line",
      title: "one",
      x_axis: "x",
      y_axis: "y",
      empty: false,
      series: [{ label: "s", points: [[5, 42]] }],
    })!;
    const html = render(spec);
    const dots = html.match(/class="chart-dot"/g) ?? [];
    expect(dots).toHaveLength(1);
    expect(html).not.toContain("<polyline");
    // the single value is labelled honestly
    expect(html).toContain("42");
  });
});
