import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import CodeIntelPanel from "../components/CodeIntelPanel";
import {
  parseCodeExplained,
  parseCodeProposed,
  codeMethodLabel,
  type TelemetryEnvelope,
} from "../core/events";
import {
  type CodeIntel,
  HudState,
  initialState,
  reduce,
} from "../core/state";

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

/* ------------------------------------------------------------------------ *
 * parseCodeExplained — GROUNDED + CITED, NEVER FABRICATES. A hit with no
 * file_path is not a citation and is dropped; an empty/absent hits array is the
 * HONEST {hits:0} not-indexed reply (an empty array, never null). Never throws.
 * ------------------------------------------------------------------------ */
describe("parseCodeExplained (grounded + cited)", () => {
  it("parses a cited answer: question, method, and real file+offset+snippet hits", () => {
    const out = parseCodeExplained({
      question: "how is the config parsed",
      method: "neural-embedding",
      hits: [
        { file_path: "daemon/src/config.rs", byte_offset: 1280, snippet: "fn parse_config()" },
        { file_path: "daemon/src/config.rs", byte_offset: 4096, snippet: "KNOWN_KEYS" },
      ],
    });
    expect(out.question).toBe("how is the config parsed");
    expect(out.method).toBe("neural-embedding");
    expect(out.hits).toHaveLength(2);
    expect(out.hits[0]).toEqual({
      filePath: "daemon/src/config.rs",
      byteOffset: 1280,
      snippet: "fn parse_config()",
    });
  });

  it("the {hits:0} honest not-indexed reply yields an empty hits array (not null)", () => {
    const out = parseCodeExplained({ hits: 0 });
    expect(out.hits).toEqual([]);
    const out2 = parseCodeExplained({}); // no hits field at all
    expect(out2.hits).toEqual([]);
  });

  it("drops a hit with no file_path — a hit with no file to cite is not a citation", () => {
    const out = parseCodeExplained({
      hits: [
        { byte_offset: 10, snippet: "no path" },
        { file_path: "", snippet: "empty path" },
        { file_path: "a.rs", byte_offset: 5, snippet: "real" },
      ],
    });
    expect(out.hits).toHaveLength(1);
    expect(out.hits[0].filePath).toBe("a.rs");
  });

  it("defaults method to lexical-bm25 (never OVER-states a result as neural)", () => {
    expect(parseCodeExplained({ hits: [] }).method).toBe("lexical-bm25");
  });

  it("never throws on junk", () => {
    expect(() => parseCodeExplained({ hits: "nope", method: 42 })).not.toThrow();
    expect(parseCodeExplained({ hits: "nope" }).hits).toEqual([]);
  });
});

/* ------------------------------------------------------------------------ *
 * parseCodeProposed — a proposal the user cannot apply (no <ts>) is NEVER
 * surfaced. SECRET-FREE: only the ts + the grounded-hit count survive.
 * ------------------------------------------------------------------------ */
describe("parseCodeProposed (propose-only, ts-anchored)", () => {
  it("parses a well-formed code.proposed payload", () => {
    expect(parseCodeProposed({ ts: 1770000000, grounded_hits: 3 }, "T")).toEqual({
      ts: 1770000000,
      groundedHits: 3,
      at: "T",
    });
  });

  it("returns null when ts is missing or non-finite (no card without an apply <ts>)", () => {
    expect(parseCodeProposed({ grounded_hits: 3 }, "T")).toBeNull();
    expect(parseCodeProposed({ ts: "soon" }, "T")).toBeNull();
    expect(parseCodeProposed({ ts: Number.NaN }, "T")).toBeNull();
  });

  it("defaults grounded_hits to 0 when absent (honest 'grounded in 0 chunks')", () => {
    expect(parseCodeProposed({ ts: 9 }, "T")!.groundedHits).toBe(0);
  });

  it("never throws on junk", () => {
    expect(() => parseCodeProposed({}, "T")).not.toThrow();
    expect(parseCodeProposed({}, "T")).toBeNull();
  });
});

describe("codeMethodLabel (honest method)", () => {
  it("labels neural vs BM25 vs an unknown future method", () => {
    expect(codeMethodLabel("neural-embedding")).toMatch(/NEURAL/);
    expect(codeMethodLabel("lexical-bm25")).toMatch(/BM25/);
    expect(codeMethodLabel("hyper-rank")).toBe("HYPER-RANK");
  });
});

