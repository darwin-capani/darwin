import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import AnswerCrossCheckPanel from "../components/AnswerCrossCheckPanel";
import {
  crossCheckBadgeFor,
  crossCheckStatusIsEmpty,
  debateBadgeFor,
  debateStatusIsEmpty,
  parseCrossCheckStatus,
  parseDebateStatus,
  type CrossCheckStatus,
  type DebateStatus,
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
    ts: `2026-06-17T12:00:${String(counter % 60).padStart(2, "0")}Z`,
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

function render(
  crossCheck: CrossCheckStatus | null,
  debate: DebateStatus | null,
): string {
  return renderToStaticMarkup(
    createElement(AnswerCrossCheckPanel, { crossCheck, debate }),
  );
}

/* The daemon's lean badge-telemetry shapes (anthropic.rs
 * cross_check_badge_telemetry / debate_badge_telemetry). The honest notes are
 * taken verbatim from CROSS_CHECK_NOTE / DEBATE_NOTE in the daemon. */
const CROSS_NOTE =
  "A bounded plausibility cross-check of a tool result before it is surfaced as " +
  "fact. It only DOWNGRADES confidence and FLAGS a questionable result — it NEVER " +
  "removes a confirmation gate and is NOT a correctness guarantee. Ships OFF by " +
  "default.";

const DEBATE_NOTE =
  "For high-stakes asks only, a second independent model answers the same " +
  "question. Agreement RAISES confidence; disagreement SURFACES BOTH answers " +
  "(never silently picked or averaged); if the second model is unavailable it " +
  "falls back to one and says so. At most two model calls; ships OFF by default.";

/* #21 cross-check, gate ON, each possible outcome. */
const crossPlausible: Record<string, unknown> = {
  cross_check_on: true,
  outcome: "plausible",
  badge: "CHECKED",
  note: CROSS_NOTE,
};
const crossFlagged: Record<string, unknown> = {
  cross_check_on: true,
  outcome: "flagged",
  badge: "UNVERIFIED",
  note: CROSS_NOTE,
};
/* The SHIPPED DEFAULT: [answers].cross_check OFF — outcome "off" + null badge. */
const crossOff: Record<string, unknown> = {
  cross_check_on: false,
  outcome: "off",
  badge: null,
  note: CROSS_NOTE,
};

/* #22 debate, gate ON, each possible outcome. */
const debateAgree: Record<string, unknown> = {
  debate_on: true,
  outcome: "agree",
  badge: "CORROBORATED",
  note: DEBATE_NOTE,
};
const debateDisagree: Record<string, unknown> = {
  debate_on: true,
  outcome: "disagree",
  badge: "DISPUTED",
  note: DEBATE_NOTE,
};
const debateFallback: Record<string, unknown> = {
  debate_on: true,
  outcome: "fallback",
  badge: "ONE-MODEL",
  note: DEBATE_NOTE,
};
/* The SHIPPED DEFAULT / every ordinary turn: outcome "off" + null badge. */
const debateOff: Record<string, unknown> = {
  debate_on: false,
  outcome: "off",
  badge: null,
  note: DEBATE_NOTE,
};

/* === #21 CROSS-CHECK ===================================================== */

describe("crossCheckBadgeFor", () => {
  it("derives the HUD badge from the outcome token (single source of truth)", () => {
    expect(crossCheckBadgeFor("off")).toBeNull();
    expect(crossCheckBadgeFor("plausible")).toBe("CHECKED");
    expect(crossCheckBadgeFor("flagged")).toBe("UNVERIFIED");
  });
});

describe("parseCrossCheckStatus", () => {
  it("parses a plausible (CHECKED) outcome", () => {
    const v = parseCrossCheckStatus(crossPlausible);
    expect(v.crossCheckOn).toBe(true);
    expect(v.outcome).toBe("plausible");
    expect(v.badge).toBe("CHECKED");
    expect(v.note).toBe(CROSS_NOTE);
  });

  it("parses a flagged (UNVERIFIED) outcome", () => {
    const v = parseCrossCheckStatus(crossFlagged);
    expect(v.outcome).toBe("flagged");
    expect(v.badge).toBe("UNVERIFIED");
  });

  it("the OFF default yields the empty (renders-nothing) status", () => {
    const v = parseCrossCheckStatus(crossOff);
    expect(v.crossCheckOn).toBe(false);
    expect(v.outcome).toBe("off");
    expect(v.badge).toBeNull();
    expect(crossCheckStatusIsEmpty(v)).toBe(true);
  });

  it("DERIVES the badge from the outcome — a spoofed wire badge never wins", () => {
    const v = parseCrossCheckStatus({
      cross_check_on: true,
      outcome: "plausible",
      badge: "UNVERIFIED", // lie on the wire
      note: CROSS_NOTE,
    });
    expect(v.outcome).toBe("plausible");
    expect(v.badge).toBe("CHECKED"); // derived, not "UNVERIFIED"
  });

  it("collapses an unknown outcome token to OFF (no unrecognized badge)", () => {
    const v = parseCrossCheckStatus({
      cross_check_on: true,
      outcome: "definitely-correct-trust-me",
      badge: "CHECKED",
      note: CROSS_NOTE,
    });
    expect(v.outcome).toBe("off");
    expect(v.badge).toBeNull();
    expect(crossCheckStatusIsEmpty(v)).toBe(true);
  });

  it("never throws on junk and yields an honest OFF status", () => {
    const v = parseCrossCheckStatus({
      outcome: 7,
      cross_check_on: "yes",
      note: null,
    });
    expect(v.outcome).toBe("off");
    expect(v.badge).toBeNull();
    expect(v.crossCheckOn).toBe(false);
    expect(v.note).toBe("");
    expect(crossCheckStatusIsEmpty(v)).toBe(true);
  });

  it("is SECRET-FREE: drops the raw tool result / flag reasons / embedding", () => {
    const v = parseCrossCheckStatus({
      cross_check_on: true,
      outcome: "flagged",
      badge: "UNVERIFIED",
      note: CROSS_NOTE,
      flags: ["the tool returned nothing, but the answer states a fact"],
      raw_result: "SECRET_TOOL_OUTPUT",
      embedding: [0.123456, 0.654321],
    });
    expect(Object.keys(v).sort()).toEqual([
      "badge",
      "crossCheckOn",
      "note",
      "outcome",
    ]);
  });
});

describe("answer.cross_checked reducer", () => {
  it("folds a plausible outcome onto state", () => {
    const s = tel(connected(), env("answer.cross_checked", crossPlausible));
    expect(s.crossCheckStatus).not.toBeNull();
    expect(s.crossCheckStatus?.outcome).toBe("plausible");
    expect(s.crossCheckStatus?.badge).toBe("CHECKED");
  });

  it("folds a flagged outcome (a check tripped)", () => {
    const s = tel(connected(), env("answer.cross_checked", crossFlagged));
    expect(s.crossCheckStatus?.badge).toBe("UNVERIFIED");
  });

  it("the OFF default leaves nothing to render (stays null, same reference)", () => {
    const base = connected();
    const s = tel(base, env("answer.cross_checked", crossOff));
    expect(s.crossCheckStatus).toBeNull();
    expect(s.crossCheckStatus).toBe(base.crossCheckStatus);
  });

  it("a fresh turn REPLACES the prior outcome (per-turn, no cross-turn leak)", () => {
    let s = tel(connected(), env("answer.cross_checked", crossFlagged));
    expect(s.crossCheckStatus?.badge).toBe("UNVERIFIED");
    s = tel(s, env("answer.cross_checked", crossPlausible));
    expect(s.crossCheckStatus?.badge).toBe("CHECKED");
  });

  it("an off-gate turn after a checked turn CLEARS the prior badge", () => {
    let s = tel(connected(), env("answer.cross_checked", crossPlausible));
    expect(s.crossCheckStatus).not.toBeNull();
    s = tel(s, env("answer.cross_checked", crossOff));
    expect(s.crossCheckStatus).toBeNull();
  });
});

/* === #22 DEBATE ========================================================== */

describe("debateBadgeFor", () => {
  it("derives the HUD badge from the outcome token (single source of truth)", () => {
    expect(debateBadgeFor("off")).toBeNull();
    expect(debateBadgeFor("agree")).toBe("CORROBORATED");
    expect(debateBadgeFor("disagree")).toBe("DISPUTED");
    expect(debateBadgeFor("fallback")).toBe("ONE-MODEL");
  });
});

describe("parseDebateStatus", () => {
  it("parses an agree (CORROBORATED) outcome", () => {
    const v = parseDebateStatus(debateAgree);
    expect(v.debateOn).toBe(true);
    expect(v.outcome).toBe("agree");
    expect(v.badge).toBe("CORROBORATED");
    expect(v.note).toBe(DEBATE_NOTE);
  });

  it("parses a disagree (DISPUTED) and a fallback (ONE-MODEL) outcome", () => {
    expect(parseDebateStatus(debateDisagree).badge).toBe("DISPUTED");
    expect(parseDebateStatus(debateFallback).badge).toBe("ONE-MODEL");
  });

  it("the OFF default yields the empty (renders-nothing) status", () => {
    const v = parseDebateStatus(debateOff);
    expect(v.debateOn).toBe(false);
    expect(v.outcome).toBe("off");
    expect(v.badge).toBeNull();
    expect(debateStatusIsEmpty(v)).toBe(true);
  });

  it("DERIVES the badge from the outcome — a spoofed wire badge never wins", () => {
    const v = parseDebateStatus({
      debate_on: true,
      outcome: "disagree",
      badge: "CORROBORATED", // lie: a disagreement must never read as consensus
      note: DEBATE_NOTE,
    });
    expect(v.outcome).toBe("disagree");
    expect(v.badge).toBe("DISPUTED"); // derived — disagreement is never hidden
  });

  it("collapses an unknown outcome token to OFF (no unrecognized badge)", () => {
    const v = parseDebateStatus({
      debate_on: true,
      outcome: "unanimous-trust-me",
      badge: "CORROBORATED",
      note: DEBATE_NOTE,
    });
    expect(v.outcome).toBe("off");
    expect(v.badge).toBeNull();
    expect(debateStatusIsEmpty(v)).toBe(true);
  });

  it("never throws on junk and yields an honest OFF status", () => {
    const v = parseDebateStatus({ outcome: {}, debate_on: 1, note: 9 });
    expect(v.outcome).toBe("off");
    expect(v.badge).toBeNull();
    expect(v.debateOn).toBe(false);
    expect(v.note).toBe("");
    expect(debateStatusIsEmpty(v)).toBe(true);
  });

  it("is SECRET-FREE: drops both raw answers / level / embedding", () => {
    const v = parseDebateStatus({
      debate_on: true,
      outcome: "disagree",
      badge: "DISPUTED",
      note: DEBATE_NOTE,
      answer_a: "SECRET_LOCAL_ANSWER",
      answer_b: "SECRET_CLOUD_ANSWER",
      level: "high",
      embedding: [0.111111],
    });
    expect(Object.keys(v).sort()).toEqual([
      "badge",
      "debateOn",
      "note",
      "outcome",
    ]);
  });
});

describe("answer.debated reducer", () => {
  it("folds an agree outcome onto state", () => {
    const s = tel(connected(), env("answer.debated", debateAgree));
    expect(s.debateStatus?.outcome).toBe("agree");
    expect(s.debateStatus?.badge).toBe("CORROBORATED");
  });

  it("folds a disagree outcome (both answers surfaced honestly)", () => {
    const s = tel(connected(), env("answer.debated", debateDisagree));
    expect(s.debateStatus?.badge).toBe("DISPUTED");
  });

  it("the OFF default / ordinary turn leaves nothing (stays null, same ref)", () => {
    const base = connected();
    const s = tel(base, env("answer.debated", debateOff));
    expect(s.debateStatus).toBeNull();
    expect(s.debateStatus).toBe(base.debateStatus);
  });

  it("a fresh turn REPLACES the prior outcome (per-turn, no cross-turn leak)", () => {
    let s = tel(connected(), env("answer.debated", debateDisagree));
    expect(s.debateStatus?.badge).toBe("DISPUTED");
    s = tel(s, env("answer.debated", debateAgree));
    expect(s.debateStatus?.badge).toBe("CORROBORATED");
  });

  it("an ordinary (off) turn after a debated turn CLEARS the prior badge", () => {
    let s = tel(connected(), env("answer.debated", debateAgree));
    expect(s.debateStatus).not.toBeNull();
    s = tel(s, env("answer.debated", debateOff));
    expect(s.debateStatus).toBeNull();
  });
});

/* === PANEL =============================================================== */

describe("AnswerCrossCheckPanel", () => {
  it("renders nothing when both statuses are null (shipped OFF default)", () => {
    expect(render(null, null)).toBe("");
  });

  it("renders nothing for off-gate statuses (reducer holds them null)", () => {
    const cc = parseCrossCheckStatus(crossOff);
    const db = parseDebateStatus(debateOff);
    expect(render(cc, db)).toBe("");
  });

  it("surfaces the CHECKED badge with HONEST copy (only adds caution)", () => {
    const html = render(parseCrossCheckStatus(crossPlausible), null);
    expect(html).toContain("ANSWER // CROSS-CHECK");
    expect(html).toContain("CHECKED");
    const lower = html.toLowerCase();
    expect(lower).toContain("never removes a confirmation gate");
    expect(lower).toContain("not a correctness guarantee");
    expect(lower).toContain("ships off");
  });

  it("surfaces the UNVERIFIED badge (a check tripped, confidence downgraded)", () => {
    const html = render(parseCrossCheckStatus(crossFlagged), null);
    expect(html).toContain("UNVERIFIED");
    expect(html.toLowerCase()).toContain("downgraded");
  });

  it("surfaces the CORROBORATED badge (two models agreed, raises confidence)", () => {
    const html = render(null, parseDebateStatus(debateAgree));
    expect(html).toContain("CORROBORATED");
    expect(html.toLowerCase()).toContain("agreed");
  });

  it("surfaces DISPUTED honestly — models disagree, both surfaced (never hidden)", () => {
    const html = render(null, parseDebateStatus(debateDisagree));
    expect(html).toContain("DISPUTED");
    const lower = html.toLowerCase();
    expect(lower).toContain("disagree");
    expect(lower).toContain("both answers are surfaced");
    expect(lower).not.toContain("consensus reached");
  });

  it("surfaces ONE-MODEL fallback honestly (second model unavailable, says so)", () => {
    const html = render(null, parseDebateStatus(debateFallback));
    expect(html).toContain("ONE-MODEL");
    const lower = html.toLowerCase();
    expect(lower).toContain("unavailable");
    expect(lower).toContain("runtime-gated");
  });

  it("renders BOTH rows when both passes ran this turn", () => {
    const html = render(
      parseCrossCheckStatus(crossFlagged),
      parseDebateStatus(debateDisagree),
    );
    expect(html).toContain("UNVERIFIED");
    expect(html).toContain("DISPUTED");
    expect(html).toContain("TOOL RESULT");
    expect(html).toContain("TWO MODELS");
  });

  it("shows only the row whose pass ran (the other gate off renders nothing)", () => {
    // Cross-check ran, debate off (ordinary turn): only the TOOL RESULT row.
    const html = render(
      parseCrossCheckStatus(crossPlausible),
      parseDebateStatus(debateOff),
    );
    expect(html).toContain("TOOL RESULT");
    expect(html).not.toContain("TWO MODELS");
  });

  it("is SECRET-FREE: never renders the raw tool result or the raw answers", () => {
    const cc = parseCrossCheckStatus({
      cross_check_on: true,
      outcome: "flagged",
      badge: "UNVERIFIED",
      note: CROSS_NOTE,
      raw_result: "SECRET_TOOL_OUTPUT",
    });
    const db = parseDebateStatus({
      debate_on: true,
      outcome: "disagree",
      badge: "DISPUTED",
      note: DEBATE_NOTE,
      answer_a: "SECRET_LOCAL_ANSWER",
      answer_b: "SECRET_CLOUD_ANSWER",
    });
    const html = render(cc, db);
    expect(html).toContain("UNVERIFIED");
    expect(html).toContain("DISPUTED");
    expect(html).not.toContain("SECRET_TOOL_OUTPUT");
    expect(html).not.toContain("SECRET_LOCAL_ANSWER");
    expect(html).not.toContain("SECRET_CLOUD_ANSWER");
  });
});
