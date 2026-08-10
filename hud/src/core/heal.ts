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

/** What the ACCEPT & APPLY surface says about a review score.
 *
 *  The gauge used to render a bare percentage with no bar beside it: 8% and 88%
 *  looked like the same kind of fact, and the reviewer is the ONLY stage in the
 *  whole pipeline that judges whether the patch is a good idea (every staged
 *  gate is mechanical and blind to the diagnosis). The floor is NOT hard-coded
 *  here — the daemon sends it on heal.proposal (`confidence_floor`), because two
 *  copies of a threshold drift. `floor === null` is an older daemon that does
 *  not send one; then this reports the score and says the bar is unknown rather
 *  than inventing one. */
export interface ConfidenceNote {
  text: string;
  /** True when the score is below the daemon's own floor — render it as a
   *  warning, not as a neutral stat. */
  belowFloor: boolean;
}

export function confidenceNote(
  confidence: number | null,
  floor: number | null,
): ConfidenceNote {
  if (confidence === null || !Number.isFinite(confidence)) {
    return { text: "no score (older daemon)", belowFloor: false };
  }
  const pct = confidencePct(confidence);
  if (floor === null || !Number.isFinite(floor)) {
    return { text: `${pct}% (floor not reported)`, belowFloor: false };
  }
  const floorPct = confidencePct(floor);
  // Mirrors heal::meets_confidence_floor — inclusive at the bar.
  const below = confidence < floor;
  return {
    text: below
      ? `${pct}% — BELOW the ${floorPct}% review floor`
      : `${pct}% (floor ${floorPct}%)`,
    belowFloor: below,
  };
}

/** Pull the adversarial reviewer's own sentence out of a proposal's report.md.
 *
 *  WHAT THE REVIEWER ACTUALLY THOUGHT was written to disk and never shown: the
 *  panel displayed a number and the first three lines of report.md, and the
 *  verdict sentence lives under its own heading further down. Returns "" when
 *  the section is absent (older report, or a rejection report). PURE, so it is
 *  tested headlessly — the component only renders what this returns. */
export function reviewVerdict(report: string): string {
  const lines = report.split("\n");
  const start = lines.findIndex((l) =>
    l.trim().toLowerCase().startsWith("## adversarial review verdict"),
  );
  if (start < 0) return "";
  const out: string[] = [];
  for (const line of lines.slice(start + 1)) {
    // Bounded at BOTH ends: stop at the next heading, or this runs on into the
    // validation tail and the fenced diff below it.
    if (line.trimStart().startsWith("## ")) break;
    out.push(line);
  }
  return out.join("\n").trim();
}

/** Human sentence for a `heal.rejected{stage}` token.
 *
 *  The banner rendered the raw token — "STAGE: confidence", "STAGE: deadline" —
 *  and those two say OPPOSITE things about what happened, neither of which is
 *  "a gate failed". `deadline` means no patch was ever judged (a capacity
 *  failure of the machine); `confidence` means patches passed every mechanical
 *  gate and the adversarial reviewer backed none of them. An operator reading
 *  one word cannot tell those from "the model drafted three bad patches", which
 *  is what the bare token invites. PURE, so it is tested headlessly. */
export function rejectionDetail(stage: string): string {
  switch (stage) {
    case "confidence":
      return "REVIEW FLOOR — candidates passed every staged gate, but the adversarial reviewer backed none of them, so nothing was proposed. The diffs and reviews are under state/heal/rejected/.";
    case "deadline":
      return "BUDGET — the staged-validation budget ran out before any candidate could be judged. This is a capacity failure of the gate on this machine, NOT a verdict on the patches.";
    case "mutation":
      return "MUTATION PROBE — a candidate's own test still passed with its fix taken away, so it does not demonstrate the defect.";
    case "draft":
      return "DRAFT — the model returned no usable unified diff.";
    case "patch":
      return "PATCH — no candidate diff applied cleanly to a fresh staging copy.";
    case "check":
    case "clippy":
    case "test":
      return `STAGED ${stage.toUpperCase()} — no candidate survived it.`;
    default:
      return `STAGE: ${stage}`;
  }
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

/** The minimum shape a two-step-confirm gate needs: which phase it is in and
 *  when it was armed. Structural, so any consequential control can reuse the
 *  gate WITHOUT copying its arithmetic (core/queueControls.ts is the second
 *  caller — the distill promote/rollback swap). ONE implementation, two
 *  callers: a copy would be free to drift, and this gate is the only thing
 *  standing between a double-click and a consequential commit. */
export interface TwoStepGate {
  phase: string;
  armedAt: number | null;
}

/** Is the confirm click currently allowed? Only while `confirming` AND at least
 *  REARM_MS after the arming click. PURE — the single implementation both the
 *  self-heal apply gate and the distill promote/rollback gate consult. */
export function twoStepConfirmReady(gate: TwoStepGate, now: number): boolean {
  return (
    gate.phase === "confirming" &&
    gate.armedAt !== null &&
    now - gate.armedAt >= REARM_MS
  );
}

/** Is the confirm click currently allowed? Only while `confirming` AND at least
 *  REARM_MS after the arming `accept` click. PURE — the guard the UI consults
 *  and the reducer enforces. Delegates to [`twoStepConfirmReady`]. */
export function confirmReady(state: ApplyState, now: number): boolean {
  return twoStepConfirmReady(state, now);
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
      // NAME THE GATES THAT ACTUALLY RUN. This said "cargo check + full test"
      // while the script had long since grown clippy -D warnings, the mutation
      // probe and the review-confidence floor — so the one line telling the
      // operator what their click is doing understated it by three gates.
      return "Re-validating (check + clippy + test + mutation probe + review floor)…";
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
