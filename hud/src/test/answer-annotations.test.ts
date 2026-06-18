import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import AnswerSourcesPanel from "../components/AnswerSourcesPanel";
import {
  answerAnnotationIsEmpty,
  confidenceLabel,
  parseAnswerAnnotation,
  type AnswerAnnotation,
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

function render(annotation: AnswerAnnotation | null): string {
  return renderToStaticMarkup(createElement(AnswerSourcesPanel, { annotation }));
}

/** The daemon's answer_annotation_telemetry shape for a CITED turn (both gates
 *  on): two REAL tool-result sources + a grounded confidence self-report. Mirrors
 *  anthropic.rs answer_annotation_telemetry / the annotation_telemetry_is_secret_
 *  free_and_honest test fixture — real locators/snippets only. */
const annotatedCited: Record<string, unknown> = {
  cite_on: true,
  confidence_on: true,
  from_my_knowledge: false,
  sources: [
    {
      source: "doc_search",
      citation: "indexed files",
      snippet: "the launch is on the 14th",
    },
    {
      source: "episodic_recall",
      citation: "past episodes",
      snippet: "we discussed the venue last week",
    },
  ],
  confidence: { level: "grounded", reason: "from your notes" },
};

/** A NO-RETRIEVAL turn with cite on: empty sources + from_my_knowledge true +
 *  an inferred confidence self-report. The honest "from my own knowledge" case. */
const annotatedFromKnowledge: Record<string, unknown> = {
  cite_on: true,
  confidence_on: true,
  from_my_knowledge: true,
  sources: [],
  confidence: { level: "inferred", reason: "reasoned from general knowledge" },
};

/** The SHIPPED DEFAULT: both [answers] gates OFF — empty sources + null
 *  confidence + from_my_knowledge false. The HUD must render NOTHING. */
const annotatedOff: Record<string, unknown> = {
  cite_on: false,
  confidence_on: false,
  from_my_knowledge: false,
  sources: [],
  confidence: null,
};

/* ------------------------------------------------------------------------- */

describe("parseAnswerAnnotation", () => {
  it("records the real tool-result citations from a cited turn", () => {
    const a = parseAnswerAnnotation(annotatedCited);
    expect(a.citeOn).toBe(true);
    expect(a.confidenceOn).toBe(true);
    expect(a.fromMyKnowledge).toBe(false);
    expect(a.sources).toHaveLength(2);
    // The recorded sources are exactly the real tool results — never fabricated.
    expect(a.sources[0]).toEqual({
      source: "doc_search",
      citation: "indexed files",
      snippet: "the launch is on the 14th",
    });
    expect(a.sources[1].source).toBe("episodic_recall");
    expect(a.sources[1].citation).toBe("past episodes");
    expect(a.confidence).toEqual({ level: "grounded", reason: "from your notes" });
  });

  it("a no-retrieval turn is from-my-knowledge with NO fabricated citation", () => {
    const a = parseAnswerAnnotation(annotatedFromKnowledge);
    expect(a.fromMyKnowledge).toBe(true);
    expect(a.sources).toHaveLength(0); // never invents a source
    expect(a.confidence).toEqual({
      level: "inferred",
      reason: "reasoned from general knowledge",
    });
  });

  it("both gates OFF yields the empty (renders-nothing) annotation", () => {
    const a = parseAnswerAnnotation(annotatedOff);
    expect(a.sources).toHaveLength(0);
    expect(a.confidence).toBeNull();
    expect(a.fromMyKnowledge).toBe(false);
    expect(answerAnnotationIsEmpty(a)).toBe(true);
  });

  it("drops a source with no tool name or no real locator (never fabricates)", () => {
    const a = parseAnswerAnnotation({
      cite_on: true,
      confidence_on: false,
      from_my_knowledge: false,
      sources: [
        { source: "doc_search", citation: "indexed files", snippet: "real" },
        { source: "", citation: "x", snippet: "no tool" }, // dropped
        { source: "web_search", snippet: "no citation" }, // dropped
        { citation: "orphan", snippet: "no source" }, // dropped
      ],
    });
    expect(a.sources).toHaveLength(1);
    expect(a.sources[0].source).toBe("doc_search");
  });

  it("drops an unknown confidence level rather than rendering a bad badge", () => {
    const a = parseAnswerAnnotation({
      cite_on: false,
      confidence_on: true,
      from_my_knowledge: false,
      sources: [],
      confidence: { level: "very-sure", reason: "nope" },
    });
    expect(a.confidence).toBeNull();
  });

  it("never throws on junk and yields an honest empty annotation", () => {
    const a = parseAnswerAnnotation({ sources: "not-an-array", confidence: 7 });
    expect(a.sources).toEqual([]);
    expect(a.confidence).toBeNull();
    expect(answerAnnotationIsEmpty(a)).toBe(true);
  });
});

describe("confidenceLabel", () => {
  it("maps each self-report level to its honest label", () => {
    expect(confidenceLabel("grounded")).toBe("GROUNDED");
    expect(confidenceLabel("inferred")).toBe("INFERRED");
    expect(confidenceLabel("uncertain")).toBe("UNCERTAIN");
  });
});

describe("answer.annotated reducer", () => {
  it("folds a cited annotation onto state", () => {
    const s = tel(connected(), env("answer.annotated", annotatedCited));
    expect(s.answerAnnotation).not.toBeNull();
    expect(s.answerAnnotation?.sources).toHaveLength(2);
    expect(s.answerAnnotation?.sources[0].citation).toBe("indexed files");
    expect(s.answerAnnotation?.confidence?.level).toBe("grounded");
  });

  it("both gates OFF leaves nothing to render (stays null, same reference)", () => {
    const base = connected();
    const s = tel(base, env("answer.annotated", annotatedOff));
    expect(s.answerAnnotation).toBeNull();
    // A stream of off-gate turns must not churn the tree.
    expect(s.answerAnnotation).toBe(base.answerAnnotation);
  });

  it("a fresh turn REPLACES the prior annotation (per-turn, no cross-turn leak)", () => {
    // Turn N: cited with two real sources.
    let s = tel(connected(), env("answer.annotated", annotatedCited));
    expect(s.answerAnnotation?.sources).toHaveLength(2);
    // Turn N+1: from-my-knowledge (no retrieval) — must NOT keep N's sources.
    s = tel(s, env("answer.annotated", annotatedFromKnowledge));
    expect(s.answerAnnotation?.fromMyKnowledge).toBe(true);
    expect(s.answerAnnotation?.sources).toHaveLength(0);
  });

  it("an off-gate turn after a cited turn CLEARS the prior sources", () => {
    // Turn N: cited. Turn N+1: gates off (empty) -> the panel clears to null so
    // turn N's sources never linger onto a later off-gate turn.
    let s = tel(connected(), env("answer.annotated", annotatedCited));
    expect(s.answerAnnotation).not.toBeNull();
    s = tel(s, env("answer.annotated", annotatedOff));
    expect(s.answerAnnotation).toBeNull();
  });
});

describe("AnswerSourcesPanel", () => {
  it("renders nothing when there is no annotation (shipped OFF default)", () => {
    expect(render(null)).toBe("");
  });

  it("renders nothing for an off-gate turn (reducer holds it null)", () => {
    const s = tel(connected(), env("answer.annotated", annotatedOff));
    expect(render(s.answerAnnotation)).toBe("");
  });

  it("surfaces the REAL cited sources + the confidence self-report", () => {
    const a = parseAnswerAnnotation(annotatedCited);
    const html = render(a);
    expect(html).toContain("ANSWER // PROVENANCE");
    // Every real source locator + tool + snippet is surfaced.
    expect(html).toContain("indexed files");
    expect(html).toContain("doc_search");
    expect(html).toContain("the launch is on the 14th");
    expect(html).toContain("past episodes");
    expect(html).toContain("episodic_recall");
    // The confidence chip (the model's self-report).
    expect(html).toContain("GROUNDED");
    expect(html).toContain("from your notes");
    // Honest framing — a self-report, not a measured score.
    expect(html.toLowerCase()).toContain("self-report");
    // No fabricated "from my own knowledge" label on a cited turn.
    expect(html).not.toContain("FROM MY OWN KNOWLEDGE");
  });

  it("shows the honest from-my-knowledge label when no retrieval ran", () => {
    const a = parseAnswerAnnotation(annotatedFromKnowledge);
    const html = render(a);
    expect(html).toContain("FROM MY OWN KNOWLEDGE");
    expect(html.toLowerCase()).toContain("no source was consulted");
    // The confidence self-report still surfaces.
    expect(html).toContain("INFERRED");
  });

  it("is SECRET-FREE: only the real locators/snippets + self-report render", () => {
    // A daemon that (incorrectly) tried to leak a secret/embedding alongside the
    // honest fields: the panel reads ONLY the honest fields, never the secret.
    const a = parseAnswerAnnotation({
      cite_on: true,
      confidence_on: true,
      from_my_knowledge: false,
      sources: [
        {
          source: "doc_search",
          citation: "indexed files",
          snippet: "real snippet",
          embedding: [0.123456, 0.654321],
          audio: "AUDIO_BLOB",
        },
      ],
      confidence: { level: "grounded", reason: "real reason" },
    });
    // The parsed source carries ONLY the three honest fields.
    expect(Object.keys(a.sources[0]).sort()).toEqual(["citation", "snippet", "source"]);
    const html = render(a);
    expect(html).toContain("real snippet");
    expect(html).not.toContain("0.123456");
    expect(html).not.toContain("AUDIO_BLOB");
    expect(html).not.toContain("embedding");
  });
});
