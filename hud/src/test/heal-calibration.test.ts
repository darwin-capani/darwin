import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import AlertPanel from "../components/AlertPanel";
import SelfHealPanel from "../components/SelfHealPanel";
import {
  calibrationRows,
  CALIBRATION_MAX_ROWS,
  parseHealCalibration,
  type HealCalibration,
} from "../core/heal";
import type { TelemetryEnvelope } from "../core/events";
import { initialState, reduce, type HudState } from "../core/state";

/**
 * DEAD FIELD: `calibration` on heal.proposal / heal.rejected.
 *
 * The daemon has shipped this payload on both terminal heal events since the
 * two self-heal tunables became settable, and the HUD DECLARED it in its wire
 * types and rendered none of it. Every candidate's review score, every cargo
 * stage's real wall time on this machine, and `candidates_unaffordable` — the
 * one number that says `[self_heal].attempt_budget_secs` is too small — were
 * computed, serialized, transmitted, and dropped.
 *
 * These tests hold all three links of the chain: the reducer must keep it, the
 * SELF-REPAIR panel must render it beside ACCEPT & APPLY, and the REJECTED
 * banner must render it too (a rejection is where the budget/floor numbers are
 * most actionable). Removing any one link fails a NAMED test below.
 */

const envelope = (event: string, data: Record<string, unknown>): TelemetryEnvelope => ({
  ts: "2026-08-10T00:00:00Z",
  source: "system",
  event,
  data,
});

const apply = (s: HudState, event: string, data: Record<string, unknown>): HudState =>
  reduce(s, { type: "telemetry", envelope: envelope(event, data), at: 1_000 });

/** A realistic attempt: 3 drafted, 1 never staged, 1 review call lost, one
 *  cargo stage cut off by the budget. */
const WIRE_CALIBRATION = {
  confidences: [
    { candidate: 1, confidence: 0.82, reviewed: true },
    { candidate: 2, confidence: 0.4, reviewed: true },
    { candidate: 3, confidence: 0.0, reviewed: false },
  ],
  stages: [
    { candidate: 1, stage: "cargo_check", secs: 41, ok: true, cut_off: false },
    { candidate: 1, stage: "cargo_test", secs: 233, ok: true, cut_off: false },
    { candidate: 2, stage: "cargo_test", secs: 180, ok: false, cut_off: true },
  ],
  attempt_spent_secs: 611,
  attempt_budget_secs: 900,
  candidate_budget_secs: 450,
  confidence_floor: 0.6,
  candidates_drafted: 3,
  candidates_unaffordable: 1,
};

describe("calibration parse (pure)", () => {
  it("reads every field the daemon actually sends", () => {
    const c = parseHealCalibration({ calibration: WIRE_CALIBRATION })!;
    expect(c.reviews).toHaveLength(3);
    expect(c.reviews[2]).toEqual({ candidate: 3, confidence: 0, reviewed: false });
    expect(c.stages).toHaveLength(3);
    expect(c.stages[2].cutOff).toBe(true);
    expect(c.attemptSpentSecs).toBe(611);
    expect(c.attemptBudgetSecs).toBe(900);
    expect(c.candidateBudgetSecs).toBe(450);
    expect(c.confidenceFloor).toBe(0.6);
    expect(c.candidatesDrafted).toBe(3);
    expect(c.candidatesUnaffordable).toBe(1);
  });

  it("an older daemon (no calibration) yields null, not a shell of zeros", () => {
    expect(parseHealCalibration({})).toBeNull();
    expect(parseHealCalibration({ calibration: null })).toBeNull();
    expect(parseHealCalibration({ calibration: {} })).toBeNull();
    expect(parseHealCalibration(null)).toBeNull();
  });

  it("a missing `reviewed` flag reads as NOT reviewed, never as a review", () => {
    const c = parseHealCalibration({
      calibration: { confidences: [{ candidate: 1, confidence: 0 }] },
    })!;
    expect(c.reviews[0].reviewed).toBe(false);
  });

  it("bounds a runaway frame", () => {
    const many = Array.from({ length: CALIBRATION_MAX_ROWS + 20 }, (_, i) => ({
      candidate: i,
      confidence: 0.5,
      reviewed: true,
    }));
    const c = parseHealCalibration({ calibration: { confidences: many } })!;
    expect(c.reviews).toHaveLength(CALIBRATION_MAX_ROWS);
  });
});

