import { describe, expect, it } from "vitest";
import {
  AMP_ATTACK,
  AMP_RELEASE,
  HUE_CYAN,
  HUE_VIOLET,
  WAVE_SILENT_ENTER,
  WAVE_SILENT_EXIT,
  ampFollow,
  coreVisualTarget,
  damp,
  dampHue,
  syntheticSpeechEnvelope,
  waveSilentTarget,
} from "../core/visuals";
import type { CoreState } from "../core/state";

const ALL: CoreState[] = [
  "offline",
  "idle",
  "listening",
  "processing",
  "thinking-local",
  "thinking-cloud",
  "speaking",
];

describe("coreVisualTarget", () => {
  it("returns a finite target for every state", () => {
    for (const s of ALL) {
      const t = coreVisualTarget(s, 0.02);
      for (const v of Object.values(t)) {
        expect(Number.isFinite(v)).toBe(true);
      }
    }
  });

  it("only thinking-cloud shifts hue to violet and streams particles upward", () => {
    for (const s of ALL) {
      const t = coreVisualTarget(s, 0);
      if (s === "thinking-cloud") {
        expect(t.hue).toBe(HUE_VIOLET);
        expect(t.upward).toBe(1);
      } else {
        expect(t.hue).toBe(HUE_CYAN);
        expect(t.upward).toBe(0);
      }
    }
  });

  it("offline is the dimmest state, thinking-local the brightest cyan", () => {
    const intensities = ALL.map((s) => coreVisualTarget(s, 0).intensity);
    expect(Math.min(...intensities)).toBe(coreVisualTarget("offline", 0).intensity);
    expect(coreVisualTarget("thinking-local", 0).intensity).toBeGreaterThan(
      coreVisualTarget("idle", 0).intensity,
    );
  });

  it("processing spins faster than idle (accelerated spin)", () => {
    expect(coreVisualTarget("processing", 0).spin).toBeGreaterThan(
      coreVisualTarget("idle", 0).spin,
    );
  });

  it("listening intensity glows with rms, but pulse depth does NOT (orb holds still)", () => {
    const quiet = coreVisualTarget("listening", 0.0);
    const loud = coreVisualTarget("listening", 0.2);
    // Brightness still tracks the voice (a glow, not movement).
    expect(loud.intensity).toBeGreaterThan(quiet.intensity);
    // Size/pulse is audio-INDEPENDENT now — the orb must not move with audio;
    // the audio energy goes to the particle shell (uFlow) instead.
    expect(loud.pulseDepth).toBe(quiet.pulseDepth);
    expect(loud.pulseDepth).toBeLessThanOrEqual(0.06);
  });
});

describe("coreVisualTarget agent-hue override (CONTRACT part C.3)", () => {
  it("a null/undefined agent hue leaves the default behavior unchanged", () => {
    for (const s of ALL) {
      const base = coreVisualTarget(s, 0.05);
      expect(coreVisualTarget(s, 0.05, null)).toEqual(base);
      expect(coreVisualTarget(s, 0.05, undefined)).toEqual(base);
    }
  });

  it("overrides the hue across EVERY state when an agent is active", () => {
    const AGENT_HUE = 320; // veronica magenta
    for (const s of ALL) {
      expect(coreVisualTarget(s, 0.02, AGENT_HUE).hue).toBe(AGENT_HUE);
    }
  });

  it("overrides even thinking-cloud's violet but keeps the upward stream", () => {
    const t = coreVisualTarget("thinking-cloud", 0, 120); // gecko green
    expect(t.hue).toBe(120);
    expect(t.upward).toBe(1); // cloud-routing motion is preserved under the agent color
  });

  it("only the hue changes — intensity/spin/pulse still track the state", () => {
    const base = coreVisualTarget("thinking-local", 0.1);
    const tinted = coreVisualTarget("thinking-local", 0.1, 50);
    expect(tinted.hue).toBe(50);
    expect(tinted.intensity).toBe(base.intensity);
    expect(tinted.spin).toBe(base.spin);
    expect(tinted.pulseHz).toBe(base.pulseHz);
    expect(tinted.converge).toBe(base.converge);
  });

  it("normalizes the override hue into [0,360) and tolerates non-finite", () => {
    expect(coreVisualTarget("idle", 0, 375).hue).toBe(15);
    expect(coreVisualTarget("idle", 0, -40).hue).toBe(320);
    expect(coreVisualTarget("idle", 0, Number.NaN).hue).toBe(HUE_CYAN); // falls back to base
  });

  it("returns finite targets under any agent hue", () => {
    for (const s of ALL) {
      const t = coreVisualTarget(s, 0.03, 265);
      for (const v of Object.values(t)) expect(Number.isFinite(v)).toBe(true);
    }
  });
});

