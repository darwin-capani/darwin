import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import SettingsModal from "../components/SettingsModal";
import StatusBar from "../components/StatusBar";
import {
  applyVoiceIdEnrollProgress,
  applyVoiceIdEnrollStarted,
  applyVoiceIdEnrolled,
  applyVoiceIdForgot,
  applyVoiceIdVerify,
  modelTierInitial,
  sttTierInitial,
  voiceIdDisplay,
  voiceIdInitial,
  voiceIdLabel,
  voiceIdSimilarityPct,
  voiceIdTone,
  voiceTierInitial,
  voiceModeInitial,
  type TelemetryEnvelope,
  type VoiceIdStatus,
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

/* ------------------------------------------------------------------ parsing */

describe("voiceid folding helpers (events.ts)", () => {
  it("seeds the honest OFF / not-enrolled resting state", () => {
    const v = voiceIdInitial();
    expect(v).toEqual({
      enabled: false,
      enrolled: false,
      verified: false,
      score: null,
      enrolling: false,
      captured: null,
      need: null,
    });
  });

  it("applyVoiceIdVerify folds the secret-free verify fields only", () => {
    const v = applyVoiceIdVerify(voiceIdInitial(), {
      verified: true,
      score: 0.91,
      enabled: true,
      enrolled: true,
      // a stray field on the wire must be ignored (never surfaced)
      embedding: [1, 2, 3],
    });
    expect(v.enabled).toBe(true);
    expect(v.enrolled).toBe(true);
    expect(v.verified).toBe(true);
    expect(v.score).toBeCloseTo(0.91);
    // never carries the embedding / any extra field
    expect(Object.keys(v).sort()).toEqual(
      ["captured", "enabled", "enrolled", "enrolling", "need", "score", "verified"].sort(),
    );
  });

  it("clamps the score to [0,1] and nulls a missing/non-finite score", () => {
    expect(applyVoiceIdVerify(voiceIdInitial(), { score: 1.4 }).score).toBe(1);
    expect(applyVoiceIdVerify(voiceIdInitial(), { score: -0.2 }).score).toBe(0);
    expect(applyVoiceIdVerify(voiceIdInitial(), {}).score).toBeNull();
    expect(applyVoiceIdVerify(voiceIdInitial(), { score: "nope" }).score).toBeNull();
  });

  it("a verify with no fields is fail-safe: verified=false (never a stale true)", () => {
    const seeded: VoiceIdStatus = { ...voiceIdInitial(), verified: true, enrolled: true };
    expect(applyVoiceIdVerify(seeded, {}).verified).toBe(false);
  });

  it("enroll_started opens a capture session and resets any verdict", () => {
    const seeded = applyVoiceIdVerify(voiceIdInitial(), {
      verified: true,
      score: 0.9,
      enabled: true,
      enrolled: true,
    });
    const v = applyVoiceIdEnrollStarted(seeded, { need: 3 });
    expect(v.enrolling).toBe(true);
    expect(v.captured).toBe(0);
    expect(v.need).toBe(3);
    expect(v.verified).toBe(false);
    expect(v.score).toBeNull();
  });

  it("enroll_progress advances counters", () => {
    let v = applyVoiceIdEnrollStarted(voiceIdInitial(), { need: 3 });
    v = applyVoiceIdEnrollProgress(v, { captured: 1, need: 2 });
    expect(v).toMatchObject({ enrolling: true, captured: 1, need: 2 });
    v = applyVoiceIdEnrollProgress(v, { captured: 2, need: 1 });
    expect(v).toMatchObject({ captured: 2, need: 1 });
  });

  it("enrolled closes the session and marks a profile on file (no fabricated verdict)", () => {
    let v = applyVoiceIdEnrollStarted({ ...voiceIdInitial(), enabled: true }, { need: 3 });
    v = applyVoiceIdEnrolled(v);
    expect(v.enrolled).toBe(true);
    expect(v.enrolling).toBe(false);
    // a verdict is NOT fabricated on enrol — it rests until the next verify
    expect(v.verified).toBe(false);
    expect(v.score).toBeNull();
  });

  it("forgot clears the profile (falls back to not-enrolled)", () => {
    let v = applyVoiceIdVerify(voiceIdInitial(), {
      verified: true,
      score: 0.9,
      enabled: true,
      enrolled: true,
    });
    v = applyVoiceIdForgot(v);
    expect(v.enrolled).toBe(false);
    expect(v.verified).toBe(false);
    expect(v.score).toBeNull();
    // the master switch can stay enabled; with nothing enrolled it enforces nothing
    expect(v.enabled).toBe(true);
  });
});

/* ----------------------------------------------------------- display states */

describe("voiceIdDisplay (pure derivation)", () => {
  const base = voiceIdInitial();

  it("OFF when the master switch is off (default)", () => {
    expect(voiceIdDisplay(base)).toBe("off");
    expect(voiceIdLabel("off")).toBe("OFF");
    expect(voiceIdTone("off")).toBe("idle");
  });

  it("NOT ENROLLED when enabled but no profile", () => {
    expect(voiceIdDisplay({ ...base, enabled: true })).toBe("unenrolled");
    expect(voiceIdLabel("unenrolled")).toBe("NOT ENROLLED");
    expect(voiceIdTone("unenrolled")).toBe("idle");
  });

  it("ENROLLING dominates while a capture session is open", () => {
    expect(
      voiceIdDisplay({ ...base, enabled: true, enrolling: true, captured: 1, need: 2 }),
    ).toBe("enrolling");
    expect(voiceIdTone("enrolling")).toBe("warn");
  });

  it("ENROLLED resting state when enrolled with no fresh verdict", () => {
    expect(
      voiceIdDisplay({ ...base, enabled: true, enrolled: true, score: null }),
    ).toBe("enrolled");
    expect(voiceIdTone("enrolled")).toBe("idle");
  });

  it("VERIFIED when enrolled + this turn matched", () => {
    const d = voiceIdDisplay({
      ...base,
      enabled: true,
      enrolled: true,
      verified: true,
      score: 0.9,
    });
    expect(d).toBe("verified");
    expect(voiceIdTone("verified")).toBe("good");
    expect(voiceIdLabel("verified")).toBe("VERIFIED");
  });

  it("UNRECOGNIZED when enrolled + this turn did not match", () => {
    const d = voiceIdDisplay({
      ...base,
      enabled: true,
      enrolled: true,
      verified: false,
      score: 0.3,
    });
    expect(d).toBe("unrecognized");
    expect(voiceIdTone("unrecognized")).toBe("bad");
    expect(voiceIdLabel("unrecognized")).toBe("UNRECOGNIZED");
  });

  it("similarity is a percentage, null when no verdict", () => {
    expect(voiceIdSimilarityPct({ ...base, score: 0.876 })).toBe(88);
    expect(voiceIdSimilarityPct({ ...base, score: null })).toBeNull();
  });
});

/* --------------------------------------------------------------- reducer */

describe("reducer voiceid.* events", () => {
  it("seeds voiceId as OFF before any event", () => {
    expect(voiceIdDisplay(initialState().voiceId)).toBe("off");
  });

  it("threads a voiceid.verify verdict into state", () => {
    const s = tel(connected(), env("voiceid.verify", {
      verified: true,
      score: 0.93,
      enabled: true,
      enrolled: true,
    }));
    expect(voiceIdDisplay(s.voiceId)).toBe("verified");
    expect(voiceIdSimilarityPct(s.voiceId)).toBe(93);
  });

  it("an unrecognized turn shows UNRECOGNIZED", () => {
    const s = tel(connected(), env("voiceid.verify", {
      verified: false,
      score: 0.41,
      enabled: true,
      enrolled: true,
    }));
    expect(voiceIdDisplay(s.voiceId)).toBe("unrecognized");
  });

  it("walks the enrollment lifecycle started -> progress -> enrolled", () => {
    let s = tel(connected(), env("voiceid.enroll_started", { need: 3 }));
    // enabled wasn't asserted by enroll_started, but with a session open it ENROLLS
    s = { ...s, voiceId: { ...s.voiceId, enabled: true } };
    expect(voiceIdDisplay(s.voiceId)).toBe("enrolling");
    s = tel(s, env("voiceid.enroll_progress", { captured: 1, need: 2 }));
    expect(s.voiceId.captured).toBe(1);
    expect(s.voiceId.need).toBe(2);
    s = tel(s, env("voiceid.enrolled", { n_samples: 3 }));
    expect(s.voiceId.enrolled).toBe(true);
    expect(s.voiceId.enrolling).toBe(false);
    expect(voiceIdDisplay(s.voiceId)).toBe("enrolled");
  });

  it("voiceid.forgot returns to NOT ENROLLED", () => {
    let s = tel(connected(), env("voiceid.verify", {
      verified: true,
      score: 0.9,
      enabled: true,
      enrolled: true,
    }));
    s = tel(s, env("voiceid.forgot", { had_profile: true }));
    expect(voiceIdDisplay(s.voiceId)).toBe("unenrolled");
  });

  it("a malformed voiceid.verify never throws and never surfaces an embedding", () => {
    const s = tel(connected(), env("voiceid.verify", {
      verified: "yes",
      score: { nested: true },
      embedding: [0.1, 0.2],
      audio: "raw",
    }));
    // verified coerces false (non-bool), score nulls (non-number)
    expect(s.voiceId.verified).toBe(false);
    expect(s.voiceId.score).toBeNull();
    // no embedding/audio key ever lands in state
    expect(Object.keys(s.voiceId)).not.toContain("embedding");
    expect(Object.keys(s.voiceId)).not.toContain("audio");
  });
});

/* ------------------------------------------------------------ render: chip */

const noop = () => {};

function renderStatusBar(voiceId: VoiceIdStatus): string {
  return renderToStaticMarkup(
    createElement(StatusBar, {
      connected: true,
      coreState: "idle" as const,
      cloudKeyPresent: true,
      inferenceOffline: false,
      heal: null,
      cloudModel: null,
      activeAgent: null,
      voiceId,
      modelTier: modelTierInitial(),
      voiceTier: voiceTierInitial(),
      sttTier: sttTierInitial(),
      voiceMode: voiceModeInitial(),
      onOpenSettings: noop,
      onOpenDeck: noop,
    }),
  );
}

describe("StatusBar voice-id chip", () => {
  it("renders OFF in the shipped-OFF default", () => {
    const html = renderStatusBar(voiceIdInitial());
    expect(html).toContain("VOICE OFF");
  });

  it("renders VERIFIED with a similarity readout (not a guarantee)", () => {
    const html = renderStatusBar({
      ...voiceIdInitial(),
      enabled: true,
      enrolled: true,
      verified: true,
      score: 0.94,
    });
    expect(html).toContain("VOICE VERIFIED");
    expect(html).toContain("94%");
    expect(html).toContain("good");
    // the honest hover copy must say it is NOT a biometric
    expect(html).toContain("NOT a biometric");
    expect(html).toContain("similarity");
  });

  it("renders UNRECOGNIZED in the bad tone", () => {
    const html = renderStatusBar({
      ...voiceIdInitial(),
      enabled: true,
      enrolled: true,
      verified: false,
      score: 0.3,
    });
    expect(html).toContain("VOICE UNRECOGNIZED");
    expect(html).toContain("bad");
  });

  it("renders ENROLLING with capture progress", () => {
    const html = renderStatusBar({
      ...voiceIdInitial(),
      enabled: true,
      enrolling: true,
      captured: 1,
      need: 2,
    });
    expect(html).toContain("VOICE ENROLLING");
    expect(html).toContain("1/3");
  });

  it("does NOT present the score as a probability of identity", () => {
    const html = renderStatusBar({
      ...voiceIdInitial(),
      enabled: true,
      enrolled: true,
      verified: true,
      score: 0.94,
    });
    expect(html).not.toMatch(/probability/i);
    expect(html.toLowerCase()).not.toContain("guarantee that");
  });
});

/* -------------------------------------------------------- render: settings */

function renderSettings(voiceId: VoiceIdStatus): string {
  return renderToStaticMarkup(
    createElement(SettingsModal, {
      mcp: null,
      voiceId,
      modelTier: modelTierInitial(),
      sttTier: sttTierInitial(),
      onClose: noop,
    }),
  );
}

describe("SettingsModal voice-id section", () => {
  it("shows the section with the honest framing and lockstep keys", () => {
    const html = renderSettings(voiceIdInitial());
    expect(html).toContain("VOICE-ID");
    // honest copy
    expect(html).toContain("NOT a biometric");
    expect(html).toContain("raises the bar");
    expect(html).toContain("similarity");
    // the EXACT daemon [voice_id] key names (lockstep)
    expect(html).toContain("[voice_id]");
    expect(html).toContain("enabled");
    expect(html).toContain("threshold");
    expect(html).toContain("min_enroll_samples");
    expect(html).toContain("gate_scope");
    // the default threshold matches the daemon default
    expect(html).toContain("0.86");
    // the spoken intents
    expect(html).toContain("enroll my voice");
    expect(html).toContain("forget my voice");
  });

  it("reflects the live OFF state", () => {
    const html = renderSettings(voiceIdInitial());
    expect(html).toContain("OFF");
  });

  it("reflects a live VERIFIED verdict with the similarity", () => {
    const html = renderSettings({
      ...voiceIdInitial(),
      enabled: true,
      enrolled: true,
      verified: true,
      score: 0.9,
    });
    expect(html).toContain("VERIFIED");
    expect(html).toContain("similarity 90%");
  });

  it("never frames the score as a security guarantee", () => {
    const html = renderSettings({
      ...voiceIdInitial(),
      enabled: true,
      enrolled: true,
      verified: true,
      score: 0.9,
    });
    expect(html).not.toMatch(/probability/i);
    // it must explicitly say it is NOT a guarantee
    expect(html).toContain("not a guarantee");
  });
});
