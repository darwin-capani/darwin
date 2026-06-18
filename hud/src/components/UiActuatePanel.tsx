import type { UiActuateOutcome, UiActuateOutcomeKind } from "../core/events";
import type { UiActuateSurface } from "../core/state";
import Frame from "./Frame";

/**
 * UI AUTOMATION // GATED — the read-only surface for the SINGLE MOST DANGEROUS
 * feature (#44, the capstone): physically actuating the UI (a CGEvent mouse
 * click / keyboard type / key combo) on the user's behalf. It mirrors the
 * shell / code-apply / confirm-park postures: a consequential action shown
 * READ-ONLY, with NO control that actuates anything — every action PARKS for
 * the user's SPOKEN confirm, PER ACTION, and never auto-runs.
 *
 * It is fed ONLY by the ui_actuate.* events the daemon broadcasts from
 * ui_actuate_tool (SECRET-FREE — never typed text, never coordinates, never a
 * fabricated result; only the action CLASS + a faithful target description):
 *   - ui_actuate.blocked reason=disabled    -> OFF / LOCKED — the inert shipped-
 *       OFF gate ([ui_automation].enabled=false or lockdown). Carries no action.
 *   - ui_actuate.blocked reason=device_gated -> the Accessibility-TCC seam
 *       refused/failed (consent absent). Nothing was actuated, never a fake OK.
 *   - ui_actuate.refused  -> the PURE planner refused a degenerate / off-screen
 *       instruction PRE-actuation, naming why. Never parked, never acted.
 *   - ui_actuate.preview  -> the DryRun faithful PER-ACTION preview; the single
 *       action is PARKED awaiting the user's spoken confirm. It has NOT run.
 *   - ui_actuate.actuating -> entered the Execute leg AFTER the full gate (master
 *       switch ON + the spoken-confirm replay + voice-id + !lockdown) AND the
 *       device TCC consent; the one action is being performed.
 *   - ui_actuate.actuated  -> the FAITHFUL single-action result (the one action
 *       fired). There is NO typed text / coordinate on the wire and NONE shown.
 *
 * HONESTY CONTRACT (do not regress — the same posture as the shell + confirm-park
 * + code-apply surfaces this mirrors):
 *   - EVERY ACTION IS CONSEQUENTIAL + PER-ACTION. There is deliberately NO button
 *     that actuates or confirms anything. ONE confirm authorizes EXACTLY ONE
 *     actuation — a second action PARKS AGAIN with its own confirm. NEVER batched,
 *     NEVER an autonomous loop. A ui_actuate.preview (parked) is a different,
 *     earlier event than the ui_actuate.actuating/actuated that only follow the
 *     full gate.
 *   - DEVICE-GATED. The actuation needs the Accessibility (TCC) permission —
 *     runtime user consent, not SBPL-grantable. With consent absent it is honestly
 *     blocked (reason=device_gated), never a fabricated success.
 *   - DEGENERATE INSTRUCTIONS REFUSED. An empty/off-screen/degenerate instruction
 *     is honestly refused PRE-actuation (ui_actuate.refused) — it never reaches the
 *     gate, the park, or the actuation.
 *   - OFF BY DEFAULT. The [ui_automation] feature ships disabled; ui_actuate.blocked
 *     reason=disabled is the inert OFF/LOCKED state, shown plainly, not an error.
 *   - THE VISION APP STAYS READ-ONLY. The OCR that LOCATES a control is a separate,
 *     read-only surface; this actuate op is the only thing that can ever click/type.
 *   - NEVER A FABRICATED RESULT. The daemon never puts a coordinate/typed-text on
 *     the wire, so the panel NEVER shows one — only the action class + faithful
 *     target + the honest outcome.
 *
 * The reducer only ever sets `uiActuate` from defensively-parsed ui_actuate.*
 * events (the action class, a faithful target, an outcome kind, a short reason —
 * never coordinates/typed text/secrets), so this component can trust its fields.
 */
