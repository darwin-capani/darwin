import type { CausaStep, CausaTrace } from "../core/events";
import Frame from "./Frame";

/**
 * CAUSA // DECISION TRACE — the causal decision-trace explainer (daemon
 * explain.rs -> `causa.trace`): the ordered decision steps of a recent turn,
 * surfaced by "why did you do that" / "why <Agent>". A read-only node-list: each
 * step is a stage (intent -> selector -> agent -> route -> identity -> capability
 * -> outcome) with what was chosen and why.
 *
 * HONESTY CONTRACT (do not regress):
 *   - REVIEW-ONLY: nothing here re-executes anything — it explains a past turn.
 *   - The trace is reconstructed ONLY from signals the daemon already recorded;
 *     an unrecorded ask arrives as an HONEST-EMPTY frame and the panel says so,
 *     never showing a fabricated rationale.
 *   - The utterance + outcome are pre-redacted daemon-side (and bounded here);
 *     the step labels are decision tokens, never secrets.
 */
export default function CausaTracePanel({ trace }: { trace: CausaTrace | null }) {
  if (trace === null) return null;

  const askLabel =
    trace.query === "agent" && trace.agentQuery !== ""
      ? `WHY · ${trace.agentQuery.toUpperCase()}`
      : "WHY DID YOU DO THAT";

  return (
    <div className="causa-panel">
      <Frame title="CAUSA // DECISION TRACE" tag="REVIEW ONLY">
        <div className="causa-body">
          <div className="causa-head">
            <span className="causa-pill">{askLabel}</span>
            {!trace.empty && (
              <span className="causa-meta dim-note">
                turn #{trace.turnRef}
                {trace.agent !== "" ? ` · ${trace.agent}` : ""}
              </span>
            )}
          </div>
          {trace.empty ? (
            <div className="causa-empty dim-note">
              {trace.query === "agent" && trace.agentQuery !== ""
                ? `no recent trace of the ${trace.agentQuery} agent — nothing in the last few turns matches`
                : "no decision trace recorded yet — I keep only the last few turns, and there's nothing to explain"}
            </div>
          ) : (
            <>
              {trace.utterance !== "" && (
                <div className="causa-utterance dim-note">“{trace.utterance}”</div>
              )}
              <ol className="causa-steps">
                {trace.steps.map((step, i) => (
                  <StepRow key={`${step.stage}-${i}`} step={step} />
                ))}
              </ol>
            </>
          )}
          <div className="causa-foot dim-note">
            Review only — reconstructed from the turn's recorded decision signals;
            nothing is re-executed, and an unrecorded turn stays honestly empty.
          </div>
        </div>
      </Frame>
    </div>
  );
}

/** One decision step: the stage marker, what was chosen, why, and any recorded
 *  alternatives (usually none — the daemon never invents rejected candidates). */
function StepRow({ step }: { step: CausaStep }) {
  return (
    <li className={`causa-step ${step.stage}`}>
      <span className="causa-stage">{step.stage.toUpperCase()}</span>
      <span className="causa-chosen">{step.chosen}</span>
      {step.why !== "" && <span className="causa-why dim-note">{step.why}</span>}
      {step.alternatives.length > 0 && (
        <span className="causa-alts dim-note">
          alternatives: {step.alternatives.join(", ")}
        </span>
      )}
    </li>
  );
}
