import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import CustomsPanel from "../components/CustomsPanel";
import {
  egressByteLabel,
  egressIsClean,
  parseEgressManifest,
  type EgressManifest,
  type TelemetryEnvelope,
} from "../core/events";
import { HudState, initialState, reduce } from "../core/state";

/* helpers ------------------------------------------------------------------ */

let counter = 0;
function env(
  event: string,
  data: Record<string, unknown> = {},
  source = "cloud",
): TelemetryEnvelope {
  counter += 1;
  return {
    ts: `2026-07-15T12:00:${String(counter % 60).padStart(2, "0")}Z`,
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

function renderPanel(manifest: EgressManifest | null): string {
  return renderToStaticMarkup(createElement(CustomsPanel, { manifest }));
}

/** The daemon's EgressManifest::telemetry() for a FULL (untrimmed) cloud turn —
 *  every category present, nothing withheld. Mirrors boundary.rs's fixtures:
 *  category/sensitivity/count/bytes only, never a value. */
const manifestFull: Record<string, unknown> = {
  items: [
    { category: "system_prompt", sensitivity: "public", count: 1, bytes: 2048 },
    { category: "persona", sensitivity: "public", count: 1, bytes: 512 },
    { category: "facts", sensitivity: "personal", count: 3, bytes: 96 },
    { category: "history", sensitivity: "personal", count: 2, bytes: 140 },
    { category: "world_rows", sensitivity: "contextual", count: 1, bytes: 64 },
    { category: "utterance", sensitivity: "personal", count: 1, bytes: 18 },
  ],
  total_bytes: 2878,
  trim: "none",
  trimmed: false,
  withheld: [],
  read_only: true,
  local_path_egresses: false,
};

/** The daemon's telemetry() for a NoFacts-trimmed turn — facts dropped from the
 *  sent items, withheld = ["facts"], trimmed = true. */
const manifestTrimmed: Record<string, unknown> = {
  items: [
    { category: "system_prompt", sensitivity: "public", count: 1, bytes: 2048 },
    { category: "history", sensitivity: "personal", count: 2, bytes: 140 },
    { category: "utterance", sensitivity: "personal", count: 1, bytes: 18 },
  ],
  total_bytes: 2206,
  trim: "no_facts",
  trimmed: true,
  withheld: ["facts"],
  read_only: true,
  local_path_egresses: false,
};

/* parseEgressManifest — inventory + defensive parse + pinned contract --------- */

describe("parseEgressManifest (CUSTOMS // EGRESS)", () => {
  it("reads the full inventory from the wire", () => {
    const m = parseEgressManifest(manifestFull);
    expect(m.items).toHaveLength(6);
    expect(m.totalBytes).toBe(2878);
    expect(m.trim).toBe("none");
    expect(m.trimmed).toBe(false);
    expect(m.withheld).toEqual([]);
    // Each row carries only shape: category / sensitivity / count / bytes.
    const facts = m.items.find((it) => it.category === "facts");
    expect(facts?.sensitivity).toBe("personal");
    expect(facts?.count).toBe(3);
    expect(facts?.bytes).toBe(96);
  });

  it("labels a trimmed turn honestly with its withheld categories", () => {
    const m = parseEgressManifest(manifestTrimmed);
    expect(m.trimmed).toBe(true);
    expect(m.trim).toBe("no_facts");
    expect(m.withheld).toEqual(["facts"]);
    // The withheld category is NOT in the sent items (drop is real).
    expect(m.items.some((it) => it.category === "facts")).toBe(false);
    expect(m.items.some((it) => it.category === "history")).toBe(true);
  });

  it("PINS the honesty contract — a hostile payload cannot flip it", () => {
    // A malicious frame claiming CUSTOMS mutated state / gated the local path is
    // NOT honored: CUSTOMS is read-only and never gates the local path.
    const m = parseEgressManifest({
      items: [{ category: "facts", sensitivity: "personal", count: 1, bytes: 10 }],
      read_only: false, // hostile claim
      local_path_egresses: true, // hostile claim
    });
    expect(m.readOnly).toBe(true);
    expect(m.localPathEgresses).toBe(false);
  });

  it("only marks trimmed when the wire says so AND something was withheld", () => {
    // A frame claiming trimmed=true but with an empty withheld list is NOT trimmed
    // (honest: nothing was actually held back).
    const m = parseEgressManifest({ items: [], trim: "no_facts", trimmed: true, withheld: [] });
    expect(m.trimmed).toBe(false);
  });

  it("never understates its own egress: total falls back to the item byte sum", () => {
    // A wire total SMALLER than the items add up to is not trusted (a manifest
    // must never understate what is leaving) — the item sum wins.
    const m = parseEgressManifest({
      items: [
        { category: "facts", sensitivity: "personal", count: 1, bytes: 100 },
        { category: "history", sensitivity: "personal", count: 1, bytes: 200 },
      ],
      total_bytes: 50, // dishonestly small
    });
    expect(m.totalBytes).toBe(300);
  });

  it("normalizes an unknown sensitivity to personal (never understates)", () => {
    const m = parseEgressManifest({
      items: [{ category: "facts", sensitivity: "TOP_SECRET", count: 1, bytes: 5 }],
    });
    expect(m.items[0].sensitivity).toBe("personal");
  });

  it("drops rows with no category and floors negative counts/bytes", () => {
    const m = parseEgressManifest({
      items: [
        { category: "facts", sensitivity: "personal", count: -3, bytes: -9 },
        { sensitivity: "personal", count: 1, bytes: 10 }, // no category => dropped
        { category: "", count: 1, bytes: 1 }, // blank category => dropped
      ],
    });
    expect(m.items).toHaveLength(1);
    expect(m.items[0].count).toBe(0);
    expect(m.items[0].bytes).toBe(0);
  });

  it("drops non-string withheld entries", () => {
    const m = parseEgressManifest({ items: [], trimmed: true, withheld: ["facts", 7, null, "history"] });
    expect(m.withheld).toEqual(["facts", "history"]);
  });

  it("defaults trim to none + honest-empty on a garbled/empty payload", () => {
    const m = parseEgressManifest({ items: "nope" });
    expect(m.items).toEqual([]);
    expect(m.trim).toBe("none");
    expect(m.trimmed).toBe(false);
    expect(m.totalBytes).toBe(0);
  });

  it("never throws on hostile junk", () => {
    expect(() => parseEgressManifest({})).not.toThrow();
    expect(() => parseEgressManifest({ items: [null, undefined, 3] })).not.toThrow();
    expect(() => parseEgressManifest({ items: 5, withheld: 9, trim: 1 })).not.toThrow();
  });
});

/* egressIsClean + egressByteLabel ------------------------------------------- */

describe("egressIsClean + egressByteLabel", () => {
  it("egressIsClean is true only for the untrimmed pass-through", () => {
    expect(egressIsClean(parseEgressManifest(manifestFull))).toBe(true);
    expect(egressIsClean(parseEgressManifest(manifestTrimmed))).toBe(false);
  });

  it("egressByteLabel renders compact human sizes", () => {
    expect(egressByteLabel(0)).toBe("0 B");
    expect(egressByteLabel(512)).toBe("512 B");
    expect(egressByteLabel(1024)).toBe("1.0 KB");
    expect(egressByteLabel(2878)).toBe("2.8 KB");
    expect(egressByteLabel(2 * 1024 * 1024)).toBe("2.0 MB");
    // Garbled/negative floors to 0 B (never a phantom size).
    expect(egressByteLabel(-5)).toBe("0 B");
    expect(egressByteLabel(NaN)).toBe("0 B");
  });
});

/* reducer folding ----------------------------------------------------------- */

describe("reducer: boundary.manifest", () => {
  it("folds a boundary.manifest into egressManifest", () => {
    const s = tel(connected(), env("boundary.manifest", manifestFull));
    expect(s.egressManifest).not.toBeNull();
    expect(s.egressManifest?.items).toHaveLength(6);
    expect(s.egressManifest?.trimmed).toBe(false);
  });

  it("pins the honesty contract when folding (read-only, no local-path gate)", () => {
    const s = tel(
      connected(),
      env("boundary.manifest", { items: [], read_only: false, local_path_egresses: true }),
    );
    expect(s.egressManifest?.readOnly).toBe(true);
    expect(s.egressManifest?.localPathEgresses).toBe(false);
  });

  it("replaces the prior manifest in place with the latest cloud turn", () => {
    let s = tel(connected(), env("boundary.manifest", manifestFull));
    expect(s.egressManifest?.trimmed).toBe(false);
    s = tel(s, env("boundary.manifest", manifestTrimmed));
    expect(s.egressManifest?.trimmed).toBe(true);
    expect(s.egressManifest?.withheld).toEqual(["facts"]);
  });

  it("is null on a fresh (local-only / no cloud egress) state", () => {
    expect(connected().egressManifest).toBeNull();
  });
});

/* panel rendering ----------------------------------------------------------- */

describe("CustomsPanel render", () => {
  it("renders nothing until a cloud turn produced a manifest", () => {
    expect(renderPanel(null)).toBe("");
  });

  it("renders the full inventory with sensitivity bands, counts and sizes", () => {
    const html = renderPanel(parseEgressManifest(manifestFull));
    expect(html).toContain("CUSTOMS // EGRESS");
    expect(html).toContain("CLOUD EGRESS MANIFEST");
    expect(html).toContain("FULL CONTEXT");
    // Category + sensitivity + a human byte size are all surfaced.
    expect(html).toContain("facts");
    expect(html).toContain("personal");
    expect(html).toContain("world_rows");
    expect(html).toContain("contextual");
    expect(html).toContain("2.8 KB"); // the total
    expect(html).toContain("3 units"); // the facts count
    // The clean pass-through copy + the read-only contract are stated.
    expect(html).toContain("No trim active");
    expect(html).toContain("never leaves the box");
  });

  it("renders a trimmed turn: the trim label + the withheld category", () => {
    const html = renderPanel(parseEgressManifest(manifestTrimmed));
    expect(html).toContain("TRIMMED");
    expect(html).toContain("NO_FACTS");
    expect(html).toContain("WITHHELD");
    // The withheld category is shown as a held chip...
    expect(html).toContain('class="customs-held-cat">facts');
    // ...and is NOT among the SENT inventory rows (drop is real, not just labeled).
    // Inventory rows use `customs-cat`; a withheld chip uses `customs-held-cat`.
    expect(html).not.toContain('class="customs-cat">facts');
    // History IS still a sent inventory row.
    expect(html).toContain('class="customs-cat">history');
    expect(html).toContain("reduce-only");
  });

  it("never claims to gate the local path (honesty contract in the copy)", () => {
    const html = renderPanel(parseEgressManifest(manifestFull));
    expect(html).toContain("LOCAL model never leaves the box");
  });
});
