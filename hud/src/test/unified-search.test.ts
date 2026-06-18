import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import UnifiedSearchPanel from "../components/UnifiedSearchPanel";
import {
  parseUnifiedSearchResult,
  unifiedCoverageSummary,
  unifiedSkipReasonLabel,
  unifiedSourceLabel,
  unifiedSourceOnDevice,
  type TelemetryEnvelope,
  type UnifiedSearchResult,
} from "../core/events";
import { HudState, initialState, reduce } from "../core/state";

/* helpers ------------------------------------------------------------------ */

let counter = 0;
function env(
  event: string,
  data: Record<string, unknown> = {},
  source = "local",
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

/** A realistic `unified.searched` payload mirroring the daemon's
 *  anthropic.rs::unified_search_tool emission: on-device sources searched, a
 *  connected cloud source (gmail) searched, two cloud sources skipped (slack not
 *  connected, calendar not requested). Hits CITE real (test) items, grouped by
 *  source in the daemon's deterministic ranked order. */
const searchedMixed: Record<string, unknown> = {
  query: "renewal clause",
  coverage: {
    searched: ["docsearch", "episodic", "facts", "world", "gmail"],
    skipped: [
      { source: "slack", reason: "not_connected" },
      { source: "calendar", reason: "not_requested" },
    ],
  },
  hits: [
    {
      source: "docsearch",
      source_label: "Files",
      citation: "/Users/me/notes/lease.md (offset 1840)",
      title: "lease.md",
      snippet: "The renewal clause auto-extends the term by twelve months.",
      score: 0.91,
      ts: null,
    },
    {
      source: "episodic",
      source_label: "Past conversations",
      citation: "episode @ 2026-06-10T09:00:00Z",
      title: "",
      snippet: "We discussed the lease renewal last Tuesday.",
      score: 0.71,
      ts: "2026-06-10T09:00:00Z",
    },
    {
      source: "gmail",
      source_label: "Gmail",
      citation: "gmail:18f2ab",
      title: "Re: lease",
      snippet: "Landlord confirmed the renewal terms.",
      score: 0.55,
      ts: "2026-06-12T15:00:00Z",
    },
  ],
};

/** An all-empty fan-out: on-device sources searched, found nothing; the honest
 *  empty result still carries the coverage. */
const searchedEmpty: Record<string, unknown> = {
  query: "no match anywhere",
  coverage: {
    searched: ["docsearch", "facts"],
    skipped: [{ source: "gmail", reason: "not_connected" }],
  },
  hits: [],
};

/* ------------------------------------------------------------------------ *
 * Source / skip-reason label + on-device helpers.                            *
 * ------------------------------------------------------------------------ */
describe("unified source + skip-reason helpers", () => {
  it("labels each known source with its honest human name", () => {
    expect(unifiedSourceLabel("docsearch")).toBe("Files");
    expect(unifiedSourceLabel("episodic")).toBe("Past conversations");
    expect(unifiedSourceLabel("facts")).toBe("Memory");
    expect(unifiedSourceLabel("world")).toBe("World model");
    expect(unifiedSourceLabel("gmail")).toBe("Gmail");
    expect(unifiedSourceLabel("calendar")).toBe("Calendar");
    expect(unifiedSourceLabel("slack")).toBe("Slack");
  });

  it("renders an unknown future source verbatim rather than hiding it", () => {
    expect(unifiedSourceLabel("notion")).toBe("notion");
  });

  it("marks exactly the four on-device sources as on-device", () => {
    for (const s of ["docsearch", "episodic", "facts", "world"]) {
      expect(unifiedSourceOnDevice(s)).toBe(true);
    }
    for (const s of ["gmail", "calendar", "slack", "notion"]) {
      expect(unifiedSourceOnDevice(s)).toBe(false);
    }
  });

  it("labels each skip reason with its honest clause", () => {
    expect(unifiedSkipReasonLabel("not_connected")).toBe("not connected");
    expect(unifiedSkipReasonLabel("no_index")).toBe("no index built");
    expect(unifiedSkipReasonLabel("not_requested")).toBe("not requested");
    expect(unifiedSkipReasonLabel("future")).toBe("future");
  });
});

/* ------------------------------------------------------------------------ *
 * unifiedCoverageSummary — honest, never conflates searched with skipped.    *
 * ------------------------------------------------------------------------ */
describe("unifiedCoverageSummary (honest coverage line)", () => {
  it("mirrors the daemon's Coverage::summary() for a mixed fan-out", () => {
    const r = parseUnifiedSearchResult(searchedMixed);
    expect(unifiedCoverageSummary(r.coverage)).toBe(
      "Searched Files, Past conversations, Memory, World model, Gmail. " +
        "Skipped Slack (not connected), Calendar (not requested).",
    );
  });

  it("omits the skipped clause when nothing was skipped", () => {
    expect(
      unifiedCoverageSummary({ searched: ["docsearch", "facts"], skipped: [] }),
    ).toBe("Searched Files, Memory.");
  });

  it("says 'Searched no sources.' for an empty searched set (never implies reach)", () => {
    expect(unifiedCoverageSummary({ searched: [], skipped: [] })).toBe(
      "Searched no sources.",
    );
    // Even with skips, the searched clause stays honest about reaching nothing.
    expect(
      unifiedCoverageSummary({
        searched: [],
        skipped: [{ source: "gmail", reason: "not_connected" }],
      }),
    ).toBe("Searched no sources. Skipped Gmail (not connected).");
  });
});

/* ------------------------------------------------------------------------ *
 * parseUnifiedSearchResult — only REAL cited hits, honest coverage, no fab.  *
 * ------------------------------------------------------------------------ */
describe("parseUnifiedSearchResult (defensive, cite-only)", () => {
  it("parses a well-formed mixed result with attributed, cited hits", () => {
    const r = parseUnifiedSearchResult(searchedMixed);
    expect(r.query).toBe("renewal clause");
    expect(r.coverage.searched).toEqual([
      "docsearch",
      "episodic",
      "facts",
      "world",
      "gmail",
    ]);
    expect(r.coverage.skipped).toEqual([
      { source: "slack", reason: "not_connected" },
      { source: "calendar", reason: "not_requested" },
    ]);
    expect(r.hits.length).toBe(3);
    expect(r.hits[0]).toEqual({
      source: "docsearch",
      sourceLabel: "Files",
      citation: "/Users/me/notes/lease.md (offset 1840)",
      title: "lease.md",
      snippet: "The renewal clause auto-extends the term by twelve months.",
      score: 0.91,
      ts: null,
    });
    // The episodic hit carries its real timestamp; gmail too.
    expect(r.hits[1].ts).toBe("2026-06-10T09:00:00Z");
    expect(r.hits[2].source).toBe("gmail");
  });

  it("drops a hit with no source (not attributable) — never fabricates", () => {
    const r = parseUnifiedSearchResult({
      query: "q",
      hits: [
        { citation: "gmail:1", snippet: "no source", score: 0.5 }, // no source -> dropped
        {
          source: "facts",
          citation: "user.timezone",
          snippet: "real",
          score: 0.4,
        },
      ],
    });
    expect(r.hits.length).toBe(1);
    expect(r.hits[0].source).toBe("facts");
  });

  it("drops a hit with no citation anchor (not a real citation) — never fabricates", () => {
    const r = parseUnifiedSearchResult({
      query: "q",
      hits: [
        { source: "docsearch", snippet: "no anchor", score: 0.5 }, // no citation -> dropped
        42, // non-object -> dropped
        {
          source: "docsearch",
          citation: "/a/b.md (offset 3)",
          snippet: "real",
          score: 0.4,
        },
      ],
    });
    expect(r.hits.length).toBe(1);
    expect(r.hits[0].citation).toBe("/a/b.md (offset 3)");
    // A missing source_label falls back to the derived honest label.
    expect(r.hits[0].sourceLabel).toBe("Files");
  });

  it("drops a skip entry with no source (meaningless) — never a blank skip", () => {
    const r = parseUnifiedSearchResult({
      query: "q",
      coverage: { searched: ["facts"], skipped: [{ reason: "not_connected" }] },
      hits: [],
    });
    expect(r.coverage.skipped).toEqual([]);
  });

  it("defaults a skip reason to not_requested (conservative honest) when absent", () => {
    const r = parseUnifiedSearchResult({
      coverage: { searched: [], skipped: [{ source: "slack" }] },
    });
    expect(r.coverage.skipped[0]).toEqual({
      source: "slack",
      reason: "not_requested",
    });
  });

  it("keeps the daemon's deterministic searched order verbatim", () => {
    const r = parseUnifiedSearchResult({
      coverage: { searched: ["world", "facts", "docsearch"] },
    });
    expect(r.coverage.searched).toEqual(["world", "facts", "docsearch"]);
  });

  it("yields an honest empty result (searched X, found nothing), never null", () => {
    const r = parseUnifiedSearchResult(searchedEmpty);
    expect(r.hits).toEqual([]);
    expect(r.coverage.searched).toEqual(["docsearch", "facts"]);
    expect(r.query).toBe("no match anywhere");
  });

  it("ts is null (not invented) when the source carries no timestamp", () => {
    const r = parseUnifiedSearchResult({
      hits: [{ source: "facts", citation: "k", snippet: "s", score: 0.1 }],
    });
    expect(r.hits[0].ts).toBeNull();
  });

  it("never throws on junk", () => {
    expect(() => parseUnifiedSearchResult({ hits: "nope", coverage: 7 })).not.toThrow();
    const r = parseUnifiedSearchResult({ hits: "nope", coverage: 7 });
    expect(r.hits).toEqual([]);
    expect(r.coverage).toEqual({ searched: [], skipped: [] });
  });
});

/* ------------------------------------------------------------------------ *
 * The reducer arm. unified.searched sets the cited result; NEVER null after  *
 * a frame; latest-wins.                                                       *
 * ------------------------------------------------------------------------ */
describe("unified.searched reducer", () => {
  it("starts with no unified search", () => {
    expect(connected().unifiedSearch).toBeNull();
  });

  it("sets the cited result from unified.searched", () => {
    const s = tel(connected(), env("unified.searched", searchedMixed));
    expect(s.unifiedSearch).not.toBeNull();
    expect(s.unifiedSearch!.hits.length).toBe(3);
    expect(s.unifiedSearch!.coverage.searched.length).toBe(5);
    expect(s.unifiedSearch!.coverage.skipped.length).toBe(2);
  });

  it("a later search replaces the prior one (latest-wins)", () => {
    let s = tel(connected(), env("unified.searched", searchedMixed));
    s = tel(s, env("unified.searched", searchedEmpty));
    expect(s.unifiedSearch!.hits).toEqual([]);
    expect(s.unifiedSearch!.query).toBe("no match anywhere");
  });

  it("an empty fan-out sets an honest empty result (coverage preserved)", () => {
    const s = tel(connected(), env("unified.searched", searchedEmpty));
    expect(s.unifiedSearch!.hits).toEqual([]);
    expect(s.unifiedSearch!.coverage.searched).toEqual(["docsearch", "facts"]);
  });
});

/* ------------------------------------------------------------------------ *
 * The panel (rendered headlessly). PRIVATE + REVIEW-ONLY: groups BY SOURCE,   *
 * cites real items, honest coverage (searched vs skipped, never conflated),   *
 * no action button, honest empty state.                                       *
 * ------------------------------------------------------------------------ */
describe("UnifiedSearchPanel (grouped, cited, honest, review-only)", () => {
  const render = (result: UnifiedSearchResult | null) =>
    renderToStaticMarkup(createElement(UnifiedSearchPanel, { result }));

  it("renders nothing before any unified search", () => {
    expect(render(null)).toBe("");
  });

  it("groups hits BY SOURCE with the honest source labels", () => {
    const html = render(parseUnifiedSearchResult(searchedMixed));
    // Each searched source that produced a hit gets a group header.
    expect(html).toContain("Files");
    expect(html).toContain("Past conversations");
    expect(html).toContain("Gmail");
    expect(html).toContain("REVIEW ONLY");
  });

  it("renders each hit's REAL citation anchor + snippet + score", () => {
    const html = render(parseUnifiedSearchResult(searchedMixed));
    expect(html).toContain("/Users/me/notes/lease.md (offset 1840)");
    expect(html).toContain("episode @ 2026-06-10T09:00:00Z");
    expect(html).toContain("gmail:18f2ab");
    expect(html).toContain("The renewal clause auto-extends the term");
    expect(html).toContain("0.910"); // score, fixed(3)
    expect(html).toContain("renewal clause"); // the query
  });

  it("shows the timestamp only for time-stamped hits (never invents one)", () => {
    const html = render(parseUnifiedSearchResult(searchedMixed));
    // Episodic + gmail carry a ts; the docsearch hit (ts null) shows none.
    expect(html).toContain("2026-06-10T09:00:00Z");
    expect(html).toContain("2026-06-12T15:00:00Z");
  });

  it("renders the coverage line: searched sources AND skipped-with-reason, not conflated", () => {
    const html = render(parseUnifiedSearchResult(searchedMixed));
    expect(html).toContain("SEARCHED");
    expect(html).toContain("SKIPPED");
    // A skipped cloud source is shown with its honest reason, never as searched.
    expect(html).toContain("Slack");
    expect(html).toContain("not connected");
    expect(html).toContain("Calendar");
    expect(html).toContain("not requested");
    // The full honest sentence is the coverage aria/title.
    expect(html).toContain(
      "Searched Files, Past conversations, Memory, World model, Gmail. " +
        "Skipped Slack (not connected), Calendar (not requested).",
    );
  });

  it("badges on-device sources distinctly from cloud (privacy posture at a glance)", () => {
    const html = render(parseUnifiedSearchResult(searchedMixed));
    expect(html).toContain("unified-src on-device"); // Files/Memory/...
    expect(html).toContain("unified-src cloud"); // Gmail
  });

  it("shows the honest NOTHING-FOUND state but still prints coverage (never a fake hit)", () => {
    const html = render(parseUnifiedSearchResult(searchedEmpty));
    expect(html).toMatch(/Nothing matched/i);
    expect(html).toContain("honest result");
    // Coverage is still reported so the user knows the reach.
    expect(html).toContain("SEARCHED");
    expect(html).toContain("Files");
    expect(html).toContain("Memory");
  });

  it("says 'no sources' honestly when nothing was searched", () => {
    const html = render(
      parseUnifiedSearchResult({
        coverage: { searched: [], skipped: [{ source: "gmail", reason: "not_connected" }] },
        hits: [],
      }),
    );
    expect(html).toContain("no sources");
    expect(html).toContain("Gmail");
    expect(html).toContain("not connected");
  });

  it("has NO action button — searching is a spoken intent", () => {
    const html = render(parseUnifiedSearchResult(searchedMixed));
    expect(html).not.toContain("<button");
    expect(html).toContain("search everything for");
  });

  it("states the on-device / private / cloud-only-when-connected / never-fabricates honesty in the footer", () => {
    const html = render(parseUnifiedSearchResult(searchedMixed));
    expect(html).toMatch(/on-device/i);
    expect(html).toMatch(/never leave/i);
    expect(html).toMatch(/only when connected/i);
    expect(html).toMatch(/nothing is fabricated|never silently dropped/i);
  });

  it("renders an unknown future source verbatim (forward-compatible, never hidden)", () => {
    const html = render(
      parseUnifiedSearchResult({
        query: "q",
        coverage: { searched: ["notion"], skipped: [] },
        hits: [
          {
            source: "notion",
            source_label: "Notion",
            citation: "notion:abc",
            snippet: "a future source",
            score: 0.3,
          },
        ],
      }),
    );
    expect(html).toContain("Notion");
    expect(html).toContain("notion:abc");
  });
});
