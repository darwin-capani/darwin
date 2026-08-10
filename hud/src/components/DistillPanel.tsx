import { useCallback, useEffect, useReducer, useRef, useState } from "react";
import type { DistillStatus } from "../core/events";
import { REARM_MS } from "../core/heal";
import {
  armedSwap,
  distillPromoteState,
  distillRollbackState,
  distillTrainState,
  initialSwapState,
  swapConfirmPrompt,
  swapConfirmReady,
  swapReduce,
} from "../core/queueControls";
import { inTauri } from "../tauri/bridge";
import { sendCommand } from "../tauri/command";
import Frame from "./Frame";

/**
 * SELF-DISTILL // LoRA — the honest state of the on-device self-distillation
 * pipeline (daemon distill.rs), and the three controls that drive it.
 *
 * HONESTY CONTRACT (do not regress):
 *   - SHIPS OFF: OFF unless the operator enables [distill]. The pill says so.
 *   - The device dependency (Apple Silicon + mlx-lm) is UNVERIFIABLE from the
 *     daemon, so an armed pipeline reads ARMED · NEEDS DEVICE (verified=false)
 *     — never a fabricated "ready to train".
 *   - PROMOTION IS MEASURED-GATED: a trained adapter goes live ONLY on a strict
 *     held-out win over the base model (deliberate promote, or the ships-OFF
 *     auto_promote chain), and it is reversible. The frame tag + footnote say
 *     exactly that; the LIVE line appears only for a daemon-VERIFIED live
 *     adapter (adapterLive), with its measured losses when present.
 *   - A pointer the server would refuse (base mismatch) or whose fusing an
 *     explicit quant decides at load time is surfaced as its own warning line —
 *     never rendered as "live".
 *
 * THE CONTROLS. This panel shipped read-only, so "ARMED · GATHERING" described a
 * pipeline that could never train: `distill::distill_now` had exactly ONE caller
 * (its command arm) and no control sent it.
 *
 *   TRAIN     -> {cmd:"distill"}. Trains + STAGES only. Local; never the network.
 *                Staging is not promotion, so one click, no confirm.
 *   PROMOTE   -> {cmd:"distill_promote"}  \  THESE SWAP WHICH MODEL ANSWERS YOU.
 *   ROLL BACK -> {cmd:"distill_rollback"} /  TWO-STEP CONFIRM, see below.
 *
 * WHY THE TWO-STEP. Promote/rollback change which brain answers the owner, which
 * is the same class of action as the self-heal panel's ACCEPT & APPLY — so this
 * reuses THAT gate rather than shipping a bare button or a second copy of the
 * arithmetic: arm on the first click, commit only on a second click at least
 * REARM_MS later, so a double-click cannot skip the confirm. The gate is
 * per-action (arming PROMOTE cannot commit a ROLLBACK) and the verb sent is read
 * back off the armed state, so the send can never disagree with the button.
 *
 * WHAT STILL BOUNDS PROMOTE daemon-side, structurally: `distill::promote_last`
 * refuses without [distill].enabled, a TRAINED manifest and the staged
 * safetensors actually on disk, then runs a HELD-OUT eval (base vs adapter) and
 * promotes only on an improvement clearing [distill].min_improvement. A click
 * cannot install a worse model — the gate measures first and refuses. Rollback
 * is idempotent and says so when nothing was live.
 *
 * All enablement + gate logic is PURE in core/queueControls.ts (the HUD's vitest
 * is node-env with no DOM — a click cannot be simulated); only the sends live
 * here.
 */