describe("calibration arithmetic (pure)", () => {
  const rows = (c: Partial<HealCalibration>) =>
    calibrationRows({
      reviews: [],
      stages: [],
      attemptSpentSecs: null,
      attemptBudgetSecs: null,
      candidateBudgetSecs: null,
      confidenceFloor: null,
      candidatesDrafted: null,
      candidatesUnaffordable: null,
      ...c,
    });
  const find = (c: Partial<HealCalibration>, key: string) =>
    rows(c).find((r) => r.key === key);

  it("states the remaining budget margin as a NUMBER (900 - 611 = 289)", () => {
    const r = find({ attemptBudgetSecs: 900, attemptSpentSecs: 611 }, "budget")!;
    expect(r.text).toBe("spent 611s of 900s — 289s left");
    expect(r.warn).toBe(false);
  });

  it("an over-spent budget says OVER by N, never a negative remainder", () => {
    const r = find({ attemptBudgetSecs: 900, attemptSpentSecs: 940 }, "budget")!;
    expect(r.text).toBe("spent 940s of 900s — OVER by 40s");
    expect(r.warn).toBe(true);
  });

  it("candidates_unaffordable names the tunable to raise, and warns", () => {
    const r = find({ candidatesUnaffordable: 1, candidatesDrafted: 3 }, "unaffordable")!;
    expect(r.text).toContain("1 of 3 drafted candidates");
    expect(r.text).toContain("attempt_budget_secs");
    expect(r.warn).toBe(true);
  });

  it("counts how many candidates cleared the floor — not just the winner's score", () => {
    const r = find(
      {
        confidenceFloor: 0.6,
        reviews: [
          { candidate: 1, confidence: 0.82, reviewed: true },
          { candidate: 2, confidence: 0.4, reviewed: true },
          { candidate: 3, confidence: 0, reviewed: false },
        ],
      },
      "reviews",
    )!;
    expect(r.text).toContain("1 of 3 at or above the 60% floor");
    // A lost review call is NOT a zero verdict, and the line must say so.
    expect(r.text).toContain("NO REVIEW, not a zero verdict");
    expect(r.warn).toBe(true);
  });

  it("a review call that never returned is excluded from 'cleared', never counted as passing", () => {
    const r = find(
      {
        confidenceFloor: 0,
        reviews: [{ candidate: 1, confidence: 0, reviewed: false }],
      },
      "reviews",
    )!;
    // confidence 0 >= floor 0 would otherwise read as "cleared".
    expect(r.text).toContain("0 of 1 at or above the 0% floor");
  });

  it("surfaces the slowest cargo stage and any the budget cut off", () => {
    const r = find(
      {
        stages: [
          { candidate: 1, stage: "cargo_check", secs: 41, ok: true, cutOff: false },
          { candidate: 1, stage: "cargo_test", secs: 233, ok: true, cutOff: false },
          { candidate: 2, stage: "cargo_test", secs: 180, ok: false, cutOff: true },
        ],
      },
      "stages",
    )!;
    expect(r.text).toContain("3 ran, 454s total"); // 41 + 233 + 180
    expect(r.text).toContain("slowest cargo_test 233s (candidate 1)");
    expect(r.text).toContain("1 CUT OFF by the budget");
    expect(r.warn).toBe(true);
  });
});