export default function UiActuatePanel({
  uiActuate,
}: {
  uiActuate: UiActuateSurface | null;
}) {
  // Nothing to show until a ui_actuate.* event lands — render nothing rather than
  // a placeholder, mirroring the other event-fed panels (ShellPanel, McpPanel).
  // The feature ships OFF, so no event arrives until [ui_automation].enabled and
  // an actuation is attempted.
  if (uiActuate === null || uiActuate.last === null) return null;
  const last = uiActuate.last;

  return (
    <div className="actuate-panel">
      <Frame title="UI AUTOMATION // GATED" tag="CONSEQUENTIAL · PER-ACTION · OFF-DEFAULT">
        <div className="actuate-body">
          <StatusRow last={last} />
          <OutcomeRow last={last} />

          <div className="actuate-foot dim-note">
            Every action is <b>consequential</b> — it parks for your spoken confirm{" "}
            <b>PER ACTION</b>, <b>NEVER batched</b>, <b>NEVER autonomous</b>. One
            confirm authorizes exactly <b>one</b> actuation; a second action parks
            again. The actuation is <b>device-gated</b> (the macOS Accessibility
            permission — runtime consent, not grantable by sandbox). Gated UI
            automation ships <b>OFF by default</b> — enable{" "}
            <code>[ui_automation].enabled</code> to allow it. The Vision app that
            LOCATES a control stays <b>read-only</b>; this is the only op that can
            ever click or type, and I never show a fabricated actuation result.
          </div>
        </div>
      </Frame>
    </div>
  );
}

/** The gated status line — OFF / LOCKED is the honest default (the shipped-OFF
 *  gate). When an action is parked / in flight / actuated, the status reflects
 *  that it rode the per-action consequential gate. */
function StatusRow({ last }: { last: UiActuateOutcome }) {
  const off = last.kind === "blocked-off";
  return (
    <div className="actuate-head">
      <span className="actuate-title">STATUS</span>
      <span
        className={`actuate-pill ${off ? "off" : "armed"}`}
        title={
          off
            ? "gated UI automation is OFF / LOCKED by default — it actuates nothing until [ui_automation].enabled, and not while locked down"
            : "an action rode the per-action consequential gate — it never auto-ran; it parked for your spoken confirm first, one confirm per action"
        }
      >
        {off ? "OFF / LOCKED" : "GATED"}
      </span>
    </div>
  );
}

/** The last proposed action + its HONEST outcome. NEVER shows coordinates or
 *  typed text — only the action CLASS (Click/Type/Key), the faithful target
 *  description, the outcome, and (for a block/refusal) the short reason. */
function OutcomeRow({ last }: { last: UiActuateOutcome }) {
  const label = outcomeLabel(last.kind);

  return (
    <div className="actuate-outcome">
      <div className="actuate-head">
        <span className="actuate-title">LAST ACTION</span>
        <span
          className={`actuate-pill ${outcomePillClass(last.kind)}`}
          title={outcomePillTitle(last.kind)}
        >
          {label}
        </span>
      </div>

      {/* The proposed PER-ACTION plan — the action CLASS + the faithful target,
          shown as TEXT, never a runnable control. There is exactly one action
          here, never a batch. The OFF gate / a refusal carries no action. */}
      {last.action.length > 0 ? (
        <div className="actuate-plan" role="note">
          <span className="actuate-kind" aria-hidden="true">
            {actionKindLabel(last.action)}
          </span>
          <code className="actuate-target">
            {last.target.length > 0 ? last.target : "(no target named)"}
          </code>
        </div>
      ) : (
        <div className="actuate-empty dim-note">
          {emptyLead(last.kind)}
        </div>
      )}

      <div className="actuate-detail dim-note">
        {outcomeLead(last)}
        {last.kind === "refused" && last.reason.length > 0 && (
          <span className="actuate-reason"> ({last.reason})</span>
        )}
        {last.kind === "blocked-device" && last.reason.length > 0 && (
          <span className="actuate-reason"> ({last.reason})</span>
        )}
      </div>
    </div>
  );
}

/** The single action CLASS, shown as an uppercase tag. The daemon already spoke
 *  the class ("click"/"type"/"key"); anything else is shown faithfully verbatim
 *  rather than guessed. NEVER a coordinate or typed text. */
