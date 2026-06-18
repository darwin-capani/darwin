import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import ReportPanel from "../components/ReportPanel";
import {
  parseReportReadout,
  REPORT_HEADINGS_CAP,
  REPORT_CITATIONS_CAP,
  type ReportReadout,
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

function render(report: ReportReadout | null): string {
  return renderToStaticMarkup(createElement(ReportPanel, { report }));
}

/* fixtures — mirror daemon/src/router.rs report.built wire shape ------------ */

/** A built report on a topic with two sections + two REAL citations, exactly as
 *  router.rs emits it from a built Report. */
const builtPayload: Record<string, unknown> = {
  verb: "report",
  report: {
    title: "the JWST",
    empty: false,
    section_count: 2,
    headings: ["Overview", "Instruments"],
    citation_count: 2,
    citations: [
      { id: 1, title: "NASA — JWST Overview", url: "https://nasa.gov/jwst" },
      { id: 2, title: "ESA — Webb Facts", url: "https://esa.int/webb" },
    ],
  },
};

/** The honest-empty report — no citable source on the topic. */
const emptyPayload: Record<string, unknown> = {
  verb: "report_empty",
  report: {
    title: "obscure topic",
    empty: true,
    section_count: 0,
    headings: [],
    citation_count: 0,
    citations: [],
  },
};

/* parse: cites only real sources, honest-empty ------------------------------ */

describe("parseReportReadout — cites only real sources, honest-empty", () => {
  it("parses the title, counts, headings, and REAL citations", () => {
    const r = parseReportReadout(builtPayload);
    expect(r).not.toBeNull();
    expect(r!.title).toBe("the JWST");
    expect(r!.empty).toBe(false);
    expect(r!.sectionCount).toBe(2);
    expect(r!.headings).toEqual(["Overview", "Instruments"]);
    expect(r!.citationCount).toBe(2);
    expect(r!.citations).toEqual([
      { id: 1, title: "NASA — JWST Overview", url: "https://nasa.gov/jwst" },
      { id: 2, title: "ESA — Webb Facts", url: "https://esa.int/webb" },
    ]);
  });

  it("returns null when there is no report object (off/error verbs)", () => {
    expect(parseReportReadout({ verb: "report_off", report: null })).toBeNull();
    expect(parseReportReadout({ verb: "error" })).toBeNull();
    expect(parseReportReadout({ verb: "report", report: "nope" })).toBeNull();
  });

  it("drops a citation with no usable locator — never fabricates one", () => {
    const r = parseReportReadout({
      verb: "report",
      report: {
        title: "t",
        empty: false,
        section_count: 1,
        headings: ["S"],
        citation_count: 1,
        citations: [
          { id: 1, title: "Real", url: "https://real.test" },
          { id: 9, title: "   ", url: "   " }, // no locator -> dropped
          { id: 7 }, // no title/url -> dropped
          "garbage", // not an object -> dropped
        ],
      },
    });
    expect(r).not.toBeNull();
    expect(r!.citations).toHaveLength(1);
    expect(r!.citations[0].url).toBe("https://real.test");
    // The fabricated/blank entries never surface.
    expect(r!.citations.some((c) => c.id === 9)).toBe(false);
  });

  it("keeps a citation with a url but no title (a real locator is enough)", () => {
    const r = parseReportReadout({
      verb: "report",
      report: {
        title: "t",
        empty: false,
        section_count: 1,
        headings: ["S"],
        citation_count: 1,
        citations: [{ id: 3, url: "https://only-url.test" }],
      },
    });
    expect(r!.citations).toEqual([{ id: 3, title: "", url: "https://only-url.test" }]);
  });

  it("re-derives empty (no citation AND no section) regardless of the wire flag", () => {
    // Wire says NOT empty but nothing citable -> honest-empty wins.
    const a = parseReportReadout({
      verb: "report",
      report: {
        title: "t",
        empty: false,
        section_count: 0,
        headings: [],
        citation_count: 0,
        citations: [],
      },
    });
    expect(a!.empty).toBe(true);
    // Real citations present -> not empty even if the wire said so.
    const b = parseReportReadout({
      verb: "report",
      report: {
        title: "t",
        empty: true,
        section_count: 1,
        headings: ["S"],
        citation_count: 1,
        citations: [{ id: 1, title: "R", url: "https://r.test" }],
      },
    });
    expect(b!.empty).toBe(false);
  });

  it("parses the honest-empty report", () => {
    const r = parseReportReadout(emptyPayload);
    expect(r).not.toBeNull();
    expect(r!.empty).toBe(true);
    expect(r!.citations).toHaveLength(0);
    expect(r!.sectionCount).toBe(0);
  });

  it("bounds headings + citations to the VIEW caps", () => {
    const headings = Array.from({ length: REPORT_HEADINGS_CAP + 5 }, (_, i) => `H${i}`);
    const citations = Array.from({ length: REPORT_CITATIONS_CAP + 5 }, (_, i) => ({
      id: i,
      title: `T${i}`,
      url: `https://x${i}.test`,
    }));
    const r = parseReportReadout({
      verb: "report",
      report: {
        title: "t",
        empty: false,
        section_count: headings.length,
        headings,
        citation_count: citations.length,
        citations,
      },
    });
    expect(r!.headings.length).toBeLessThanOrEqual(REPORT_HEADINGS_CAP);
    expect(r!.citations.length).toBeLessThanOrEqual(REPORT_CITATIONS_CAP);
    // The COUNTS report the daemon's real totals even when the preview is bounded.
    expect(r!.citationCount).toBe(citations.length);
    expect(r!.sectionCount).toBe(headings.length);
  });

  it("drops blank headings", () => {
    const r = parseReportReadout({
      verb: "report",
      report: {
        title: "t",
        empty: false,
        section_count: 2,
        headings: ["Real", "   ", ""],
        citation_count: 1,
        citations: [{ id: 1, title: "R", url: "https://r.test" }],
      },
    });
    expect(r!.headings).toEqual(["Real"]);
  });
});

/* reducer ------------------------------------------------------------------ */

describe("reduce(report.built)", () => {
  it("ships OFF by default — no report until a report.built arrives", () => {
    expect(initialState().report).toBeNull();
    expect(connected().report).toBeNull();
  });

  it("folds a parsed readout onto state.report", () => {
    const s = tel(connected(), env("report.built", builtPayload));
    expect(s.report).not.toBeNull();
    expect(s.report!.title).toBe("the JWST");
    expect(s.report!.citations).toHaveLength(2);
  });

  it("keeps the prior reference when the off/error verb carries no report", () => {
    const s0 = tel(connected(), env("report.built", builtPayload));
    const s1 = tel(s0, env("report.built", { verb: "report_off", report: null }));
    // Same reference — an off round never blanks a real report already shown.
    expect(s1).toBe(s0);
    expect(s1.report).toBe(s0.report);
  });

  it("a fresh report replaces the prior one", () => {
    let s = tel(connected(), env("report.built", builtPayload));
    s = tel(s, env("report.built", emptyPayload));
    expect(s.report!.title).toBe("obscure topic");
    expect(s.report!.empty).toBe(true);
  });
});

/* render ------------------------------------------------------------------- */

describe("ReportPanel render", () => {
  it("renders nothing until a report arrives", () => {
    expect(render(null)).toBe("");
  });

  it("surfaces the title, section + citation counts, and the REAL citations", () => {
    const r = parseReportReadout(builtPayload)!;
    const html = render(r);
    expect(html).toContain("the JWST");
    expect(html).toContain("2 SECTIONS");
    expect(html).toContain("2 CITED");
    expect(html).toContain("Overview");
    expect(html).toContain("Instruments");
    expect(html).toContain("https://nasa.gov/jwst");
    expect(html).toContain("https://esa.int/webb");
  });

  it("renders the honest-empty state — never a fabricated body/citation", () => {
    const r = parseReportReadout(emptyPayload)!;
    const html = render(r);
    expect(html.toLowerCase()).toContain("no sources to report on");
    expect(html).toContain("NO SOURCES");
    // No citation rows.
    expect(html).not.toContain("report-citation-url");
  });

  it("shows the honest total + 'more' note when the preview is bounded", () => {
    const r: ReportReadout = {
      title: "big",
      empty: false,
      sectionCount: 20,
      headings: ["A", "B"],
      citationCount: 30,
      citations: [{ id: 1, title: "R", url: "https://r.test" }],
    };
    const html = render(r);
    expect(html).toContain("20 SECTIONS");
    expect(html).toContain("30 CITED");
    // honest "+ N more" affordance for both the bounded sections and sources
    expect(html.toLowerCase()).toContain("more");
  });

  it("never shows a citation the readout does not carry", () => {
    const r = parseReportReadout(builtPayload)!;
    const html = render(r);
    expect(html).not.toContain("https://fabricated");
    expect(html).not.toContain("[99]");
  });
});
