import { useCallback, useState } from "react";
import type { SyncStatus } from "../core/events";
import { syncNowState } from "../core/queueControls";
import { inTauri } from "../tauri/bridge";
import { sendCommand } from "../tauri/command";
import Frame from "./Frame";

/**
 * SYNC // FEDERATED MEMORY — the honest state of the E2E-encrypted cross-device
 * fact sync (daemon sync.rs), and the control that runs a pass.
 *
 * HONESTY CONTRACT (do not regress):
 *   - SHIPS OFF: OFF unless the operator enables [sync]. The pill says so.
 *   - E2E-ENCRYPTED: a bundle never leaves the box unsealed; the transport is
 *     armed-but-inert (moving a sealed bundle to a paired device is the
 *     device-gated leg). ARMED · NEEDS PAIRING until a shared key exists.
 *   - NEVER SILENTLY CLOBBERS: a divergence between devices is surfaced as a
 *     pending conflict, never an invisible overwrite.
 *   - HONEST SCOPE: deletions don't propagate (no tombstones) — the footnote
 *     says so rather than implying a full mirror.
 *
 * THE CONTROL. This panel shipped read-only and the `sync` verb had no client at
 * all — it was not even in the Tauri relay's allow-list — so `sync::sync_now`'s
 * only caller was its own command arm and the panel reported on a pass nothing
 * could start.
 *
 * WHAT A CLICK DOES. `sync_now` gathers this device's syncable facts, SEALS them
 * into the local outbox, and merges any sealed bundle a paired device left in the
 * inbox. With no shared pairing key it refuses rather than writing anything in
 * the clear.
 *
 * IT CAN REACH THE NETWORK, AND THIS PANEL SAYS SO. When `[sync].peer_endpoint`
 * is configured, `sync_now` also POSTs the sealed bundle to it (`transport_push`,
 * a reqwest call). "Armed-but-inert" is the UNPAIRED case ONLY — the daemon's own
 * `status_payload` derives `transport_inert = !peer_configured` for exactly this
 * reason, after the pinned-true version was found asserting the facts stay home
 * at the moment they leave. So the footnote and the button's title are
 * CONDITIONED on `peerConfigured` rather than promising an inertness that stops
 * being true the moment a peer is set.
 *
 * It adds no AUTHORITY: the payload is facts, sealed, addressed to the owner's
 * own Mac, and nothing consequential runs at the far end.
 */
export default function SyncPanel({ sync }: { sync: SyncStatus | null }) {
  const shell = inTauri();
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<string | null>(null);

  const syncNow = useCallback(async () => {
    if (!syncNowState(sync, shell, busy).canSend) return;
    setBusy(true);
    setResult(null);
    try {
      const reply = await sendCommand({ cmd: "sync" });
      // sync_now's own prose — off / no key / sealed N facts / conflicts parked.
      // Never a fabricated "synced".
      setResult(reply.ok ? reply.reply ?? "Done." : reply.error ?? "Could not sync.");
    } catch {
      setResult("command failed — nothing was sealed");
    } finally {
      setBusy(false);
    }
  }, [sync, shell, busy]);

  if (sync === null) return null;

  const state = pipelineState(sync);
  const control = syncNowState(sync, shell, busy);
  return (
    <div className="sync-panel">
      <Frame title="SYNC // FEDERATED MEMORY" tag="E2E · NEVER PLAINTEXT">
        <div className="sync-body">
          <div className="sync-head">
            <span className={`sync-pill ${state.cls}`}>{state.label}</span>
            <span className="sync-facts dim-note">
              {sync.syncableFacts} fact{sync.syncableFacts === 1 ? "" : "s"} syncable
            </span>
          </div>
          {sync.pendingConflicts > 0 && (
            <div className="sync-conflicts">
              {sync.pendingConflicts} conflict{sync.pendingConflicts === 1 ? "" : "s"} to
              resolve — a peer's value never overwrote yours silently
            </div>
          )}
          <div className="sync-controls">
            <button
              type="button"
              className="sync-btn"
              onClick={() => void syncNow()}
              disabled={!control.canSend}
              title={control.reason}
            >
              {busy ? "SEALING…" : "SYNC NOW"}
            </button>
          </div>
          {result ? <div className="sync-result dim-note">{result}</div> : null}
          <div className="sync-foot dim-note">
            Your facts sync across your own devices, end-to-end encrypted (a
            bundle never leaves the box unsealed).{" "}
            {sync.peerConfigured
              ? "A paired endpoint is configured, so SYNC NOW also sends the sealed bundle to it over the network."
              : "The network transport is armed but inert — no peer endpoint is configured, so nothing leaves this Mac."}{" "}
            Deletions don&rsquo;t propagate yet.
          </div>
        </div>
      </Frame>
    </div>
  );
}

function pipelineState(s: SyncStatus): { label: string; cls: string } {
  if (!s.enabled) return { label: "OFF", cls: "off" };
  if (!s.keyPresent) return { label: "ARMED · NEEDS PAIRING", cls: "armed" };
  return { label: "ARMED · PAIRED", cls: "ready" };
}