function actionKindLabel(action: string): string {
  switch (action.toLowerCase()) {
    case "click":
      return "CLICK";
    case "type":
      return "TYPE";
    case "key":
      return "KEY";
    default:
      // Faithful to whatever class the daemon spoke — never fabricate one.
      return action.toUpperCase();
  }
}

/** The short uppercase badge for an outcome — the headline status word. */
function outcomeLabel(kind: UiActuateOutcomeKind): string {
  switch (kind) {
    case "blocked-off":
      return "BLOCKED — MASTER OFF / LOCKED";
    case "blocked-device":
      return "REFUSED — NO ACCESSIBILITY";
    case "refused":
      return "REFUSED — PLANNER";
    case "parked":
      return "PARKED — AWAITING CONFIRM";
    case "actuating":
      return "ACTUATING";
    case "actuated":
      return "ACTUATED";
  }
}

/** The honest one-line lead for the action area when there is no action (the OFF
 *  gate / a planner refusal carry none). */
function emptyLead(kind: UiActuateOutcomeKind): string {
  switch (kind) {
    case "blocked-off":
      return "No action — gated UI automation is off / locked, so nothing was planned, parked, or actuated.";
    case "blocked-device":
      return "No action actuated — the Accessibility permission is absent, so the device seam honestly refused. Nothing was clicked or typed.";
    case "refused":
      return "No action — the instruction was refused before anything was planned.";
    default:
      // parked / actuating / actuated always carry an action (the parser drops a
      // phantom with none), so this branch is unreachable in practice.
      return "No action.";
  }
}

/** The honest one-line lead for an outcome. */
function outcomeLead(last: UiActuateOutcome): string {
  switch (last.kind) {
    case "blocked-off":
      return "Gated UI automation is OFF / LOCKED — nothing was planned, parked, or actuated.";
    case "blocked-device":
      return "The action did NOT fire — the macOS Accessibility permission is not granted, so the device-gated seam refused. Never a fabricated success.";
    case "refused":
      return "Refused before anything was planned — a degenerate or off-screen instruction is refused outright. It never reached the gate, the park, or the actuation.";
    case "parked":
      return "This is the faithful PER-ACTION preview — the single action is PARKED for your spoken confirm. It has NOT fired, and it never auto-runs. One confirm authorizes exactly this one action.";
    case "actuating":
      return "Confirmed and gated through (master switch + spoken confirm + voice-id + not locked down + Accessibility consent) — performing this one action now.";
    case "actuated":
      return "The faithful result — this single action fired. A second action would park again with its own confirm; nothing is batched or autonomous.";
  }
}

/** The pill colour class for an outcome (review-only attention vocabulary — no
 *  red alert chrome for the honest OFF / parked / refused states). */
function outcomePillClass(kind: UiActuateOutcomeKind): string {
  switch (kind) {
    case "blocked-off":
      return "off";
    case "blocked-device":
      return "denied";
    case "refused":
      return "denied";
    case "parked":
      return "parked";
    case "actuating":
      return "executing";
    case "actuated":
      return "ran";
  }
}

function outcomePillTitle(kind: UiActuateOutcomeKind): string {
  switch (kind) {
    case "blocked-off":
      return "the OFF / LOCKED gate — the inert shipped-OFF default ([ui_automation].enabled=false or locked down), not an error";
    case "blocked-device":
      return "the device-gated Accessibility-TCC seam refused (consent absent) — nothing was actuated, never a fake success";
    case "refused":
      return "the pure planner refused a degenerate/off-screen instruction PRE-actuation — never parked, never actuated";
    case "parked":
      return "parked for your spoken confirm — ONE confirm authorizes exactly this ONE action; it has NOT fired and never auto-runs";
    case "actuating":
      return "confirmed and gated through (master + confirm + voice-id + !lockdown + Accessibility consent) — performing this one action";
    case "actuated":
      return "the faithful single-action result — a second action re-parks with its own confirm; never batched, never autonomous";
  }
}
