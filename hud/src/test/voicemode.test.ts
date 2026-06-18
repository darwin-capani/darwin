import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import StatusBar from "../components/StatusBar";
import {
  applyVoiceMode,
  modelTierInitial,
  prosodyProfileLabel,
  sttTierInitial,
  voiceIdInitial,
  voiceModeInitial,
  voiceModeRichDetail,
  voiceModeTone,
  voiceModeWhisperDetail,
  voiceTierInitial,
  type TelemetryEnvelope,
  type VoiceModeStatus,
} from "../core/events";
import { HudState, initialState, reduce } from "../core/state";

/* helpers ------------------------------------------------------------------ */

let counter = 0;
function env(
  event: string,
  data: Record<string, unknown> = {},
  source = "voice",
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

/* ----------------------------------------------------------------- folding */

describe("voice-mode folding helpers (events.ts)", () => {
  it("seeds the honest OFF/neutral resting default both features ship at", () => {
    expect(voiceModeInitial()).toEqual({
      profile: "neutral",
      backend: null,
      rich: false,
      whisper: false,
      terse: false,
      rate: 1.0,
      volume: 1.0,
      seen: false,
    });
  });

  it("folds an EL-v3 urgent + rich verdict", () => {
    const v = applyVoiceMode(voiceModeInitial(), {
      profile: "urgent",
      backend: "elevenlabs",
      rich: true,
      whisper: false,
      terse: false,
      rate: 1.08,
      volume: 1.0,
    });
    expect(v.profile).toBe("urgent");
    expect(v.backend).toBe("elevenlabs");
    expect(v.rich).toBe(true);
    expect(v.seen).toBe(true);
  });

  it("folds a Kokoro calm verdict — coarse, NEVER rich", () => {
    const v = applyVoiceMode(voiceModeInitial(), {
      profile: "calm",
      backend: "kokoro",
      rich: false,
      rate: 0.95,
      volume: 1.0,
    });
    expect(v.profile).toBe("calm");
    expect(v.backend).toBe("kokoro");
    expect(v.rich).toBe(false);
  });

  it("HONESTY: never honours a rich:true claim on a non-EL backend", () => {
    // A hostile/garbled frame claiming rich prosody on Kokoro must NOT surface as
    // rich — rich prosody is ElevenLabs-v3-gated and is never faked on-device.
    const v = applyVoiceMode(voiceModeInitial(), {
      profile: "warm",
      backend: "kokoro",
      rich: true, // a lie — pinned to false because backend is not elevenlabs
    });
    expect(v.rich).toBe(false);
  });

  it("folds whisper-on (terser + softer) delivery facts", () => {
    const v = applyVoiceMode(voiceModeInitial(), {
      profile: "neutral",
      backend: "kokoro",
      rich: false,
      whisper: true,
      terse: true,
      rate: 1.0,
      volume: 0.45,
    });
    expect(v.whisper).toBe(true);
    expect(v.terse).toBe(true);
    expect(v.volume).toBe(0.45);
  });

  it("drops a malformed frame back to the honest neutral default", () => {
    // Unknown profile -> neutral; non-finite rate/volume -> 1.0; no booleans -> off.
    const v = applyVoiceMode(voiceModeInitial(), {
      profile: "ecstatic", // unknown -> neutral
      rate: "fast", // wrong type -> 1.0
      volume: null, // missing-ish -> 1.0
    });
    expect(v.profile).toBe("neutral");
    expect(v.rich).toBe(false);
    expect(v.whisper).toBe(false);
    expect(v.terse).toBe(false);
    expect(v.rate).toBe(1.0);
    expect(v.volume).toBe(1.0);
  });

  it("never reads a key/voice id/text even if a frame smuggled one in", () => {
    const v = applyVoiceMode(voiceModeInitial(), {
      profile: "calm",
      backend: "elevenlabs",
      rich: true,
      el_key: "sk-should-be-ignored",
      voice_id: "EL_SECRET_VOICE",
      text: "the literal spoken sentence",
    });
    const s = JSON.stringify(v);
    expect(s).not.toContain("sk-should-be-ignored");
    expect(s).not.toContain("EL_SECRET_VOICE");
    expect(s).not.toContain("the literal spoken sentence");
  });

  it("labels + tones the profiles honestly", () => {
    expect(prosodyProfileLabel("neutral")).toBe("NEUTRAL");
    expect(prosodyProfileLabel("urgent")).toBe("URGENT");
    // Urgent = attention accent (amber); calm/warm = calm default; neutral = idle.
    expect(voiceModeTone("urgent")).toBe("warn");
    expect(voiceModeTone("calm")).toBe("good");
    expect(voiceModeTone("warm")).toBe("good");
    expect(voiceModeTone("neutral")).toBe("idle");
  });

  it("rich detail states the EL-v3 gate honestly per backend", () => {
    const rich = applyVoiceMode(voiceModeInitial(), {
      profile: "urgent",
      backend: "elevenlabs",
      rich: true,
    });
    expect(voiceModeRichDetail(rich).toLowerCase()).toContain("rich prosody active");
    expect(voiceModeRichDetail(rich).toLowerCase()).toContain("v3");

    const kokoro = applyVoiceMode(voiceModeInitial(), {
      profile: "calm",
      backend: "kokoro",
      rich: false,
    });
    const kDetail = voiceModeRichDetail(kokoro).toLowerCase();
    expect(kDetail).toContain("coarse");
    expect(kDetail).toContain("never faked");

    const nonV3 = applyVoiceMode(voiceModeInitial(), {
      profile: "warm",
      backend: "elevenlabs",
      rich: false, // EL but not v3
    });
    expect(voiceModeRichDetail(nonV3).toLowerCase()).toContain("not v3");
  });

  it("whisper detail states the never-silence guarantee", () => {
    const on: VoiceModeStatus = { ...voiceModeInitial(), whisper: true, seen: true };
    const off: VoiceModeStatus = { ...voiceModeInitial(), seen: true };
    expect(voiceModeWhisperDetail(on).toLowerCase()).toContain("never suppresses");
    expect(voiceModeWhisperDetail(on).toLowerCase()).toContain("delivery only");
    expect(voiceModeWhisperDetail(off).toLowerCase()).toContain("never whether");
  });
});

/* ----------------------------------------------------------- state folding */

describe("voice.prosody in the HUD reducer", () => {
  it("starts in the seeded OFF/neutral default", () => {
    expect(initialState().voiceMode).toEqual(voiceModeInitial());
    expect(initialState().voiceMode.seen).toBe(false);
  });

  it("folds a voice.prosody telemetry frame", () => {
    let s = connected();
    s = tel(
      s,
      env("voice.prosody", {
        profile: "urgent",
        backend: "elevenlabs",
        rich: true,
        whisper: false,
        terse: false,
        rate: 1.08,
        volume: 1.0,
      }),
    );
    expect(s.voiceMode.profile).toBe("urgent");
    expect(s.voiceMode.rich).toBe(true);
    expect(s.voiceMode.seen).toBe(true);
    // A later on-device calm reply flips tone + drops rich honestly.
    s = tel(
      s,
      env("voice.prosody", { profile: "calm", backend: "kokoro", rich: false }),
    );
    expect(s.voiceMode.profile).toBe("calm");
    expect(s.voiceMode.rich).toBe(false);
    expect(s.voiceMode.backend).toBe("kokoro");
  });

  it("a garbled frame degrades to neutral, never throws or fabricates rich", () => {
    let s = connected();
    s = tel(s, env("voice.prosody", { junk: true, rich: true }));
    expect(s.voiceMode.profile).toBe("neutral");
    expect(s.voiceMode.rich).toBe(false); // no backend -> not honoured
    expect(s.voiceMode.seen).toBe(true);
  });
});

/* -------------------------------------------------------- StatusBar render */

const noop = () => {};

function renderStatusBar(voiceMode: VoiceModeStatus): string {
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
      voiceMode,
      onOpenSettings: noop,
      onOpenDeck: noop,
    }),
  );
}

