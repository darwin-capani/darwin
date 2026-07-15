import type { VaultStatus } from "../core/events";
import { vaultLabel, vaultTone } from "../core/events";

/**
 * VAULT MODE ("go dark") indicator — a small at-a-glance chip fed by the daemon's
 * `vault.status` (daemon/src/vault.rs), emitted at startup and on every toggle.
 *
 * It answers one question honestly: is DARWIN forced LOCAL-ONLY right now? When the
 * vault is ACTIVE the router refuses to escalate to the Anthropic cloud fallback
 * (the turn stays on the local MLX brain, or honestly says it can't do this
 * offline) and CUSTOMS is forced to its maximal reduce-only trim.
 *
 * HONESTY CONTRACT (do not regress):
 *   - RESTRICT-ONLY. Vault can only ever REMOVE cloud + tighten the egress trim,
 *     never add either. The chip states this on hover, grounded in the payload's
 *     `restrict_only` contract — it never claims vault took an outward action.
 *   - HONEST SCOPE. Vault forces the local-only cloud DECISION + the maximal
 *     CUSTOMS trim. It does NOT claim to seal every byte off the box (a
 *     user-originated web tool still egresses if the local turn invokes one), so
 *     the copy says exactly what it does and no more.
 *   - SECRET-FREE. The status is a single boolean — there is nothing here to leak.
 *
 * The reducer holds `vault` at null until the daemon emits the startup snapshot, so
 * this renders nothing until there is a real posture to show (mirroring the
 * lockdown chip and the other event-fed indicators).
 */
export default function VaultIndicator({ status }: { status: VaultStatus | null }) {
  // Nothing to show until the daemon emits the startup snapshot. Vault SHIPS OFF, so
  // the honest starting posture is null and we render nothing.
  if (status === null) return null;

  const tone = vaultTone(status);
  const label = vaultLabel(status);
  const title = status.active
    ? "VAULT (go dark) is ENGAGED — this turn stays on the local brain (no cloud escalation) and CUSTOMS is at maximal reduction. Restrict-only: vault only ever removes cloud + tightens the trim, never adds egress. Say “vault mode off” to lift it."
    : "OPEN — cloud routing is available exactly as configured. Say “go dark” to force this turn local-only.";

  return (
    <span
      className={`vault-chip ${tone} ${status.active ? "active" : "inactive"}`}
      data-tone={tone}
      title={title}
    >
      <span className="vault-dot" aria-hidden="true" />
      <span className="vault-label">{label}</span>
    </span>
  );
}