describe("syntheticSpeechEnvelope", () => {
  it("stays within [0,1] and actually moves", () => {
    let min = 1;
    let max = 0;
    for (let t = 0; t < 10; t += 0.016) {
      const v = syntheticSpeechEnvelope(t);
      expect(v).toBeGreaterThanOrEqual(0);
      expect(v).toBeLessThanOrEqual(1);
      min = Math.min(min, v);
      max = Math.max(max, v);
    }
    expect(max - min).toBeGreaterThan(0.3); // visibly pulsing
  });

  it("is deterministic in t", () => {
    expect(syntheticSpeechEnvelope(1.234)).toBe(syntheticSpeechEnvelope(1.234));
  });
});

describe("damping helpers", () => {
  it("damp approaches the target monotonically", () => {
    let v = 0;
    let prev = 0;
    for (let i = 0; i < 60; i++) {
      v = damp(v, 1, 6, 1 / 60);
      expect(v).toBeGreaterThan(prev);
      prev = v;
    }
    expect(v).toBeGreaterThan(0.9);
    expect(v).toBeLessThanOrEqual(1);
  });

  it("damp with huge lambda snaps to the target", () => {
    expect(damp(0, 1, 1000, 1)).toBeCloseTo(1, 6);
  });

  it("dampHue moves cyan toward violet without wrapping through red", () => {
    let h = HUE_CYAN;
    for (let i = 0; i < 240; i++) h = dampHue(h, HUE_VIOLET, 6, 1 / 60);
    expect(h).toBeGreaterThan(HUE_CYAN);
    expect(h).toBeLessThanOrEqual(HUE_VIOLET + 0.5);
    expect(h).toBeCloseTo(HUE_VIOLET, 0);
  });

  it("dampHue takes the short path across 0/360", () => {
    const h = dampHue(350, 10, 1000, 1); // effectively snaps to the target
    expect(h).toBeCloseTo(10, 1); // crossed 0/360 instead of sweeping through 180
  });
});

describe("waveSilentTarget (waveform shimmer/data crossfade)", () => {
  it("enters silent mode only below the ENTER threshold", () => {
    expect(waveSilentTarget(0, WAVE_SILENT_ENTER - 0.0001)).toBe(1);
    expect(waveSilentTarget(0, WAVE_SILENT_ENTER)).toBe(0); // at the edge: hold
  });

  it("leaves silent mode only above the EXIT threshold", () => {
    expect(waveSilentTarget(1, WAVE_SILENT_EXIT + 0.0001)).toBe(0);
    expect(waveSilentTarget(1, WAVE_SILENT_EXIT)).toBe(1); // at the edge: hold
  });

  it("holds the previous target inside the hysteresis band (no oscillation)", () => {
    const mid = (WAVE_SILENT_ENTER + WAVE_SILENT_EXIT) / 2;
    expect(waveSilentTarget(1, mid)).toBe(1);
    expect(waveSilentTarget(0, mid)).toBe(0);
  });

  it("a level drifting across one boundary cannot flip-flop the target", () => {
    // The original bug: a single 0.002 threshold swapped the ENTIRE bar
    // field between data and shimmer per frame. Drift around the old value
    // now lands inside the band and holds.
    let target = 0; // currently showing live data
    for (const mean of [0.0021, 0.0019, 0.0021, 0.0019, 0.0022, 0.0018]) {
      target = waveSilentTarget(target, mean);
      expect(target).toBe(0);
    }
  });

  it("the band is sane: ENTER < EXIT", () => {
    expect(WAVE_SILENT_ENTER).toBeLessThan(WAVE_SILENT_EXIT);
  });
});

describe("ampFollow — the anti-strobe envelope", () => {
  const dt = 1 / 60;

  it("attacks faster than it releases (asymmetric, calm decay)", () => {
    expect(AMP_ATTACK).toBeGreaterThan(AMP_RELEASE);
    const up = ampFollow(0, 1, dt); // rising
    const down = 1 - ampFollow(1, 0, dt); // falling, distance moved
    expect(up).toBeGreaterThan(down); // attack covers more ground per frame
  });

  it("never overshoots and stays bounded in [0,1]", () => {
    let v = 0;
    for (let i = 0; i < 600; i++) v = ampFollow(v, 1, dt);
    expect(v).toBeLessThanOrEqual(1);
    expect(v).toBeGreaterThan(0.99);
    for (let i = 0; i < 600; i++) v = ampFollow(v, 0, dt);
    expect(v).toBeGreaterThanOrEqual(0);
    expect(v).toBeLessThan(0.01);
  });

  it("smooths a spiky 15Hz rms train — output never jumps like the input", () => {
    // Alternating loud/quiet frames (the real epileptic input). The follower
    // must move only a fraction of the gap each frame, so no single step
    // approaches the raw spike amplitude.
    let v = 0;
    let maxStep = 0;
    const spikes = [0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0];
    for (const target of spikes) {
      const prev = v;
      v = ampFollow(v, target, dt);
      maxStep = Math.max(maxStep, Math.abs(v - prev));
    }
    // A raw mapping would jump ~1.0 per frame; the follower must stay small.
    expect(maxStep).toBeLessThan(0.12);
  });
});
