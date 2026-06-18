import { describe, expect, it } from "vitest";
import { RMS_HISTORY, createAudioStore } from "../core/audioStore";

describe("audioStore", () => {
  it("starts zeroed and silent", () => {
    const s = createAudioStore();
    expect(s.lastRms).toBe(0);
    expect(s.micMuted).toBe(false);
    expect(s.mean()).toBe(0);
    for (let i = 0; i < RMS_HISTORY; i++) expect(s.at(i)).toBe(0);
  });

  it("keeps exactly RMS_HISTORY samples, oldest first", () => {
    const s = createAudioStore();
    for (let i = 0; i < RMS_HISTORY + 10; i++) {
      s.push(i / 10000, false);
    }
    expect(s.at(RMS_HISTORY - 1)).toBeCloseTo((RMS_HISTORY + 9) / 10000);
    expect(s.at(0)).toBeCloseTo(10 / 10000);
    expect(s.lastRms).toBeCloseTo((RMS_HISTORY + 9) / 10000);
  });

  it("mean tracks the window (running sum stays consistent across wrap)", () => {
    const s = createAudioStore();
    for (let i = 0; i < RMS_HISTORY * 3; i++) s.push(0.5, false);
    expect(s.mean()).toBeCloseTo(0.5, 5);
    // brute-force cross-check against at()
    let sum = 0;
    for (let i = 0; i < RMS_HISTORY; i++) sum += s.at(i);
    expect(s.mean()).toBeCloseTo(sum / RMS_HISTORY, 9);
  });

  it("tracks micMuted from the speaking flag", () => {
    const s = createAudioStore();
    s.push(0.1, true);
    expect(s.micMuted).toBe(true);
    s.push(0.1, false);
    expect(s.micMuted).toBe(false);
  });

  it("ignores non-finite and negative rms", () => {
    const s = createAudioStore();
    const v0 = s.version;
    s.push(NaN, false);
    s.push(-1, false);
    s.push(Infinity, false);
    expect(s.version).toBe(v0);
    expect(s.mean()).toBe(0);
  });

  it("bumps version on every accepted write (cheap change detection)", () => {
    const s = createAudioStore();
    const v0 = s.version;
    s.push(0.1, false);
    s.push(0.2, false);
    expect(s.version).toBe(v0 + 2);
  });

  it("reset zeroes the window and mute state", () => {
    const s = createAudioStore();
    for (let i = 0; i < 20; i++) s.push(0.3, true);
    s.reset();
    expect(s.lastRms).toBe(0);
    expect(s.micMuted).toBe(false);
    expect(s.mean()).toBe(0);
    expect(s.at(RMS_HISTORY - 1)).toBe(0);
  });
});
