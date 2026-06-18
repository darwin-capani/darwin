import type { CrossCheckStatus, DebateStatus } from "../core/events";
import Frame from "./Frame";

/**
 * ANSWER // CROSS-CHECK — the read-only indicator for the last answer's two
 * OPTIONAL answer-quality passes, siblings of the SELF-CHECK panel:
 *
 *   #21 TOOL-RESULT CROSS-CHECK (daemon/src/anthropic.rs `crosscheck` module,
 *       emitted as `answer.cross_checked` from main.rs run_pipeline). Before a
 *       tool result is surfaced as fact (or a consequential action is built from
 *       it), a BOUNDED plausibility cross-check runs — deterministic sanity checks
 *       (shape / range / contradiction / empty-vs-claimed / citation-present),
 *       plus an OPTIONAL single bounded model "does this look right?" pass for
 *       important results. The badge surfaces that per-turn outcome:
 *         - CHECKED — the checks found nothing implausible (the result passed
 *           through). NOT a correctness guarantee — just "nothing tripped".
 *         - UNVERIFIED — a check tripped (implausible / empty / uncited /
 *           contradictory); confidence was DOWNGRADED and the result FLAGGED (the
 *           answer itself carries the flag reasons; this panel never repeats them).
 *
 *   #22 MULTI-MODEL DEBATE (daemon/src/anthropic.rs `debate` module, emitted as
 *       `answer.debated`). For GATED high-stakes asks only, TWO brains answer the
 *       same question and the daemon RECONCILES — at most two model calls:
 *         - CORROBORATED — both brains substantively agreed (confidence RAISED).
 *         - DISPUTED — the brains DISAGREED; BOTH answers are surfaced honestly
 *           (the answer text carries both — never silently picked or averaged).
 *         - ONE-MODEL — the second brain was unavailable; the single answer stands
 *           and it is stated that no second opinion was obtained.
 *
 * HONESTY CONTRACT (do not regress):
 *   - CROSS-CHECK ONLY ADDS CAUTION. It DOWNGRADES confidence and FLAGS a
 *     questionable result — it NEVER removes a consequential action's confirmation
 *     gate, and CHECKED is NOT a correctness guarantee, only "nothing tripped".
 *   - DISAGREEMENT IS SURFACED, NEVER HIDDEN. DISPUTED means the two brains
 *     diverged and BOTH answers are shown; the panel never fabricates a consensus.
 *   - THE SECOND-BRAIN GAIN IS RUNTIME-GATED. When the second brain is
 *     unavailable the outcome is ONE-MODEL (stated), never a fake agreement.
 *   - SHIPPED OFF. [answers].cross_check and [answers].debate both ship false (and
 *     debate gates only high-stakes turns), so until they are deliberately enabled
 *     the daemon emits the "off" outcome (null badge) and the corresponding row
 *     renders NOTHING — behavior is byte-for-byte today's.
 *   - SECRET-FREE. The wire carries only the gate flag, the outcome token, the
 *     derived badge, and honest copy — never the raw tool result, never the raw
 *     answers, never an embedding/audio/secret.
 *   - REVIEW-ONLY. There is NO button here. Both passes are gated daemon config;
 *     this panel only SHOWS the outcome the daemon already produced.
 *
 * The reducer only ever sets `crossCheckStatus` / `debateStatus` from a
 * defensively-parsed event (the badge DERIVED from the validated outcome, never
 * trusted from the wire) and clears each to null on an empty (off / pass-did-not-
 * run) turn — so this component can trust the badges it is handed, and a null
 * badge never reaches it. With both gates off every outcome is "off" => null badge
 * => this panel renders NOTHING.
 */
