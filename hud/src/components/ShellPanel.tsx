import type { ShellOutcome, ShellOutcomeKind } from "../core/events";
import type { ShellSurface } from "../core/state";
import Frame from "./Frame";

/**
 * SHELL // SANDBOXED — the read-only surface for the HIGHEST-RISK feature (#43):
 * arbitrary command execution (daemon/src/anthropic.rs::shell_run_tool over the
 * deny-default sandbox-exec confinement in daemon/src/shell.rs). It mirrors the
 * code-apply / confirm-park postures: a consequential action shown READ-ONLY,
 * with NO control that runs anything — the command parks for the user's SPOKEN
 * confirm and never auto-runs.
 *
 * It is fed ONLY by the shell.* events the daemon broadcasts (SECRET-FREE, never
 * fabricating a result):
 *   - shell.blocked reason=disabled  -> OFF/LOCKED — the inert shipped-OFF gate.
 *   - shell.blocked reason=exec_failed -> the device-gated exec seam errored.
 *   - shell.denied  -> a denylisted (destructive/exfil) command REFUSED PRE-exec,
 *       naming the matched class. Never parked, never run.
 *   - shell.preview -> the DryRun faithful preview; the command is PARKED awaiting
 *       the user's spoken confirm. It has NOT run.
 *   - shell.executing -> entered the Execute leg AFTER the full gate (master
 *       switch ON + the spoken-confirm replay + voice-id + !lockdown); running.
 *   - shell.ran -> the FAITHFUL real result: an honest exit code + timed-out /
 *       truncated flags. There is NO output on the wire and NONE is ever shown.
 *
 * HONESTY CONTRACT (do not regress — the same posture as the confirm-park and
 * code-apply surfaces this mirrors):
 *   - EVERY COMMAND IS CONSEQUENTIAL. There is deliberately NO button that runs
 *     or confirms anything. A command PARKS for the user's spoken confirm and
 *     NEVER auto-runs — a shell.preview (parked) is a different, earlier event
 *     than the shell.executing/shell.ran that only follow the full gate.
 *   - SANDBOXED DENY-DEFAULT. When it does run it is under a deny-default SBPL
 *     profile: no network, a confined fs (writes only to a throwaway scratch
 *     dir), and NO access to the Keychain, ~/.claude, or the daemon's own state.
 *   - DESTRUCTIVE COMMANDS REFUSED. A denylisted command is honestly refused
 *     PRE-exec (shell.denied), naming the matched class — it never reaches the
 *     gate, the park, or the exec.
 *   - OFF BY DEFAULT. The [shell] feature ships disabled; shell.blocked
 *     reason=disabled is the inert OFF/LOCKED state, shown plainly, not an error.
 *   - NEVER A FABRICATED OUTPUT. The daemon never puts a command's output on the
 *     wire, so the panel NEVER shows one — only the honest exit code + flags.
 *
 * The reducer only ever sets `shell` from defensively-parsed shell.* events (the
 * command text, an outcome kind, a short reason, the run flags — never an
 * output/secret), so this component can trust the fields it is handed.
 */
export default function ShellPanel({ shell }: { shell: ShellSurface | null }) {
  // Nothing to show until a shell.* event lands — render nothing rather than a
  // placeholder, mirroring the other event-fed panels (CodeIntelPanel, McpPanel).
  // The feature ships OFF, so no event arrives until [shell].enabled and a
  // command is attempted.
  if (shell === null || shell.last === null) return null;
  const last = shell.last;

  return (
    <div className="shell-panel">
      <Frame title="SHELL // SANDBOXED" tag="CONSEQUENTIAL · OFF-DEFAULT">
        <div className="shell-body">
          <StatusRow last={last} />
          <OutcomeRow last={last} />

          <div className="shell-foot dim-note">
            Every command is <b>consequential</b> — it parks for your spoken
            confirm and <b>NEVER auto-runs</b>. When it does run it is{" "}
            <b>sandboxed deny-default</b>: no network, a confined fs (writes only
            to a throwaway scratch dir), and no access to the Keychain,{" "}
            <code>~/.claude</code>, or my own state. Destructive commands are{" "}
            <b>refused</b> outright before they can run. The sandboxed shell ships{" "}
            <b>OFF by default</b> — enable <code>[shell].enabled</code> to allow
            it. I never show a fabricated command output: only the real exit code.
          </div>
        </div>
      </Frame>
    </div>
  );
}

/** The gated status line — OFF / LOCKED is the honest default (the shipped-OFF
 *  gate). When a command is in flight or has run, the status reflects whether
 *  the shell is actively gated through. */
function StatusRow({ last }: { last: ShellOutcome }) {
  const off = last.kind === "blocked-off";
  return (
    <div className="shell-head">
      <span className="shell-title">STATUS</span>
      <span
        className={`shell-pill ${off ? "off" : "armed"}`}
        title={
          off
            ? "the sandboxed shell is OFF / LOCKED by default — it runs nothing until [shell].enabled"
            : "a command rode the consequential gate — it never auto-ran; it parked for your spoken confirm first"
        }
      >
        {off ? "OFF / LOCKED" : "GATED"}
      </span>
    </div>
  );
}

