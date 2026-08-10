import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import DistillPanel from "../components/DistillPanel";
import HandoffPanel from "../components/HandoffPanel";
import OvernightPanel from "../components/OvernightPanel";
import SyncPanel from "../components/SyncPanel";
import type {
  DistillStatus,
  HandoffStatus,
  OvernightStatus,
  SyncStatus,
} from "../core/events";
import { REARM_MS } from "../core/heal";
import {
  armedSwap,
  distillPromoteState,
  distillRollbackState,
  distillTrainState,
  handoffState,
  initialSwapState,
  normalizeOvernightTask,
  OVERNIGHT_TASK_MAX_CHARS,
  overnightQueueState,
  resumeState,
  swapConfirmPrompt,
  swapConfirmReady,
  swapReduce,
  syncNowState,
  type SwapState,
} from "../core/queueControls";

/**
 * THE FOUR PANELS THAT REPORTED ON QUEUES NOTHING COULD FILL.
 *
 * `overnight::enqueue`, `distill::distill_now`, `sync::sync_now` and
 * `handoff::build_capsule` each had exactly ONE caller — their own command arm —
 * so OvernightPanel's "N queued" counted a queue no shipped control could add
 * to, and DistillPanel's "ARMED · GATHERING" described a pipeline that could
 * never train.
 *
 * The HUD suite runs in the NODE environment: there is no DOM, so a component
 * cannot be mounted and a click cannot be dispatched. Everything that decides
 * whether a control may send lives in core/queueControls.ts and is tested here
 * directly; the rendered markup is checked with renderToStaticMarkup, which is
 * enough to prove the control EXISTS in the shipped panel and carries its honest
 * disabled reason.
 */

/* ------------------------------------------------------------- fixtures --- */

function overnight(over: Partial<OvernightStatus> = {}): OvernightStatus {
  return {
    enabled: true,
    cloudKeyPresent: true,
    depVerified: false,
    dependency: "an Anthropic API key",
    runsTools: false,
    queued: 0,
    done: 0,
    failed: 0,
    items: [],
    ...over,
  };
}

function distill(over: Partial<DistillStatus> = {}): DistillStatus {
  return {
    enabled: true,
    depVerified: false,
    dependency: "Apple Silicon + mlx-lm (verified only on-device)",
    examplesReady: 40,
    minExamples: 32,
    readyToTrain: true,
    gatedPromotion: true,
    adapterLive: false,
    adapterPointer: "none",
    lastRun: null,
    promoted: null,
    ...over,
  };
}

function sync(over: Partial<SyncStatus> = {}): SyncStatus {
  return {
    enabled: true,
    keyPresent: true,
    peerConfigured: true,
    transportInert: true,
    syncableFacts: 12,
    pendingConflicts: 0,
    deletesPropagate: false,
    ...over,
  };
}

function handoff(over: Partial<HandoffStatus> = {}): HandoffStatus {
  return {
    enabled: true,
    keyPresent: true,
    peerConfigured: true,
    transportInert: true,
    carriesCredentials: false,
    restoreParks: true,
    pendingCapsule: false,
    device: "studio",
    ...over,
  };
}

/* --------------------------------------------------- the overnight queue --- */

