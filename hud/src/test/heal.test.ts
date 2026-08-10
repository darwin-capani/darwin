import { describe, expect, it } from "vitest";
import {
  applyReduce,
  confidenceNote,
  confidencePct,
  confirmReady,
  initialApplyState,
  litSegments,
  REARM_MS,
  reviewVerdict,
  stageLabel,
  type ApplyState,
} from "../core/heal";

/* The confidence-gauge math (pre-existing helpers) — kept covered here. */
describe("confidence gauge math", () => {
  it("maps confidence 0..1 to lit segments, clamped", () => {
    expect(litSegments(0)).toBe(0);
    expect(litSegments(1)).toBe(10);
    expect(litSegments(0.55)).toBe(6); // round
    expect(litSegments(-3)).toBe(0); // clamp low
    expect(litSegments(9)).toBe(10); // clamp high
    expect(litSegments(NaN)).toBe(0); // defensive
  });

  it("confidencePct clamps to 0..100", () => {
    expect(confidencePct(0)).toBe(0);
    expect(confidencePct(1)).toBe(100);
    expect(confidencePct(0.5)).toBe(50);
    expect(confidencePct(2)).toBe(100);
    expect(confidencePct(NaN)).toBe(0);
  });
});

/* ------------------------------------------------------------------------ *
 * The two-step-confirm gate. The whole safety point is that the apply spawn
 * is reachable ONLY via accept -> (wait >= REARM_MS) -> confirm, and a fast
 * double-click cannot blow through the confirm.
 * ------------------------------------------------------------------------ */
describe("Accept two-step confirm gate", () => {
  it("starts idle", () => {
    const s = initialApplyState();
    expect(s.phase).toBe("idle");
    expect(s.armedAt).toBeNull();
  });

  it("accept arms the confirm step (idle -> confirming)", () => {
    const s = applyReduce(initialApplyState(), { type: "accept", at: 1000 });
    expect(s.phase).toBe("confirming");
    expect(s.armedAt).toBe(1000);
  });

  it("a confirm fired within the re-arm window is IGNORED (no skip)", () => {
    let s = applyReduce(initialApplyState(), { type: "accept", at: 1000 });
    // a double-click: confirm only 50ms after the accept (< REARM_MS)
    s = applyReduce(s, { type: "confirm", at: 1000 + 50 });
    expect(s.phase).toBe("confirming"); // still waiting — NOT applying
  });

  it("confirm exactly at the re-arm boundary is honored (confirming -> applying)", () => {
    let s = applyReduce(initialApplyState(), { type: "accept", at: 1000 });
    s = applyReduce(s, { type: "confirm", at: 1000 + REARM_MS });
    expect(s.phase).toBe("applying");
    expect(s.armedAt).toBeNull();
  });

  it("confirmReady mirrors the reducer guard", () => {
    const armed: ApplyState = {
      ...initialApplyState(),
      phase: "confirming",
      armedAt: 1000,
    };
    expect(confirmReady(armed, 1000 + REARM_MS - 1)).toBe(false);
    expect(confirmReady(armed, 1000 + REARM_MS)).toBe(true);
    // not confirming -> never ready
    expect(confirmReady(initialApplyState(), 99999)).toBe(false);
    // confirming but unarmed (defensive) -> never ready
    expect(confirmReady({ ...armed, armedAt: null }, 99999)).toBe(false);
  });

  it("confirm from idle (no prior accept) does nothing", () => {
    const s = applyReduce(initialApplyState(), { type: "confirm", at: 5000 });
    expect(s.phase).toBe("idle");
  });

  it("a stray accept while applying is ignored (cannot re-arm mid-apply)", () => {
    let s = applyReduce(initialApplyState(), { type: "accept", at: 1000 });
    s = applyReduce(s, { type: "confirm", at: 1000 + REARM_MS });
    expect(s.phase).toBe("applying");
    const s2 = applyReduce(s, { type: "accept", at: 9000 });
    expect(s2).toBe(s); // unchanged
  });
});

/* ------------------------------------------------------------------------ *
 * The apply lifecycle: idle -> confirming -> applying -> applied | failed.
 * ------------------------------------------------------------------------ */
