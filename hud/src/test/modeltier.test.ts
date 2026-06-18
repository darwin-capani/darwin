import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import SettingsModal, {
  MODEL_SWAP_BUTTON_PHRASES,
} from "../components/SettingsModal";
import StatusBar from "../components/StatusBar";
import {
  applyModelSwap,
  applyModelTier,
  modelTierHonest,
  modelTierInitial,
  modelTierLabel,
  modelTierModeLabel,
  modelTierReasonLabel,
  modelTierTone,
  type ModelTierStatus,
  type TelemetryEnvelope,
  type VoiceIdStatus,
  sttTierInitial,
  voiceIdInitial,
  voiceTierInitial,
  voiceModeInitial,
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

/* ----------------------------------------------------------------- folding */

describe("model-tier folding helpers (events.ts)", () => {
  it("seeds the honest AUTO / awaiting resting state", () => {
    const m = modelTierInitial();
    expect(m).toEqual({ tier: null, reason: null, manual: false, lastSwap: null });
  });

  it("applyModelTier folds the per-turn verdict (tier/reason/manual)", () => {
    const m = applyModelTier(modelTierInitial(), {
      tier: "heavy",
      reason: "auto",
      manual: false,
      intent: "conversation",
    });
    expect(m.tier).toBe("heavy");
    expect(m.reason).toBe("auto");
    expect(m.manual).toBe(false);
  });

  it("an override turn reads as MANUAL", () => {
    const m = applyModelTier(modelTierInitial(), {
      tier: "local",
      reason: "override",
      manual: true,
    });
    expect(m.tier).toBe("local");
    expect(m.reason).toBe("override");
    expect(m.manual).toBe(true);
  });

  it("derives manual from reason==override when the bool is absent", () => {
    const m = applyModelTier(modelTierInitial(), { tier: "fast", reason: "override" });
    expect(m.manual).toBe(true);
    const a = applyModelTier(modelTierInitial(), { tier: "fast", reason: "auto" });
    expect(a.manual).toBe(false);
  });

  it("ignores an unknown tier/reason (keeps the prior value, never blanks)", () => {
    const seeded = applyModelTier(modelTierInitial(), { tier: "heavy", reason: "auto" });
    const m = applyModelTier(seeded, { tier: "supercluster", reason: "vibes" });
    expect(m.tier).toBe("heavy");
    expect(m.reason).toBe("auto");
  });

  it("a fallback turn surfaces local with reason=fallback (the honest degrade)", () => {
    const m = applyModelTier(modelTierInitial(), { tier: "local", reason: "fallback" });
    expect(m.tier).toBe("local");
    expect(m.reason).toBe("fallback");
    expect(m.manual).toBe(false);
  });

  it("applyModelSwap pins a manual tier and previews it at once", () => {
    const m = applyModelSwap(modelTierInitial(), {
      intent: "heavy",
      override: "heavy",
      manual: true,
    });
    expect(m.lastSwap).toBe("heavy");
    expect(m.manual).toBe(true);
    expect(m.tier).toBe("heavy");
    expect(m.reason).toBe("override");
  });

  it("applyModelSwap Auto clears to AUTO and does NOT fabricate a tier", () => {
    const seeded = applyModelTier(modelTierInitial(), { tier: "heavy", reason: "override" });
    const m = applyModelSwap(seeded, { intent: "auto", override: null, manual: false });
    expect(m.lastSwap).toBe("auto");
    expect(m.manual).toBe(false);
    // tier is left to the next answered turn to re-confirm (not invented).
    expect(m.tier).toBe("heavy");
  });

  it("applyModelSwap a local override is the on-device privacy pin", () => {
    const m = applyModelSwap(modelTierInitial(), {
      intent: "local",
      override: "local",
      manual: true,
    });
    expect(m.tier).toBe("local");
    expect(m.manual).toBe(true);
    expect(m.reason).toBe("override");
  });

  it("never throws on a malformed swap/tier and never surfaces a stray field", () => {
    const m = applyModelSwap(modelTierInitial(), {
      intent: { nested: true },
      override: 42,
      secret: "leak",
    } as unknown as Record<string, unknown>);
    expect(Object.keys(m).sort()).toEqual(["lastSwap", "manual", "reason", "tier"].sort());
    expect(Object.keys(m)).not.toContain("secret");
  });
});

/* ---------------------------------------------------------- derivation */

describe("model-tier derivation (pure)", () => {
  it("labels the tiers honestly", () => {
    expect(modelTierLabel("local")).toBe("LOCAL");
    expect(modelTierLabel("fast")).toBe("FAST");
    expect(modelTierLabel("heavy")).toBe("HEAVY");
    expect(modelTierLabel(null)).toBe("AWAITING");
  });

  it("modes are MANUAL/AUTO; reasons are OVERRIDE/AUTO/FALLBACK", () => {
    expect(modelTierModeLabel(true)).toBe("MANUAL");
    expect(modelTierModeLabel(false)).toBe("AUTO");
    expect(modelTierReasonLabel("override")).toBe("OVERRIDE");
    expect(modelTierReasonLabel("auto")).toBe("AUTO");
    expect(modelTierReasonLabel("fallback")).toBe("FALLBACK");
    expect(modelTierReasonLabel(null)).toBe("");
  });

  it("tone: heavy=good, fast=warn, local=idle, fallback=bad (a degrade)", () => {
    expect(modelTierTone("heavy", "auto")).toBe("good");
    expect(modelTierTone("fast", "auto")).toBe("warn");
    expect(modelTierTone("local", "override")).toBe("idle");
    // fallback dominates the tone regardless of tier — it is worth noticing.
    expect(modelTierTone("local", "fallback")).toBe("bad");
    expect(modelTierTone(null, null)).toBe("idle");
  });

  it("the honest copy states LOCAL is on-device + capability-limited (not Opus)", () => {
    const local = modelTierHonest("local");
    expect(local).toMatch(/on-device/i);
    expect(local).toMatch(/private/i);
    expect(local.toLowerCase()).toContain("not opus");
    // heavy is named as the most capable cloud tier
    expect(modelTierHonest("heavy")).toMatch(/most capable/i);
  });
});

/* --------------------------------------------------------------- reducer */

describe("reducer model.tier / model.swap events", () => {
  it("seeds modelTier as AUTO/awaiting before any event", () => {
    const m = initialState().modelTier;
    expect(modelTierLabel(m.tier)).toBe("AWAITING");
    expect(modelTierModeLabel(m.manual)).toBe("AUTO");
  });

  it("threads a model.tier verdict into state", () => {
    const s = tel(connected(), env("model.tier", {
      tier: "heavy",
      reason: "auto",
      manual: false,
    }));
    expect(s.modelTier.tier).toBe("heavy");
    expect(s.modelTier.reason).toBe("auto");
    expect(s.modelTier.manual).toBe(false);
  });

  it("a model.swap pins MANUAL immediately (before the next turn)", () => {
    const s = tel(connected(), env("model.swap", {
      intent: "local",
      override: "local",
      manual: true,
    }));
    expect(s.modelTier.manual).toBe(true);
    expect(s.modelTier.tier).toBe("local");
    expect(s.modelTier.lastSwap).toBe("local");
  });

  it("auto swap then an auto turn returns to AUTO mode", () => {
    let s = tel(connected(), env("model.swap", { intent: "heavy", override: "heavy", manual: true }));
    expect(s.modelTier.manual).toBe(true);
    s = tel(s, env("model.swap", { intent: "auto", override: null, manual: false }));
    expect(s.modelTier.manual).toBe(false);
    s = tel(s, env("model.tier", { tier: "fast", reason: "auto", manual: false }));
    expect(s.modelTier.reason).toBe("auto");
    expect(s.modelTier.tier).toBe("fast");
  });

  it("a cloud-unreachable turn surfaces FALLBACK to local", () => {
    const s = tel(connected(), env("model.tier", { tier: "local", reason: "fallback", manual: false }));
    expect(s.modelTier.tier).toBe("local");
    expect(s.modelTier.reason).toBe("fallback");
  });

  it("a malformed model.tier never throws and never blanks a known tier", () => {
    let s = tel(connected(), env("model.tier", { tier: "heavy", reason: "auto" }));
    s = tel(s, env("model.tier", { tier: 99, reason: [] }));
    expect(s.modelTier.tier).toBe("heavy");
  });
});

/* ------------------------------------------------------------ render: chip */

const noop = () => {};

function renderStatusBar(modelTier: ModelTierStatus): string {
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
      modelTier,
      voiceTier: voiceTierInitial(),
      sttTier: sttTierInitial(),
      voiceMode: voiceModeInitial(),
      onOpenSettings: noop,
      onOpenDeck: noop,
    }),
  );
}

