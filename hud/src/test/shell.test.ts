import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import ShellPanel from "../components/ShellPanel";
import {
  parseShellBlocked,
  parseShellDenied,
  parseShellCommandEvent,
  parseShellRan,
  type TelemetryEnvelope,
} from "../core/events";
import {
  type ShellSurface,
  HudState,
  initialState,
  reduce,
} from "../core/state";

/* helpers ------------------------------------------------------------------ */

let counter = 0;
function env(
  event: string,
  data: Record<string, unknown> = {},
  source = "system",
): TelemetryEnvelope {
  counter += 1;
  return {
    ts: `2026-06-17T12:00:${String(counter % 60).padStart(2, "0")}Z`,
    source,
    event,
    data,
  };
}

function tel(state: HudState, e: TelemetryEnvelope, at = 1000): HudState {
  return reduce(state, { type: "telemetry", envelope: e, at });
}

function connected(at = 0): HudState {
  return reduce(initialState(), { type: "ws.connected", at });
}

/* ------------------------------------------------------------------------ *
 * parseShellBlocked — reason "disabled" is the OFF/locked gate (carries no
 * command); any other reason is the exec-seam failure. Never throws.
 * ------------------------------------------------------------------------ */
describe("parseShellBlocked (OFF/locked vs exec failure)", () => {
  it("reason=disabled is the OFF/LOCKED gate — no command, no reason surfaced", () => {
    const out = parseShellBlocked({ reason: "disabled" }, "T");
    expect(out.kind).toBe("blocked-off");
    expect(out.command).toBe("");
    expect(out.reason).toBe("");
  });

  it("reason=exec_failed is the device-gated exec seam erroring", () => {
    const out = parseShellBlocked({ reason: "exec_failed" }, "T");
    expect(out.kind).toBe("blocked-exec-failed");
    expect(out.reason).toBe("exec_failed");
  });

  it("never throws on junk", () => {
    expect(() => parseShellBlocked({ reason: 42 }, "T")).not.toThrow();
    // a non-string / absent reason is NOT "disabled" -> treated as a real block
    expect(parseShellBlocked({}, "T").kind).toBe("blocked-exec-failed");
  });
});

/* ------------------------------------------------------------------------ *
 * parseShellDenied — a denylisted command refused PRE-exec; the reason names
 * the matched destructive class. SECRET-FREE.
 * ------------------------------------------------------------------------ */
describe("parseShellDenied (destructive refused pre-exec)", () => {
  it("carries the matched class as the reason", () => {
    const out = parseShellDenied({ command: "rm -rf /", reason: "broad-rm" }, "T");
    expect(out.kind).toBe("denied");
    expect(out.command).toBe("rm -rf /");
    expect(out.reason).toBe("broad-rm");
  });

  it("defaults the reason to 'unknown' when absent (still an honest refusal)", () => {
    expect(parseShellDenied({ command: "dd if=..." }, "T").reason).toBe("unknown");
  });

  it("never throws on junk", () => {
    expect(() => parseShellDenied({ reason: 9 }, "T")).not.toThrow();
  });
});

/* ------------------------------------------------------------------------ *
 * parseShellCommandEvent — shell.preview (parked) / shell.executing carry the
 * exact command. A payload with no command text is dropped (never a phantom).
 * ------------------------------------------------------------------------ */
describe("parseShellCommandEvent (parked / executing)", () => {
  it("parses a parked preview with the exact command (has NOT run)", () => {
    const out = parseShellCommandEvent("parked", { command: "ls -la" }, "T");
    expect(out).not.toBeNull();
    expect(out!.kind).toBe("parked");
    expect(out!.command).toBe("ls -la");
  });

  it("parses an executing event", () => {
    const out = parseShellCommandEvent("executing", { command: "echo hi" }, "T");
    expect(out!.kind).toBe("executing");
    expect(out!.command).toBe("echo hi");
  });

  it("returns null with no command text (never a phantom command)", () => {
    expect(parseShellCommandEvent("parked", {}, "T")).toBeNull();
    expect(parseShellCommandEvent("parked", { command: "" }, "T")).toBeNull();
    expect(parseShellCommandEvent("executing", { command: 7 }, "T")).toBeNull();
  });
});

/* ------------------------------------------------------------------------ *
 * parseShellRan — the FAITHFUL real result: exit code + timed-out / truncated.
 * There is NO output field — the panel never shows a (fabricable) output.
 * ------------------------------------------------------------------------ */
