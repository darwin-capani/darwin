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
        </div>
      </div>
      <button className="alert-ack" onClick={onDismiss}>
        ACK
      </button>
    </div>
  );
}
