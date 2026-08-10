import { useCallback, useState } from "react";
import type { OvernightStatus } from "../core/events";
import { normalizeOvernightTask, overnightQueueState } from "../core/queueControls";
import { inTauri } from "../tauri/bridge";
import { sendCommand } from "../tauri/command";
import Frame from "./Frame";

/**
 * OVERNIGHT // ASYNC AGENTS — the honest state of the overnight task queue +
 * morning brief (daemon overnight.rs, F10), and the ONE control that fills it.
 *
 * HONESTY CONTRACT (do not regress):
 *   - SHIPS OFF: OFF unless the operator enables [overnight]. The pill says so.
 *   - CLOUD-GATED: ARMED · NEEDS KEY until an Anthropic key exists, then READY.
 *   - TOOL-LESS: overnight work drafts, never acts — the footnote states, and
 *     `runsTools` pins, that no tools are ever passed, so nothing consequential
 *     can happen while you sleep.
 *
 * THE QUEUE CONTROL. This panel shipped read-only: it counted "N queued" for a
 * queue nothing could fill. `overnight::enqueue` had exactly ONE caller — the
 * command verb — with no control and no spoken route, and the relay itself was
 * broken (the Tauri backend never carried the `prompt` field, so the daemon
 * BadRequested every possible request). Both are fixed; this is the sender.
 *
 * WHY IT ADDS NO AUTHORITY — structurally, not as a promise:
 * `overnight::run_real_task` is a `complete_plain` call with NO TOOL LOOP. A
 * queued task is never offered a tool, so it can generate TEXT and nothing else
 * — it cannot send, buy, post, write or actuate. Queueing is a LOCAL WRITE to
 * the state directory; nothing runs until the away-gate opens.
 *
 * The daemon's own reply is surfaced verbatim (queued / off / no key / queue
 * full / failed-to-save). The HUD never fabricates a "queued" it did not get:
 * a failed persist says the task is NOT queued, and this shows that sentence.
 *
 * All the enablement logic is PURE in core/queueControls.ts — the HUD's vitest
 * is node-env with no DOM, so a component cannot be mounted and a click cannot
 * be simulated; only the `sendCommand` call itself lives here.
 */
export default function OvernightPanel({ overnight }: { overnight: OvernightStatus | null }) {
  const shell = inTauri();
  const [task, setTask] = useState("");
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<string | null>(null);

  const queue = useCallback(async () => {
    // Re-derive the verdict at the click site (defense in depth — the button is
    // also disabled), so a stale render can never send an ill-formed request.
    if (!overnightQueueState(overnight, shell, task, busy).canSend) return;
    const prompt = normalizeOvernightTask(task);
    setBusy(true);
    setResult(null);
    try {
      const reply = await sendCommand({ cmd: "overnight", prompt });
      // The daemon's own prose, always — it is the only thing that knows whether
      // the queue write landed, whether a key exists, or whether the queue is
      // full. Never a fabricated "queued".
      setResult(reply.ok ? reply.reply ?? "Queued." : reply.error ?? "Could not queue that.");
      // Clear the box ONLY on an accepted send; a refusal keeps what was typed
      // so the operator does not lose it to an error they can fix.
      if (reply.ok) setTask("");
    } catch {
      setResult("command failed — nothing was queued");
    } finally {
      setBusy(false);
    }
  }, [overnight, shell, task, busy]);

  if (overnight === null) return null;

  const state = agentState(overnight);
  const control = overnightQueueState(overnight, shell, task, busy);
  return (
    <div className="overnight-panel">
      <Frame title="OVERNIGHT // ASYNC AGENTS" tag="TOOL-LESS · DRAFTS ONLY">
        <div className="overnight-body">
          <div className="overnight-head">
            <span className={`overnight-pill ${state.cls}`}>{state.label}</span>
            <span className="overnight-count dim-note">
              {overnight.queued} queued · {overnight.done} done
              {overnight.failed > 0 ? ` · ${overnight.failed} failed` : ""}
            </span>
          </div>
          {overnight.items.length > 0 && (
            <ul className="overnight-items">
              {overnight.items.map((it, i) => (
                <li key={`${it.prompt}-${i}`} className={`overnight-item ${it.status}`}>
                  <span className="overnight-item-prompt">{it.prompt}</span>
                  {it.result && <span className="overnight-item-result dim-note">{it.result}</span>}
                </li>
              ))}
            </ul>
          )}
          <div className="overnight-queue">
            <label className="sr-only" htmlFor="overnight-task">
              What should DARWIN look into overnight?
            </label>
            <input
              id="overnight-task"
              className="overnight-task"
              type="text"
              value={task}
              placeholder="what should I look into overnight?"
              maxLength={4096}
              disabled={!shell || busy || !overnight.enabled}
              onChange={(e) => setTask(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") void queue();
              }}
            />
            <button
              type="button"
              className="overnight-queue-btn"
              onClick={() => void queue()}
              disabled={!control.canSend}
              title={control.reason}
            >
              {busy ? "QUEUEING…" : "QUEUE"}
            </button>
          </div>
          {result ? <div className="overnight-result dim-note">{result}</div> : null}
          <div className="overnight-foot dim-note">
            Runs your queued tasks while you&rsquo;re away and folds the results
            into a morning brief. Overnight agents are tool-less — they research
            and draft, but can never send, buy, or change anything; that waits for
            your confirmation.
          </div>
        </div>
      </Frame>
    </div>
  );
}

function agentState(o: OvernightStatus): { label: string; cls: string } {
  if (!o.enabled) return { label: "OFF", cls: "off" };
  if (!o.cloudKeyPresent) return { label: "ARMED · NEEDS KEY", cls: "armed" };
  return { label: "READY", cls: "ready" };
}