describe("parseShellRan (faithful result, never an output)", () => {
  it("parses the honest exit code + flags", () => {
    const out = parseShellRan(
      { command: "ls", exit_code: 0, timed_out: false, truncated: true },
      "T",
    );
    expect(out!.kind).toBe("ran");
    expect(out!.command).toBe("ls");
    expect(out!.exitCode).toBe(0);
    expect(out!.timedOut).toBe(false);
    expect(out!.truncated).toBe(true);
  });

  it("a nonzero exit code is preserved faithfully", () => {
    expect(parseShellRan({ command: "false", exit_code: 1 }, "T")!.exitCode).toBe(1);
  });

  it("exit code is null when the daemon could not report one (honest unknown)", () => {
    expect(parseShellRan({ command: "x" }, "T")!.exitCode).toBeNull();
  });

  it("returns null with no command text", () => {
    expect(parseShellRan({ exit_code: 0 }, "T")).toBeNull();
  });

  it("never throws on junk", () => {
    expect(() => parseShellRan({ command: "x", exit_code: "nope", timed_out: 1 }, "T")).not.toThrow();
    expect(parseShellRan({ command: "x", timed_out: 1 }, "T")!.timedOut).toBe(false);
  });
});

/* ------------------------------------------------------------------------ *
 * The reducer arm. Each shell.* event folds into shell.last; a malformed
 * command-bearing event is dropped (the prior outcome is kept). SECRET-FREE.
 * ------------------------------------------------------------------------ */
describe("shell.* reducer", () => {
  it("shell.blocked reason=disabled surfaces the OFF/LOCKED state", () => {
    const s = tel(connected(), env("shell.blocked", { reason: "disabled" }));
    expect(s.shell).not.toBeNull();
    expect(s.shell!.last!.kind).toBe("blocked-off");
    expect(s.shell!.last!.command).toBe("");
  });

  it("shell.blocked reason=exec_failed surfaces the exec-seam failure", () => {
    const s = tel(connected(), env("shell.blocked", { reason: "exec_failed" }));
    expect(s.shell!.last!.kind).toBe("blocked-exec-failed");
  });

  it("shell.denied surfaces a refused-denylisted command with the matched class", () => {
    const s = tel(connected(), env("shell.denied", { command: "rm -rf /", reason: "broad-rm" }));
    expect(s.shell!.last!.kind).toBe("denied");
    expect(s.shell!.last!.command).toBe("rm -rf /");
    expect(s.shell!.last!.reason).toBe("broad-rm");
  });

  it("shell.preview PARKS the command (it has NOT run)", () => {
    const s = tel(connected(), env("shell.preview", { command: "ls -la" }));
    expect(s.shell!.last!.kind).toBe("parked");
    expect(s.shell!.last!.command).toBe("ls -la");
  });

  it("shell.executing then shell.ran folds the faithful result", () => {
    let s = tel(connected(), env("shell.executing", { command: "echo hi" }));
    expect(s.shell!.last!.kind).toBe("executing");
    s = tel(s, env("shell.ran", { command: "echo hi", exit_code: 0, timed_out: false, truncated: false }));
    expect(s.shell!.last!.kind).toBe("ran");
    expect(s.shell!.last!.exitCode).toBe(0);
  });

  it("drops a malformed shell.preview (no command) — keeps the prior outcome", () => {
    let s = tel(connected(), env("shell.preview", { command: "ls" }));
    expect(s.shell!.last!.command).toBe("ls");
    s = tel(s, env("shell.preview", {})); // malformed — no command
    expect(s.shell!.last!.command).toBe("ls"); // prior outcome kept
  });

  it("drops a malformed shell.ran (no command) — keeps the prior outcome", () => {
    let s = tel(connected(), env("shell.preview", { command: "ls" }));
    s = tel(s, env("shell.ran", { exit_code: 0 })); // no command
    expect(s.shell!.last!.kind).toBe("parked"); // prior outcome kept
  });

  it("never carries a secret or an output — only command/outcome/reason/flags survive", () => {
    let s = tel(
      connected(),
      env("shell.ran", {
        command: "cat secrets",
        exit_code: 0,
        output: "sk-SECRET-OUTPUT",
        token: "leak",
        api_key: "sk-SECRET",
      }),
    );
    s = tel(s, env("shell.denied", { command: "x", reason: "broad-rm", token: "leak" }));
    const serialized = JSON.stringify(s.shell);
    expect(serialized).not.toContain("SECRET");
    expect(serialized).not.toContain("leak");
    expect(serialized).not.toContain("api_key");
    expect(serialized).not.toContain("output");
  });
});

/* ------------------------------------------------------------------------ *
 * The panel itself (rendered headlessly via renderToStaticMarkup — node env, no
 * jsdom, same pattern as the code-intel test). THE SAFETY POSTURE: every command
 * is consequential — it PARKS for a spoken confirm and NEVER auto-runs; the panel
 * has NO control that runs anything and NEVER shows a fabricated output.
 * ------------------------------------------------------------------------ */
