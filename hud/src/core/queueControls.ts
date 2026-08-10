/**
 * PURE control logic for the four panels that used to REPORT on work nothing
 * could start — overnight, self-distill, sync and handoff/continuity.
 *
 * WHAT WAS WRONG. Each of those daemon verbs was implemented, gated and tested,
 * and no shipped control sent it. `overnight::enqueue`, `distill::distill_now`,
 * `sync::sync_now` and `handoff::build_capsule` each had exactly ONE caller —
 * their own command arm — so OvernightPanel's "N queued" counted a queue that
 * could not be filled and DistillPanel's "ARMED · GATHERING" described a
 * pipeline that could never train.
 *
 * WHY THE LOGIC LIVES HERE. The HUD's vitest runs in the NODE environment with
 * no DOM, so a component cannot be mounted and a click cannot be simulated.
 * Every decision that matters — may this button send? is the two-step confirm
 * satisfied? — is therefore a pure function here, unit-tested headlessly,
 * exactly like core/heal.ts and core/state.ts. The components hold only the
 * `sendCommand` call itself.
 *
 * WHAT THIS DELIBERATELY DOES NOT DO:
 *   - It NEVER decides that a command will succeed. Every one of these verbs is
 *     gated daemon-side (master switch, key, device, measured eval) and answers
 *     with honest prose; the panels surface THAT reply, never a fabricated one.
 *   - It disables a control only for a reason the HUD can know for certain (no
 *     desktop shell, a request already in flight, an empty required field, or a
 *     master switch the daemon itself reports OFF). Over-disabling on a derived
 *     status is how a control ends up dead after every HUD start.
 */
import type { DistillStatus, HandoffStatus, OvernightStatus, SyncStatus } from "./events";
import { twoStepConfirmReady, type TwoStepGate } from "./heal";

/** The daemon's own free-text clamp (daemon/src/command.rs MAX_TEXT_CHARS, and
 *  mirrored in the Tauri backend). Clamping here too means an oversized task
 *  never rides the wire only to be silently truncated at the far end. */
export const OVERNIGHT_TASK_MAX_CHARS = 4 * 1024;

/** Trim + clamp a typed overnight task to what the daemon will accept.
 *  Char-boundary safe (spread splits by code point, not UTF-16 unit). */
export function normalizeOvernightTask(raw: string): string {
  const t = raw.trim();
  if ([...t].length <= OVERNIGHT_TASK_MAX_CHARS) return t;
  return [...t].slice(0, OVERNIGHT_TASK_MAX_CHARS).join("");
}

/** The verdict for one control: may it send, and if not, the honest reason
 *  (surfaced as the button's title so a disabled control always says why). */
export interface ControlState {
  canSend: boolean;
  reason: string;
}

const NO_SHELL = "available in the DARWIN desktop app";
const BUSY = "working…";

/** OVERNIGHT — queue a task for the unattended runner.
 *
 *  Enabled needs: the desktop shell, no request in flight, `[overnight].enabled`
 *  reported ON by the daemon, and a non-empty task.
 *
 *  A MISSING CLOUD KEY DOES NOT DISABLE IT, on purpose: `overnight::enqueue`
 *  still persists the task and returns the honest "saved, but there's no
 *  Anthropic API key on file, so overnight work won't run until one is" line.
 *  Refusing the click would lose a real queue write and replace an honest daemon
 *  sentence with a guess. */
export function overnightQueueState(
  overnight: OvernightStatus | null,
  shell: boolean,
  task: string,
  busy: boolean,
): ControlState {
  if (!shell) return { canSend: false, reason: NO_SHELL };
  if (busy) return { canSend: false, reason: BUSY };
  if (overnight === null) return { canSend: false, reason: "waiting for the daemon's overnight status" };
  if (!overnight.enabled) {
    return {
      canSend: false,
      reason: "overnight agents are off — turn on [overnight].enabled in settings",
    };
  }
  if (normalizeOvernightTask(task) === "") {
    return { canSend: false, reason: "type what to look into overnight" };
  }
  return { canSend: true, reason: "queue this for the next time you're away" };
}

