import { describe, expect, it } from "vitest";
import {
  CUE_CATALOG,
  buildPlayCueRequest,
  cueDisabledReason,
  cueGateCopy,
  cueGateOpen,
  cueNames,
  cueOutcomeCopy,
  isKnownCue,
  type CueDisabledReason,
  type CuePlayOutcome,
} from "../core/sfxCue";

/* ----------------------------------------------------- catalog (palette pin) */

describe("SFX cue catalog (sfxCue.ts)", () => {
  it("mirrors the daemon palette: confirm/alert/error/success/notify/wake, in order", () => {
    expect(cueNames()).toEqual([
      "confirm",
      "alert",
      "error",
      "success",
      "notify",
      "wake",
    ]);
  });

  it("every entry has a non-empty label + blurb, and names are unique lowercase atoms", () => {
    const seen = new Set<string>();
    for (const c of CUE_CATALOG) {
      expect(c.label.trim().length).toBeGreaterThan(0);
      expect(c.blurb.trim().length).toBeGreaterThan(0);
      // Names are stable lookup atoms: lowercase, no spaces.
      expect(c.name).toBe(c.name.toLowerCase());
      expect(c.name).not.toMatch(/\s/);
      expect(seen.has(c.name)).toBe(false);
      seen.add(c.name);
    }
  });

  it("a blurb is descriptive copy, never an actual EL prompt or a secret", () => {
    // The HUD must not leak the daemon's curated generation prompt; the blurbs
    // are short paraphrases. Pin that none of them is the literal prompt opener.
    for (const c of CUE_CATALOG) {
      expect(c.blurb.toLowerCase()).not.toContain("sound-generation");
      expect(c.blurb).not.toContain("xi-api-key");
      expect(c.blurb).not.toMatch(/sk-/);
    }
  });

  it("isKnownCue is case-insensitive + trimmed and rejects unknown names", () => {
    expect(isKnownCue("confirm")).toBe(true);
    expect(isKnownCue("Confirm")).toBe(true);
    expect(isKnownCue("  success  ")).toBe(true);
    expect(isKnownCue("kaboom")).toBe(false);
    expect(isKnownCue("")).toBe(false);
  });
});

/* ------------------------------------------------------------------- gate */

describe("cue gate (mirrors voice_tier::sfx_enabled — switch + key)", () => {
  it("is open ONLY when cloud SFX is on AND a key is present", () => {
    expect(cueGateOpen({ cloudSfxOn: true, keyPresent: true })).toBe(true);
    expect(cueGateOpen({ cloudSfxOn: true, keyPresent: false })).toBe(false);
    expect(cueGateOpen({ cloudSfxOn: false, keyPresent: true })).toBe(false);
    expect(cueGateOpen({ cloudSfxOn: false, keyPresent: false })).toBe(false);
  });

  it("cueDisabledReason reports exactly what's missing (no_shell wins)", () => {
    // No shell beats everything — nothing plays without the daemon.
    expect(cueDisabledReason(false, { cloudSfxOn: true, keyPresent: true })).toBe(
      "no_shell",
    );
    // In the shell, the precise missing piece is named.
    expect(cueDisabledReason(true, { cloudSfxOn: false, keyPresent: false })).toBe(
      "switch_off_and_no_key",
    );
    expect(cueDisabledReason(true, { cloudSfxOn: false, keyPresent: true })).toBe(
      "switch_off",
    );
    expect(cueDisabledReason(true, { cloudSfxOn: true, keyPresent: false })).toBe(
      "no_key",
    );
    // Both present in the shell → enabled (null reason).
    expect(cueDisabledReason(true, { cloudSfxOn: true, keyPresent: true })).toBe(
      null,
    );
  });

  it("gate copy is honest for every state and never claims a cue played", () => {
    const states: (CueDisabledReason | null)[] = [
      null,
      "no_shell",
      "switch_off",
      "no_key",
      "switch_off_and_no_key",
    ];
    for (const r of states) {
      const copy = cueGateCopy(r);
      expect(copy.trim().length).toBeGreaterThan(0);
      // Never asserts a successful play in the gate copy.
      expect(copy.toLowerCase()).not.toContain("played the");
    }
    // The enabled copy is honest about the offline no-op (never a fabricated cue).
    expect(cueGateCopy(null).toLowerCase()).toContain("never fabricate");
    // The disabled copies name the concrete fix.
    expect(cueGateCopy("switch_off").toLowerCase()).toContain("cloud_sfx");
    expect(cueGateCopy("no_key").toLowerCase()).toContain("elevenlabs");
    expect(cueGateCopy("no_shell").toLowerCase()).toContain("desktop app");
  });
});

/* -------------------------------------------------- play-request shaping */

describe("buildPlayCueRequest", () => {
  it("shapes a request carrying ONLY the normalized cue name for a known cue", () => {
    expect(buildPlayCueRequest("confirm")).toEqual({ cue: "confirm" });
    // Case + whitespace are normalized to the catalog atom.
    expect(buildPlayCueRequest("  SUCCESS  ")).toEqual({ cue: "success" });
    // The request shape is exactly {cue} — no key/prompt/path field.
    const req = buildPlayCueRequest("wake");
    expect(req && Object.keys(req)).toEqual(["cue"]);
  });

  it("returns null for an unknown name (never fabricates a request)", () => {
    expect(buildPlayCueRequest("kaboom")).toBeNull();
    expect(buildPlayCueRequest("")).toBeNull();
    expect(buildPlayCueRequest("   ")).toBeNull();
  });
});

/* --------------------------------------------------- outcome → prose */

describe("cueOutcomeCopy", () => {
  it("maps each outcome to honest, cue-scoped prose", () => {
    expect(cueOutcomeCopy("played", "confirm")).toContain("confirm");
    expect(cueOutcomeCopy("played", "confirm").toLowerCase()).toContain("played");
    expect(cueOutcomeCopy("cached", "alert").toLowerCase()).toContain("cache");
    // A no-op NEVER reads as "played".
    const disabled = cueOutcomeCopy("disabled", "error").toLowerCase();
    expect(disabled).toContain("did not play");
    expect(disabled).toContain("nothing was produced");
    const unknown = cueOutcomeCopy("unknown", "kaboom").toLowerCase();
    expect(unknown).toContain("no built-in cue");
    const failed = cueOutcomeCopy("failed", "notify").toLowerCase();
    expect(failed).toContain("nothing was produced");
    expect(cueOutcomeCopy("no_shell", "wake").toLowerCase()).toContain("desktop app");
  });

  it("never leaks a path/secret and falls back to a generic label for a blank cue", () => {
    const outcomes: CuePlayOutcome[] = [
      "played",
      "cached",
      "disabled",
      "unknown",
      "failed",
      "no_shell",
    ];
    for (const o of outcomes) {
      const copy = cueOutcomeCopy(o, "");
      expect(copy).not.toMatch(/\.wav/);
      expect(copy).not.toMatch(/sk-/);
      // A blank cue still yields a readable line (generic "cue" label).
      expect(copy.trim().length).toBeGreaterThan(0);
    }
  });
});
