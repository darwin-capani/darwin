import type {
  AuditAnchor,
  AuditEntry,
  AuditSnapshot,
  LiveGateEvent,
  PolicyDecision,
} from "../core/events";
import Frame from "./Frame";

/**
 * AUDIT // CONSEQUENTIAL GATE — the read-only accountability surface for the
 * crown-jewel consequential gate (daemon/src/audit.rs). It surfaces the daemon's
 * APPEND-ONLY, HASH-CHAINED, tamper-EVIDENT log of every consequential decision:
 * a chain-OK / TAMPER indicator (verify_chain), the recent decisions (newest-
 * first: agent, tool, REDACTED target, the policy decision, and the outcome), the
 * bounded total, and a truncation note when a prune re-rooted the chain. Between
 * authoritative audit.snapshot frames it also folds in the LIVE chokepoint events
 * (policy.blocked / policy.auto_approved / confirm.parked) so it reacts at once.
 *
 * SAFETY + HONESTY CONTRACT (do not regress):
 *   - REVIEW-ONLY. There is NO button here that records, prunes, confirms, denies,
 *     or rewrites the log. Accountability is read-only — the gate decisions are
 *     made at the daemon chokepoints, not here.
 *   - SECRET-FREE. Every field shown is the secret-free subset the daemon emits
 *     (the target is ALREADY redacted twice daemon-side; the raw tool input, the
 *     chain bytes prev_hash/entry_hash, and any token never reach this panel —
 *     there is no such field on the wire to render).
 *   - tamper-EVIDENT, NOT tamper-PROOF. The chain detects a careless edit / insert
 *     / delete / reorder, but a root attacker who rewrites the WHOLE on-disk chain
 *     could make it verify again. The footer says so — it is an integrity
 *     tripwire, not a vault.
 *   - The decisions it shows are governed by the policy + the master switch + the
 *     voice-id + the confirmation gate; ALWAYS is a deliberate, logged, master-
 *     gated loosening (inert when the master switch is OFF), and NEVER always wins.
 */
export default function AuditPanel({
  audit,
  liveGate,
}: {
  audit: AuditSnapshot | null;
  liveGate: LiveGateEvent[];
}) {
  // No snapshot AND no live event yet — render nothing rather than a placeholder,
  // mirroring the other event-fed panels.
  if (audit === null && liveGate.length === 0) return null;

  const enabled = audit?.enabled ?? true;
  const chain = audit?.chain ?? null;
  const entries = audit?.entries ?? [];
  const total = audit?.total ?? 0;
  const truncated = audit?.truncated ?? false;

  // The chain indicator: OK (verified suffix) / TAMPER (broken) / AWAITING (no
  // snapshot loaded yet). Fail toward the honest "can't confirm" — never a false
  // green.
  const chainState = chain === null ? "awaiting" : chain.ok ? "ok" : "broken";

  return (
    <div className="audit-panel">
      <Frame title="AUDIT // CONSEQUENTIAL GATE" tag="REVIEW ONLY">
        <div className="audit-body">
          {!enabled ? (
            <div className="audit-off dim-note">
              The audit log is OFF. Consequential decisions are still gated
              (confirmation + master switch + voice-id), but they are not being
              recorded. Enable <code>[audit].enabled</code> in darwin.toml to keep a
              tamper-evident record.
            </div>
          ) : (
            <>
              {/* The chain-OK / TAMPER indicator. */}
              <div className={`audit-chain ${chainState}`}>
                <span className="audit-chain-led" aria-hidden="true" />
                <span className="audit-chain-label">
                  {chainState === "ok"
                    ? "CHAIN OK"
                    : chainState === "broken"
                      ? "CHAIN TAMPER DETECTED"
                      : "CHAIN — AWAITING"}
                </span>
                <span className="audit-chain-detail dim-note">
                  {chainState === "ok"
                    ? `${chain!.count} entr${chain!.count === 1 ? "y" : "ies"} verified`
                    : chainState === "broken"
                      ? `broke at #${chain!.brokenSeq ?? "?"} — ${chain!.reason ?? "verification failed"}`
                      : "no snapshot yet"}
                </span>
              </div>

              {/* The EXTERNAL WITNESS, directly under the chain verdict it
                  qualifies — the two rows are allowed to disagree, and when they
                  do that disagreement IS the finding. */}
              <AnchorRow anchor={audit?.anchor ?? null} />

              {truncated && (
                <div className="audit-truncated dim-note">
                  The log was pruned at its retention cap — the oldest entries were
                  dropped and the chain RE-ROOTED. The surviving suffix still
                  verifies as a fresh chain from its new root (the gap is explicit,
                  not silent).
                </div>
              )}

              {/* The decision timeline: live events first (immediate), then the
                  authoritative snapshot entries. */}
              <div className="audit-timeline">
                {liveGate.length === 0 && entries.length === 0 ? (
                  <div className="audit-empty dim-note">
                    No consequential decision recorded yet. With an empty policy
                    every consequential action ASKS (parks for a spoken
                    confirmation) — exactly today&apos;s gate.
                  </div>
                ) : (
                  <>
                    {liveGate.map((ev) => (
                      <LiveRow key={`live-${ev.seq}`} ev={ev} />
                    ))}
                    {entries.map((e) => (
                      <EntryRow key={`seq-${e.seq}`} entry={e} />
                    ))}
                  </>
                )}
              </div>

              {total > entries.length && (
                <div className="audit-more dim-note">
                  Showing the {entries.length} most recent of {total} recorded
                  decisions (the full bounded log lives on-device).
                </div>
              )}
            </>
          )}

          <div className="audit-foot dim-note">
            This log is tamper-EVIDENT, not tamper-PROOF: the hash chain catches a
            careless edit, insert, delete, or reorder, but a root attacker who
            rewrites the WHOLE on-disk chain could make it verify again. It is an
            integrity tripwire, not a vault. Nothing here is a secret — the target
            is already redacted and the raw input never reaches the log. The master
            switch + voice-id + the confirmation gate remain the hard backstop;
            ALWAYS is a deliberate, logged, master-gated loosening, and NEVER always
            wins.
          </div>
        </div>
      </Frame>
    </div>
  );
}