/** SELF-DISTILL // TRAIN — train + STAGE a LoRA adapter. Local only; staging is
 *  not promotion, so this is the NON-consequential half of the pipeline and
 *  needs no confirm step.
 *
 *  A THIN DATASET DOES NOT DISABLE IT: `distill_now` refuses a thin dataset with
 *  its own honest count, which is more useful than a greyed-out button — and the
 *  example count in the status frame is a snapshot that a fresh graded turn can
 *  outdate between frames. */
export function distillTrainState(
  distill: DistillStatus | null,
  shell: boolean,
  busy: boolean,
): ControlState {
  if (!shell) return { canSend: false, reason: NO_SHELL };
  if (busy) return { canSend: false, reason: BUSY };
  if (distill === null) return { canSend: false, reason: "waiting for the daemon's distill status" };
  if (!distill.enabled) {
    return {
      canSend: false,
      reason: "self-distillation is off — turn on [distill].enabled in settings",
    };
  }
  return { canSend: true, reason: "train and stage a personal adapter (staged, not live)" };
}

/** SELF-DISTILL // PROMOTE — the CONSEQUENTIAL half: it swaps which model
 *  answers you. Gated daemon-side on a measured held-out win, and gated here
 *  behind the two-step confirm below.
 *
 *  Requires `[distill].enabled`: promoting is an ARMING action, and a pipeline
 *  the operator has switched off should not be able to install a new brain. */
export function distillPromoteState(
  distill: DistillStatus | null,
  shell: boolean,
  busy: boolean,
): ControlState {
  if (!shell) return { canSend: false, reason: NO_SHELL };
  if (busy) return { canSend: false, reason: BUSY };
  if (distill === null) return { canSend: false, reason: "waiting for the daemon's distill status" };
  if (!distill.enabled) {
    return {
      canSend: false,
      reason: "self-distillation is off — turn on [distill].enabled in settings",
    };
  }
  return {
    canSend: true,
    reason: "measure the staged adapter against base and promote it only if it wins",
  };
}

/** SELF-DISTILL // ROLLBACK — clear the live adapter; the next load serves base.
 *
 *  DELIBERATELY NOT GATED ON `[distill].enabled`. Rollback is the SAFE
 *  direction, and an operator who switched the pipeline off after a promotion
 *  must still be able to get back to the base model. `clear_promotion` is
 *  idempotent and says so when nothing was live, so an extra click is honest,
 *  never a lie. It still rides the same two-step confirm — it changes which
 *  model answers, and that is the line, in either direction. */
export function distillRollbackState(
  _distill: DistillStatus | null,
  shell: boolean,
  busy: boolean,
): ControlState {
  if (!shell) return { canSend: false, reason: NO_SHELL };
  if (busy) return { canSend: false, reason: BUSY };
  return { canSend: true, reason: "clear the live personal adapter and serve base again" };
}

/** SYNC // SYNC NOW — seal this device's syncable facts into the outbox, merge
 *  whatever a paired device left in the inbox, AND — when `[sync].peer_endpoint`
 *  is configured — POST the sealed bundle to that endpoint over the network.
 *
 *  THAT LAST CLAUSE IS WHY THIS DOC EXISTS. It used to read "the network
 *  transport is armed-but-inert, so nothing egresses on this click", and that is
 *  not true: `sync::sync_now` calls `transport_push` — a real reqwest POST of the
 *  sealed bundle — on the non-empty-`peer_endpoint` branch. "Armed-but-inert"
 *  describes the UNPAIRED case only, and the daemon already knows it:
 *  `sync::status_payload` derives `transport_inert = !peer_configured` precisely
 *  because pinning that field true "asserted [the facts do not leave the machine]
 *  at exactly the moment they do". A button whose tooltip promised a click cannot
 *  egress would reprint that defect in the one sentence an operator reads BEFORE
 *  clicking, so the reason NAMES the delivery whenever the daemon reports a peer.
 *
 *  It stays ENABLED either way — the endpoint is the owner's OWN paired Mac and
 *  the bundle is sealed — but the operator is told which of the two a click is.
 *
 *  A missing pairing key does NOT disable it either: `sync_now` refuses honestly
 *  ("never exports in the clear") rather than writing a plaintext bundle. */