describe("overnight queue control", () => {
  it("is enabled with the shell, the switch on, and a task typed", () => {
    const v = overnightQueueState(overnight(), true, "look into the Kessler papers", false);
    expect(v.canSend).toBe(true);
  });

  it("refuses without the desktop shell (there is no command channel in a browser)", () => {
    const v = overnightQueueState(overnight(), false, "anything", false);
    expect(v.canSend).toBe(false);
    expect(v.reason).toMatch(/desktop app/i);
  });

  it("refuses while a request is already in flight", () => {
    expect(overnightQueueState(overnight(), true, "x", true).canSend).toBe(false);
  });

  it("refuses when [overnight].enabled is off, and names the switch", () => {
    const v = overnightQueueState(overnight({ enabled: false }), true, "x", false);
    expect(v.canSend).toBe(false);
    expect(v.reason).toMatch(/\[overnight\]\.enabled/);
  });

  it("refuses an empty or whitespace-only task", () => {
    expect(overnightQueueState(overnight(), true, "", false).canSend).toBe(false);
    expect(overnightQueueState(overnight(), true, "   \n\t ", false).canSend).toBe(false);
  });

  it("refuses before the daemon has reported any overnight status", () => {
    expect(overnightQueueState(null, true, "x", false).canSend).toBe(false);
  });

  /* THE ONE THAT IS EASY TO GET WRONG. `enqueue` still persists the task without
     a cloud key and answers with the honest "no key on file" line. Disabling the
     button would lose a real queue write and replace that sentence with a guess. */
  it("STILL SENDS with no cloud key — the queue write is real and the daemon is honest about the key", () => {
    const v = overnightQueueState(overnight({ cloudKeyPresent: false }), true, "x", false);
    expect(v.canSend).toBe(true);
  });

  it("normalizes a task: trims, and clamps to the daemon's own char limit", () => {
    expect(normalizeOvernightTask("  padded  ")).toBe("padded");
    expect(normalizeOvernightTask("   ")).toBe("");
    const huge = "x".repeat(OVERNIGHT_TASK_MAX_CHARS + 500);
    expect([...normalizeOvernightTask(huge)].length).toBe(OVERNIGHT_TASK_MAX_CHARS);
    // Char-boundary safe: an astral-plane task is clamped by code point, not by
    // UTF-16 unit, so it can never be cut into a lone surrogate.
    const astral = "\u{1F680}".repeat(OVERNIGHT_TASK_MAX_CHARS + 10);
    expect([...normalizeOvernightTask(astral)].length).toBe(OVERNIGHT_TASK_MAX_CHARS);
  });
});

/* ------------------------------------------- distill train / promote gate --- */

describe("distill controls", () => {
  it("TRAIN is enabled when the pipeline is on", () => {
    expect(distillTrainState(distill(), true, false).canSend).toBe(true);
  });

  it("TRAIN refuses with no shell, while busy, or with [distill].enabled off", () => {
    expect(distillTrainState(distill(), false, false).canSend).toBe(false);
    expect(distillTrainState(distill(), true, true).canSend).toBe(false);
    const off = distillTrainState(distill({ enabled: false }), true, false);
    expect(off.canSend).toBe(false);
    expect(off.reason).toMatch(/\[distill\]\.enabled/);
  });

  /* A thin dataset is refused daemon-side with its own honest count, and the
     example count in a status frame is a snapshot a fresh graded turn outdates.
     Disabling on it is how a control ends up dead on a live pipeline. */
  it("TRAIN still sends on a thin dataset — distill_now refuses with its own count", () => {
    const thin = distill({ examplesReady: 3, readyToTrain: false });
    expect(distillTrainState(thin, true, false).canSend).toBe(true);
  });

  it("PROMOTE requires [distill].enabled — an off pipeline must not install a brain", () => {
    expect(distillPromoteState(distill(), true, false).canSend).toBe(true);
    expect(distillPromoteState(distill({ enabled: false }), true, false).canSend).toBe(false);
  });

  /* ROLLBACK IS THE SAFE DIRECTION and must stay reachable. An operator who
     promoted an adapter and then switched [distill] off would otherwise be stuck
     on the personal model with no way back to base from the HUD. */
  it("ROLLBACK stays available even with [distill].enabled off — you can always get back to base", () => {
    expect(distillRollbackState(distill({ enabled: false }), true, false).canSend).toBe(true);
    expect(distillRollbackState(null, true, false).canSend).toBe(true);
    // ...but still not without a command channel, and not mid-flight.
    expect(distillRollbackState(distill(), false, false).canSend).toBe(false);
    expect(distillRollbackState(distill(), true, true).canSend).toBe(false);
  });
});

/* ------------------------------------------------- the model-swap gate ------ *
 * The whole safety point: a command send is reachable ONLY via arm -> (wait >=
 * REARM_MS) -> confirm, and a fast double-click cannot blow through the confirm.
 * Same contract as the self-heal ACCEPT & APPLY gate, and the same arithmetic —
 * this delegates to heal.ts::twoStepConfirmReady rather than copying it.
 * --------------------------------------------------------------------------- */

