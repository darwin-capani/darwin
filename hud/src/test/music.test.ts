import { describe, expect, it } from "vitest";
import {
  DEFAULT_LENGTH_SECONDS,
  MAX_LENGTH_SECONDS,
  MIN_LENGTH_SECONDS,
  buildComposeMusicRequest,
  classifyMusicReply,
  composeMusicErrorCopy,
  composeMusicOutcomeCopy,
  musicDisabledReason,
  musicGateCopy,
  musicGateOpen,
  type MusicDisabledReason,
  type MusicOutcome,
} from "../core/music";

/* ------------------------------------------------------------------- gate */

describe("compose-music gate (mirrors the daemon key + non-Local-tier gate)", () => {
  it("is open ONLY when cloud music is on AND a key is present", () => {
    expect(musicGateOpen({ cloudMusicOn: true, keyPresent: true })).toBe(true);
    expect(musicGateOpen({ cloudMusicOn: true, keyPresent: false })).toBe(false);
    expect(musicGateOpen({ cloudMusicOn: false, keyPresent: true })).toBe(false);
    expect(musicGateOpen({ cloudMusicOn: false, keyPresent: false })).toBe(false);
  });

  it("musicDisabledReason reports exactly what's missing (no_shell wins)", () => {
    // No shell beats everything — nothing is created without the daemon.
    expect(
      musicDisabledReason(false, { cloudMusicOn: true, keyPresent: true }),
    ).toBe("no_shell");
    // In the shell, the precise missing piece is named.
    expect(
      musicDisabledReason(true, { cloudMusicOn: false, keyPresent: false }),
    ).toBe("music_off_and_no_key");
    expect(
      musicDisabledReason(true, { cloudMusicOn: false, keyPresent: true }),
    ).toBe("music_off");
    expect(
      musicDisabledReason(true, { cloudMusicOn: true, keyPresent: false }),
    ).toBe("no_key");
    // Both present in the shell → enabled (null reason).
    expect(
      musicDisabledReason(true, { cloudMusicOn: true, keyPresent: true }),
    ).toBe(null);
  });

  it("gate copy is honest for every state and never claims a creation", () => {
    const states: (MusicDisabledReason | null)[] = [
      null,
      "no_shell",
      "music_off",
      "no_key",
      "music_off_and_no_key",
    ];
    for (const r of states) {
      const copy = musicGateCopy(r);
      expect(copy.trim().length).toBeGreaterThan(0);
      // Never asserts a successful creation in the gate copy.
      expect(copy.toLowerCase()).not.toContain("composed a track");
    }
    // The enabled copy is honest about the offline no-op (never a fabricated track).
    expect(musicGateCopy(null).toLowerCase()).toContain("never fabricate");
    // The disabled copies name the concrete fix.
    expect(musicGateCopy("music_off").toLowerCase()).toContain("cloud_music");
    expect(musicGateCopy("no_key").toLowerCase()).toContain("elevenlabs");
    expect(musicGateCopy("no_shell").toLowerCase()).toContain("desktop app");
  });
});

/* -------------------------------------------------- compose-music shaping */