export function syncNowState(
  sync: SyncStatus | null,
  shell: boolean,
  busy: boolean,
): ControlState {
  if (!shell) return { canSend: false, reason: NO_SHELL };
  if (busy) return { canSend: false, reason: BUSY };
  if (sync === null) return { canSend: false, reason: "waiting for the daemon's sync status" };
  if (!sync.enabled) {
    return { canSend: false, reason: "sync is off — turn on [sync].enabled in settings" };
  }
  return {
    canSend: true,
    reason: sync.peerConfigured
      ? "seal this device's facts into the outbox AND send the sealed bundle over the network to the paired device at [sync].peer_endpoint"
      : "seal this device's facts into the outbox — no [sync].peer_endpoint is configured, so nothing leaves this Mac",
  };
}

/** HANDOFF // HAND OFF — seal the current session's REDACTED capsule to the
 *  paired Mac's outbox. Local write; the capsule carries no credential. */
export function handoffState(
  handoff: HandoffStatus | null,
  shell: boolean,
  busy: boolean,
): ControlState {
  if (!shell) return { canSend: false, reason: NO_SHELL };
  if (busy) return { canSend: false, reason: BUSY };
  if (handoff === null) return { canSend: false, reason: "waiting for the daemon's handoff status" };
  if (!handoff.enabled) {
    return { canSend: false, reason: "continuity handoff is off — turn on [handoff].enabled in settings" };
  }
  return { canSend: true, reason: "seal this session to your other Mac's inbox" };
}

/** HANDOFF // RESUME — open + verify every capsule a paired Mac staged, and
 *  PARK. Not a consequential swap, so no confirm step.
 *
 *  WHAT IT DOES NOT DO, and the tooltip must not imply otherwise: it does not
 *  re-enter the capsule's context into this session. `Command::Resume` calls
 *  `handoff::resume_from_inbox`, reads `.note` off the first `RestoredSession`
 *  and DROPS the vector — nothing installs the transcript window, active agent,
 *  mission ref, world deltas, draft ids or focus profile, and `handoff::restore`
 *  is a pure fn that only builds that note. So the capsule is opened, decrypted,
 *  version-checked and reported; the restore LEG itself is not built. That is a
 *  daemon-side gap and the owner's to close (see the summary) — but a HUD button
 *  labelled RESUME must not be the thing that overstates it, which is why this
 *  reason says "open and verify" and names the missing half.
 *
 *  `pendingCapsule` is a HINT, not a gate: it is a status snapshot, and a
 *  capsule that lands between frames would leave a correct click refused.
 *  `resume` answers "No session capsule from a paired Mac to resume, sir."
 *  when there is nothing staged, which is the honest outcome. */
export function resumeState(
  handoff: HandoffStatus | null,
  shell: boolean,
  busy: boolean,
): ControlState {
  if (!shell) return { canSend: false, reason: NO_SHELL };
  if (busy) return { canSend: false, reason: BUSY };
  if (handoff === null) return { canSend: false, reason: "waiting for the daemon's handoff status" };
  if (!handoff.enabled) {
    return { canSend: false, reason: "continuity handoff is off — turn on [handoff].enabled in settings" };
  }
  return {
    canSend: true,
    reason: handoff.pendingCapsule
      ? "open and verify the staged capsule (it parks — authority does not transfer, and its context is not re-entered into this session)"
      : "open a staged capsule if your other Mac left one (it parks)",
  };
}