describe("apply lifecycle", () => {
  function toApplying(): ApplyState {
    let s = applyReduce(initialApplyState(), { type: "accept", at: 0 });
    s = applyReduce(s, { type: "confirm", at: REARM_MS });
    return s;
  }

  it("stage updates only land while applying", () => {
    let s = toApplying();
    s = applyReduce(s, { type: "applyStage", stage: "revalidating" });
    expect(s.stage).toBe("revalidating");
    s = applyReduce(s, { type: "applyStage", stage: "rebuilding" });
    expect(s.stage).toBe("rebuilding");

    // a stage update arriving in idle is ignored
    const idle = applyReduce(initialApplyState(), {
      type: "applyStage",
      stage: "applying",
    });
    expect(idle.phase).toBe("idle");
    expect(idle.stage).toBe("");
  });

  it("applyOk -> applied with restart-aware message", () => {
    const s = applyReduce(toApplying(), {
      type: "applyOk",
      restarted: true,
      message: "Healed. DARWIN restarted on the new build.",
    });
    expect(s.phase).toBe("applied");
    expect(s.restarted).toBe(true);
    expect(s.message).toMatch(/restarted/i);
  });

  it("applyFail -> failed and carries the reason", () => {
    const s = applyReduce(toApplying(), {
      type: "applyFail",
      message: "Validation/apply failed (revalidating). Patch NOT applied.",
    });
    expect(s.phase).toBe("failed");
    expect(s.message).toMatch(/NOT applied/i);
  });

  it("a terminal action cannot fire unless applying (no spurious success)", () => {
    const idleOk = applyReduce(initialApplyState(), {
      type: "applyOk",
      restarted: false,
      message: "x",
    });
    expect(idleOk.phase).toBe("idle");
    const confirming = applyReduce(initialApplyState(), { type: "accept", at: 0 });
    const okFromConfirming = applyReduce(confirming, {
      type: "applyOk",
      restarted: false,
      message: "x",
    });
    expect(okFromConfirming.phase).toBe("confirming"); // not jumped to applied
  });

  it("reset backs out of confirming and clears terminal states, but NOT mid-apply", () => {
    // confirming -> reset -> idle
    const confirming = applyReduce(initialApplyState(), { type: "accept", at: 0 });
    expect(applyReduce(confirming, { type: "reset" }).phase).toBe("idle");

    // applied -> reset -> idle
    const applied = applyReduce(
      applyReduce(initialApplyState(), { type: "accept", at: 0 }),
      { type: "confirm", at: REARM_MS },
    );
    const ok = applyReduce(applied, {
      type: "applyOk",
      restarted: false,
      message: "done",
    });
    expect(applyReduce(ok, { type: "reset" }).phase).toBe("idle");

    // applying -> reset is REFUSED (spawn in flight)
    const applying = applyReduce(
      applyReduce(initialApplyState(), { type: "accept", at: 0 }),
      { type: "confirm", at: REARM_MS },
    );
    expect(applyReduce(applying, { type: "reset" }).phase).toBe("applying");
  });
});

describe("stage labels", () => {
  it("maps script stage tokens to human spinner text", () => {
    // THE LABEL MUST NAME THE GATES THAT RUN. It said "cargo check + full
    // test" long after the script grew clippy -D warnings, the mutation probe
    // and the review-confidence floor — understating what the operator's click
    // was doing by three gates. Assert each of them by name.
    const revalidating = stageLabel("revalidating");
    for (const gate of ["clippy", "test", "mutation", "review floor"]) {
      expect(revalidating.toLowerCase()).toContain(gate);
    }
    expect(stageLabel("applying")).toBe("Applying…");
    expect(stageLabel("rebuilding")).toBe("Rebuilding…");
    expect(stageLabel("")).toBe("Starting…");
    expect(stageLabel("starting…")).toBe("Starting…");
    // unknown token still renders something sensible
    expect(stageLabel("whatever")).toBe("whatever…");
  });
});

/* ------------------------------------------------------------------------ *
 * The review score against its FLOOR, and the reviewer's own sentence.
 *
 * The gauge used to render a bare percentage: 8% and 88% looked like the same
 * kind of fact next to an ACCEPT & APPLY button, and the adversarial review is
 * the ONLY stage in the pipeline that judges whether the patch is a good idea
 * (every staged gate is mechanical and blind to the diagnosis).
 * ------------------------------------------------------------------------ */
describe("review confidence against the daemon's floor", () => {
  it("calls out a score BELOW the floor and names the bar", () => {
    const note = confidenceNote(0.1, 0.25);
    expect(note.belowFloor).toBe(true);
    expect(note.text).toContain("10%");
    expect(note.text).toContain("25%");
    expect(note.text.toUpperCase()).toContain("BELOW");
  });

  it("a score at or above the floor is not a warning", () => {
    expect(confidenceNote(0.82, 0.25).belowFloor).toBe(false);
    // INCLUSIVE AT THE BAR, exactly like heal::meets_confidence_floor
    // (`confidence >= CONFIDENCE_FLOOR`). An exclusive comparison here would
    // paint the one score the daemon deliberately allows as a failure.
    expect(confidenceNote(0.25, 0.25).belowFloor).toBe(false);
    expect(confidenceNote(0.82, 0.25).text).toContain("floor 25%");
  });

  it("NEVER invents a floor the daemon did not send", () => {
    // An older daemon sends no confidence_floor. Rendering a hard-coded one
    // here would be a second copy of a threshold — the drift shape this whole
    // change exists to avoid.
    const note = confidenceNote(0.05, null);
    expect(note.belowFloor).toBe(false);
    expect(note.text).toContain("5%");
    expect(note.text).toMatch(/not reported/i);
    expect(confidenceNote(null, 0.25).text).toMatch(/no score/i);
  });
});

describe("the reviewer's verdict, extracted from report.md", () => {
  const report = [
    "# Self-heal proposal — 1765432100",
    "",
    "- review confidence: 0.82 (floor 0.25 — cleared)",
    "",
    "## Adversarial review verdict",
    "",
    "Fixes the root cause; the guard is in the wrong layer but harmless.",
    "",
    "## Validation output (tail)",
    "",
    "```",
    "$ cargo test",
    "```",
  ].join("\n");

  it("returns the verdict section, and NOTHING after it", () => {
    const v = reviewVerdict(report);
    expect(v).toBe("Fixes the root cause; the guard is in the wrong layer but harmless.");
    // A window bounded only at its head runs on into the validation tail and
    // the fenced diff below it — the too-wide-window trap.
    expect(v).not.toContain("cargo test");
  });

  it("returns empty for a report with no verdict section", () => {
    expect(reviewVerdict("# Self-heal REJECTED — 1\n\nno candidate passed")).toBe("");
    expect(reviewVerdict("")).toBe("");
  });
});