describe("calibration reaches state and pixels", () => {
  const proposalFrame = (extra: Record<string, unknown> = {}) => ({
    ts: 1_760_000_000,
    files: ["src/heal.rs"],
    validated: true,
    confidence: 0.82,
    confidence_floor: 0.6,
    subsystem: "heal",
    signature: "burst",
    responsiveness: "DIRECT",
    calibration: WIRE_CALIBRATION,
    ...extra,
  });

  it("heal.proposal keeps the calibration payload in state", () => {
    const s = apply(initialState(), "heal.proposal", proposalFrame());
    expect(s.healProposal?.calibration).not.toBeNull();
    expect(s.healProposal?.calibration?.candidatesUnaffordable).toBe(1);
    expect(s.healProposal?.calibration?.stages).toHaveLength(3);
  });

  it("heal.rejected keeps it too — the budget/floor half", () => {
    const s = apply(initialState(), "heal.rejected", {
      ts: 1_760_000_000,
      stage: "deadline",
      calibration: WIRE_CALIBRATION,
    });
    expect(s.healAlert?.kind).toBe("rejected");
    expect(s.healAlert?.calibration?.attemptBudgetSecs).toBe(900);
  });

  it("SelfHealPanel RENDERS the calibration beside ACCEPT & APPLY", () => {
    const s = apply(initialState(), "heal.proposal", proposalFrame());
    const html = renderToStaticMarkup(
      createElement(SelfHealPanel, {
        diagnosing: s.healDiagnosing,
        proposal: s.healProposal,
        onDismiss: () => {},
      }),
    );
    // The block exists, in the panel that carries the apply button.
    expect(html).toContain("ACCEPT &amp; APPLY");
    expect(html).toContain("ATTEMPT CALIBRATION");
    // The three numbers the payload exists for.
    expect(html).toContain("289s left"); // budget margin, computed
    expect(html).toContain("1 of 3 drafted candidates"); // candidates_unaffordable
    expect(html).toContain("1 of 3 at or above the 60% floor"); // every score, not just the winner's
    // Per-candidate + per-stage detail.
    expect(html).toContain("no review returned");
    expect(html).toContain("cut off");
  });

  it("SelfHealPanel renders NO calibration block for an older daemon", () => {
    const s = apply(initialState(), "heal.proposal", proposalFrame({ calibration: undefined }));
    const html = renderToStaticMarkup(
      createElement(SelfHealPanel, {
        diagnosing: s.healDiagnosing,
        proposal: s.healProposal,
        onDismiss: () => {},
      }),
    );
    expect(html).toContain("ACCEPT &amp; APPLY");
    expect(html).not.toContain("ATTEMPT CALIBRATION");
  });

  it("AlertPanel RENDERS it on a rejected banner", () => {
    const s = apply(initialState(), "heal.rejected", {
      ts: 1_760_000_000,
      stage: "deadline",
      calibration: WIRE_CALIBRATION,
    });
    const html = renderToStaticMarkup(
      createElement(AlertPanel, { alert: s.healAlert, onDismiss: () => {} }),
    );
    expect(html).toContain("SELF-HEAL PATCH REJECTED");
    expect(html).toContain("ATTEMPT BUDGET");
    expect(html).toContain("289s left");
    expect(html).toContain("attempt_budget_secs");
  });

  it("AlertPanel renders no calibration for blocked (the pipeline never ran)", () => {
    const s = apply(initialState(), "heal.blocked", { reason: "no_api_key" });
    const html = renderToStaticMarkup(
      createElement(AlertPanel, { alert: s.healAlert, onDismiss: () => {} }),
    );
    expect(html).toContain("SELF-HEAL BLOCKED");
    expect(html).not.toContain("ATTEMPT BUDGET");
  });
});

/**
 * SECOND DEAD FIELD ON THE SAME BANNER: `HealAlert.refTs`.
 *
 * The daemon emits `ts` on heal.rejected / heal.applied; the reducer has always
 * parsed it into `refTs`; and no component read it — grep `refTs` in hud/src and
 * only SelfHealPanel's PROPOSAL use comes back. It is the directory name
 * heal.rs::record_artifact wrote patch.diff and report.md under
 * (state/heal/rejected/<ts>/), while rejectionDetail's sentence names only the
 * parent directory. Same class as `calibration`: computed, shipped, typed, and
 * unreadable.
 */
describe("the alert's attempt id reaches pixels", () => {
  const render = (s: HudState) =>
    renderToStaticMarkup(
      createElement(AlertPanel, { alert: s.healAlert, onDismiss: () => {} }),
    );

  it("AlertPanel RENDERS the rejected attempt id AND the directory to open", () => {
    const s = apply(initialState(), "heal.rejected", {
      ts: 1_760_000_000,
      stage: "deadline",
      calibration: WIRE_CALIBRATION,
    });
    expect(s.healAlert?.refTs).toBe(1_760_000_000);
    const html = render(s);
    expect(html).toContain("ATTEMPT 1760000000");
    // The exact subdirectory, not just the parent rejectionDetail already names.
    expect(html).toContain("state/heal/rejected/1760000000/");
  });

  it("names the applied attempt too, without inventing a rejected artifact path", () => {
    const s = apply(initialState(), "heal.applied", { ts: 1_760_000_000 });
    const html = render(s);
    expect(html).toContain("ATTEMPT 1760000000");
    expect(html).not.toContain("state/heal/rejected/");
  });

  it("blocked names no attempt — the pipeline never ran, so there is none", () => {
    const s = apply(initialState(), "heal.blocked", { reason: "no_api_key" });
    expect(s.healAlert?.refTs).toBeNull();
    expect(render(s)).not.toContain("ATTEMPT ");
  });
});
