/**
 * Pure self-heal presentation helpers — no DOM/React imports, so the
 * confidence-gauge math is verifiable headlessly under vitest (node env)
 * alongside the reducer. The SelfHealPanel component imports these.
 */

/** Number of segments in the review-confidence gauge. */
export const CONFIDENCE_SEGMENTS = 10;

/** Map a review confidence (0..1) to the count of lit gauge segments. Clamps
 *  defensively so a stray >1 or <0 (or NaN) never overruns the bar. */
export function litSegments(
  confidence: number,
  segments: number = CONFIDENCE_SEGMENTS,
): number {
  if (!Number.isFinite(confidence)) return 0;
  const c = Math.max(0, Math.min(1, confidence));
  return Math.round(c * segments);
}

/** Confidence as a 0..100 integer percent (clamped). */
export function confidencePct(confidence: number): number {
  if (!Number.isFinite(confidence)) return 0;
  return Math.round(Math.max(0, Math.min(1, confidence)) * 100);
}

/* ------------------------------------------------------------------------ *
 * Accept-and-apply lifecycle — a PURE state machine, separated from the React
 * component so the two-step-confirm guard and the apply lifecycle are testable
 * headlessly under vitest (node env), exactly like the reducer in state.ts.
 *
 * SAFETY: the only transition that should ultimately spawn the gated apply
 * script is confirming -> applying, and it must be reachable ONLY through the
 * `confirm` action AFTER an `accept` (two distinct clicks). `accept` arms a
 * confirm gate; a re-arm guard (REARM_MS) means a `confirm` fired too soon
 * after the `accept` is IGNORED, so a double-click cannot skip the confirm.
 * ------------------------------------------------------------------------ */

/** How long after the first (accept) click the confirm click is ignored, so a
 *  fast double-click cannot blow through the two-step gate. */
export const REARM_MS = 400;

/** Discrete UI phases of the Accept button / apply flow. */
export type ApplyPhase = "idle" | "confirming" | "applying" | "applied" | "failed";

/** The apply lifecycle state. `armedAt` is the timestamp of the `accept` click
 *  (the start of the re-arm window); `stage` is the latest script stage label
 *  while applying; `message` is the terminal success/failure text. */
export interface ApplyState {
  phase: ApplyPhase;
  /** ms timestamp of the accept click, or null outside `confirming`. */
  armedAt: number | null;
  /** Stage label shown during `applying` (and carried to the terminal text). */
  stage: string;
  /** Terminal human message for `applied` / `failed`. */
  message: string;
  /** True on the applied path when the daemon was auto-restarted. */
  restarted: boolean;
}

export type ApplyAction =
  /** First click: arm the confirm gate. `at` is the click timestamp. */
  | { type: "accept"; at: number }
  /** Second click: only honored when confirming AND past the re-arm window. */
  | { type: "confirm"; at: number }
  /** The spawn began (commit to the applying phase). */
  | { type: "applyStart" }
  /** A progress stage label arrived from the script. */
  | { type: "applyStage"; stage: string }
  /** Terminal success from heal_apply. */
  | { type: "applyOk"; restarted: boolean; message: string }
  /** Terminal failure (gate failed / script error). Patch NOT applied. */
  | { type: "applyFail"; message: string }
  /** Back out of the confirm step (or reset after terminal) to idle. */
  | { type: "reset" };

export function initialApplyState(): ApplyState {
  return { phase: "idle", armedAt: null, stage: "", message: "", restarted: false };
}

/** Is the confirm click currently allowed? Only while `confirming` AND at least
 *  REARM_MS after the arming `accept` click. PURE — the guard the UI consults
 *  and the reducer enforces. */
export function confirmReady(state: ApplyState, now: number): boolean {
  return (
    state.phase === "confirming" &&
    state.armedAt !== null &&
    now - state.armedAt >= REARM_MS
  );
}

/** PURE apply-lifecycle reducer. Mirrors the reducer-style purity of state.ts so
 *  the two-step gate + lifecycle are unit-testable without a DOM. */
export function applyReduce(state: ApplyState, action: ApplyAction): ApplyState {
  switch (action.type) {
    case "accept": {
      // Arming the confirm gate is only meaningful from idle (or a re-armed
      // terminal state via reset->accept). A stray accept mid-apply is ignored.
      if (state.phase !== "idle") return state;
      return { ...initialApplyState(), phase: "confirming", armedAt: action.at };
    }
    case "confirm": {
      // The two-step gate + re-arm guard: ignore a confirm that is not ready
      // (wrong phase, or fired within REARM_MS of the accept click).
      if (!confirmReady(state, action.at)) return state;
      // Enter applying immediately so the button cannot be clicked a third time;
      // the caller then spawns heal_apply and feeds stage/terminal actions.
      return { ...state, phase: "applying", armedAt: null, stage: "starting…" };
    }
    case "applyStart": {
      if (state.phase !== "applying") return state;
      return { ...state, stage: state.stage || "starting…" };
    }
    case "applyStage": {
      if (state.phase !== "applying") return state;
      return { ...state, stage: action.stage };
    }
    case "applyOk": {
      // Only a flow that is applying can succeed.
      if (state.phase !== "applying") return state;
      return {
        ...state,
        phase: "applied",
        armedAt: null,
        message: action.message,
        restarted: action.restarted,
      };
    }
    case "applyFail": {
      if (state.phase !== "applying") return state;
      return { ...state, phase: "failed", armedAt: null, message: action.message };
    }
    case "reset": {
      // DISMISS / back-out: never allowed mid-apply (the spawn is in flight).
      if (state.phase === "applying") return state;
      if (state.phase === "idle") return state;
      return initialApplyState();
    }
    default:
      return state;
  }
}

/** Human stage label for the spinner from a raw script stage token. PURE. */
export function stageLabel(stage: string): string {
  switch (stage) {
    case "revalidating":
      return "Re-validating (cargo check + full test)…";
    case "applying":
      return "Applying…";
    case "rebuilding":
      return "Rebuilding…";
    case "starting…":
    case "":
      return "Starting…";
    default:
      return `${stage}…`;
  }
}
