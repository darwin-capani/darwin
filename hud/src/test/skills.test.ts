import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import SkillsPanel from "../components/SkillsPanel";
import {
  parseSkillsCatalog,
  type SkillsCatalog,
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
    ts: `2026-06-15T12:00:${String(counter % 60).padStart(2, "0")}Z`,
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

/** A realistic mock skills.catalog payload: a few pure skills across categories,
 *  one consequential and one source-gated, plus the per-category counts the daemon
 *  emits (including an empty heading). Mirrors Registry::catalog_snapshot. */
const mockPayload: Record<string, unknown> = {
  enabled: true,
  count: 4,
  categories: [
    { slug: "utilities", count: 2 },
    { slug: "text", count: 1 },
    { slug: "knowledge", count: 1 },
    { slug: "fun", count: 0 },
  ],
  skills: [
    {
      name: "base64_encode",
      category: "utilities",
      description: "Base64-encode a string.",
      consequential: false,
      source_gated: false,
    },
    {
      name: "send_webhook",
      category: "utilities",
      description: "POST to a configured webhook.",
      consequential: true,
      source_gated: false,
    },
    {
      name: "word_count",
      category: "text",
      description: "Count the words in some text.",
      consequential: false,
      source_gated: false,
    },
    {
      name: "define_word",
      category: "knowledge",
      description: "Look up a dictionary definition.",
      consequential: false,
      source_gated: true,
    },
  ],
};

/* ------------------------------------------------------------------------ *
 * The defensive parser. NEVER returns null (a catalog frame always yields an *
 * honest snapshot), NEVER carries a secret, drops malformed entries.         *
 * ------------------------------------------------------------------------ */
describe("parseSkillsCatalog (defensive)", () => {
  it("parses a well-formed skills.catalog payload", () => {
    const s = parseSkillsCatalog(mockPayload);
    expect(s.enabled).toBe(true);
    expect(s.count).toBe(4);
    expect(s.categories).toEqual([
      { slug: "utilities", count: 2 },
      { slug: "text", count: 1 },
      { slug: "knowledge", count: 1 },
      { slug: "fun", count: 0 },
    ]);
    expect(s.skills.length).toBe(4);
    const conseq = s.skills.find((x) => x.name === "send_webhook")!;
    expect(conseq.consequential).toBe(true);
    expect(conseq.sourceGated).toBe(false);
    const gated = s.skills.find((x) => x.name === "define_word")!;
    expect(gated.sourceGated).toBe(true);
    expect(gated.category).toBe("knowledge");
    expect(gated.description).toBe("Look up a dictionary definition.");
  });

  it("defaults to the shipped-ON snapshot when fields are absent", () => {
    const s = parseSkillsCatalog({});
    expect(s.enabled).toBe(true); // pure library ships ON
    expect(s.count).toBe(0);
    expect(s.categories).toEqual([]);
    expect(s.skills).toEqual([]);
  });

  it("honors an honest OFF master switch", () => {
    const s = parseSkillsCatalog({ enabled: false, count: 0, skills: [] });
    expect(s.enabled).toBe(false);
  });

  it("drops malformed skill + category entries, never throws", () => {
    const s = parseSkillsCatalog({
      enabled: "yes", // non-bool -> default true
      categories: [
        { slug: "utilities", count: 3 },
        { count: 5 }, // no slug -> dropped
        42, // non-object -> dropped
        { slug: "text", count: -2 }, // negative -> clamped to 0
      ],
      skills: [
        { category: "utilities", description: "no name" }, // dropped
        { name: "no_cat", description: "no category" }, // dropped
        7, // non-object -> dropped
        { name: "ok_skill", category: "text" }, // minimal, valid
      ],
    });
    expect(s.enabled).toBe(true); // non-bool -> default
    expect(s.categories).toEqual([
      { slug: "utilities", count: 3 },
      { slug: "text", count: 0 },
    ]);
    expect(s.skills.length).toBe(1);
    expect(s.skills[0].name).toBe("ok_skill");
    // Absent markers default to a pure read-only skill.
    expect(s.skills[0].consequential).toBe(false);
    expect(s.skills[0].sourceGated).toBe(false);
    expect(s.skills[0].description).toBe(""); // absent -> empty
  });

  it("defaults count to the parsed skills length when count is absent/invalid", () => {
    const s = parseSkillsCatalog({
      skills: [
        { name: "a", category: "text" },
        { name: "b", category: "text" },
      ],
    });
    expect(s.count).toBe(2);
  });

  it("NEVER surfaces an unexpected/secret field", () => {
    const s = parseSkillsCatalog({
      enabled: true,
      count: 1,
      skills: [
        {
          name: "x",
          category: "utilities",
          description: "d",
          // Hostile extra fields a malformed payload might carry — the parser
          // surfaces ONLY the known discovery fields.
          token: "sk-SECRET",
          run: "rm -rf /",
          secret: "leak",
        },
      ],
    });
    const blob = JSON.stringify(s);
    expect(blob).not.toContain("SECRET");
    expect(blob).not.toContain("leak");
    expect(blob).not.toContain("rm -rf");
    expect(blob).not.toContain("token");
    expect(blob).not.toContain("\"run\"");
  });

  it("never throws on junk", () => {
    expect(() => parseSkillsCatalog({ skills: "nope" })).not.toThrow();
    expect(parseSkillsCatalog({ skills: "nope" }).skills).toEqual([]);
    expect(() => parseSkillsCatalog({ categories: 5 })).not.toThrow();
    expect(parseSkillsCatalog({ categories: 5 }).categories).toEqual([]);
  });
});

/* ------------------------------------------------------------------------ *
 * The reducer arm. A skills.catalog event ALWAYS sets a snapshot (honest      *
 * on/off); a malformed payload yields an honest snapshot, never a stale one.  *
 * ------------------------------------------------------------------------ */
describe("skills.catalog reducer", () => {
  it("starts null and sets the catalog from a well-formed event", () => {
    expect(connected().skills).toBeNull();
    const s = tel(connected(), env("skills.catalog", mockPayload));
    expect(s.skills).not.toBeNull();
    expect(s.skills!.enabled).toBe(true);
    expect(s.skills!.count).toBe(4);
    expect(s.skills!.skills.length).toBe(4);
  });

  it("sets an honest OFF snapshot when the subsystem is disabled", () => {
    const s = tel(connected(), env("skills.catalog", { enabled: false, count: 0, skills: [] }));
    expect(s.skills).not.toBeNull();
    expect(s.skills!.enabled).toBe(false);
    expect(s.skills!.skills).toEqual([]);
  });

  it("never stores a secret in state", () => {
    const s = tel(
      connected(),
      env("skills.catalog", {
        enabled: true,
        count: 1,
        skills: [{ name: "x", category: "utilities", run: "rm -rf /", token: "sk-SECRET" }],
      }),
    );
    const blob = JSON.stringify(s.skills);
    expect(blob).not.toContain("SECRET");
    expect(blob).not.toContain("rm -rf");
  });
});

/* ------------------------------------------------------------------------ *
 * The panel (rendered headlessly, node env). REVIEW-ONLY + secret-free.       *
 * ------------------------------------------------------------------------ */
describe("SkillsPanel (review-only, secret-free)", () => {
  const render = (skills: SkillsCatalog | null) =>
    renderToStaticMarkup(createElement(SkillsPanel, { skills }));

  it("renders nothing before any snapshot", () => {
    expect(render(null)).toBe("");
  });

  it("shows the real count + ENABLED state and the review-only tag", () => {
    const html = render(parseSkillsCatalog(mockPayload));
    expect(html).toContain("4 skills");
    expect(html).toContain("ENABLED");
    expect(html).toContain("REVIEW ONLY");
  });

  it("shows the honest OFF note when disabled", () => {
    const html = render(parseSkillsCatalog({ enabled: false, count: 0, skills: [] }));
    expect(html).toMatch(/library is OFF/i);
    expect(html).toContain("DISABLED");
  });

  it("renders the catalog: names, descriptions, categories, and counts", () => {
    const html = render(parseSkillsCatalog(mockPayload));
    expect(html).toContain("base64_encode");
    expect(html).toContain("word_count");
    expect(html).toContain("Base64-encode a string.");
    // Category chips with their counts (the empty "fun" heading still appears).
    expect(html).toContain("Utilities");
    expect(html).toContain("Knowledge");
    expect(html).toContain("Fun");
  });

  it("badges a consequential skill GATED and a source-gated skill NEEDS SOURCE", () => {
    const html = render(parseSkillsCatalog(mockPayload));
    expect(html).toContain("GATED"); // send_webhook (consequential)
    expect(html).toContain("NEEDS SOURCE"); // define_word (source-gated)
  });

  it("does NOT badge a pure read-only skill", () => {
    const html = render(
      parseSkillsCatalog({
        enabled: true,
        count: 1,
        skills: [{ name: "pure_one", category: "utilities", description: "d" }],
      }),
    );
    expect(html).not.toContain("GATED");
    expect(html).not.toContain("NEEDS SOURCE");
  });

  it("does NOT claim a fabricated marketplace count", () => {
    const html = render(parseSkillsCatalog(mockPayload));
    expect(html).not.toContain("13,700");
    expect(html).not.toContain("13700");
    expect(html).toMatch(/not a populated community marketplace/i);
  });

  it("never renders an unexpected/secret value", () => {
    const html = render(
      parseSkillsCatalog({
        enabled: true,
        count: 1,
        skills: [
          {
            name: "x",
            category: "utilities",
            description: "d",
            token: "sk-SECRET",
            run: "rm -rf /",
          },
        ],
      }),
    );
    expect(html).not.toContain("SECRET");
    expect(html).not.toContain("rm -rf");
  });

  it("renders the empty-library state honestly", () => {
    const html = render(parseSkillsCatalog({ enabled: true, count: 0, skills: [] }));
    expect(html).toMatch(/no skills in the library yet/i);
  });
});