/* ------------------------------------------------------------------------ *
 * The reducer arms. code.explained sets the cited explanation (or the honest
 * empty); code.proposed surfaces a PROPOSE-ONLY review (no auto-apply); rejected
 * / blocked are HONEST non-error notes; "disabled" is the shipped-OFF state and
 * raises NOTHING. SECRET-FREE.
 * ------------------------------------------------------------------------ */
describe("code.* reducer", () => {
  it("code.explained surfaces a cited explanation with the real hits + method", () => {
    const s = tel(
      connected(),
      env("code.explained", {
        question: "where is config parsed",
        method: "neural-embedding",
        hits: [{ file_path: "config.rs", byte_offset: 12, snippet: "parse" }],
      }),
    );
    expect(s.codeIntel).not.toBeNull();
    expect(s.codeIntel!.explained!.question).toBe("where is config parsed");
    expect(s.codeIntel!.explained!.method).toBe("neural-embedding");
    expect(s.codeIntel!.explained!.hits).toHaveLength(1);
  });

  it("code.explained {hits:0} is the HONEST not-indexed state (empty hits, shown)", () => {
    const s = tel(connected(), env("code.explained", { hits: 0 }));
    expect(s.codeIntel).not.toBeNull();
    expect(s.codeIntel!.explained).not.toBeNull();
    expect(s.codeIntel!.explained!.hits).toEqual([]);
  });

  it("code.proposed surfaces a PROPOSE-ONLY review with ts (the apply <ts>) + hit count", () => {
    const s = tel(connected(), env("code.proposed", { ts: 1770000000, grounded_hits: 4 }, "system"));
    expect(s.codeIntel!.proposal).not.toBeNull();
    expect(s.codeIntel!.proposal!.ts).toBe(1770000000);
    expect(s.codeIntel!.proposal!.groundedHits).toBe(4);
  });

  it("ignores a malformed code.proposed (no review card without an apply <ts>)", () => {
    const s = tel(connected(), env("code.proposed", { grounded_hits: 4 }, "system"));
    expect(s.codeIntel).toBeNull();
  });

  it("an explain and a proposal coexist (different actions, both kept)", () => {
    let s = tel(connected(), env("code.explained", { hits: [{ file_path: "a.rs", snippet: "x" }] }));
    s = tel(s, env("code.proposed", { ts: 5, grounded_hits: 1 }, "system"));
    expect(s.codeIntel!.explained!.hits).toHaveLength(1);
    expect(s.codeIntel!.proposal!.ts).toBe(5);
  });

  it("a fresh explain REPLACES the prior explanation but keeps the pending proposal", () => {
    let s = tel(connected(), env("code.proposed", { ts: 7, grounded_hits: 2 }, "system"));
    s = tel(s, env("code.explained", { question: "q1", hits: [{ file_path: "a.rs", snippet: "1" }] }));
    s = tel(s, env("code.explained", { question: "q2", hits: [{ file_path: "b.rs", snippet: "2" }] }));
    expect(s.codeIntel!.explained!.question).toBe("q2");
    expect(s.codeIntel!.proposal!.ts).toBe(7); // proposal survived the new explain
  });

  it("code.rejected records an HONEST non-error note and clears any pending proposal", () => {
    let s = tel(connected(), env("code.proposed", { ts: 7, grounded_hits: 1 }, "system"));
    expect(s.codeIntel!.proposal).not.toBeNull();
    s = tel(s, env("code.rejected", { reason: "not-a-diff" }, "system"));
    expect(s.codeIntel!.proposal).toBeNull();
    expect(s.codeIntel!.note).toEqual(
      expect.objectContaining({ kind: "rejected", detail: "not-a-diff" }),
    );
  });

  it("code.blocked with reason=disabled is the shipped-OFF state — raises NO note", () => {
    const s = tel(connected(), env("code.blocked", { reason: "disabled", tool: "code_explain" }, "system"));
    expect(s.codeIntel).toBeNull();
  });

  it("code.blocked with a real abort stage records an honest note (not red chrome)", () => {
    const s = tel(connected(), env("code.blocked", { reason: "draft", tool: "code_propose_diff" }, "system"));
    expect(s.codeIntel!.note).toEqual(
      expect.objectContaining({ kind: "blocked", detail: "draft" }),
    );
  });

  it("a fresh proposal clears a stale rejected/blocked note", () => {
    let s = tel(connected(), env("code.rejected", { reason: "oversize" }, "system"));
    expect(s.codeIntel!.note).not.toBeNull();
    s = tel(s, env("code.proposed", { ts: 9, grounded_hits: 1 }, "system"));
    expect(s.codeIntel!.note).toBeNull();
    expect(s.codeIntel!.proposal!.ts).toBe(9);
  });

  it("never carries a secret — only the question/cited chunks/ts/count survive", () => {
    let s = tel(
      connected(),
      env("code.explained", {
        question: "q",
        hits: [{ file_path: "a.rs", byte_offset: 1, snippet: "ok", api_key: "sk-SECRET" }],
        token: "leak",
      }),
    );
    s = tel(s, env("code.proposed", { ts: 3, grounded_hits: 1, api_key: "sk-SECRET", token: "leak" }, "system"));
    const serialized = JSON.stringify(s.codeIntel);
    expect(serialized).not.toContain("SECRET");
    expect(serialized).not.toContain("leak");
    expect(serialized).not.toContain("api_key");
  });
});