describe("StatusBar voice-mode chip", () => {
  it("renders NOTHING in the resting OFF/neutral default (not yet seen)", () => {
    const html = renderStatusBar(voiceModeInitial());
    expect(html).not.toContain("voicemode-chip");
    expect(html).not.toContain("TONE");
  });

  it("renders the URGENT + RICH tone in the amber accent for EL-v3", () => {
    const html = renderStatusBar({
      profile: "urgent",
      backend: "elevenlabs",
      rich: true,
      whisper: false,
      terse: false,
      rate: 1.08,
      volume: 1.0,
      seen: true,
    });
    expect(html).toContain("TONE URGENT");
    expect(html).toContain("RICH");
    expect(html).toContain("warn");
  });

  it("renders CALM + coarse honestly for Kokoro (never fakes rich)", () => {
    const html = renderStatusBar({
      profile: "calm",
      backend: "kokoro",
      rich: false,
      whisper: false,
      terse: false,
      rate: 0.95,
      volume: 1.0,
      seen: true,
    });
    expect(html).toContain("TONE CALM");
    expect(html).toContain("coarse");
    expect(html).not.toContain("· RICH");
    // The hover copy states the EL-v3 gate + never-faked posture.
    expect(html.toLowerCase()).toContain("never faked");
  });

  it("appends a WHISPER marker only while discreet mode is engaged", () => {
    const on = renderStatusBar({
      profile: "neutral",
      backend: "kokoro",
      rich: false,
      whisper: true,
      terse: true,
      rate: 1.0,
      volume: 0.45,
      seen: true,
    });
    expect(on).toContain("WHISPER");
    // The copy states whisper never suppresses a required confirmation.
    expect(on.toLowerCase()).toContain("never suppresses a required confirmation");

    const off = renderStatusBar({
      profile: "neutral",
      backend: "kokoro",
      rich: false,
      whisper: false,
      terse: false,
      rate: 1.0,
      volume: 1.0,
      seen: true,
    });
    expect(off).not.toContain("WHISPER");
  });

  it("never renders a key/voice id/text in the chip", () => {
    const html = renderStatusBar({
      profile: "warm",
      backend: "elevenlabs",
      rich: true,
      whisper: false,
      terse: false,
      rate: 1.0,
      volume: 1.0,
      seen: true,
    });
    expect(html).not.toContain("xi-api-key");
    expect(html).not.toContain("sk-");
  });
});