export default function DistillPanel({ distill }: { distill: DistillStatus | null }) {
  const shell = inTauri();
  const [training, setTraining] = useState(false);
  const [trainResult, setTrainResult] = useState<string | null>(null);
  // The model-swap gate is a LOCAL pure-reducer machine (core/queueControls.ts):
  // idle -> confirming -> running -> done|failed, two-step with a re-arm guard.
  const [swap, dispatch] = useReducer(swapReduce, undefined, initialSwapState);
  // Force a re-render when the re-arm window elapses so COMMIT flips from
  // disabled to enabled without waiting for another event.
  const [, bump] = useState(0);
  const rearmTimer = useRef<number | null>(null);
  // In-flight latch. The reducer already refuses a second `confirm`, and the
  // confirm block unmounts the moment the gate enters `running` — but both of
  // those depend on React having flushed the state update between two clicks.
  // A ref is updated synchronously, so a duplicate SEND is impossible regardless
  // of when the re-render lands.
  const sending = useRef(false);

  useEffect(() => {
    return () => {
      if (rearmTimer.current !== null) window.clearTimeout(rearmTimer.current);
    };
  }, []);

  const train = useCallback(async () => {
    // The same `busy` the button is disabled on — a swap in flight blocks a
    // train too, so the click site and the render can never disagree.
    if (sending.current) return;
    if (!distillTrainState(distill, shell, training || swap.phase === "running").canSend) return;
    sending.current = true;
    setTraining(true);
    setTrainResult(null);
    try {
      const reply = await sendCommand({ cmd: "distill" });
      // The daemon's own prose (off / thin dataset / STAGED / failed). Never a
      // fabricated "trained" — only distill_now knows what happened on disk.
      setTrainResult(reply.ok ? reply.reply ?? "Done." : reply.error ?? "Could not train.");
    } catch {
      setTrainResult("command failed — nothing was trained");
    } finally {
      sending.current = false;
      setTraining(false);
    }
  }, [distill, shell, training, swap.phase]);

  const arm = useCallback((action: "promote" | "rollback") => {
    const now = Date.now();
    dispatch({ type: "arm", action, at: now });
    if (rearmTimer.current !== null) window.clearTimeout(rearmTimer.current);
    rearmTimer.current = window.setTimeout(() => bump((n) => n + 1), REARM_MS + 20);
  }, []);

  const commit = useCallback(async () => {
    const now = Date.now();
    // The gate decides BOTH whether a commit may run and WHICH action it is, so
    // the verb below can never disagree with the button the operator armed.
    // Enforced at the click site as well as in the reducer (defense in depth).
    if (sending.current) return;
    const action = armedSwap(swap, now);
    if (action === null) return;
    sending.current = true;
    dispatch({ type: "confirm", at: now });
    try {
      const reply =
        action === "promote"
          ? await sendCommand({ cmd: "distill_promote" })
          : await sendCommand({ cmd: "distill_rollback" });
      // promote_last / clear_promotion each answer with the measured outcome —
      // promoted, refused-because-it-did-not-win, or "nothing was live". Surface
      // that sentence, never an invented success.
      if (reply.ok) {
        dispatch({ type: "settle", message: reply.reply ?? "Done." });
      } else {
        dispatch({ type: "fail", message: reply.error ?? "The swap did not go through." });
      }
    } catch {
      dispatch({ type: "fail", message: "command failed — the live model is unchanged" });
    } finally {
      sending.current = false;
    }
  }, [swap]);

  if (distill === null) return null;

  const state = pipelineState(distill);
  const live = distill.adapterLive ? distill.promoted : null;
  const busy = training || swap.phase === "running";
  const trainCtl = distillTrainState(distill, shell, busy);
  const promoteCtl = distillPromoteState(distill, shell, busy);
  const rollbackCtl = distillRollbackState(distill, shell, busy);
  const now = Date.now();
  const ready = swapConfirmReady(swap, now);
  return (
    <div className="distill-panel">
      <Frame
        title="SELF-DISTILL // LoRA"
        tag={distill.adapterLive ? "ADAPTER LIVE · MEASURED WIN" : "PROMOTION · MEASURED-GATED"}
      >
        <div className="distill-body">
          <div className="distill-head">
            <span className={`distill-pill ${state.cls}`}>{state.label}</span>
            <span className="distill-examples dim-note">
              {distill.examplesReady}/{distill.minExamples} graded examples ready
            </span>
          </div>
          {live !== null && (
            <div className="distill-live dim-note">
              live adapter
              {live.heldOutAdapterLoss !== null && live.heldOutBaseLoss !== null
                ? `: beat base ${live.heldOutAdapterLoss.toFixed(3)} vs ${live.heldOutBaseLoss.toFixed(3)} held-out`
                : ""}
              {" · reversible"}
            </div>
          )}
          {distill.adapterPointer === "installed-mismatch" && (
            <div className="distill-live dim-note">
              installed adapter doesn&apos;t match the resident model — base serves
            </div>
          )}
          {distill.adapterPointer === "installed-quant-undecided" && (
            <div className="distill-live dim-note">
              installed adapter — an explicit quant decides at the server&apos;s model load
            </div>
          )}
          {distill.lastRun !== null && (
            <div className="distill-run dim-note">
              last run: {distill.lastRun.status}
              {" · "}
              {distill.lastRun.exampleCount} examples
              {" · "}
              {distill.lastRun.promoted ? "PROMOTED" : "staged (not live)"}
            </div>
          )}

          {/* ---- CONTROLS. Train is one click (staging is not promotion);
                  promote/rollback ride the two-step model-swap gate. ---- */}
          <div className="distill-controls">
            <button
              type="button"
              className="distill-btn"
              onClick={() => void train()}
              disabled={!trainCtl.canSend}
              title={trainCtl.reason}
            >
              {training ? "TRAINING…" : "TRAIN + STAGE"}
            </button>
            {swap.phase === "idle" && (
              <>
                <button
                  type="button"
                  className="distill-btn"
                  onClick={() => arm("promote")}
                  disabled={!promoteCtl.canSend}
                  title={promoteCtl.reason}
                >
                  PROMOTE…
                </button>
                <button
                  type="button"
                  className="distill-btn"
                  onClick={() => arm("rollback")}
                  disabled={!rollbackCtl.canSend}
                  title={rollbackCtl.reason}
                >
                  ROLL BACK…
                </button>
              </>
            )}
          </div>
          {trainResult ? <div className="distill-result dim-note">{trainResult}</div> : null}

          {swap.phase === "confirming" && swap.action !== null && (
            <div className="distill-confirm">
              <span className="distill-confirm-q">{swapConfirmPrompt(swap.action)}</span>
              <button
                type="button"
                className="distill-commit"
                onClick={() => void commit()}
                disabled={!ready}
                title={
                  ready
                    ? "commit the swap"
                    : "hold on — the confirm arms a moment after the first click"
                }
              >
                {ready ? "YES, SWAP THE MODEL" : "ARMING…"}
              </button>
              <button
                type="button"
                className="distill-cancel"
                onClick={() => dispatch({ type: "reset" })}
              >
                CANCEL
              </button>
            </div>
          )}
          {swap.phase === "running" && (
            <div className="distill-result dim-note">
              {swap.action === "promote"
                ? "measuring the staged adapter against base…"
                : "clearing the live adapter…"}
            </div>
          )}
          {(swap.phase === "done" || swap.phase === "failed") && (
            <div className="distill-result dim-note">
              {swap.message}{" "}
              <button
                type="button"
                className="distill-cancel"
                onClick={() => dispatch({ type: "reset" })}
              >
                DISMISS
              </button>
            </div>
          )}

          <div className="distill-foot dim-note">
            Learns a personal adapter from your own redacted, un-redirected turns.
            It goes live only if it measurably beats the base model on your
            held-out turns — a tie or a loss keeps the current model — and
            promotion is reversible.
          </div>
        </div>
      </Frame>
    </div>
  );
}

function pipelineState(d: DistillStatus): { label: string; cls: string } {
  if (!d.enabled) return { label: "OFF", cls: "off" };
  if (d.readyToTrain) {
    // Dataset is ready; the device gate is still separate + unverified.
    return { label: "ARMED · NEEDS DEVICE", cls: "armed" };
  }
  return { label: "ARMED · GATHERING", cls: "armed" };
}
