import type { ForgeAlert, ForgeProposal } from "../core/state";
import Frame from "./Frame";

const ALERT_TITLES: Record<ForgeAlert["kind"], string> = {
  rejected: "SELF-FORGE DRAFT REJECTED",
  blocked: "SELF-FORGE BLOCKED",
};

/** Red error banner for a rejected/blocked forge attempt (mirrors the heal
 *  AlertPanel). NOT shown for the shipped-OFF "disabled" state — the reducer
 *  never raises a forgeAlert for that, so this only ever shows a real failure.
 *  Carries a short reason only; NEVER a secret. */
function ForgeAlertBanner({
  alert,
  onDismiss,
}: {
  alert: ForgeAlert;
  onDismiss: () => void;
}) {
  return (
    <div className="alert-panel red" role="alert">
      <svg viewBox="0 0 20 18" className="tri" aria-hidden="true">
        <path d="M10 1.5 19 16.5 H1 Z" fill="none" strokeWidth="1.4" />
        <line x1="10" y1="7" x2="10" y2="11.4" strokeWidth="1.6" />
        <circle cx="10" cy="13.8" r="0.9" stroke="none" />
      </svg>
      <div className="alert-body">
        <div className="alert-title">{ALERT_TITLES[alert.kind]}</div>
        <div className="alert-detail">{alert.detail}</div>
      </div>
      <button className="alert-ack" onClick={onDismiss}>
        ACK
      </button>
    </div>
  );
}

/**
 * SELF-FORGE // PROPOSALS — the Self-Forge review surface, mirroring the
 * self-heal review panel (warn-amber "attention" chrome, "HUMAN REVIEW
 * REQUIRED"). It lists the pending forge PROPOSAL: the forged app's name, what
 * the pipeline guarantees (validated in a confined staging copy), and the EXACT
 * manual deploy command (scripts/apply_forge.sh <ts>).
 *
 * SAFETY CONTRACT (do not regress — same posture as the heal panel):
 *   - REVIEW-ONLY. There is deliberately NO button that applies/deploys/installs
 *     or runs the app. The ONLY deploy route is the human running the shown
 *     terminal command after reviewing — surfacing that gated command is the
 *     whole design.
 *   - It makes explicit that NOTHING is installed or running yet: the app was
 *     only built + tested in a confined staging copy and proposed for review.
 *   - It NEVER renders a secret (it carries only the app name + the <ts>).
 *
 * A proposal is an ATTENTION state, NOT an error (warn-amber chrome, like the
 * heal proposal panel) — the rejected/blocked ERROR states live on the red
 * AlertPanel banner, not here. The reducer only ever sets `forgeProposal` from a
 * defensively-parsed forge.proposed event (a malformed payload yields no card),
 * so this component can trust the fields it is handed.
 */
export default function ForgePanel({
  proposal,
  alert,
  onDismiss,
}: {
  proposal: ForgeProposal | null;
  alert: ForgeAlert | null;
  onDismiss: () => void;
}) {
  // The red error banner (rejected/blocked) takes the screen when present; a
  // pending proposal is the warn-amber review card. Nothing to show when both
  // are clear.
  if (alert) {
    return <ForgeAlertBanner alert={alert} onDismiss={onDismiss} />;
  }
  if (!proposal) return null;

  // The EXACT manual deploy command — the ONLY way the app is ever installed.
  const applyCmd = `scripts/apply_forge.sh ${proposal.ts}`;

  return (
    <div className="forge-panel">
      <Frame
        className="self-heal attn"
        title="SELF-FORGE // PROPOSALS"
        tag="HUMAN REVIEW REQUIRED"
      >
        <div className="sh-body">
          <div className="sh-state-line">
            <span className="sh-state-label">APP DRAFTED — READY FOR REVIEW</span>
          </div>

          <div className="sh-row">
            <span className="sh-k">APP</span>
            <span className="sh-v">{proposal.name}</span>
          </div>
          <div className="sh-row">
            <span className="sh-k">WHAT IT DOES</span>
            <span className="sh-v">
              A new sandboxed micro-app authored from your goal, born default-deny
              with minimal declared permissions.
            </span>
          </div>
          <div className="sh-row">
            <span className="sh-k">VALIDATION</span>
            <span className="sh-v sh-pass">
              PASSED — built + tested in a confined staging copy
            </span>
          </div>

          {/* The EXACT manual deploy command — the ONLY install route. */}
          <div className="sh-review">
            <div className="sh-review-label">TO INSTALL (MANUAL — REVIEW FIRST)</div>
            <div className="sh-cmd" role="note">
              <span className="sh-cmd-prompt" aria-hidden="true">
                $
              </span>
              <code>{applyCmd}</code>
            </div>
          </div>

          <div className="sh-safety dim-note">
            Nothing is installed or running yet. The app was only built and tested
            in a confined staging copy and proposed for review — it is NOT in
            apps/, NOT registered, and NOT running. Review the proposal under
            state/forge/proposals/{proposal.ts}/, then run the command above to
            install it. There is no auto-deploy.
          </div>

          <div className="sh-foot">
            <span className="sh-foot-hint dim-note">
              report.md + the authored app (manifest.toml, source, tests) are
              written under state/forge/proposals/{proposal.ts}/
            </span>
            <div className="sh-actions">
              <button className="sh-ack" onClick={onDismiss}>
                DISMISS
              </button>
            </div>
          </div>
        </div>
      </Frame>
    </div>
  );
}