describe("StatusBar model-tier chip", () => {
  it("renders AWAITING / AUTO in the seeded resting state", () => {
    const html = renderStatusBar(modelTierInitial());
    expect(html).toContain("MODEL AWAITING");
    expect(html).toContain("AUTO");
  });

  it("renders HEAVY in the good tone with the AUTO mode", () => {
    const html = renderStatusBar({
      tier: "heavy",
      reason: "auto",
      manual: false,
      lastSwap: null,
    });
    expect(html).toContain("MODEL HEAVY");
    expect(html).toContain("AUTO");
    expect(html).toContain("good");
  });

  it("renders LOCAL as MANUAL with the honest on-device/not-Opus hover copy", () => {
    const html = renderStatusBar({
      tier: "local",
      reason: "override",
      manual: true,
      lastSwap: "local",
    });
    expect(html).toContain("MODEL LOCAL");
    expect(html).toContain("MANUAL");
    // honest hover: on-device + capability ceiling, NOT a safety gate change
    expect(html.toLowerCase()).toContain("on-device");
    expect(html.toLowerCase()).toContain("not opus");
    expect(html).toContain("MODEL-ONLY");
  });

  it("flags FALLBACK inline in the bad tone (a cloud degrade)", () => {
    const html = renderStatusBar({
      tier: "local",
      reason: "fallback",
      manual: false,
      lastSwap: null,
    });
    expect(html).toContain("FALLBACK");
    expect(html).toContain("bad");
  });

  it("never implies LOCAL equals the heavy cloud model's quality", () => {
    const html = renderStatusBar({
      tier: "local",
      reason: "override",
      manual: true,
      lastSwap: "local",
    });
    // must not claim local is as good as / equal to Opus
    expect(html.toLowerCase()).not.toMatch(/local.{0,20}(equals|as good as|same as).{0,20}opus/);
  });
});