describe("distill promote/rollback two-step gate", () => {
  it("starts idle with nothing armed", () => {
    const s = initialSwapState();
    expect(s.phase).toBe("idle");
    expect(s.action).toBeNull();
    expect(swapConfirmReady(s, 1_000)).toBe(false);
    expect(armedSwap(s, 1_000)).toBeNull();
  });

  it("a confirm with nothing armed is ignored — no send is reachable from idle", () => {
    const s = swapReduce(initialSwapState(), { type: "confirm", at: 10_000 });
    expect(s.phase).toBe("idle");
  });

  it("A DOUBLE-CLICK CANNOT SKIP THE CONFIRM", () => {
    const armed = swapReduce(initialSwapState(), { type: "arm", action: "promote", at: 1_000 });
    expect(armed.phase).toBe("confirming");
    // The second half of a double-click lands well inside the re-arm window.
    const tooSoon = swapReduce(armed, { type: "confirm", at: 1_000 + REARM_MS - 1 });
    expect(tooSoon.phase).toBe("confirming");
    expect(swapConfirmReady(armed, 1_000 + REARM_MS - 1)).toBe(false);
    expect(armedSwap(armed, 1_000 + REARM_MS - 1)).toBeNull();
    // A deliberate second click, after the window, commits.
    expect(swapConfirmReady(armed, 1_000 + REARM_MS)).toBe(true);
    const running = swapReduce(armed, { type: "confirm", at: 1_000 + REARM_MS });
    expect(running.phase).toBe("running");
    expect(running.armedAt).toBeNull();
  });

  it("the gate is PER-ACTION: arming promote cannot commit a rollback", () => {
    const armed = swapReduce(initialSwapState(), { type: "arm", action: "promote", at: 0 });
    // A second arm for the OTHER action while confirming is ignored, so the
    // action under the confirm button can never be swapped out beneath it.
    const restamped = swapReduce(armed, { type: "arm", action: "rollback", at: REARM_MS * 3 });
    expect(restamped).toEqual(armed);
    expect(armedSwap(restamped, REARM_MS * 3)).toBe("promote");
  });

  it("a third click while running cannot double-send", () => {
    let s = swapReduce(initialSwapState(), { type: "arm", action: "rollback", at: 0 });
    s = swapReduce(s, { type: "confirm", at: REARM_MS });
    expect(s.phase).toBe("running");
    const again = swapReduce(s, { type: "confirm", at: REARM_MS * 4 });
    expect(again).toEqual(s);
    // ...and an arm mid-run is ignored too.
    expect(swapReduce(s, { type: "arm", action: "promote", at: REARM_MS * 4 })).toEqual(s);
  });

  it("only a running gate can settle or fail, and carries the daemon's own words", () => {
    const idle = initialSwapState();
    expect(swapReduce(idle, { type: "settle", message: "Promoted." })).toEqual(idle);
    expect(swapReduce(idle, { type: "fail", message: "boom" })).toEqual(idle);
    let s = swapReduce(idle, { type: "arm", action: "promote", at: 0 });
    s = swapReduce(s, { type: "confirm", at: REARM_MS });
    const done = swapReduce(s, { type: "settle", message: "I won't promote a phantom, sir." });
    expect(done.phase).toBe("done");
    expect(done.message).toBe("I won't promote a phantom, sir.");
  });

  it("reset backs out of the confirm and clears a terminal result, but never mid-run", () => {
    const armed = swapReduce(initialSwapState(), { type: "arm", action: "promote", at: 0 });
    expect(swapReduce(armed, { type: "reset" })).toEqual(initialSwapState());
    const running = swapReduce(armed, { type: "confirm", at: REARM_MS });
    expect(swapReduce(running, { type: "reset" })).toEqual(running);
    const failed = swapReduce(running, { type: "fail", message: "nope" });
    expect(swapReduce(failed, { type: "reset" })).toEqual(initialSwapState());
    // A reset from idle is a no-op (identity), not a churned new object.
    const idle = initialSwapState();
    expect(swapReduce(idle, { type: "reset" })).toBe(idle);
  });

  it("an unknown event leaves the gate untouched", () => {
    const s = initialSwapState();
    expect(swapReduce(s, { type: "nonsense" } as never)).toBe(s);
  });

  it("the confirm question names the consequence for each direction", () => {
    expect(swapConfirmPrompt("promote")).toMatch(/answer you/i);
    expect(swapConfirmPrompt("rollback")).toMatch(/base model/i);
  });

  /* THE GATE IS SHARED, NOT COPIED. A drifting second implementation is exactly
     how a two-step confirm quietly becomes one step, so pin that this gate
     honors the SAME window the self-heal panel does. */
  it("uses the self-heal REARM_MS window, not a private one", () => {
    const armed: SwapState = swapReduce(initialSwapState(), {
      type: "arm",
      action: "promote",
      at: 500,
    });
    expect(swapConfirmReady(armed, 500 + REARM_MS - 1)).toBe(false);
    expect(swapConfirmReady(armed, 500 + REARM_MS)).toBe(true);
  });
});

