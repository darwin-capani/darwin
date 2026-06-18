import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import LifeLogPanel from "../components/LifeLogPanel";
import {
  LIFELOG_SUMMARY_CAP,
  LIFELOG_THEME_CAP,
  lifeLogPeriodLabel,
  parseLifeLogDigest,
  type LifeLogDigest,
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

function render(digest: LifeLogDigest | null): string {
  return renderToStaticMarkup(createElement(LifeLogPanel, { digest }));
}

/* fixtures — mirror daemon/src/lifelog.rs LifeLogCard wire shape ------------- */

/** A populated WEEK digest over the user's REAL recorded (already-redacted)
 *  episodes: the real count, the rendered line, and the bounded themes / topics /
 *  recent summaries (every field already redacted by the episodic store). */
const weekDigest: Record<string, unknown> = {
  period: "this week",
  empty: false,
  episode_count: 3,
  digest_text: "This week you had 3 recorded turns — mostly on the rocket engine.",
  themes: ["rocket", "engine"],
  topics: ["rocket engine design", "launch window"],
  recent_summaries: [
    "worked on the rocket engine design",
    "reviewed the launch window with [redacted]",
  ],
};

/** An EMPTY window — nothing logged. The HUD honest-empty state. */
const emptyDigest: Record<string, unknown> = {
  period: "today",
  empty: true,
  episode_count: 0,
  digest_text: "Nothing logged for today, sir.",
  themes: [],
  topics: [],
  recent_summaries: [],
};

/* ------------------------------------------------------------------------- */

describe("parseLifeLogDigest", () => {
  it("records the period, real count, rendered text, and bounded redacted lists", () => {
    const d = parseLifeLogDigest(weekDigest)!;
    expect(d.period).toBe("this week");
    expect(d.empty).toBe(false);
    expect(d.episodeCount).toBe(3);
    expect(d.digestText).toContain("3 recorded turns");
    expect(d.themes).toEqual(["rocket", "engine"]);
    expect(d.topics).toEqual(["rocket engine design", "launch window"]);
    expect(d.recentSummaries).toHaveLength(2);
    expect(d.recentSummaries[1]).toContain("[redacted]");
  });

  it("an empty window parses to the honest-empty digest (zero count, no lists)", () => {
    const d = parseLifeLogDigest(emptyDigest)!;
    expect(d.empty).toBe(true);
    expect(d.episodeCount).toBe(0);
    expect(d.themes).toEqual([]);
    expect(d.topics).toEqual([]);
    expect(d.recentSummaries).toEqual([]);
  });

  it("drops an unrecognized period rather than rendering a fabricated one", () => {
    expect(parseLifeLogDigest({ ...weekDigest, period: "last decade" })).toBeNull();
    expect(parseLifeLogDigest({ ...weekDigest, period: 42 })).toBeNull();
  });

  it("drops non-string + empty list entries and caps the lists", () => {
    const big = Array.from({ length: 40 }, (_, i) => `theme ${i}`);
    const d = parseLifeLogDigest({
      period: "today",
      empty: false,
      episode_count: 1,
      digest_text: "x",
      themes: ["kept", 7, "", "  ", null, "also-kept"],
      topics: big,
      recent_summaries: Array.from({ length: 40 }, (_, i) => `s${i}`),
    })!;
    expect(d.themes).toEqual(["kept", "also-kept"]); // non-strings + blanks dropped
    expect(d.topics).toHaveLength(LIFELOG_THEME_CAP); // capped
    expect(d.recentSummaries).toHaveLength(LIFELOG_SUMMARY_CAP); // capped
  });

  it("never throws on junk and yields a dropped (null) digest when period is junk", () => {
    expect(parseLifeLogDigest({ themes: "not-an-array", episode_count: "x" })).toBeNull();
  });
});

describe("lifeLogPeriodLabel", () => {
  it("maps each period to its honest header label", () => {
    expect(lifeLogPeriodLabel("today")).toBe("TODAY");
    expect(lifeLogPeriodLabel("this week")).toBe("THIS WEEK");
  });
});

describe("lifelog.digest reducer", () => {
  it("folds a populated digest onto state", () => {
    const s = tel(connected(), env("lifelog.digest", weekDigest));
    expect(s.lifelog).not.toBeNull();
    expect(s.lifelog?.episodeCount).toBe(3);
    expect(s.lifelog?.themes).toContain("rocket");
  });

  it("a fresh digest REPLACES the prior one", () => {
    let s = tel(connected(), env("lifelog.digest", weekDigest));
    expect(s.lifelog?.period).toBe("this week");
    s = tel(s, env("lifelog.digest", emptyDigest));
    expect(s.lifelog?.period).toBe("today");
    expect(s.lifelog?.empty).toBe(true);
  });

  it("a malformed digest is DROPPED (same reference, no churn)", () => {
    const base = tel(connected(), env("lifelog.digest", weekDigest));
    const after = tel(base, env("lifelog.digest", { period: "never" }));
    expect(after).toBe(base); // unparseable period -> reducer returns same state
    expect(after.lifelog?.period).toBe("this week");
  });
});

describe("LifeLogPanel", () => {
  it("renders nothing before any life-log command", () => {
    expect(render(null)).toBe("");
  });

  it("surfaces the period, real count, digest text, themes, topics, and summaries", () => {
    const html = render(parseLifeLogDigest(weekDigest));
    expect(html).toContain("LIFE-LOG // DIGEST");
    expect(html).toContain("THIS WEEK");
    expect(html).toContain("3 TURNS");
    expect(html).toContain("3 recorded turns");
    expect(html).toContain("rocket");
    expect(html).toContain("launch window");
    expect(html).toContain("worked on the rocket engine design");
    // Honest framing — summarizes YOUR real episodes, never invents.
    expect(html.toLowerCase()).toContain("never invents");
  });

  it("shows the honest-empty state for a window with nothing logged", () => {
    const html = render(parseLifeLogDigest(emptyDigest));
    expect(html).toContain("TODAY");
    expect(html).toContain("0 TURNS");
    expect(html.toLowerCase()).toContain("nothing logged");
    expect(html.toLowerCase()).toContain("will not invent");
    // No fabricated themes/topics/summaries on an empty window.
    expect(html).not.toContain("RECENT");
    expect(html).not.toContain("THEMES");
  });

  it("is SECRET-FREE: only the redacted digest fields render, never a leaked secret", () => {
    // A daemon that (incorrectly) rode a secret/raw episode alongside the
    // honest redacted fields: the parser reads ONLY the known fields.
    const d = parseLifeLogDigest({
      period: "today",
      empty: false,
      episode_count: 1,
      digest_text: "redacted digest line",
      themes: ["rocket"],
      topics: ["engine"],
      recent_summaries: ["worked on the rocket engine"],
      raw_episode: "RAW_UNREDACTED_EPISODE",
      embedding: [0.999999],
      secret: "OWNER_SECRET",
    })!;
    expect(Object.keys(d).sort()).toEqual([
      "digestText",
      "empty",
      "episodeCount",
      "period",
      "recentSummaries",
      "themes",
      "topics",
    ]);
    const html = render(d);
    expect(html).toContain("redacted digest line");
    expect(html).not.toContain("RAW_UNREDACTED_EPISODE");
    expect(html).not.toContain("0.999999");
    expect(html).not.toContain("OWNER_SECRET");
  });
});