/* ------------------------------------------------------------------------ *
 * The panel itself (rendered headlessly via renderToStaticMarkup — node env, no
 * jsdom, same pattern as forge/mark-forge tests). THE SAFETY POSTURE: cited
 * explanations + a propose-only diff shown READ-ONLY with the EXACT MANUAL apply
 * command, and NO button that applies/runs anything.
 * ------------------------------------------------------------------------ */
describe("CodeIntelPanel (grounded + propose-only, no auto-apply)", () => {
  const render = (code: CodeIntel | null) =>
    renderToStaticMarkup(createElement(CodeIntelPanel, { code }));

  it("renders nothing until a code.* event lands", () => {
    expect(render(null)).toBe("");
    expect(render({ explained: null, proposal: null, note: null })).toBe("");
  });

  it("shows a cited explanation: the question, the method, and the real file+offset", () => {
    const html = render({
      explained: {
        question: "how is config parsed",
        method: "neural-embedding",
        hits: [{ filePath: "daemon/src/config.rs", byteOffset: 1280, snippet: "fn parse()" }],
      },
      proposal: null,
      note: null,
    });
    expect(html).toContain("how is config parsed");
    expect(html).toContain("daemon/src/config.rs");
    expect(html).toContain("@1280");
    expect(html).toContain("fn parse()");
    expect(html).toMatch(/NEURAL/);
  });

  it("shows the HONEST not-indexed result (never fabricates code) on empty hits", () => {
    const html = render({
      explained: { question: "what is foo", method: "lexical-bm25", hits: [] },
      proposal: null,
      note: null,
    });
    expect(html).toMatch(/nothing in the code index matched/i);
    expect(html).toMatch(/no code is invented/i);
  });

  it("shows the PROPOSE-ONLY diff with the EXACT MANUAL apply command", () => {
    const html = render({
      explained: null,
      proposal: { ts: 1770000000, groundedHits: 3, at: "T" },
      note: null,
    });
    expect(html).toContain("scripts/apply_code_diff.sh 1770000000");
    expect(html).toMatch(/propose-only/i);
    expect(html).toMatch(/untouched/i);
  });

  it("has NO apply/run/confirm button — review-only (the manual command is the only apply path)", () => {
    const html = render({
      explained: { question: "q", method: "neural-embedding", hits: [{ filePath: "a.rs", byteOffset: 0, snippet: "x" }] },
      proposal: { ts: 5, groundedHits: 1, at: "T" },
      note: null,
    });
    // There is NO clickable control at all in this read-only panel: no button,
    // no link, no inline handler. The ONLY apply path is the human running the
    // shown terminal command (mirrors the forge/heal review panels).
    expect(html).not.toContain("<button");
    expect(html).not.toMatch(/<a\b/);
    expect(html).not.toMatch(/onclick/i);
    // The apply route is present ONLY as the manual shell command, never a control.
    expect(html).toContain("scripts/apply_code_diff.sh 5");
  });

  it("states the propose-only + not-guaranteed honesty in copy", () => {
    const html = render({
      explained: null,
      proposal: { ts: 5, groundedHits: 1, at: "T" },
      note: null,
    });
    expect(html).toMatch(/no one-click apply/i);
    expect(html).toMatch(/not guaranteed/i);
    expect(html).toMatch(/re-validates/i);
  });

  it("shows an honest non-error note (rejected/blocked) — NO red alert chrome", () => {
    const html = render({
      explained: null,
      proposal: null,
      note: { kind: "rejected", detail: "not-a-diff", at: "T" },
    });
    expect(html).toMatch(/draft rejected/i);
    expect(html).toMatch(/nothing changed/i);
    expect(html).toContain("not-a-diff");
    // Honest non-error: it must NOT use the red alert-panel chrome.
    expect(html).not.toContain("alert-panel");
    expect(html).not.toContain('role="alert"');
  });
});
