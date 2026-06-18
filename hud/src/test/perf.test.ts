import { describe, expect, it } from "vitest";
import {
  COOLDOWN_MS,
  PERF_TIERS,
  PerfGovernor,
  SUSTAIN_FRAMES,
} from "../core/perf";

describe("PerfGovernor", () => {
  it("starts at the full-scene tier", () => {
    const g = new PerfGovernor();
    expect(g.tier).toBe(0);
    expect(g.current()).toEqual(PERF_TIERS[0]);
  });

  it("holds tier 0 under sustained 60fps", () => {
    const g = new PerfGovernor();
    for (let i = 0; i < 1000; i++) g.sample(16.7);
    expect(g.tier).toBe(0);
  });

  it("drops a tier under sustained over-budget frames", () => {
    const g = new PerfGovernor();
    for (let i = 0; i < SUSTAIN_FRAMES + 60; i++) g.sample(30);
    expect(g.tier).toBe(1);
  });

  it("brief spikes do not drop a tier", () => {
    const g = new PerfGovernor();
    for (let cycle = 0; cycle < 50; cycle++) {
      for (let i = 0; i < 5; i++) g.sample(40); // 5-frame hitch
      for (let i = 0; i < 120; i++) g.sample(14); // 2s recovery
    }
    expect(g.tier).toBe(0);
  });

  it("keeps degrading down the ladder but never past the last tier", () => {
    const g = new PerfGovernor();
    // 50ms frames: plenty of wall-clock to burn through every cooldown.
    const frames =
      (SUSTAIN_FRAMES + Math.ceil(COOLDOWN_MS / 50) + 100) * PERF_TIERS.length * 2;
    for (let i = 0; i < frames; i++) {
      g.sample(50);
    }
    expect(g.tier).toBe(PERF_TIERS.length - 1);
    expect(g.current().bloom).toBe(false);
  });

  it("cooldown is WALL-CLOCK: consecutive drops are >= COOLDOWN_MS apart", () => {
    // Frame-counted cooldowns shrink in real terms exactly when frames are
    // slow — the visible back-to-back tier cuts. Verify the gap holds in ms
    // at a degraded ~33fps (30ms frames).
    const g = new PerfGovernor();
    let elapsedMs = 0;
    while (g.tier === 0) {
      g.sample(30);
      elapsedMs += 30;
      expect(elapsedMs).toBeLessThan(300_000);
    }
    const msAtFirstDrop = elapsedMs;
    while (g.tier === 1) {
      g.sample(30);
      elapsedMs += 30;
      if (elapsedMs > 600_000) break;
    }
    expect(elapsedMs - msAtFirstDrop).toBeGreaterThanOrEqual(COOLDOWN_MS);
  });

  it("cooldown holds even longer in frame terms at very slow frame rates", () => {
    // At 100ms frames (10fps) the cooldown still spans COOLDOWN_MS of wall
    // clock — i.e. at least COOLDOWN_MS/100 samples with no drop.
    const g = new PerfGovernor();
    while (g.tier === 0) g.sample(100);
    let samples = 0;
    while (g.tier === 1 && samples < 10_000) {
      g.sample(100);
      samples += 1;
    }
    expect(samples * 100).toBeGreaterThanOrEqual(COOLDOWN_MS);
  });

  it("ignores nonsense frame times", () => {
    const g = new PerfGovernor();
    g.sample(NaN);
    g.sample(-5);
    g.sample(Infinity);
    expect(g.tier).toBe(0);
  });

  it("tier table sheds load monotonically", () => {
    for (let i = 1; i < PERF_TIERS.length; i++) {
      const prev = PERF_TIERS[i - 1];
      const cur = PERF_TIERS[i];
      const prevLoad = prev.particles + (prev.bloom ? 1 : 0);
      const curLoad = cur.particles + (cur.bloom ? 1 : 0);
      expect(curLoad).toBeLessThan(prevLoad);
    }
  });

  it("tier 0 has the maximum particle count (single-buffer drawRange invariant)", () => {
    // CoreScene allocates the particle buffer ONCE at PERF_TIERS[0].particles
    // and sheds via setDrawRange — every tier must fit inside that buffer.
    for (const tier of PERF_TIERS) {
      expect(tier.particles).toBeLessThanOrEqual(PERF_TIERS[0].particles);
    }
  });
});