/** The last command + its HONEST outcome. NEVER shows a fabricated output — only
 *  the command text, the outcome, and (for a real run) the exit code + flags. */
function OutcomeRow({ last }: { last: ShellOutcome }) {
  const label = outcomeLabel(last.kind);
  const lead = outcomeLead(last);

  return (
    <div className="shell-outcome">
      <div className="shell-head">
        <span className="shell-title">LAST COMMAND</span>
        <span
          className={`shell-pill ${outcomePillClass(last.kind)}`}
          title={outcomePillTitle(last.kind)}
        >
          {label}
        </span>
      </div>

      {/* The exact command (faithful, the daemon's own text), shown as text — not
          a runnable control. The OFF gate carries no command. */}
      {last.command.length > 0 ? (
        <div className="shell-cmd" role="note">
          <span className="shell-cmd-prompt" aria-hidden="true">
            $
          </span>
          <code>{last.command}</code>
        </div>
      ) : (
        <div className="shell-empty dim-note">
          No command — the sandboxed shell is off, so nothing was classified,
          parked, or run.
        </div>
      )}

      <div className="shell-detail dim-note">
        {lead}
        {last.kind === "denied" && last.reason.length > 0 && (
          <span className="shell-reason"> (matched class: {last.reason})</span>
        )}
        {last.kind === "blocked-exec-failed" && last.reason.length > 0 && (
          <span className="shell-reason"> ({last.reason})</span>
        )}
        {last.kind === "ran" && <RanFacts last={last} />}
      </div>
    </div>
  );
}

/** The faithful run facts — the real exit code + timed-out / truncated flags.
 *  NO output is ever shown (none is on the wire); these are the honest signals. */
function RanFacts({ last }: { last: ShellOutcome }) {
  const ok = last.exitCode === 0 && !last.timedOut;
  return (
    <span className="shell-facts">
      <span
        className={`shell-exit ${ok ? "ok" : "nonzero"}`}
        title="the real process exit code (0 = success); never a fabricated value"
      >
        exit {last.exitCode === null ? "unknown" : last.exitCode}
      </span>
      {last.timedOut && (
        <span className="shell-flag" title="the run hit its sandbox timeout">
          TIMED OUT
        </span>
      )}
      {last.truncated && (
        <span
          className="shell-flag"
          title="the bounded output was truncated (the output itself is never shown)"
        >
          OUTPUT TRUNCATED
        </span>
      )}
    </span>
  );
}

/** The short uppercase badge for an outcome — the headline status word. */
function outcomeLabel(kind: ShellOutcomeKind): string {
  switch (kind) {
    case "blocked-off":
      return "BLOCKED — MASTER OFF";
    case "blocked-exec-failed":
      return "BLOCKED";
    case "denied":
      return "REFUSED — DENYLISTED";
    case "parked":
      return "PARKED — AWAITING CONFIRM";
    case "executing":
      return "EXECUTING";
    case "ran":
      return "RAN";
  }
}

/** The honest one-line lead for an outcome. */
function outcomeLead(last: ShellOutcome): string {
  switch (last.kind) {
    case "blocked-off":
      return "The sandboxed shell is OFF / LOCKED — nothing was classified, parked, or run.";
    case "blocked-exec-failed":
      return "The command did not run — the sandboxed exec seam errored; nothing else ran.";
    case "denied":
      return "Refused before it could run — it matches a destructive/unsafe pattern, so I refuse it outright. It never reached the gate or the sandbox.";
    case "parked":
      return "This is the faithful preview — the command is PARKED for your spoken confirm. It has NOT run, and it never auto-runs.";
    case "executing":
      return "Confirmed and gated through — running it now in the deny-default sandbox.";
    case "ran":
      return "The faithful real result of the sandboxed run (the honest exit code below — never a fabricated output):";
  }
}

/** The pill colour class for an outcome (review-only attention vocabulary — no
 *  red alert chrome for the honest OFF / parked / denied states). */
function outcomePillClass(kind: ShellOutcomeKind): string {
  switch (kind) {
    case "blocked-off":
      return "off";
    case "blocked-exec-failed":
      return "note";
    case "denied":
      return "denied";
    case "parked":
      return "parked";
    case "executing":
      return "executing";
    case "ran":
      return "ran";
  }
}

function outcomePillTitle(kind: ShellOutcomeKind): string {
  switch (kind) {
    case "blocked-off":
      return "the OFF / LOCKED gate — the inert shipped-OFF default, not an error";
    case "blocked-exec-failed":
      return "the device-gated exec seam errored — nothing ran";
    case "denied":
      return "a denylisted destructive/exfil command refused PRE-exec — never parked, never run";
    case "parked":
      return "parked for your spoken confirm — it has NOT run and never auto-runs";
    case "executing":
      return "confirmed and gated through — running in the deny-default sandbox";
    case "ran":
      return "the faithful real result — the honest exit code + flags, never an output";
  }
}
