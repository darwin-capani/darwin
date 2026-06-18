import type { EvalReport, OptimizerProposal } from "../core/events";
import Frame from "./Frame";

/**
 * EVAL // OPTIMIZER — the MEASURED scorecard + the propose-only optimizer status.
 *
 * Fed by the daemon's periodic `eval.report` telemetry (daemon/src/eval.rs,
 * every 30s with a 20s startup delay) and the propose-only optimizer's
 * `optimize.proposed` events (daemon/src/optimize.rs). AGGREGATE-ONLY +
 * REVIEW-ONLY by construction:
 *
 *   - MEASURED, NEVER FABRICATED — every number here is a real measurement the
 *     daemon already took: latency p50/p95 from the pipeline clocks, token sums
 *     from the cloud `usage` counters, routing accuracy from the held-out trace
 *     split, correction rate from recorded traces. A metric whose window/corpus
 *     is empty reads "AWAITING TURNS" — never an invented value.
 *   - RUNTIME-GATED LIVE FEED — latency + cost are fed by REAL turns/cloud calls,
 *     so a fresh daemon shows them "awaiting turns" until real turns happen. The
 *     panel says so plainly (the runtime-gated foot note).
 *   - COST $ IS AN ESTIMATE — the dollar figure is a published $/1M multiplier
 *     over the measured token sums, NOT a billed number. Always labelled EST.
 *   - OPTIMIZER IS PROPOSE-ONLY + OFF BY DEFAULT — it NEVER auto-tunes routing.
 *     A pending proposal is shown READ-ONLY with the MANUAL apply command
 *     (scripts/apply_optimization.sh <ts>); there is deliberately no one-click
 *     apply. The panel never mutates anything.
 *
 * The reducer only ever sets `evalReport` from a defensively-parsed `eval.report`
 * (parseEvalReport never returns null and never carries PII) and
 * `optimizerProposal` from a parsed `optimize.proposed` (cleared on a
 * none/suppressed round), so this component can trust the fields it is handed.
 */
export default function EvalPanel({
  report,
  proposal,
}: {
  report: EvalReport | null;
  proposal: OptimizerProposal | null;
}) {
  // No report yet (daemon has not emitted eval.report — the first lands ~20s
  // after startup) — render nothing rather than a placeholder, mirroring the
  // other event-fed panels (McpPanel, MemoryPanel).
  if (report === null) return null;

  const { latency, cost, accuracy, optimizer } = report;
  const optimizerOff = !optimizer.enabled;

  return (
    <div className="eval-panel">
      <Frame title="EVAL // OPTIMIZER" tag="MEASURED · REVIEW ONLY">
        <div className="eval-body">
          {/* LATENCY — measured p50/p95 from the pipeline clocks. */}
          <section className="eval-section">
            <div className="eval-section-head">
              <span className="eval-section-title">LATENCY</span>
              <span className="eval-runtime-pill" title="fed by real turns — runtime-gated">
                LIVE TURNS
              </span>
            </div>
            {latency.measured ? (
              <div className="eval-metrics">
                <Stat label="TOTAL p50" value={`${latency.totalP50Ms} ms`} prominent />
                <Stat label="TOTAL p95" value={`${latency.totalP95Ms} ms`} prominent />
                <Stat label="STT p50" value={`${latency.sttP50Ms} ms`} />
                <Stat label="CLASSIFY p50" value={`${latency.classifyP50Ms} ms`} />
                <Stat label="ROUTE p50" value={`${latency.routeP50Ms} ms`} />
                <Stat label="QUEUE p50" value={`${latency.queueP50Ms} ms`} />
                <span className="eval-n">n={latency.n}</span>
              </div>
            ) : (
              <Awaiting />
            )}
          </section>

          {/* COST — rolling token sums + a transparently-LABELLED $ estimate. */}
          <section className="eval-section">
            <div className="eval-section-head">
              <span className="eval-section-title">COST</span>
              <span className="eval-runtime-pill" title="fed by real cloud calls — runtime-gated">
                CLOUD CALLS
              </span>
            </div>
            {cost.measured ? (
              <div className="eval-metrics">
                <Stat label="TOTAL TOKENS" value={fmtTokens(cost.totalTokens)} prominent />
                <Stat
                  label="EST. $"
                  value={`~$${fmtUsd(cost.estCostUsd)}`}
                  prominent
                  title="ESTIMATE — published $/1M-token rate over the measured token sums, not a billed figure"
                />
                <Stat label="INPUT" value={fmtTokens(cost.inputTokens)} />
                <Stat label="OUTPUT" value={fmtTokens(cost.outputTokens)} />
                <Stat label="CACHE READ" value={fmtTokens(cost.cacheReadTokens)} />
                <span className="eval-n">n={cost.n}</span>
                <span className="eval-estimate-note dim-note">$ is an estimate</span>
              </div>
            ) : (
              <Awaiting />
            )}
          </section>

          {/* ACCURACY — routing score (held-out) + the live correction rate. */}
          <section className="eval-section">
            <div className="eval-section-head">
              <span className="eval-section-title">ACCURACY</span>
            </div>
            <div className="eval-metrics">
              <Stat
                label="ROUTING SCORE"
                value={accuracy.routingAccuracy === null ? null : fmtPct(accuracy.routingAccuracy)}
                sub={`held-out n=${accuracy.heldOutN}`}
                prominent
                title="routing accuracy over the same held-out trace split the optimizer judges on"
              />
              <Stat
                label="CORRECTION RATE"
                value={accuracy.correctionRate === null ? null : fmtPct(accuracy.correctionRate)}
                sub={`${accuracy.corrections}/${accuracy.usableN} turns`}
                prominent
                title="share of turns the user re-routed on the next turn — the optimizer's learnable signal"
              />
            </div>
          </section>

          {/* OPTIMIZER STATUS — OFF / mode, always PROPOSE-ONLY. */}
          <section className="eval-section eval-optimizer">
            <div className="eval-section-head">
              <span className="eval-section-title">OPTIMIZER</span>
              <span className={`eval-opt-pill ${optimizerOff ? "off" : "on"}`}>
                {optimizerOff ? "OFF" : `ON · ${optimizer.mode.toUpperCase() || "PROPOSE"}`}
              </span>
              <span className="eval-opt-posture" title="the optimizer only ever writes a reviewable proposal; it never auto-tunes routing">
                {optimizer.posture.toUpperCase()}
              </span>
            </div>

            {proposal !== null ? (
              <ProposalCard proposal={proposal} />
            ) : (
              <div className="eval-opt-note dim-note">
                {optimizerOff
                  ? "Optimizer is OFF (the shipped default). No traces are scored and no proposal is written. It NEVER auto-tunes routing — when enabled it only writes a reviewable proposal you apply by hand."
                  : "No pending proposal. The optimizer is PROPOSE-ONLY: when a candidate beats the held-out baseline it writes a reviewable artifact you apply by hand (scripts/apply_optimization.sh). It never changes live routing on its own."}
              </div>
            )}
          </section>

          <div className="eval-foot dim-note">
            All numbers are MEASURED from real turns; an empty window reads
            "awaiting turns", never a fabricated value. Latency + cost are
            runtime-gated (a live mic + cloud feed them). The optimizer is
            propose-only and ships OFF — it never tunes routing itself.
          </div>
        </div>
      </Frame>
    </div>
  );
}

