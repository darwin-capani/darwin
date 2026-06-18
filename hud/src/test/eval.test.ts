import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import EvalPanel from "../components/EvalPanel";
import {
  parseEvalReport,
  parseOptimizerProposal,
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

/** A realistic MEASURED eval.report payload — modelled EXACTLY on
 *  daemon/src/eval.rs::report_snapshot: latency + cost are "measured" with real
 *  per-stage p50/p95 + token sums, accuracy carries numeric rates, the optimizer
 *  is OFF + propose-only. */
const measuredReport: Record<string, unknown> = {
  latency: {
    status: "measured",
    n: 40,
    total_p50_ms: 820,
    total_p95_ms: 1650,
    queue_p50_ms: 12,
    queue_p95_ms: 30,
    stt_p50_ms: 210,
    stt_p95_ms: 480,
    classify_p50_ms: 95,
    classify_p95_ms: 180,
    route_p50_ms: 450,
    route_p95_ms: 1100,
  },
  cost: {
    status: "measured",
    n: 18,
    input_tokens: 124000,
    output_tokens: 36000,
    cache_read_tokens: 50000,
    total_tokens: 210000,
    est_cost_usd: 0.9024,
    cost_is_estimate: true,
  },
  accuracy: {
    routing_accuracy: 0.873,
    held_out_n: 64,
    correction_rate: 0.042,
    corrections: 3,
    usable_n: 72,
  },
  optimizer: { enabled: false, mode: "propose", posture: "propose-only" },
  runtime_gated: ["latency", "cost"],
};

/** A fresh-daemon eval.report: latency + cost windows empty ("awaiting turns"),
 *  accuracy corpus empty too. The honest all-awaiting snapshot. */
const awaitingReport: Record<string, unknown> = {
  latency: { status: "awaiting turns", n: 0 },
  cost: { status: "awaiting turns", n: 0 },
  accuracy: {
    routing_accuracy: "awaiting turns",
    held_out_n: 0,
    correction_rate: "awaiting turns",
    corrections: 0,
    usable_n: 0,
  },
  optimizer: { enabled: false, mode: "propose", posture: "propose-only" },
  runtime_gated: ["latency", "cost"],
};

/* ------------------------------------------------------------------------ *
 * parseEvalReport — defensive, AGGREGATE-ONLY, never null, never PII.        *
 * ------------------------------------------------------------------------ */
describe("parseEvalReport (defensive)", () => {
  it("parses a fully-MEASURED report", () => {
    const r = parseEvalReport(measuredReport);
    expect(r.latency.measured).toBe(true);
    expect(r.latency.n).toBe(40);
    expect(r.latency.totalP50Ms).toBe(820);
    expect(r.latency.totalP95Ms).toBe(1650);
    expect(r.latency.sttP50Ms).toBe(210);
    expect(r.latency.classifyP50Ms).toBe(95);
    expect(r.latency.routeP50Ms).toBe(450);
    expect(r.latency.queueP50Ms).toBe(12);

    expect(r.cost.measured).toBe(true);
    expect(r.cost.inputTokens).toBe(124000);
    expect(r.cost.outputTokens).toBe(36000);
    expect(r.cost.cacheReadTokens).toBe(50000);
    expect(r.cost.totalTokens).toBe(210000);
    expect(r.cost.estCostUsd).toBeCloseTo(0.9024, 4);
    expect(r.cost.costIsEstimate).toBe(true);

    expect(r.accuracy.routingAccuracy).toBeCloseTo(0.873, 3);
    expect(r.accuracy.heldOutN).toBe(64);
    expect(r.accuracy.correctionRate).toBeCloseTo(0.042, 3);
    expect(r.accuracy.corrections).toBe(3);
    expect(r.accuracy.usableN).toBe(72);

    expect(r.optimizer.enabled).toBe(false);
    expect(r.optimizer.mode).toBe("propose");
    expect(r.optimizer.posture).toBe("propose-only");
    expect(r.runtimeGated).toEqual(["latency", "cost"]);
  });

  it("maps an 'awaiting turns' window to NOT measured / null rates (never a fake 0)", () => {
    const r = parseEvalReport(awaitingReport);
    expect(r.latency.measured).toBe(false);
    expect(r.latency.n).toBe(0);
    expect(r.cost.measured).toBe(false);
    // The literal "awaiting turns" string for a rate becomes null — never a number.
    expect(r.accuracy.routingAccuracy).toBeNull();
    expect(r.accuracy.correctionRate).toBeNull();
    expect(r.accuracy.heldOutN).toBe(0);
    expect(r.accuracy.usableN).toBe(0);
  });

  it("NEVER returns null and tolerates a garbled/partial payload", () => {
    const r = parseEvalReport({});
    expect(r.latency.measured).toBe(false);
    expect(r.cost.measured).toBe(false);
    expect(r.accuracy.routingAccuracy).toBeNull();
    expect(r.accuracy.correctionRate).toBeNull();
    // Optimizer defaults to the honest OFF + propose-only posture.
    expect(r.optimizer.enabled).toBe(false);
    expect(r.optimizer.posture).toBe("propose-only");
    expect(r.runtimeGated).toEqual([]);
  });

  it("does not read a 'measured' status with n=0 as measured (status + n both required)", () => {
    const r = parseEvalReport({ latency: { status: "measured", n: 0 } });
    expect(r.latency.measured).toBe(false);
  });

  it("clamps an out-of-range rate to null and floors/clamps negative counts to 0", () => {
    const r = parseEvalReport({
      accuracy: {
        routing_accuracy: 1.4, // out of [0,1] -> null
        correction_rate: -0.1, // out of [0,1] -> null
        held_out_n: -5, // negative -> 0
        corrections: 2.9, // floored -> 2
        usable_n: 10,
      },
    });
    expect(r.accuracy.routingAccuracy).toBeNull();
    expect(r.accuracy.correctionRate).toBeNull();
    expect(r.accuracy.heldOutN).toBe(0);
    expect(r.accuracy.corrections).toBe(2);
    expect(r.accuracy.usableN).toBe(10);
  });

  it("defaults cost_is_estimate to true (a cost figure is ALWAYS an estimate)", () => {
    const r = parseEvalReport({ cost: { status: "measured", n: 1, total_tokens: 5 } });
    expect(r.cost.costIsEstimate).toBe(true);
  });

  it("is AGGREGATE-ONLY — a hostile extra field (an utterance) never survives", () => {
    const r = parseEvalReport({
      latency: { status: "measured", n: 1, total_p50_ms: 10, utterance: "my SECRET password" },
      cost: { status: "measured", n: 1, account: "leak@example.com" },
      accuracy: { routing_accuracy: 0.5, transcript: "PII text" },
    });
    const blob = JSON.stringify(r);
    expect(blob).not.toContain("SECRET");
    expect(blob).not.toContain("leak@example.com");
    expect(blob).not.toContain("PII");
    expect(blob).not.toContain("utterance");
  });
});

/* ------------------------------------------------------------------------ *
 * parseOptimizerProposal — SECRET-FREE, requires ts + a measured improvement.*
 * ------------------------------------------------------------------------ */
describe("parseOptimizerProposal (defensive)", () => {
  it("parses a well-formed optimize.proposed payload", () => {
    const p = parseOptimizerProposal({
      ts: 1718541234,
      improvement: 0.031,
      baseline_accuracy: 0.842,
      candidate_accuracy: 0.873,
      changes: 2,
      mode: "propose",
    });
    expect(p).not.toBeNull();
    expect(p!.ts).toBe(1718541234);
    expect(p!.improvement).toBeCloseTo(0.031, 3);
    expect(p!.baselineAccuracy).toBeCloseTo(0.842, 3);
    expect(p!.candidateAccuracy).toBeCloseTo(0.873, 3);
    expect(p!.changes).toBe(2);
  });

  it("returns null without a ts (no apply target) or without a measured improvement", () => {
    expect(parseOptimizerProposal({ improvement: 0.03 })).toBeNull();
    expect(parseOptimizerProposal({ ts: 123 })).toBeNull();
    expect(parseOptimizerProposal({})).toBeNull();
  });

  it("leaves baseline/candidate null when absent; never throws on junk", () => {
    const p = parseOptimizerProposal({ ts: 9, improvement: 0.01 });
    expect(p).not.toBeNull();
    expect(p!.baselineAccuracy).toBeNull();
    expect(p!.candidateAccuracy).toBeNull();
    expect(p!.changes).toBe(0);
  });
});

/* ------------------------------------------------------------------------ *
 * Reducer — eval.report / optimize.proposed / optimize.none|suppressed.      *
 * ------------------------------------------------------------------------ */
describe("reducer: eval + optimizer", () => {
  it("starts with no eval report and no pending proposal", () => {
    const s = initialState();
    expect(s.evalReport).toBeNull();
    expect(s.optimizerProposal).toBeNull();
  });

  it("folds an eval.report into state", () => {
    const s = tel(connected(), env("eval.report", measuredReport));
    expect(s.evalReport).not.toBeNull();
    expect(s.evalReport!.latency.measured).toBe(true);
    expect(s.evalReport!.cost.totalTokens).toBe(210000);
    expect(s.evalReport!.accuracy.routingAccuracy).toBeCloseTo(0.873, 3);
    expect(s.evalReport!.optimizer.enabled).toBe(false);
  });

  it("a later report REPLACES the prior snapshot (rolling, not stale)", () => {
    let s = tel(connected(), env("eval.report", awaitingReport));
    expect(s.evalReport!.latency.measured).toBe(false);
    s = tel(s, env("eval.report", measuredReport));
    expect(s.evalReport!.latency.measured).toBe(true);
    expect(s.evalReport!.latency.totalP50Ms).toBe(820);
  });

  it("folds an optimize.proposed into a pending proposal", () => {
    const s = tel(
      connected(),
      env("optimize.proposed", {
        ts: 1718500000,
        improvement: 0.025,
        baseline_accuracy: 0.84,
        candidate_accuracy: 0.865,
        changes: 1,
      }),
    );
    expect(s.optimizerProposal).not.toBeNull();
    expect(s.optimizerProposal!.ts).toBe(1718500000);
    expect(s.optimizerProposal!.improvement).toBeCloseTo(0.025, 3);
  });

  it("a malformed optimize.proposed (no ts) is ignored — no phantom card", () => {
    const s = tel(connected(), env("optimize.proposed", { improvement: 0.02 }));
    expect(s.optimizerProposal).toBeNull();
  });

  it("optimize.none clears a pending proposal (can't-make-worse round)", () => {
    let s = tel(connected(), env("optimize.proposed", { ts: 1, improvement: 0.05 }));
    expect(s.optimizerProposal).not.toBeNull();
    s = tel(s, env("optimize.none", { ts: 2 }));
    expect(s.optimizerProposal).toBeNull();
  });

  it("optimize.suppressed clears a pending proposal (master switch off)", () => {
    let s = tel(connected(), env("optimize.proposed", { ts: 1, improvement: 0.05 }));
    s = tel(s, env("optimize.suppressed", { reason: "optimize.enabled = false" }));
    expect(s.optimizerProposal).toBeNull();
  });

  it("optimize.none with no pending proposal returns the SAME reference (no churn)", () => {
    const s0 = connected();
    const s1 = tel(s0, env("optimize.none", { ts: 1 }));
    expect(s1.optimizerProposal).toBeNull();
    // same-reference bail-out for the proposal-free no-op path
    expect(s1).toBe(s0);
  });
});

/* ------------------------------------------------------------------------ *
 * Panel render — measured / awaiting-turns / proposal, honest copy.          *
 * ------------------------------------------------------------------------ */
describe("EvalPanel render", () => {
  function html(report: HudState["evalReport"], proposal: HudState["optimizerProposal"] = null) {
    return renderToStaticMarkup(createElement(EvalPanel, { report, proposal }));
  }

  it("renders nothing before the first report (null)", () => {
    expect(html(null)).toBe("");
  });

  it("renders MEASURED latency p50/p95, token sums + est $, and accuracy rates", () => {
    const out = html(parseEvalReport(measuredReport));
    // Latency p50/p95 (measured)
    expect(out).toContain("820 ms");
    expect(out).toContain("1650 ms");
    expect(out).toContain("TOTAL p95");
    // Cost: rolling tokens + a labelled ESTIMATE dollar figure
    expect(out).toContain("210,000");
    expect(out).toContain("~$0.9024");
    expect(out).toContain("EST.");
    expect(out.toLowerCase()).toContain("estimate");
    // Accuracy: routing score + correction rate as percentages
    expect(out).toContain("87.3%");
    expect(out).toContain("4.2%");
    expect(out).toContain("held-out n=64");
  });

  it("shows OFF + PROPOSE-ONLY optimizer status with honest copy", () => {
    const out = html(parseEvalReport(measuredReport));
    expect(out).toContain("OFF");
    expect(out.toUpperCase()).toContain("PROPOSE-ONLY");
    // Honest: it never auto-tunes
    expect(out.toLowerCase()).toContain("never");
  });

  it("shows AWAITING TURNS for an empty latency/cost window — never a fake number", () => {
    const out = html(parseEvalReport(awaitingReport));
    expect(out).toContain("AWAITING TURNS");
    // No fabricated measurement leaks through.
    expect(out).not.toContain(" ms");
    // The accuracy rates also read awaiting, not 0%.
    expect(out).not.toContain("0.0%");
  });

  it("renders a pending proposal READ-ONLY with the MANUAL apply command (no auto-apply)", () => {
    const out = html(
      parseEvalReport(measuredReport),
      parseOptimizerProposal({
        ts: 1718541234,
        improvement: 0.031,
        baseline_accuracy: 0.842,
        candidate_accuracy: 0.873,
        changes: 2,
      }),
    );
    expect(out).toContain("PROPOSAL");
    expect(out).toContain("+3.1%");
    expect(out).toContain("scripts/apply_optimization.sh 1718541234");
    expect(out).toContain("84.2%");
    expect(out).toContain("87.3%");
    // Review-only: nothing changed in live routing.
    expect(out.toLowerCase()).toContain("review-only");
    expect(out.toLowerCase()).toContain("nothing changed in live routing");
  });

  it("with no proposal, the optimizer note explains the propose-only posture", () => {
    const out = html(parseEvalReport(measuredReport), null);
    expect(out).not.toContain("apply_optimization.sh");
    expect(out.toLowerCase()).toContain("propose-only");
  });
});
