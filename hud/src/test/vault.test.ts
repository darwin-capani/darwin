import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import VaultIndicator from "../components/VaultIndicator";
import {
  parseVaultStatus,
  vaultInitial,
  vaultLabel,
  vaultTone,
  type TelemetryEnvelope,
  type VaultStatus,
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
    ts: `2026-07-15T12:00:${String(counter % 60).padStart(2, "0")}Z`,
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

/* ======================================================================== *
 * parseVaultStatus — defensive, fail-SAFE, secret-free                       *
 * ======================================================================== */
describe("parseVaultStatus (defensive, fail-safe)", () => {
  it("parses the shipped-OFF default snapshot", () => {
    const v = parseVaultStatus({ active: false, read_only: true, restrict_only: true });
    expect(v.active).toBe(false);
  });

  it("parses an ACTIVE (go-dark engaged) snapshot", () => {
    const v = parseVaultStatus({ active: true });
    expect(v.active).toBe(true);
  });

  it("defaults to NOT-active when the flag is absent/garbled (never a phantom go-dark, never stuck)", () => {
    expect(parseVaultStatus({}).active).toBe(false);
    expect(parseVaultStatus({ active: "yes" }).active).toBe(false);
    expect(parseVaultStatus({ active: 1 }).active).toBe(false);
    expect(() => parseVaultStatus({ active: {} })).not.toThrow();
  });

  it("surfaces ONLY the one boolean (extra wire fields ignored)", () => {
    const v = parseVaultStatus({ active: true, secret: "leak", note: "x" });
    expect(JSON.stringify(v)).not.toContain("leak");
    expect(Object.keys(v)).toEqual(["active"]);
  });

  it("vaultInitial is the honest OFF default", () => {
    expect(vaultInitial()).toEqual({ active: false });
  });
});

/* ======================================================================== *
 * vaultTone / vaultLabel                                                     *
 * ======================================================================== */
describe("vaultTone / vaultLabel", () => {
  it("VAULT is the deliberate (warn) tone when engaged — not lockdown's alarm", () => {
    const v = parseVaultStatus({ active: true });
    expect(vaultLabel(v)).toBe("VAULT");
    expect(vaultTone(v)).toBe("warn");
  });
  it("OPEN is the calm (ok) tone — the shipped default", () => {
    const v = parseVaultStatus({ active: false });
    expect(vaultLabel(v)).toBe("OPEN");
    expect(vaultTone(v)).toBe("ok");
  });
});

/* ======================================================================== *
 * Reducer arm — vault.status snapshot                                        *
 * ======================================================================== */
describe("vault.status reducer", () => {
  it("is null before any snapshot (honest awaiting — vault ships OFF)", () => {
    expect(connected().vault).toBeNull();
  });

  it("sets the posture from a well-formed active event", () => {
    const s = tel(connected(), env("vault.status", { active: true }));
    expect(s.vault).not.toBeNull();
    expect(s.vault!.active).toBe(true);
  });

  it("a malformed payload yields the honest NOT-active snapshot, not a stale one", () => {
    const s = tel(connected(), env("vault.status", { active: "garbage" }));
    expect(s.vault).not.toBeNull();
    expect(s.vault!.active).toBe(false);
  });

  it("a later vault.status telemetry replaces the prior posture (authoritative toggle)", () => {
    const on = tel(connected(), env("vault.status", { active: true }));
    expect(on.vault!.active).toBe(true);
    const off = tel(on, env("vault.status", { active: false }));
    expect(off.vault!.active).toBe(false);
  });
});

/* ======================================================================== *
 * VaultIndicator component — renders the at-a-glance chip                     *
 * ======================================================================== */
function renderIndicator(status: VaultStatus | null): string {
  return renderToStaticMarkup(createElement(VaultIndicator, { status }));
}

describe("VaultIndicator", () => {
  it("renders nothing before the snapshot arrives (not cluttered)", () => {
    const html = renderIndicator(null);
    expect(html).toBe("");
  });

  it("renders OPEN in the ok tone for the shipped-OFF default", () => {
    const html = renderIndicator(parseVaultStatus({ active: false }));
    expect(html).toContain("vault-chip");
    expect(html).toContain("OPEN");
    expect(html).toContain("ok");
    expect(html).toContain("inactive");
  });

  it("renders VAULT in the warn tone when engaged, with honest restrict-only hover copy", () => {
    const html = renderIndicator(parseVaultStatus({ active: true }));
    expect(html).toContain("vault-chip");
    expect(html).toContain("VAULT");
    expect(html).toContain("warn");
    expect(html).toContain("active");
    // Honest scope: local brain, no cloud escalation, restrict-only — never an
    // overclaim of sealing every byte.
    expect(html.toLowerCase()).toContain("local brain");
    expect(html.toLowerCase()).toContain("restrict-only");
    expect(html.toLowerCase()).toContain("maximal");
  });

  it("the chip is SECRET-FREE (only the boolean-derived label/copy renders)", () => {
    const html = renderIndicator(parseVaultStatus({ active: true, secret: "leak" }));
    expect(html).not.toContain("leak");
  });
});
