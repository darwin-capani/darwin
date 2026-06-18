import type { VerifyStatus } from "../core/events";
import Frame from "./Frame";

/**
 * ANSWER // SELF-CHECK — the read-only indicator for the last answer's OPTIONAL
 * second self-verification pass (daemon/src/anthropic.rs `verify` module, emitted
 * as `answer.verified` from main.rs run_pipeline).
 *
 * When the [answers].verify gate is on AND the turn was important enough to gate
 * in (a factual / retrieval / consequential turn — never a trivial greeting), the
 * model critiques its OWN draft answer ONCE against the REAL sources that turn
 * used, and at most ONCE revises it. The badge surfaces that per-turn outcome:
 *   - VERIFIED — the self-check flagged nothing unsupported (the answer passed
 *     through unchanged). NOT a correctness guarantee — just "the self-check
 *     found nothing to flag".
 *   - REVISED — the self-check flagged an issue and the bounded revise corrected
 *     or qualified the answer (the answer text changed).
 *   - FLAGGED — the self-check raised a concern that was not resolved; the answer
 *     itself carries the honest caveat (this panel never repeats the claim text).
 *
 * HONESTY CONTRACT (do not regress):
 *   - A SECOND SELF-CHECK REDUCES — DOES NOT ELIMINATE — ERRORS. The copy says
 *     so plainly. VERIFIED does NOT mean guaranteed-correct; it means the model's
 *     own re-read found nothing to flag against the sources it used.
 *   - SELF-CRITIQUE AGAINST THE SOURCES IT USED. The check is the model grading
 *     its own draft against the real tool-result sources this turn consulted —
 *     not an external oracle, not a measured accuracy score.
 *   - RUNS ONLY ON IMPORTANT TURNS, BOUNDED. The gate skips trivial turns; the
 *     pass is at most one critique + one revise — never an unbounded loop. The
 *     extra call is a latency/cost tradeoff, taken only when it is worth it.
 *   - SHIPPED OFF. [answers].verify ships false, so until it is deliberately
 *     enabled the daemon emits the "off" outcome (null badge) and this panel
 *     renders NOTHING — behavior is byte-for-byte today's.
 *   - SECRET-FREE. The wire carries only the gate flag, the outcome token, the
 *     derived badge, and honest copy — never the flagged-claim text, never any
 *     content beyond the answer, never an embedding/audio/secret.
 *   - REVIEW-ONLY. There is NO button here. Verification is gated daemon config;
 *     this panel only SHOWS the outcome the daemon already produced.
 *
 * The reducer only ever sets `verifyStatus` from a defensively-parsed
 * `answer.verified` (the badge DERIVED from the validated outcome, never trusted
 * from the wire) and clears it to null on an empty (off / pass-did-not-run) turn
 * — so this component can trust the badge it is handed, and a null badge never
 * reaches it.
 */
export default function VerifyPanel({
  status,
}: {
  status: VerifyStatus | null;
}) {
  // Nothing to show until an answer.verified carries a real outcome. The
  // [answers].verify gate ships OFF, so the reducer holds `verifyStatus` at null
  // (and clears it whenever the pass did not run this turn) — render nothing
  // rather than a placeholder, mirroring the other event-fed panels
  // (AnswerSourcesPanel, DocSearchPanel, UnifiedSearchPanel).
  if (status === null || status.badge === null) return null;

  return (
    <div className="verify-panel">
      <Frame title="ANSWER // SELF-CHECK" tag="HONEST · REVIEW ONLY">
        <div className="verify-body">
          <div className="verify-row">
            <span className="verify-head">SELF-CHECK</span>
            <span
              className={`verify-pill verify-${status.outcome}`}
              title={pillTitle(status.outcome)}
            >
              {status.badge}
            </span>
            <span className="verify-meaning">{meaning(status.outcome)}</span>
          </div>

          <div className="verify-foot dim-note">
            A second self-check ran against the sources this answer used — the
            model re-read its own draft and {verbForOutcome(status.outcome)}.
            It <b>reduces, but does not eliminate</b>, errors:{" "}
            <b>VERIFIED does not mean guaranteed-correct</b>, only that the
            self-check found nothing to flag. The check is the model grading
            itself against the real sources it consulted, not a measured accuracy
            score. It runs only on important turns, is at most one critique plus
            one revise, and ships OFF (<code>[answers].verify</code>).
          </div>
        </div>
      </Frame>
    </div>
  );
}

/** One-line plain meaning of an outcome, shown beside the badge. */
function meaning(outcome: VerifyStatus["outcome"]): string {
  switch (outcome) {
    case "verified-clean":
      return "the self-check found nothing to flag";
    case "revised":
      return "the self-check corrected the answer";
    case "flagged":
      return "the self-check raised an unresolved caveat";
    case "off":
      return "";
  }
}

/** What the self-check did, woven into the honest footnote. */
function verbForOutcome(outcome: VerifyStatus["outcome"]): string {
  switch (outcome) {
    case "verified-clean":
      return "found nothing unsupported to flag";
    case "revised":
      return "corrected or qualified it";
    case "flagged":
      return "raised a concern (the answer carries the caveat)";
    case "off":
      return "did not run";
  }
}

/** Hover copy for the badge pill — honest about what it does and does not mean. */
function pillTitle(outcome: VerifyStatus["outcome"]): string {
  switch (outcome) {
    case "verified-clean":
      return "the model's own second self-check found nothing unsupported against the sources it used — this REDUCES, not eliminates, errors and is NOT a correctness guarantee";
    case "revised":
      return "the second self-check flagged an issue and one bounded revise corrected or qualified the answer against the sources it used";
    case "flagged":
      return "the second self-check raised a concern it could not resolve; the answer itself carries the honest caveat — a self-check found something, not a verdict that the answer is wrong";
    case "off":
      return "";
  }
}