/* ----------------------------------------------- sync + handoff controls --- */

describe("sync and handoff controls", () => {
  it("SYNC NOW needs the shell and [sync].enabled", () => {
    expect(syncNowState(sync(), true, false).canSend).toBe(true);
    expect(syncNowState(sync(), false, false).canSend).toBe(false);
    expect(syncNowState(sync(), true, true).canSend).toBe(false);
    expect(syncNowState(null, true, false).canSend).toBe(false);
    const off = syncNowState(sync({ enabled: false }), true, false);
    expect(off.canSend).toBe(false);
    expect(off.reason).toMatch(/\[sync\]\.enabled/);
  });

  /* No pairing key does NOT disable it: sync_now refuses honestly rather than
     ever exporting in the clear, and that sentence is more useful than a grey
     button. */
  it("SYNC NOW still sends with no pairing key — sync_now refuses in its own words", () => {
    expect(syncNowState(sync({ keyPresent: false }), true, false).canSend).toBe(true);
  });

  /* THE CLAIM THAT WAS WRONG, PINNED SO IT CANNOT COME BACK.

     This control was wired with the note "the network transport is armed-but-
     inert, so nothing egresses on this click". `sync::sync_now` calls
     `transport_push` — a reqwest POST of the sealed bundle — whenever
     `[sync].peer_endpoint` is non-empty, and its only other caller is its own
     command arm, so this button is the first shipped client that can fire it.
     The daemon already derives `transport_inert = !peer_configured` because the
     pinned-true version asserted the facts stay home at the moment they leave.
     A tooltip promising inertness would reprint that defect at the click. */
  it("SYNC NOW names the network delivery when the daemon reports a configured peer", () => {
    const paired = syncNowState(sync({ peerConfigured: true }), true, false);
    expect(paired.canSend).toBe(true);
    expect(paired.reason).toMatch(/network/i);
    expect(paired.reason).toMatch(/peer_endpoint/);
    // ...and it must not ALSO claim the click stays local.
    expect(paired.reason).not.toMatch(/inert|nothing leaves/i);
  });

  it("SYNC NOW claims nothing leaves the Mac ONLY when no peer endpoint is set", () => {
    const solo = syncNowState(sync({ peerConfigured: false }), true, false);
    expect(solo.canSend).toBe(true);
    expect(solo.reason).toMatch(/nothing leaves this Mac/i);
    expect(solo.reason).not.toMatch(/network/i);
  });

  it("HAND OFF needs the shell and [handoff].enabled", () => {
    expect(handoffState(handoff(), true, false).canSend).toBe(true);
    expect(handoffState(handoff(), false, false).canSend).toBe(false);
    expect(handoffState(null, true, false).canSend).toBe(false);
    const off = handoffState(handoff({ enabled: false }), true, false);
    expect(off.canSend).toBe(false);
    expect(off.reason).toMatch(/\[handoff\]\.enabled/);
  });

  /* pendingCapsule is a HINT, not a gate — a capsule that lands between status
     frames would otherwise leave a correct click refused, and `resume` answers
     "No session capsule … to resume, sir." when there is nothing staged. */
  it("RESUME sends whether or not a capsule is reported staged, and says which", () => {
    const staged = resumeState(handoff({ pendingCapsule: true }), true, false);
    const empty = resumeState(handoff({ pendingCapsule: false }), true, false);
    expect(staged.canSend).toBe(true);
    expect(empty.canSend).toBe(true);
    expect(staged.reason).not.toBe(empty.reason);
    expect(staged.reason).toMatch(/parks/i);
    expect(empty.reason).toMatch(/parks/i);
  });

  /* ...AND IT MUST NOT PROMISE THE HALF THAT DOES NOT EXIST. `Command::Resume`
     reads `.note` off the first RestoredSession and drops the vector;
     `handoff::restore` is a pure fn and nothing outside handoff.rs's own tests
     consumes `RestoredSession.capsule`. So the capsule is opened, decrypted and
     version-checked — the transcript window, active agent, mission ref, world
     deltas, draft ids and focus profile are never re-entered. Building that leg
     is a daemon feature and the owner's call; a HUD tooltip claiming it already
     happened is this hunt's own defect, one layer down. */
  it("RESUME says OPEN AND VERIFY, never that a session was restored", () => {
    for (const pending of [true, false]) {
      const r = resumeState(handoff({ pendingCapsule: pending }), true, false);
      expect(r.reason).toMatch(/open/i);
      expect(r.reason).not.toMatch(/restore/i);
    }
    expect(resumeState(handoff({ pendingCapsule: true }), true, false).reason).toMatch(
      /not re-entered/i,
    );
  });

  it("RESUME still needs the shell and the master switch", () => {
    expect(resumeState(handoff(), false, false).canSend).toBe(false);
    expect(resumeState(handoff({ enabled: false }), true, false).canSend).toBe(false);
    expect(resumeState(null, true, false).canSend).toBe(false);
  });
});

