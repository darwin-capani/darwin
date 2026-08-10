import { useCallback, useState } from "react";
import type { HandoffStatus } from "../core/events";
import { handoffState, resumeState } from "../core/queueControls";
import { inTauri } from "../tauri/bridge";
import { sendCommand } from "../tauri/command";
import Frame from "./Frame";

/**
 * HANDOFF // CONTINUITY — the honest state of resuming a LIVE session on another
 * of the owner's Macs (daemon handoff.rs), and the two controls that move one.
 *
 * HONESTY CONTRACT (do not regress):
 *   - SHIPS OFF: OFF unless the operator enables [handoff] (it rides [sync],
 *     also OFF). The pill says so.
 *   - E2E-ENCRYPTED, SAME SEALED PATH: a session capsule never leaves the box
 *     unsealed; the transport is armed-but-inert. ARMED · NEEDS PAIRING until a
 *     shared key exists, then ARMED · PAIRED.
 *   - AUTHORITY NEVER TRANSFERS: the capsule carries NO resolved credential /
 *     bearer (redacted like a macro), and restoring context PARKS — every
 *     consequential step on the receiving Mac re-hits the fresh confirm +
 *     voice-id + master switch. The footnote says so plainly.
 *
 * THE CONTROLS. This panel shipped read-only: "Resume on <device>" was PROSE,
 * and neither `handoff` nor `resume` was even in the Tauri relay's allow-list,
 * so `handoff::build_capsule` and `resume_from_inbox` had no caller but their own
 * command arms. A staged capsule could be announced and never opened.
 *
 *   HAND OFF -> {cmd:"handoff"}. Seals THIS session's REDACTED capsule into the
 *               paired Mac's outbox. A LOCAL WRITE ONLY: there is NO delivery
 *               leg at all — nothing reads state/handoff/outbox and sync.rs's
 *               `transport_push` is called only by `sync_now`, for SyncBundles
 *               (handoff.rs::hand_off says so and is hermetically tested). Say
 *               that, not "the transport is inert": inert implies a hop that
 *               goes live once a peer is paired, which is the exact wording
 *               handoff.rs records deleting for asserting a hop that never
 *               happens. Unlike SYNC NOW, this click cannot egress under ANY
 *               config. No key means an honest refusal, not a plaintext capsule.
 *   RESUME   -> {cmd:"resume"}. Opens + VERIFIES every staged capsule and PARKS.
 *               Not "restores": `Command::Resume` reads the note off the first
 *               RestoredSession and drops the vector, and `handoff::restore` is a
 *               pure fn, so nothing installs the transcript window, agent,
 *               mission ref, deltas, drafts or focus profile. The capsule is
 *               decrypted, version-checked and reported; the restore leg is a
 *               daemon-side gap for the owner, not something a button may claim.
 *               Either way it is one click, not a swap: restoring context would
 *               not be restoring permission, and a session that merely LOOKED
 *               authorised is the dangerous reading this panel refuses to allow.
 */
export default function HandoffPanel({ handoff }: { handoff: HandoffStatus | null }) {
  const shell = inTauri();
  // WHICH action is in flight, not merely "something is". Both controls disable
  // together (one command channel, one panel), but only the running one says so
  // — a "WORKING…" label on the seal button while a RESUME is running would be
  // the panel narrating the wrong operation.
  const [running, setRunning] = useState<"handoff" | "resume" | null>(null);
  const [result, setResult] = useState<string | null>(null);
  const busy = running !== null;

  const handOff = useCallback(async () => {
    if (!handoffState(handoff, shell, busy).canSend) return;
    setRunning("handoff");
    setResult(null);
    try {
      const reply = await sendCommand({ cmd: "handoff" });
      // hand_off's own prose — off / no key / sealed. Never a fabricated seal.
      setResult(reply.ok ? reply.reply ?? "Done." : reply.error ?? "Could not hand off.");
    } catch {
      setResult("command failed — nothing was sealed");
    } finally {
      setRunning(null);
    }
  }, [handoff, shell, busy]);

  const resume = useCallback(async () => {
    if (!resumeState(handoff, shell, busy).canSend) return;
    setRunning("resume");
    setResult(null);
    try {
      const reply = await sendCommand({ cmd: "resume" });
      // resume's own prose — off / no key / nothing staged / restored-and-parked.
      setResult(reply.ok ? reply.reply ?? "Done." : reply.error ?? "Could not resume.");
    } catch {
      setResult("command failed — nothing was restored");
    } finally {
      setRunning(null);
    }
  }, [handoff, shell, busy]);

  if (handoff === null) return null;

  const state = pipelineState(handoff);
  const device = handoff.device.trim() || "your other Mac";
  const sealCtl = handoffState(handoff, shell, busy);
  const resumeCtl = resumeState(handoff, shell, busy);
  return (
    <div className="handoff-panel">
      <Frame title="HANDOFF // CONTINUITY" tag="E2E · AUTHORITY STAYS">
        <div className="handoff-body">
          <div className="handoff-head">
            <span className={`handoff-pill ${state.cls}`}>{state.label}</span>
            {state.cls === "ready" && (
              <span className="handoff-resume dim-note">Resume on {device}</span>
            )}
          </div>
          {handoff.pendingCapsule && (
            <div className="handoff-pending">
              A session capsule from {device} is staged — restoring it repopulates
              context and parks; authority did not transfer.
            </div>
          )}
          <div className="handoff-controls">
            <button
              type="button"
              className="handoff-btn"
              onClick={() => void handOff()}
              disabled={!sealCtl.canSend}
              title={sealCtl.reason}
            >
              {running === "handoff" ? "SEALING…" : "HAND OFF THIS SESSION"}
            </button>
            <button
              type="button"
              className="handoff-btn"
              onClick={() => void resume()}
              disabled={!resumeCtl.canSend}
              title={resumeCtl.reason}
            >
              {running === "resume" ? "RESTORING…" : "RESUME STAGED"}
            </button>
          </div>
          {result ? <div className="handoff-result dim-note">{result}</div> : null}
          <div className="handoff-foot dim-note">
            Resume a live session on your own Mac, end-to-end encrypted (the capsule
            never leaves the box unsealed). It carries NO credentials, and restoring
            context never restores permission &mdash; every consequential step
            re-asks with a fresh confirm, voice-id, and the master switch. The
            network transport is armed but inert.
          </div>
        </div>
      </Frame>
    </div>
  );
}

function pipelineState(h: HandoffStatus): { label: string; cls: string } {
  if (!h.enabled) return { label: "OFF", cls: "off" };
  if (!h.keyPresent) return { label: "ARMED · NEEDS PAIRING", cls: "armed" };
  return { label: "ARMED · PAIRED", cls: "ready" };
}