/** One measured stat cell. `value === null` renders the honest "AWAITING TURNS"
 *  placeholder so a metric with no data is never shown as a fabricated number. */
function Stat({
  label,
  value,
  sub,
  prominent,
  title,
}: {
  label: string;
  value: string | null;
  sub?: string;
  prominent?: boolean;
  title?: string;
}) {
  return (
    <div className={`eval-stat ${prominent ? "prominent" : ""}`} title={title}>
      <span className="eval-stat-label">{label}</span>
      {value === null ? (
        <span className="eval-stat-awaiting">AWAITING TURNS</span>
      ) : (
        <span className="eval-stat-value">{value}</span>
      )}
      {sub !== undefined && <span className="eval-stat-sub">{sub}</span>}
    </div>
  );
}

/** The "awaiting turns" block for a whole runtime-gated section (latency/cost)
 *  whose rolling window is still empty. Honest, never a fake "0 ms". */
function Awaiting() {
  return (
    <div className="eval-awaiting dim-note">
      AWAITING TURNS — no measurements yet. This is fed by real turns; the math is
      proven, the live feed needs activity.
    </div>
  );
}

/** A pending optimizer proposal, READ-ONLY. Shows the measured held-out
 *  improvement + the MANUAL apply command — there is no one-click apply. */
function ProposalCard({ proposal }: { proposal: OptimizerProposal }) {
  return (
    <div className="eval-proposal">
      <div className="eval-proposal-head">
        <span className="eval-proposal-tag">PROPOSAL</span>
        <span className="eval-proposal-improve" title="measured held-out routing-accuracy gain (candidate − baseline)">
          +{fmtPct(proposal.improvement)} ROUTING
        </span>
        <span className="eval-proposal-changes">{proposal.changes} change{proposal.changes === 1 ? "" : "s"}</span>
      </div>
      {proposal.baselineAccuracy !== null && proposal.candidateAccuracy !== null && (
        <div className="eval-proposal-detail dim-note">
          {fmtPct(proposal.baselineAccuracy)} → {fmtPct(proposal.candidateAccuracy)} held-out
        </div>
      )}
      <div className="eval-proposal-apply">
        <span className="eval-proposal-apply-label">APPLY BY HAND</span>
        <code className="eval-proposal-cmd">scripts/apply_optimization.sh {proposal.ts}</code>
      </div>
      <div className="eval-proposal-note dim-note">
        Review-only. Nothing changed in live routing — this is a reviewable
        proposal. Run the command above to adopt it yourself.
      </div>
    </div>
  );
}

/* formatting helpers ------------------------------------------------------- */

/** A 0..1 rate as a 1-dp percentage (e.g. 0.873 -> "87.3%"). */
function fmtPct(v: number): string {
  return `${(v * 100).toFixed(1)}%`;
}

/** A token count with thousands separators (e.g. 12345 -> "12,345"). */
function fmtTokens(n: number): string {
  return n.toLocaleString("en-US");
}

/** A dollar estimate to 4dp (the est. cost is tiny per window; keep precision). */
function fmtUsd(v: number): string {
  return v.toFixed(4);
}