describe("buildComposeMusicRequest (mirrors the daemon decide floor)", () => {
  it("shapes a request from a non-empty prompt, omitting length when blank", () => {
    const r = buildComposeMusicRequest("  an 8-bit happy birthday  ", "");
    expect(r.ok).toBe(true);
    if (r.ok) {
      // Trimmed; the blank length is OMITTED (the daemon defaults it).
      expect(r.request).toEqual({ prompt: "an 8-bit happy birthday" });
      expect(Object.keys(r.request)).toEqual(["prompt"]);
    }
  });

  it("omits length for an undefined / null / non-numeric length too", () => {
    for (const len of [undefined, null, "  ", "abc"]) {
      const r = buildComposeMusicRequest("an 8-bit happy birthday", len);
      expect(r.ok).toBe(true);
      if (r.ok) expect(r.request.lengthMs).toBeUndefined();
    }
  });

  it("converts a given length (seconds) to milliseconds", () => {
    const r = buildComposeMusicRequest("lo-fi beat", String(DEFAULT_LENGTH_SECONDS));
    expect(r.ok).toBe(true);
    if (r.ok) expect(r.request.lengthMs).toBe(DEFAULT_LENGTH_SECONDS * 1000);
    // A numeric value also works.
    const n = buildComposeMusicRequest("lo-fi beat", 45);
    expect(n.ok).toBe(true);
    if (n.ok) expect(n.request.lengthMs).toBe(45_000);
  });

  it("clamps the length into the [MIN, MAX] seconds band", () => {
    const tooShort = buildComposeMusicRequest("x", "1");
    expect(tooShort.ok).toBe(true);
    if (tooShort.ok) expect(tooShort.request.lengthMs).toBe(MIN_LENGTH_SECONDS * 1000);
    const tooLong = buildComposeMusicRequest("x", "99999");
    expect(tooLong.ok).toBe(true);
    if (tooLong.ok) expect(tooLong.request.lengthMs).toBe(MAX_LENGTH_SECONDS * 1000);
    // A non-positive value clamps up to the floor (never a 0/negative length).
    const zero = buildComposeMusicRequest("x", "0");
    expect(zero.ok).toBe(true);
    if (zero.ok) expect(zero.request.lengthMs).toBe(MIN_LENGTH_SECONDS * 1000);
  });

  it("rejects an empty / whitespace prompt (the daemon floor)", () => {
    expect(buildComposeMusicRequest("   ", "30")).toEqual({
      ok: false,
      error: "no_prompt",
    });
    expect(buildComposeMusicRequest("", "")).toEqual({
      ok: false,
      error: "no_prompt",
    });
  });

  it("error copy names the concrete fix", () => {
    expect(composeMusicErrorCopy("no_prompt").toLowerCase()).toContain("music");
  });
});

/* ------------------------------------------------- outcome → prose */

describe("classifyMusicReply (PROSE-based, fail-safe — never the bare ok flag)", () => {
  it("an ok reply with clean success prose is a genuine creation", () => {
    expect(
      classifyMusicReply(true, "Composed a 30s track from “an 8-bit happy birthday”."),
    ).toBe("created");
  });

  it("a gate-closed / offline / no-key reply is an honest unavailable no-op", () => {
    expect(
      classifyMusicReply(
        false,
        "Composing music needs the cloud tier, but you're working offline — nothing was created.",
      ),
    ).toBe("unavailable");
    expect(
      classifyMusicReply(
        false,
        "I can't compose music without an ElevenLabs key — add one in Settings. Nothing was created.",
      ),
    ).toBe("unavailable");
    expect(
      classifyMusicReply(false, "Music generation is unavailable right now."),
    ).toBe("unavailable");
  });

  it("any other failure reads as failed (never a fabricated success)", () => {
    expect(
      classifyMusicReply(
        false,
        "I couldn't compose that track just now — the cloud request didn't go through. Nothing was created.",
      ),
    ).toBe("failed");
    // A protocol-level relay failure also reads as failed, never created.
    expect(classifyMusicReply(false, "unknown_command")).toBe("failed");
  });

  it("FAIL-SAFE: an ok:true dispatch carrying gated/failure PROSE never reads created", () => {
    // The command channel returns ok:true for any dispatched verb; a closed gate /
    // failure rides in the prose. Such a reply must NOT read "created".
    expect(
      classifyMusicReply(
        true,
        "Composing music needs the cloud tier, but you're working offline — nothing was created.",
      ),
    ).toBe("unavailable");
    expect(
      classifyMusicReply(
        true,
        "I couldn't compose that track just now — the cloud request didn't go through. Nothing was created.",
      ),
    ).toBe("failed");
  });
});

describe("compose-music outcome copy is honest + secret-free", () => {
  it("maps each outcome and never claims a phantom creation", () => {
    expect(composeMusicOutcomeCopy("created", "an 8-bit happy birthday")).toContain(
      "an 8-bit happy birthday",
    );
    const unavailable = composeMusicOutcomeCopy("unavailable", "x").toLowerCase();
    expect(unavailable).toContain("nothing was created");
    const failed = composeMusicOutcomeCopy("failed", "x").toLowerCase();
    expect(failed).toContain("nothing was created");
    expect(composeMusicOutcomeCopy("no_shell", "x").toLowerCase()).toContain(
      "desktop app",
    );
  });

  it("never leaks a secret-shaped marker and falls back for a blank prompt", () => {
    const outcomes: MusicOutcome[] = ["created", "unavailable", "failed", "no_shell"];
    for (const o of outcomes) {
      const copy = composeMusicOutcomeCopy(o, "");
      expect(copy).not.toMatch(/sk-/);
      expect(copy).not.toMatch(/\.wav/);
      expect(copy).not.toMatch(/\.mp3/);
      expect(copy.trim().length).toBeGreaterThan(0);
    }
  });
});
