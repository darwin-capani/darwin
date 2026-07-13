import type { RewindItem, SessionRewind } from "../core/events";
import Frame from "./Frame";

/**
 * REWIND // SESSION TIMELINE — the review-only reconstruction of an asked time
 * window (daemon rewind.rs -> `session.rewind`): recorded turns (the redacted,
 * privacy-gated episode record) interleaved with the audit log's gated-action
 * verdicts, oldest first.
 *
 * HONESTY CONTRACT (do not regress):
 *   - REVIEW-ONLY: nothing here re-executes, re-parks, or replays anything —
 *     the panel only shows what was recorded, and says so.
 *   - The record is the GATED record: transient turns (screen reads) and
 *     voice-unverified turns are absent by privacy design; the footer says
 *     "recorded", never "everything".
 *   - Items past the daemon's cap arrive disclosed (`itemsOmitted`) and are
 *     shown as "+N more" — a capped list is never presented as complete.
 */
export default function SessionRewindPanel({ rewind }: { rewind: SessionRewind | null }) {
  if (rewind === null) return null;

  return (
    <div className="rewind-panel">
      <Frame title="REWIND // SESSION TIMELINE" tag="REVIEW ONLY">
        <div className="rewind-body">
          <div className="rewind-head">
            <span className="rewind-pill">{rewind.label.toUpperCase()}</span>
            <span className="rewind-counts dim-note">
              {rewind.countsFloor ? "\u2265" : ""}
              {rewind.turnCount} turn{rewind.turnCount === 1 ? "" : "s"} ·{" "}
              {rewind.countsFloor ? "\u2265" : ""}
              {rewind.actionCount} gated action{rewind.actionCount === 1 ? "" : "s"}
              {rewind.itemsOmitted > 0
                ? ` · ${rewind.itemsOmitted} earlier not shown`
                : ""}
            </span>
          </div>
          {rewind.empty ? (
            <div className="rewind-empty dim-note">
              nothing recorded in this window — the episode log keeps only
              completed, non-transient turns
            </div>
          ) : (
            <ul className="rewind-items">
              {rewind.items.map((item, i) => (
                <ItemRow key={`${item.ts}-${i}`} item={item} />
              ))}
            </ul>
          )}
          <div className="rewind-foot dim-note">
            Review only — reconstructed from the recorded episode + audit
            stores; nothing is re-executed.
          </div>
        </div>
      </Frame>
    </div>
  );
}

/** One timeline row: local wall-clock time, a kind marker, the recorded line. */
function ItemRow({ item }: { item: RewindItem }) {
  return (
    <li className={`rewind-item ${item.kind}`}>
      <span className="rewind-ts dim-note">{clock(item.ts)}</span>
      <span className={`rewind-kind ${item.kind}`}>
        {item.kind === "action" ? "ACTION" : "TURN"}
      </span>
      <span className="rewind-text">{item.text}</span>
      {item.detail !== "" && item.detail !== item.text && (
        <span className="rewind-detail dim-note">{item.detail}</span>
      )}
    </li>
  );
}

/** Local HH:MM from an RFC3339 stamp; empty when unparsable (never fabricated). */
function clock(ts: string): string {
  const t = new Date(ts);
  if (Number.isNaN(t.getTime())) return "";
  return `${String(t.getHours()).padStart(2, "0")}:${String(t.getMinutes()).padStart(2, "0")}`;
}