/**
 * EXTERNAL ANCHOR — the Keychain witness row.
 *
 * WHY THIS ROW EXISTS. The footer of this panel has always admitted the hole: "a
 * root attacker who rewrites the WHOLE on-disk chain could make it verify again."
 * The daemon has had the detector for precisely that since the anchor landed — it
 * witnesses the chain head into the macOS Keychain, a protection domain the
 * SQLite-file attacker cannot silently rewrite, and re-checks it at every start.
 * On a divergence it warns, emits `audit.anchor`, and DELIBERATELY leaves the
 * witness alone so the finding keeps resurfacing. But `applyEnvelope` had no case
 * for that topic, so what a rewritten audit log looked like, from this seat, was a
 * green CHAIN OK and a line in daemon.log. The verdict now rides `audit.snapshot`.
 *
 * HONESTY CONTRACT:
 *   - It is a BOOT-TIME reading re-broadcast on the snapshot cadence, so it is
 *     dated by its own `checkedTs` and never by the 15s tick that carried it.
 *   - CORROBORATED (green) only for a verdict the daemon actually reached; an
 *     unrecognized state reads UNCLEAR, and a daemon that sent no anchor at all
 *     renders NOTHING rather than either a green shield or a false alarm.
 *   - REVIEW-ONLY, like the rest of this panel. The row states the remedy in
 *     words; there is no button here that re-witnesses, and there must not be —
 *     a one-click "re-anchor" is a one-click way to bless a rewritten chain.
 */