export default function AnswerCrossCheckPanel({
  crossCheck,
  debate,
}: {
  crossCheck: CrossCheckStatus | null;
  debate: DebateStatus | null;
}) {
  const showCross = crossCheck !== null && crossCheck.badge !== null;
  const showDebate = debate !== null && debate.badge !== null;

  // Nothing to show until a cross-check or debate carries a real outcome. Both
  // gates ship OFF (and debate only fires on gated high-stakes turns), so the
  // reducer holds both at null (clearing each whenever the pass did not run this
  // turn) — render nothing rather than a placeholder, mirroring VerifyPanel and
  // the other event-fed panels.
  if (!showCross && !showDebate) return null;

  return (
    <div className="crosscheck-panel">
      <Frame title="ANSWER // CROSS-CHECK" tag="HONEST · REVIEW ONLY">
        <div className="crosscheck-body">
          {showCross && crossCheck && (
            <div className="crosscheck-row">
              <span className="crosscheck-head">TOOL RESULT</span>
              <span
                className={`crosscheck-pill crosscheck-${crossCheck.outcome}`}
                title={crossPillTitle(crossCheck.outcome)}
              >
                {crossCheck.badge}
              </span>
              <span className="crosscheck-meaning">
                {crossMeaning(crossCheck.outcome)}
              </span>
            </div>
          )}

          {showDebate && debate && (
            <div className="crosscheck-row">
              <span className="crosscheck-head">TWO MODELS</span>
              <span
                className={`crosscheck-pill debate-${debate.outcome}`}
                title={debatePillTitle(debate.outcome)}
              >
                {debate.badge}
              </span>
              <span className="crosscheck-meaning">
                {debateMeaning(debate.outcome)}
              </span>
            </div>
          )}

          <div className="crosscheck-foot dim-note">
            {showCross && (
              <p>
                A bounded plausibility cross-check ran over the tool result before
                it was surfaced (deterministic sanity checks, plus at most one
                bounded model pass on important results). It only{" "}
                <b>downgrades confidence and flags</b> a questionable result —{" "}
                <b>it never removes a confirmation gate</b>, and CHECKED is{" "}
                <b>not a correctness guarantee</b>, only that nothing tripped. When
                a check tripped, the answer itself carries the reason. Ships OFF (
                <code>[answers].cross_check</code>).
              </p>
            )}
            {showDebate && (
              <p>
                A second independent model answered the same high-stakes question.
                Agreement <b>raises</b> confidence; when the models{" "}
                <b>disagree both answers are surfaced</b> — never silently picked or
                averaged into a fake consensus. If the second model is unavailable
                it <b>falls back to one and says so</b> (the quality gain is
                runtime-gated). At most two model calls; ships OFF (
                <code>[answers].debate</code>).
              </p>
            )}
          </div>
        </div>
      </Frame>
    </div>
  );
}

/** One-line plain meaning of a cross-check outcome, shown beside the badge. */
function crossMeaning(outcome: CrossCheckStatus["outcome"]): string {
  switch (outcome) {
    case "plausible":
      return "the plausibility checks found nothing to flag";
    case "flagged":
      return "a check tripped — confidence was downgraded, result flagged";
    case "off":
      return "";
  }
}

/** Hover copy for the cross-check pill — honest about what it does and does not
 *  mean. */
function crossPillTitle(outcome: CrossCheckStatus["outcome"]): string {
  switch (outcome) {
    case "plausible":
      return "the bounded plausibility cross-check found nothing implausible in the tool result — this is NOT a correctness guarantee, only that no deterministic (or bounded model) check tripped";
    case "flagged":
      return "a plausibility check tripped (implausible / empty / uncited / contradictory): confidence was DOWNGRADED and the result FLAGGED — this only ADDS caution, it NEVER removes a confirmation gate";
    case "off":
      return "";
  }
}

/** One-line plain meaning of a debate outcome, shown beside the badge. */
function debateMeaning(outcome: DebateStatus["outcome"]): string {
  switch (outcome) {
    case "agree":
      return "two models agreed — confidence raised";
    case "disagree":
      return "models disagree — both answers surfaced";
    case "fallback":
      return "second model unavailable — one model only";
    case "off":
      return "";
  }
}

/** Hover copy for the debate pill — honest about the reconciliation posture. */
function debatePillTitle(outcome: DebateStatus["outcome"]): string {
  switch (outcome) {
    case "agree":
      return "two independent models substantively agreed on this high-stakes answer, so confidence was raised — agreement is corroboration, not proof";
    case "disagree":
      return "the two models DISAGREED; BOTH answers are surfaced in the response so you can see the divergence — neither was silently picked, and they were never averaged into a fake consensus";
    case "fallback":
      return "the second model was unavailable, so only one model answered and that is stated plainly — no second opinion was obtained and none was fabricated";
    case "off":
      return "";
  }
}