/* -------------------------------------------------------- render: settings */

const seededVoice: VoiceIdStatus = voiceIdInitial();

function renderSettings(modelTier: ModelTierStatus): string {
  return renderToStaticMarkup(
    createElement(SettingsModal, {
      mcp: null,
      voiceId: seededVoice,
      modelTier,
      sttTier: sttTierInitial(),
      onClose: noop,
    }),
  );
}

describe("SettingsModal model-tier section", () => {
  it("shows the section with the honest framing and lockstep config keys", () => {
    const html = renderSettings(modelTierInitial());
    expect(html).toContain("MODEL TIER");
    // honest copy
    expect(html).toContain("MODEL-ONLY");
    expect(html.toLowerCase()).toContain("on-device");
    expect(html.toLowerCase()).toContain("not opus");
    expect(html.toLowerCase()).toContain("heuristic");
    expect(html).toContain("private");
    // the EXACT daemon [router] config keys (lockstep)
    expect(html).toContain("[router]");
    expect(html).toContain("conversation_route");
    expect(html).toContain("cloud_confidence_threshold");
    // the route values match the daemon defaults
    expect(html).toContain("cloud_heavy");
    expect(html).toContain("cloud_fast");
    expect(html).toContain("local");
  });

  it("offers the four tier controls (HEAVY / FAST / LOCAL / AUTO)", () => {
    const html = renderSettings(modelTierInitial());
    expect(html).toContain("HEAVY");
    expect(html).toContain("FAST");
    expect(html).toContain("LOCAL");
    expect(html).toContain("AUTO");
  });

  // The HUD half of the round-trip the daemon's
  // `settings_button_phrases_round_trip_to_their_intent` locks: the button
  // click handlers send EXACTLY these phrases, and the daemon classifier must
  // recognize each. The AUTO phrase regressing to "auto, you pick the model"
  // (which classifies as None -> leaks to the normal answer path, override never
  // cleared) is the concrete bug this guards. Keep the two literal sets in sync.
  it("the four button phrases are the exact strings the daemon classifier matches", () => {
    expect(MODEL_SWAP_BUTTON_PHRASES).toEqual({
      heavy: "use the most powerful model",
      fast: "use the fast model",
      local: "work offline, stay on device",
      // NOT "auto, you pick the model" — that classifies as None. Must be a
      // phrase classify_model_swap maps to ModelSwapIntent::Auto (clears override).
      auto: "auto mode",
    });
    // The AUTO control must NOT use the historically broken phrase.
    expect(MODEL_SWAP_BUTTON_PHRASES.auto).not.toBe("auto, you pick the model");
  });

  it("reflects a live HEAVY / AUTO verdict", () => {
    const html = renderSettings({ tier: "heavy", reason: "auto", manual: false, lastSwap: null });
    expect(html).toContain("HEAVY");
    expect(html).toContain("AUTO");
  });

  it("reflects a live MANUAL override and a FALLBACK degrade", () => {
    const manual = renderSettings({ tier: "local", reason: "override", manual: true, lastSwap: "local" });
    expect(manual).toContain("MANUAL");
    expect(manual).toContain("OVERRIDE");
    const fb = renderSettings({ tier: "local", reason: "fallback", manual: false, lastSwap: null });
    expect(fb).toContain("FALLBACK");
  });

  it("never frames LOCAL as a substitute for the heavy model on hard tasks", () => {
    const html = renderSettings(modelTierInitial());
    expect(html.toLowerCase()).toContain("not a substitute");
  });
});