function AnchorRow({ anchor }: { anchor: AuditAnchor | null }) {
  // No verdict on the wire: an older daemon, or one that never got to check.
  // Claim nothing — this row must not invent either colour.
  if (anchor === null) return null;

  const diverged = anchor.state === "mismatch";
  const cls = anchor.ok ? "ok" : diverged ? "broken" : "awaiting";
  const label = anchor.ok
    ? "EXTERNAL ANCHOR OK"
    : diverged
      ? "EXTERNAL ANCHOR DIVERGED"
      : anchor.state === "no_anchor"
        ? "EXTERNAL ANCHOR — NOT YET WITNESSED"
        : "EXTERNAL ANCHOR — UNCLEAR";

  return (
    <>
      <div className={`audit-anchor ${cls}`}>
        <span className="audit-chain-led" aria-hidden="true" />
        <span className="audit-chain-label">{label}</span>
        <span className="audit-chain-detail dim-note">
          {anchor.ok
            ? `Keychain witness corroborates the chain${seqNote(anchor)}`
            : diverged
              ? `witnessed #${anchor.anchoredSeq ?? "?"}, live head ${
                  anchor.liveSeq === null ? "GONE — the chain is empty" : `#${anchor.liveSeq}`
                }`
              : anchor.state === "no_anchor"
                ? "no witness was stored when this daemon started — one is written at startup once there is a chain to witness"
                : `the stored witness could not be read (${anchor.state})`}
          {checkedNote(anchor.checkedTs)}
        </span>
      </div>
      {diverged && (
        <div className="audit-anchor-note">
          The audit log&apos;s head no longer matches the witness held in your
          Keychain. Ordinary growth and ordinary pruning both corroborate — a
          divergence is what a REWRITTEN log looks like, and the local chain can
          verify clean while this disagrees. Treat the recent timeline as
          unproven, capture a security triage bundle to preserve the evidence
          (that also re-witnesses the current head), and check what had access to{" "}
          <code>state/audit.db</code>. DARWIN will not re-witness on its own: doing
          so silently would bless the rewrite.
        </div>
      )}
    </>
  );
}

/** " at #412" when the daemon named a seq, otherwise nothing — never "#?" on the
 *  reassuring path, where a fabricated detail would read as corroboration. */
function seqNote(anchor: AuditAnchor): string {
  return anchor.anchoredSeq === null ? "" : ` at #${anchor.anchoredSeq}`;
}

/** " · checked HH:MM" — the age of the READING, not of the frame that carried it.
 *  Omitted entirely when the daemon sent no stamp. */
function checkedNote(ts: string | null): string {
  if (ts === null) return "";
  const at = clock(ts);
  return at === "" ? "" : ` · checked ${at}`;
}

/** One authoritative audit entry row (from a snapshot). Shows the secret-free
 *  fields only. */
function EntryRow({ entry }: { entry: AuditEntry }) {
  return (
    <div className="audit-row">
      <span className="audit-seq">#{entry.seq}</span>
      <DecisionPill decision={entry.decision} />
      <OutcomePill outcome={entry.outcome} />
      <span className="audit-agent">{entry.agent || "—"}</span>
      <span className="audit-tool">{entry.tool}</span>
      {entry.target ? (
        <span className="audit-target" title="redacted target summary (secret-free)">
          {entry.target}
        </span>
      ) : null}
      <span className="audit-ts dim-note">{clock(entry.ts)}</span>
    </div>
  );
}

/** One LIVE chokepoint event row (between snapshots). Carries no target — the
 *  chokepoint events are tool/agent only. Badged LIVE so it reads as the
 *  immediate surface, not the durable record. */
function LiveRow({ ev }: { ev: LiveGateEvent }) {
  const label =
    ev.kind === "blocked"
      ? "BLOCKED"
      : ev.kind === "auto_approved"
        ? "AUTO-APPROVED"
        : "PARKED";
  const cls =
    ev.kind === "blocked" ? "never" : ev.kind === "auto_approved" ? "always" : "ask";
  return (
    <div className="audit-row live">
      <span className="audit-live-tag" title="live chokepoint event (between snapshots)">
        LIVE
      </span>
      <span className={`audit-outcome ${cls}`}>{label}</span>
      <span className="audit-agent">{ev.agent || "—"}</span>
      <span className="audit-tool">{ev.tool}</span>
      {ev.via ? <span className="audit-via dim-note">{ev.via}</span> : null}
      <span className="audit-ts dim-note">{clock(ev.ts)}</span>
    </div>
  );
}

/** The policy DECISION this entry rendered (always/never/ask). */
function DecisionPill({ decision }: { decision: PolicyDecision }) {
  const label =
    decision === "always" ? "ALWAYS" : decision === "never" ? "NEVER" : "ASK";
  return <span className={`audit-decision ${decision}`}>{label}</span>;
}

/** The OUTCOME (what actually happened). Maps the known daemon tokens to a short
 *  label + a tone class; an unknown future token renders verbatim (forward-
 *  tolerant) with a neutral tone. */
function OutcomePill({ outcome }: { outcome: string }) {
  const map: Record<string, { label: string; tone: string }> = {
    proposed: { label: "PROPOSED", tone: "neutral" },
    parked: { label: "PARKED", tone: "ask" },
    blocked_by_policy: { label: "BLOCKED", tone: "never" },
    auto_approved_by_policy: { label: "AUTO-APPROVED", tone: "always" },
    always_inert_master_off: { label: "INERT (MASTER OFF)", tone: "neutral" },
    confirmed: { label: "CONFIRMED", tone: "ok" },
    denied: { label: "DENIED", tone: "never" },
    executed: { label: "EXECUTED", tone: "ok" },
    dry_run: { label: "DRY-RUN", tone: "neutral" },
  };
  const m = map[outcome] ?? { label: outcome.toUpperCase() || "—", tone: "neutral" };
  return <span className={`audit-outcome ${m.tone}`}>{m.label}</span>;
}

/** Render an rfc3339 ts as a compact local HH:MM:SS, or "" if it does not parse
 *  (never throw on an odd ts). */
function clock(ts: string): string {
  const d = new Date(ts);
  if (Number.isNaN(d.getTime())) return "";
  return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });
}
