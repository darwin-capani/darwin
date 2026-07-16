import type { FleetStatus } from "../core/events";
import Frame from "./Frame";

/**
 * FLEET // POLICY — OVERWATCH: the honest state of the E2E-encrypted, signed
 * policy BASELINE the owner authored on one Mac and that every device enforces as
 * a FLOOR OF STRICTNESS (daemon fleet.rs).
 *
 * HONESTY CONTRACT (do not regress):
 *   - SHIPS OFF: OFF unless the operator enables [fleet]. The pill says so.
 *   - HARDENS ONLY: a ceiling can only force a tool STRICTER (Never / always-Ask);
 *     it can NEVER loosen a device, never grant what the master switch forbids,
 *     never override a stricter LOCAL rule. `hardensOnly` is pinned true.
 *   - OWNER-AUTHORED / SIGNED: a baseline enters ONLY as a sealed owner-authored
 *     bundle (the shared-key seal IS the signature) — never written from a model.
 *     The authoring device is shown so it's clear WHO set the ceilings.
 *   - ARMED · AWAITING BASELINE until a signed baseline is installed.
 */
export default function FleetPanel({ fleet }: { fleet: FleetStatus | null }) {
  if (fleet === null) return null;

  const state = pipelineState(fleet);
  return (
    <div className="fleet-panel">
      <Frame title="FLEET // POLICY" tag="OVERWATCH · HARDENS ONLY">
        <div className="fleet-body">
          <div className="fleet-head">
            <span className={`fleet-pill ${state.cls}`}>{state.label}</span>
            {fleet.baselineActive && (
              <span className="fleet-author dim-note">
                authored by {fleet.authoredBy || "an unknown device"}
              </span>
            )}
          </div>
          {fleet.baselineActive && fleet.rules.length > 0 && (
            <ul className="fleet-rules">
              {fleet.rules.map((r) => (
                <li key={`${r.tool}:${r.decision}`} className="fleet-rule">
                  <span className="fleet-rule-tool">{r.tool}</span>
                  <span className={`fleet-rule-decision ${r.decision}`}>
                    {r.decision === "never" ? "NEVER" : "ALWAYS ASK"}
                  </span>
                </li>
              ))}
            </ul>
          )}
          {fleet.enabled && fleet.baselineActive && fleet.rules.length === 0 && (
            <div className="fleet-empty dim-note">
              A baseline is active but sets no ceilings — every tool follows this
              device&rsquo;s own policy.
            </div>
          )}
          <div className="fleet-foot dim-note">
            A signed baseline from one of your Macs, enforced here as a floor of
            strictness. It can only tighten a tool (block it, or force a fresh
            confirmation) — never loosen one, never grant what your master switch
            forbids, never override a stricter local rule.
          </div>
        </div>
      </Frame>
    </div>
  );
}

function pipelineState(f: FleetStatus): { label: string; cls: string } {
  if (!f.enabled) return { label: "OFF", cls: "off" };
  if (!f.baselineActive) return { label: "ARMED · AWAITING BASELINE", cls: "armed" };
  return { label: "ACTIVE", cls: "ready" };
}
