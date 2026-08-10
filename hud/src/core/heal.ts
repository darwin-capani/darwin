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
 * PER-ATTEMPT CALIBRATION (heal.proposal / heal.rejected `calibration`).
 *
 * The daemon has shipped this payload on BOTH terminal heal events since the
 * two self-heal tunables — [self_heal].attempt_budget_secs and
 * .confidence_floor — became settable. It exists so those two numbers can be
 * set from measurement instead of from argument: every candidate's review
 * score (not just the winner's), every cargo stage's real wall time on THIS
 * machine, and how many drafted candidates the attempt could not afford to
 * stage at all. The HUD declared the type and rendered none of it, so the
 * numbers stayed unreadable — which is the same as not having them.
 *
 * Everything below is PURE and lives here (not in the component) so the
 * arithmetic is tested headlessly under vitest.
 * ------------------------------------------------------------------------ */

/** One candidate's adversarial-review score. */
export interface HealReviewScore {
  candidate: number;
  confidence: number;
  /** false = the review CALL never returned and was recorded as 0.0. That is NO
   *  REVIEW, not a zero verdict — averaging the two together calibrates the
   *  floor against an outage, so the two are kept apart everywhere. */
  reviewed: boolean;
}

/** One cargo stage that actually ran, with its real wall time. */
export interface HealStageTiming {
  candidate: number;
  stage: string;
  secs: number;
  /** ok:false with cutOff:false is a MERIT failure (usually much faster);
   *  cutOff:true is the budget biting. A mean over both is meaningless. */
  ok: boolean;
  cutOff: boolean;
}

export interface HealCalibration {
  reviews: HealReviewScore[];
  stages: HealStageTiming[];
  attemptSpentSecs: number | null;
  attemptBudgetSecs: number | null;
  candidateBudgetSecs: number | null;
  confidenceFloor: number | null;
  candidatesDrafted: number | null;
  candidatesUnaffordable: number | null;
}

/** Bound on rendered per-candidate / per-stage rows, so a hostile or runaway
 *  frame cannot flood the panel. A real attempt drafts a handful. */
export const CALIBRATION_MAX_ROWS = 32;

function isObj(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}
function fin(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

/** Parse the `calibration` object off a heal.proposal / heal.rejected payload.
 *  Returns null when the daemon sent none (an older daemon) or when it carries
 *  nothing usable — the panel then renders nothing rather than an empty shell
 *  of zeros, which would read as measurements. Every field is optional and
 *  independently defensive; a malformed row is dropped, never fabricated. */
export function parseHealCalibration(data: unknown): HealCalibration | null {
  if (!isObj(data)) return null;
  const raw = data.calibration;
  if (!isObj(raw)) return null;

  const reviews: HealReviewScore[] = Array.isArray(raw.confidences)
    ? raw.confidences
        .filter(isObj)
        .map((r) => ({
          candidate: fin(r.candidate) ?? 0,
          confidence: fin(r.confidence) ?? 0,
          // Absent => treat as NOT reviewed: claiming a review happened is the
          // failure mode this flag exists to prevent.
          reviewed: r.reviewed === true,
        }))
        .slice(0, CALIBRATION_MAX_ROWS)
    : [];

  const stages: HealStageTiming[] = Array.isArray(raw.stages)
    ? raw.stages
        .filter(isObj)
        .map((r) => ({
          candidate: fin(r.candidate) ?? 0,
          stage: typeof r.stage === "string" ? r.stage : "?",
          secs: Math.max(0, fin(r.secs) ?? 0),
          ok: r.ok === true,
          cutOff: r.cut_off === true,
        }))
        .slice(0, CALIBRATION_MAX_ROWS)
    : [];

  const out: HealCalibration = {
    reviews,
    stages,
    attemptSpentSecs: fin(raw.attempt_spent_secs),
    attemptBudgetSecs: fin(raw.attempt_budget_secs),
    candidateBudgetSecs: fin(raw.candidate_budget_secs),
    confidenceFloor: fin(raw.confidence_floor),
    candidatesDrafted: fin(raw.candidates_drafted),
    candidatesUnaffordable: fin(raw.candidates_unaffordable),
  };
  const empty =
    out.reviews.length === 0 &&
    out.stages.length === 0 &&
    out.attemptSpentSecs === null &&
    out.attemptBudgetSecs === null &&
    out.candidateBudgetSecs === null &&
    out.confidenceFloor === null &&
    out.candidatesDrafted === null &&
    out.candidatesUnaffordable === null;
  return empty ? null : out;
}

/** One rendered line of the calibration readout. */
export interface CalibrationRow {
  key: string;
  label: string;
  text: string;
  /** Render as a warning: this line is telling the operator a tunable is wrong. */
  warn: boolean;
}

/** Turn a calibration payload into the lines that answer the only two questions
 *  it exists to answer: is `attempt_budget_secs` big enough for THIS machine,
 *  and is `confidence_floor` in the right place?
 *
 *  ARITHMETIC IS DONE HERE, not left to the reader: the budget line states the
 *  remaining margin as a number (and says "over by" rather than showing a
 *  negative), and the review line counts how many candidates actually cleared
 *  the floor versus how many were never reviewed at all. */
export function calibrationRows(c: HealCalibration): CalibrationRow[] {
  const rows: CalibrationRow[] = [];

  if (c.attemptBudgetSecs !== null) {
    const budget = Math.round(c.attemptBudgetSecs);
    if (c.attemptSpentSecs === null) {
      rows.push({
        key: "budget",
        label: "ATTEMPT BUDGET",
        text: `${budget}s (spend not reported)`,
        warn: false,
      });
    } else {
      const spent = Math.round(c.attemptSpentSecs);
      const left = budget - spent;
      rows.push({
        key: "budget",
        label: "ATTEMPT BUDGET",
        text:
          left >= 0
            ? `spent ${spent}s of ${budget}s — ${left}s left`
            : `spent ${spent}s of ${budget}s — OVER by ${-left}s`,
        warn: left <= 0,
      });
    }
  }

  if (c.candidateBudgetSecs !== null) {
    rows.push({
      key: "candidate-budget",
      label: "PER CANDIDATE",
      text: `${Math.round(c.candidateBudgetSecs)}s ceiling`,
      warn: false,
    });
  }

  const unaffordable = c.candidatesUnaffordable ?? 0;
  if (unaffordable > 0) {
    const drafted = c.candidatesDrafted;
    rows.push({
      key: "unaffordable",
      label: "NEVER STAGED",
      text:
        (drafted === null
          ? `${unaffordable} drafted candidate${unaffordable === 1 ? "" : "s"}`
          : `${unaffordable} of ${Math.round(drafted)} drafted candidates`) +
        " — the attempt could not afford to validate them. Raise [self_heal].attempt_budget_secs.",
      warn: true,
    });
  } else if (c.candidatesDrafted !== null) {
    rows.push({
      key: "drafted",
      label: "CANDIDATES",
      text: `${Math.round(c.candidatesDrafted)} drafted, all staged`,
      warn: false,
    });
  }

  if (c.reviews.length > 0) {
    const n = c.reviews.length;
    const unreviewed = c.reviews.filter((r) => !r.reviewed).length;
    const floor = c.confidenceFloor;
    let text: string;
    let warn = unreviewed > 0;
    if (floor === null) {
      text = `${n} scored (floor not reported)`;
    } else {
      const cleared = c.reviews.filter((r) => r.reviewed && r.confidence >= floor).length;
      text = `${cleared} of ${n} at or above the ${confidencePct(floor)}% floor`;
      warn = warn || cleared === 0;
    }
    if (unreviewed > 0) {
      text += ` · ${unreviewed} review call${unreviewed === 1 ? "" : "s"} never returned (recorded 0.0 — NO REVIEW, not a zero verdict)`;
    }
    rows.push({ key: "reviews", label: "REVIEW SCORES", text, warn });
  }

  if (c.stages.length > 0) {
    const total = c.stages.reduce((a, s) => a + s.secs, 0);
    const slowest = c.stages.reduce((a, s) => (s.secs > a.secs ? s : a), c.stages[0]);
    const cut = c.stages.filter((s) => s.cutOff);
    let text = `${c.stages.length} ran, ${total}s total — slowest ${slowest.stage} ${slowest.secs}s (candidate ${slowest.candidate})`;
    if (cut.length > 0) {
      text += ` · ${cut.length} CUT OFF by the budget (${cut
        .map((s) => `${s.stage}/c${s.candidate}`)
        .join(", ")})`;
    }
    rows.push({ key: "stages", label: "CARGO STAGES", text, warn: cut.length > 0 });
  }

  return rows;
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
