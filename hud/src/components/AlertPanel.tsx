import { calibrationRows } from "../core/heal";
import type { HealAlert } from "../core/state";

/**
 * Self-heal ERROR banner — warning-triangle + alert-red language. RED is
 * reserved for genuine alerts: a *rejected* or *blocked* heal attempt, or the
 * (opt-in, dangerous) auto-mode *applied* notice signalling a live mutation.
 *
 * A validated, pending PROPOSAL is NOT an error and is intentionally NOT
 * surfaced here — SelfHealPanel renders it in warn-amber with the gated
 * scripts/apply_heal.sh review command (self-heal v2 safety contract). The
 * `HealAlert` type no longer carries a "proposal" kind, so this panel cannot
 * regress into showing one.
 *
 * Persistent until acknowledged: the banner survives ticks until the user ACKs
 * it or a newer heal event replaces it.
 */

function TriIcon() {
  return (
    <svg viewBox="0 0 20 18" className="tri" aria-hidden="true">
      <path d="M10 1.5 19 16.5 H1 Z" fill="none" strokeWidth="1.4" />
      <line x1="10" y1="7" x2="10" y2="11.4" strokeWidth="1.6" />
      <circle cx="10" cy="13.8" r="0.9" stroke="none" />
    </svg>
  );
}

const TITLES: Record<HealAlert["kind"], string> = {
  rejected: "SELF-HEAL PATCH REJECTED",
  blocked: "SELF-HEAL BLOCKED",
  applied: "SELF-HEAL PATCH APPLIED",
};

export default function AlertPanel({
  alert,
  onDismiss,
}: {
  alert: HealAlert | null;
  onDismiss: () => void;
}) {
  if (!alert) return null;
  // rejected/blocked are hard failures (red); applied is a live-mutation
  // notice — alert-worthy, but leans cyan rather than failure-red.
  const red = alert.kind === "rejected" || alert.kind === "blocked";
  // A REJECTION is the half of the calibration payload that matters most: a
  // "deadline" rejection means the budget stopped the gate and a "confidence"
  // one means the floor did, and the banner's sentence alone gives the operator
  // no number to change. Only the rejection carries one (blocked never ran;
  // applied consumed its proposal), so this is empty on the other two kinds.
  const calib = alert.calibration === null ? [] : calibrationRows(alert.calibration);

  return (
    <div className={`alert-panel ${red ? "red" : ""}`} role="alert">
      <TriIcon />
      <div className="alert-body">
        <div className="alert-title">{TITLES[alert.kind]}</div>
        <div className="alert-detail">
          {alert.detail}
          {alert.files.length > 0 && (
            <>
              {" — "}
              {alert.files.length} FILE{alert.files.length === 1 ? "" : "S"}:{" "}
              {alert.files.join(", ")}
            </>
          )}
          {/* DEAD FIELD (same class as `calibration` above): the reducer has
              always parsed the daemon's attempt `ts` into `refTs` for the
              rejected and applied kinds, and NO component read it — so the one
              number that names the attempt never reached a pixel. It is not
              decoration: heal.rs `record_artifact` writes the rejected
              attempt's patch.diff and report.md to state/heal/rejected/<ts>/,
              and rejectionDetail's own sentence points at the PARENT directory
              without saying which subdirectory to open. null on `blocked` (the
              pipeline never ran, so there is no attempt to name). */}
          {alert.refTs !== null && (
            <>
              {" — ATTEMPT "}
              {alert.refTs}
              {alert.kind === "rejected" &&
                ` (artifacts: state/heal/rejected/${alert.refTs}/)`}
            </>
          )}
        </div>
        {calib.length > 0 && (
          <ul className="alert-calib">
            {calib.map((r) => (
              <li key={r.key} className={r.warn ? "warn" : undefined}>
                <span className="alert-calib-k">{r.label}</span> {r.text}
              </li>
            ))}
          </ul>
        )}
      </div>
      <button className="alert-ack" onClick={onDismiss}>
        ACK
      </button>
    </div>
  );
}