/* ------------------------------------------------------------------------ *
 * THE MODEL-SWAP TWO-STEP GATE.
 *
 * `distill_promote` and `distill_rollback` SWAP WHICH MODEL ANSWERS THE OWNER.
 * That is the same class of action as the self-heal panel's ACCEPT & APPLY, so
 * it reuses that panel's gate rather than inventing a second one: arm on the
 * first click, and honor the commit ONLY on a second click that lands at least
 * REARM_MS later — so a double-click cannot skip the confirm. The re-arm
 * arithmetic itself is `heal.ts::twoStepConfirmReady`; there is ONE
 * implementation and two callers, because a copy is free to drift and this gate
 * is the only thing between a stray double-click and a new brain.
 * ------------------------------------------------------------------------ */

/** Which swap is armed. The gate is per-ACTION: arming PROMOTE cannot commit a
 *  ROLLBACK, because the committed action is read back off the armed state. */
export type SwapAction = "promote" | "rollback";

export type SwapPhase = "idle" | "confirming" | "running" | "done" | "failed";

export interface SwapState extends TwoStepGate {
  phase: SwapPhase;
  /** ms timestamp of the arming click, or null outside `confirming`. */
  armedAt: number | null;
  /** The action this gate is armed for / is running / has finished. */
  action: SwapAction | null;
  /** The daemon's own reply (or an honest local failure) once terminal. */
  message: string;
}

export type SwapEvent =
  /** First click: arm the confirm gate for ONE action. */
  | { type: "arm"; action: SwapAction; at: number }
  /** Second click: honored only while confirming AND past the re-arm window. */
  | { type: "confirm"; at: number }
  /** The daemon answered. `message` is ITS prose, never a fabricated success. */
  | { type: "settle"; message: string }
  /** The command channel itself failed; nothing was swapped. */
  | { type: "fail"; message: string }
  /** Back out of the confirm step, or clear a terminal result. */
  | { type: "reset" };

export function initialSwapState(): SwapState {
  return { phase: "idle", armedAt: null, action: null, message: "" };
}

/** Is the commit click currently allowed? Delegates to the self-heal gate. */
export function swapConfirmReady(state: SwapState, now: number): boolean {
  return twoStepConfirmReady(state, now);
}

/** The action a commit would actually run right now, or null if none is armed
 *  and ready. The component reads the verb off THIS, so the verb it sends can
 *  never disagree with the button the operator armed. */
export function armedSwap(state: SwapState, now: number): SwapAction | null {
  return swapConfirmReady(state, now) ? state.action : null;
}

/** PURE swap-gate reducer. The ONLY transition that may spawn a command send is
 *  `confirming -> running`, and it is reachable only through `confirm` after an
 *  `arm` (two distinct clicks, REARM_MS apart). */
export function swapReduce(state: SwapState, event: SwapEvent): SwapState {
  switch (event.type) {
    case "arm": {
      // Arming is meaningful only from idle. A stray arm mid-run — or an arm for
      // the OTHER action while one is already armed — is ignored, so the armed
      // action can never be swapped out from under the confirm click.
      if (state.phase !== "idle") return state;
      return { phase: "confirming", armedAt: event.at, action: event.action, message: "" };
    }
    case "confirm": {
      // The two-step gate + re-arm guard.
      if (!swapConfirmReady(state, event.at)) return state;
      // Enter `running` immediately so a third click cannot double-send; the
      // caller then sends the verb for `state.action` and feeds settle/fail.
      return { ...state, phase: "running", armedAt: null };
    }
    case "settle": {
      if (state.phase !== "running") return state;
      return { ...state, phase: "done", armedAt: null, message: event.message };
    }
    case "fail": {
      if (state.phase !== "running") return state;
      return { ...state, phase: "failed", armedAt: null, message: event.message };
    }
    case "reset": {
      // Never mid-run (the send is in flight), and a no-op from idle.
      if (state.phase === "running" || state.phase === "idle") return state;
      return initialSwapState();
    }
    default:
      return state;
  }
}

/** The confirm question for an armed swap. PURE, so the wording is pinned by a
 *  test rather than living only in JSX a node-env test cannot mount. */
export function swapConfirmPrompt(action: SwapAction): string {
  return action === "promote"
    ? "Promote the staged adapter to answer you? It goes live only if it measurably beats base."
    : "Roll back to the base model? The personal adapter stops answering you.";
}