describe("ShellPanel (read-only, consequential, never-auto-run)", () => {
  const render = (shell: ShellSurface | null) =>
    renderToStaticMarkup(createElement(ShellPanel, { shell }));

  it("renders nothing until a shell.* event lands", () => {
    expect(render(null)).toBe("");
    expect(render({ last: null })).toBe("");
  });

  it("shows the OFF / LOCKED status for the shipped-OFF gate (no command)", () => {
    const html = render({
      last: { kind: "blocked-off", command: "", reason: "", exitCode: null, timedOut: false, truncated: false, at: "T" },
    });
    expect(html).toMatch(/OFF \/ LOCKED/);
    expect(html).toMatch(/MASTER OFF/);
    expect(html).toMatch(/off, so nothing was classified, parked, or run/i);
  });

  it("shows a PARKED-awaiting-confirm command — and states it has NOT run / never auto-runs", () => {
    const html = render({
      last: { kind: "parked", command: "ls -la", reason: "", exitCode: null, timedOut: false, truncated: false, at: "T" },
    });
    expect(html).toMatch(/PARKED — AWAITING CONFIRM/);
    expect(html).toContain("ls -la");
    expect(html).toMatch(/has NOT run/i);
    expect(html).toMatch(/never auto-runs/i);
  });

  it("shows a REFUSED-denylisted command naming the matched class", () => {
    const html = render({
      last: { kind: "denied", command: "rm -rf /", reason: "broad-rm", exitCode: null, timedOut: false, truncated: false, at: "T" },
    });
    expect(html).toMatch(/REFUSED — DENYLISTED/);
    expect(html).toContain("rm -rf /");
    expect(html).toContain("broad-rm");
    expect(html).toMatch(/never reached the gate or the sandbox/i);
  });

  it("shows a RAN result with the honest exit code — and NEVER a command output", () => {
    const html = render({
      last: { kind: "ran", command: "echo hi", reason: "", exitCode: 0, timedOut: false, truncated: true, at: "T" },
    });
    expect(html).toMatch(/RAN/);
    expect(html).toContain("exit 0");
    expect(html).toMatch(/OUTPUT TRUNCATED/);
    expect(html).toMatch(/never a fabricated output/i);
  });

  it("a nonzero exit / timed-out run shows the honest bad signals", () => {
    const html = render({
      last: { kind: "ran", command: "false", reason: "", exitCode: 1, timedOut: true, truncated: false, at: "T" },
    });
    expect(html).toContain("exit 1");
    expect(html).toMatch(/TIMED OUT/);
  });

  it("states the honesty contract: consequential, sandboxed deny-default, refused, OFF by default", () => {
    const html = render({
      last: { kind: "blocked-off", command: "", reason: "", exitCode: null, timedOut: false, truncated: false, at: "T" },
    });
    expect(html).toMatch(/consequential/i);
    expect(html).toMatch(/parks for your spoken confirm/i);
    expect(html).toMatch(/NEVER auto-runs/);
    expect(html).toMatch(/sandboxed deny-default/i);
    expect(html).toMatch(/no network/i);
    expect(html).toMatch(/refused/i);
    expect(html).toMatch(/OFF by default/i);
  });

  it("has NO clickable control — review-only (it can NEVER run a command from the HUD)", () => {
    const html = render({
      last: { kind: "parked", command: "ls -la", reason: "", exitCode: null, timedOut: false, truncated: false, at: "T" },
    });
    // There is NO control at all in this read-only panel: no button, no link, no
    // inline handler. The command can NEVER be run from the HUD — it parks for a
    // spoken confirm. (mirrors the code-apply / confirm-park review panels.)
    expect(html).not.toContain("<button");
    expect(html).not.toMatch(/<a\b/);
    expect(html).not.toMatch(/onclick/i);
    // Honest review-only states must NOT use the red alert-panel chrome.
    expect(html).not.toContain("alert-panel");
    expect(html).not.toContain('role="alert"');
  });

  it("never renders a fabricated output even if a stray field somehow appeared", () => {
    // The type has no output field, but assert the rendered DOM carries none of
    // the run's textual output — only the honest exit code + flags.
    const html = render({
      last: { kind: "ran", command: "cat file", reason: "", exitCode: 0, timedOut: false, truncated: false, at: "T" },
    });
    expect(html).toContain("cat file"); // the command itself is shown
    expect(html).toContain("exit 0"); // the honest exit code is shown
    expect(html).not.toMatch(/stdout|stderr/i); // never an output stream
  });
});
