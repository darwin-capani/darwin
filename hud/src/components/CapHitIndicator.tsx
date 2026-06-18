/**
 * Mark-Forge cap-hit indicator — surfaces the engine's per-substep bounded-work
 * signals in the HUD panel.
 *
 * `pairs_cap_hit` / `contact_cap_hit` are REAL booleans the engine EMITS on its
 * `physics.step` telemetry (StepReport, apps/mark-forge/src/ipc.rs): a per-substep
 * budget bit on at least one substep during the step. When set, the engine
 * deterministically bounded the work (broadphase pair-enumeration / contact-solve
 * cap) and SIGNALLED it, rather than silently mis-simulating a degenerate /
 * over-dense scene.
 *
 * HONESTY: this is a RECEIVED telemetry flag, not a HUD-measured value — it shares
 * the panel's device-gated framing (the render is device-gated; the engine's
 * signals are real). Renders nothing when neither flag is set (the normal case),
 * so it stays unobtrusive.
 *
 * Pure + R3F-free on purpose, so it is render-testable in isolation
 * (react-dom/server, no DOM/jsdom) without dragging in the panel's three.js Canvas.
 */
export interface CapHitIndicatorProps {
  /** Per-substep candidate-pair budget (MAX_PAIRS_PER_SUBSTEP) bit this step. */
  pairsCapHit: boolean;
  /** Per-substep contact-solve budget (MAX_CONTACTS_PER_SUBSTEP) bit this step. */
  contactCapHit: boolean;
}

export default function CapHitIndicator({
  pairsCapHit,
  contactCapHit,
}: CapHitIndicatorProps) {
  if (!pairsCapHit && !contactCapHit) return null;
  return (
    <div
      className="mf-cap-hit"
      title="Engine-reported bounded-work signal: a per-substep budget bit this step — the work was deterministically bounded and signalled, not silently mis-simulated. A received telemetry flag, not a HUD-measured value."
    >
      {contactCapHit ? <span className="mf-cap-pill">CONTACT CAP HIT</span> : null}
      {pairsCapHit ? <span className="mf-cap-pill">PAIR BUDGET HIT</span> : null}
    </div>
  );
}
