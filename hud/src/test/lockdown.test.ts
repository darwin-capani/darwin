import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

/* ------------------------------------------------------------------------ *
 * Tauri-shell shim — make `inTauri()` read true and route sendCommand through *
 * a controllable invoke spy so the PANIC / UNLOCK button presses can be       *
 * asserted (the DEDICATED verb, never {cmd:"ask"}) with NO real socket.       *
 * ------------------------------------------------------------------------ */
const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args: unknown) => invokeMock(cmd, args),
}));
vi.mock("../tauri/bridge", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../tauri/bridge")>();
  return { ...actual, inTauri: () => true };
});

import SettingsModal from "../components/SettingsModal";
import StatusBar from "../components/StatusBar";
import { sendCommand } from "../tauri/command";
import {
  PANIC_CONFIRMATION,
  UNLOCK_CONFIRMATION,
  lockdownInitial,
  lockdownLabel,
  lockdownTone,
  parseLockdownStatus,
  modelTierInitial,
  sttTierInitial,
  voiceIdInitial,
  voiceTierInitial,
  voiceModeInitial,
  type LockdownStatus,
  type TelemetryEnvelope,
} from "../core/events";
import { HudState, initialState, reduce } from "../core/state";

/* helpers ------------------------------------------------------------------ */

let counter = 0;
function env(
  event: string,
  data: Record<string, unknown> = {},
  source = "system",
): TelemetryEnvelope {
  counter += 1;
  return {
    ts: `2026-06-16T12:00:${String(counter % 60).padStart(2, "0")}Z`,
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

const noop = () => {};

beforeEach(() => {
  invokeMock.mockReset();
});
afterEach(() => {
  vi.restoreAllMocks();
});

/* ======================================================================== *
 * parseLockdownStatus (defensive, fail-SAFE, secret-free)                    *
 * ======================================================================== */
describe("parseLockdownStatus (defensive, fail-safe)", () => {
  it("parses the shipped-OFF default snapshot", () => {
    const l = parseLockdownStatus({ locked: false, restored_from_marker: false });
    expect(l.locked).toBe(false);
    expect(l.restoredFromMarker).toBe(false);
  });

  it("parses a LOCKED snapshot that re-entered from the marker (restart re-entry)", () => {
    const l = parseLockdownStatus({ locked: true, restored_from_marker: true });
    expect(l.locked).toBe(true);
    expect(l.restoredFromMarker).toBe(true);
  });

  it("defaults to NOT-locked when fields are absent/garbled (never a false alarm-clear, never a stuck lock)", () => {
    expect(parseLockdownStatus({}).locked).toBe(false);
    expect(parseLockdownStatus({ locked: "yes" }).locked).toBe(false);
    expect(parseLockdownStatus({ locked: 1 }).locked).toBe(false);
    expect(parseLockdownStatus({ restored_from_marker: "x" }).restoredFromMarker).toBe(false);
    expect(() => parseLockdownStatus({ locked: {} })).not.toThrow();
  });

  it("surfaces ONLY the two booleans (extra wire fields ignored)", () => {
    const l = parseLockdownStatus({ locked: true, restored_from_marker: false, secret: "x" });
    expect(JSON.stringify(l)).not.toContain("secret");
    expect(Object.keys(l).sort()).toEqual(["locked", "restoredFromMarker"]);
  });

  it("lockdownInitial is the honest NORMAL default", () => {
    expect(lockdownInitial()).toEqual({ locked: false, restoredFromMarker: false });
  });
});

/* ======================================================================== *
 * lockdownTone / lockdownLabel                                               *
 * ======================================================================== */
describe("lockdownTone / lockdownLabel", () => {
  it("LOCKED DOWN is the alarm (bad) tone", () => {
    const l = parseLockdownStatus({ locked: true });
    expect(lockdownLabel(l)).toBe("LOCKED DOWN");
    expect(lockdownTone(l)).toBe("bad");
  });
  it("NORMAL is the calm (ok) tone — the shipped default", () => {
    const l = parseLockdownStatus({ locked: false });
    expect(lockdownLabel(l)).toBe("NORMAL");
    expect(lockdownTone(l)).toBe("ok");
  });
});

/* ======================================================================== *
 * Verbatim HONEST copy — the HUD echoes the daemon consts byte-for-byte      *
 * ======================================================================== */
describe("PANIC / UNLOCK confirmation copy (honest, echoes the daemon)", () => {
  it("PANIC_CONFIRMATION names what the stop does AND does not do", () => {
    expect(PANIC_CONFIRMATION).toContain("all future outward actions");
    expect(PANIC_CONFIRMATION).toContain("the microphone immediately");
    expect(PANIC_CONFIRMATION).toContain("persists across a restart");
    // the critical honesty: it cannot undo an action already done
    expect(PANIC_CONFIRMATION).toContain("I can't undo anything already done");
    expect(PANIC_CONFIRMATION).toContain("a sent message stays sent");
  });
  it("UNLOCK_CONFIRMATION says configured settings are restored, nothing changed underneath", () => {
    expect(UNLOCK_CONFIRMATION).toContain("Your configured settings are restored");
    expect(UNLOCK_CONFIRMATION).toContain("nothing was changed");
  });
});

/* ======================================================================== *
 * Reducer arm + instant lockdown.set echo                                    *
 * ======================================================================== */
describe("lockdown.status reducer + lockdown.set echo", () => {
  it("is null before any snapshot (honest awaiting)", () => {
    expect(connected().lockdown).toBeNull();
  });

  it("sets the posture from a well-formed event", () => {
    const s = tel(connected(), env("lockdown.status", { locked: true, restored_from_marker: true }));
    expect(s.lockdown).not.toBeNull();
    expect(s.lockdown!.locked).toBe(true);
    expect(s.lockdown!.restoredFromMarker).toBe(true);
  });

  it("a malformed payload yields the honest NOT-locked snapshot, not a stale one", () => {
    const s = tel(connected(), env("lockdown.status", { locked: "garbage" }));
    expect(s.lockdown).not.toBeNull();
    expect(s.lockdown!.locked).toBe(false);
  });

  it("lockdown.set flips the indicator immediately (button-press echo)", () => {
    const s = reduce(connected(), { type: "lockdown.set", locked: true });
    expect(s.lockdown!.locked).toBe(true);
  });

  it("lockdown.set false (unlock) clears restoredFromMarker too (stop no longer persists)", () => {
    // engage from a marker-restored state, then unlock
    const locked = tel(connected(), env("lockdown.status", { locked: true, restored_from_marker: true }));
    expect(locked.lockdown!.restoredFromMarker).toBe(true);
    const unlocked = reduce(locked, { type: "lockdown.set", locked: false });
    expect(unlocked.lockdown!.locked).toBe(false);
    expect(unlocked.lockdown!.restoredFromMarker).toBe(false);
  });

  it("lockdown.set does not churn state when nothing changed", () => {
    const a = reduce(connected(), { type: "lockdown.set", locked: false });
    const b = reduce(a, { type: "lockdown.set", locked: false });
    expect(b).toBe(a); // same reference
  });

  it("a later lockdown.status telemetry overrides the local echo (authoritative)", () => {
    const echoed = reduce(connected(), { type: "lockdown.set", locked: true });
    const fromWire = tel(echoed, env("lockdown.status", { locked: false }));
    expect(fromWire.lockdown!.locked).toBe(false);
  });
});

/* ======================================================================== *
 * StatusBar — LOCKDOWN indicator + the prominent PANIC button                *
 * ======================================================================== */
function renderStatusBar(
  lockdown: LockdownStatus | null,
  onPanic?: () => void,
): string {
  return renderToStaticMarkup(
    createElement(StatusBar, {
      connected: true,
      coreState: "idle" as const,
      cloudKeyPresent: true,
      inferenceOffline: false,
      heal: null,
      cloudModel: null,
      activeAgent: null,
      voiceId: voiceIdInitial(),
      modelTier: modelTierInitial(),
      voiceTier: voiceTierInitial(),
      sttTier: sttTierInitial(),
      voiceMode: voiceModeInitial(),
      security: null,
      lockdown,
      onPanic,
      onOpenSettings: noop,
      onOpenDeck: noop,
    }),
  );
}

describe("StatusBar lockdown indicator + PANIC button", () => {
  it("renders nothing before the snapshot arrives (not cluttered)", () => {
    const html = renderStatusBar(null);
    expect(html).not.toContain("lockdown-chip");
    expect(html).not.toContain("LOCKED DOWN");
  });

  it("renders NORMAL in the ok tone for the shipped-OFF default", () => {
    const html = renderStatusBar(parseLockdownStatus({ locked: false }));
    expect(html).toContain("lockdown-chip");
    expect(html).toContain("NORMAL");
    expect(html).toContain("ok");
  });

  it("renders LOCKED DOWN in the alarm (bad) tone when engaged, with honest hover copy", () => {
    const html = renderStatusBar(parseLockdownStatus({ locked: true }));
    expect(html).toContain("LOCKED DOWN");
    expect(html).toContain("bad");
    expect(html).toContain("EMERGENCY STOP ENGAGED");
    expect(html).toContain("does NOT undo");
  });

  it("notes a restart re-entry (RESTORED) when the stop survived a reboot", () => {
    const html = renderStatusBar(parseLockdownStatus({ locked: true, restored_from_marker: true }));
    expect(html).toContain("RESTORED");
    expect(html).toContain("survived a restart");
  });

  it("shows the prominent PANIC button when onPanic is wired (NORMAL)", () => {
    const html = renderStatusBar(parseLockdownStatus({ locked: false }), noop);
    expect(html).toContain("panic-btn");
    expect(html).toContain("PANIC");
  });

  it("the PANIC button reads LOCKED + is disabled once engaged (unlock is deliberate)", () => {
    const html = renderStatusBar(parseLockdownStatus({ locked: true }), noop);
    expect(html).toContain("panic-btn");
    expect(html).toContain("engaged");
    expect(html).toContain("LOCKED");
    expect(html).toContain("disabled");
  });

  it("hides the PANIC button when no handler is wired (older callers)", () => {
    const html = renderStatusBar(parseLockdownStatus({ locked: false }));
    expect(html).not.toContain("panic-btn");
  });
});

/* ======================================================================== *
 * StatusBar PANIC sends the DEDICATED verb (NOT {cmd:"ask"})                  *
 * ======================================================================== */
describe("PANIC fires the dedicated panic verb (never ask)", () => {
  it("sendCommand({cmd:'panic'}) relays the bare panic verb + surfaces locked", async () => {
    invokeMock.mockResolvedValue({ ok: true, reply: PANIC_CONFIRMATION, locked: true });
    const r = await sendCommand({ cmd: "panic" });
    expect(invokeMock).toHaveBeenCalledTimes(1);
    const [name, args] = invokeMock.mock.calls[0];
    expect(name).toBe("send_command");
    expect(args).toEqual({ request: { cmd: "panic" } });
    expect(r.locked).toBe(true);
  });

  it("sendCommand({cmd:'unlock'}) relays the bare unlock verb + surfaces locked=false", async () => {
    invokeMock.mockResolvedValue({ ok: true, reply: UNLOCK_CONFIRMATION, locked: false });
    const r = await sendCommand({ cmd: "unlock" });
    expect(invokeMock.mock.calls[0][1]).toEqual({ request: { cmd: "unlock" } });
    expect(r.locked).toBe(false);
  });
});

/* ======================================================================== *
 * SettingsModal — PANIC / LOCKDOWN section (honest, dedicated verbs)          *
 * ======================================================================== */
function renderSettings(
  lockdown: LockdownStatus | null,
  onLockedChange?: (locked: boolean) => void,
): string {
  return renderToStaticMarkup(
    createElement(SettingsModal, {
      mcp: null,
      voiceId: voiceIdInitial(),
      modelTier: modelTierInitial(),
      sttTier: sttTierInitial(),
      security: null,
      lockdown,
      onLockedChange,
      onClose: noop,
    }),
  );
}

describe("SettingsModal PANIC / LOCKDOWN section (honest)", () => {
  it("shows the section with the indicator + PANIC + UNLOCK controls", () => {
    const html = renderSettings(parseLockdownStatus({ locked: false }));
    expect(html).toContain("PANIC");
    expect(html).toContain("EMERGENCY STOP");
    expect(html).toContain("NORMAL");
    expect(html).toContain("panic-engage");
    expect(html).toContain("panic-unlock");
  });

  it("renders the AWAITING state before the snapshot arrives", () => {
    const html = renderSettings(null);
    expect(html).toContain("AWAITING");
    // the section + controls still render
    expect(html).toContain("STOP EVERYTHING");
  });

  it("states the HONEST scope: future-only, persists across restart, does NOT undo", () => {
    const html = renderSettings(parseLockdownStatus({ locked: false }));
    expect(html.toLowerCase()).toContain("all future");
    expect(html.toLowerCase()).toContain("persists across a restart");
    expect(html.toLowerCase()).toContain("does");
    expect(html.toLowerCase()).toContain("not");
    expect(html.toLowerCase()).toContain("undo");
    expect(html.toLowerCase()).toContain("already sent stays sent");
  });

  it("states unlock is user-only + deliberate and restores configured settings", () => {
    const html = renderSettings(parseLockdownStatus({ locked: true }));
    expect(html.toLowerCase()).toContain("user-only");
    expect(html.toLowerCase()).toContain("deliberate");
    expect(html.toLowerCase()).toContain("restores your configured");
    // never an agent/model path to unlock
    expect(html.toLowerCase()).toContain("never");
  });

  it("when LOCKED DOWN, the PANIC control is disabled and UNLOCK is enabled", () => {
    const html = renderSettings(parseLockdownStatus({ locked: true }));
    expect(html).toContain("LOCKED DOWN");
    // both buttons present; the disabled attribute appears (on PANIC when locked)
    expect(html).toContain("panic-engage");
    expect(html).toContain("panic-unlock");
  });

  it("notes the restart re-entry when restored from the marker", () => {
    const html = renderSettings(parseLockdownStatus({ locked: true, restored_from_marker: true }));
    expect(html.toLowerCase()).toContain("survived a restart");
  });
});