/* ------------------------------------- the controls exist in the markup ---- */

/**
 * The panels are the SHIPPED surface, so it is not enough that the pure logic is
 * right: the button has to be in the rendered tree. Node env means no DOM and no
 * clicks — renderToStaticMarkup is exactly as far as this can go, and it is the
 * half that catches "the control was never rendered at all".
 *
 * `inTauri()` is false under vitest, so every control renders DISABLED here.
 * That is the honest browser-dev state, and asserting it pins that a HUD running
 * outside the desktop app cannot fire any of these.
 */
describe("the shipped panels render their controls", () => {
  it("OvernightPanel renders a task box and a QUEUE button", () => {
    const html = renderToStaticMarkup(
      createElement(OvernightPanel, { overnight: overnight({ queued: 2 }) }),
    );
    expect(html).toContain("QUEUE");
    expect(html).toContain('id="overnight-task"');
    // No shell under vitest -> the control is disabled, never silently live.
    expect(html).toMatch(/overnight-queue-btn[^>]*disabled/);
  });

  it("DistillPanel renders TRAIN + the two arming buttons, and NO bare commit", () => {
    const html = renderToStaticMarkup(createElement(DistillPanel, { distill: distill() }));
    expect(html).toContain("TRAIN + STAGE");
    expect(html).toContain("PROMOTE…");
    expect(html).toContain("ROLL BACK…");
    // THE POINT OF THE GATE: the committing control is NOT in the idle tree. It
    // exists only after an arming click puts the gate in `confirming`.
    expect(html).not.toContain("YES, SWAP THE MODEL");
    expect(html).not.toContain("distill-commit");
  });

  /* The rendered footnote is the other place the operator reads before pressing,
     and it shipped an UNCONDITIONAL "armed but inert" beside a button that fires
     the transport. Both pairing states are pinned: neither may borrow the
     other's sentence. */
  it("SyncPanel's footnote tells the truth about the transport in BOTH pairing states", () => {
    const paired = renderToStaticMarkup(
      createElement(SyncPanel, { sync: sync({ peerConfigured: true }) }),
    );
    expect(paired).toContain("over the network");
    expect(paired).not.toContain("armed but inert");
    const solo = renderToStaticMarkup(
      createElement(SyncPanel, { sync: sync({ peerConfigured: false }) }),
    );
    expect(solo).toContain("armed but inert");
    expect(solo).not.toContain("over the network");
  });

  it("SyncPanel renders SYNC NOW; HandoffPanel renders both continuity controls", () => {
    const s = renderToStaticMarkup(createElement(SyncPanel, { sync: sync() }));
    expect(s).toContain("SYNC NOW");
    const h = renderToStaticMarkup(createElement(HandoffPanel, { handoff: handoff() }));
    expect(h).toContain("HAND OFF THIS SESSION");
    expect(h).toContain("RESUME STAGED");
  });

  it("a null status still renders nothing at all — the panels stay absent pre-snapshot", () => {
    expect(renderToStaticMarkup(createElement(OvernightPanel, { overnight: null }))).toBe("");
    expect(renderToStaticMarkup(createElement(DistillPanel, { distill: null }))).toBe("");
    expect(renderToStaticMarkup(createElement(SyncPanel, { sync: null }))).toBe("");
    expect(renderToStaticMarkup(createElement(HandoffPanel, { handoff: null }))).toBe("");
  });
});
