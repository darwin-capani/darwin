/**
 * Adaptive performance governor (contract: drop particle/bloom tiers when
 * frame time stays above 20ms). Pure — unit-tested headlessly.
 *
 * Tier changes must never read as a visual cut: the render layer keeps the
 * EffectComposer permanently mounted and LERPS bloom intensity to the tier
 * target, and sheds particles via geometry.setDrawRange on a once-allocated
 * buffer (no regenerated positions). The governor's job is only to decide
 * WHEN, with a wall-clock cooldown so consecutive drops can't land
 * back-to-back even at degraded frame rates (frame-counted cooldowns shrink
 * in real terms exactly when the scene is struggling).
 */

export interface PerfTier {
  particles: number;
  bloom: boolean;
}

/** Tier 0 is the full scene; higher tiers shed load. */
export const PERF_TIERS: readonly PerfTier[] = [
  { particles: 6000, bloom: true },
  { particles: 3000, bloom: true },
  { particles: 3000, bloom: false },
  { particles: 1500, bloom: false },
];

export const FRAME_BUDGET_MS = 20;
/** Consecutive over-budget frames (EMA) before a tier drop: ~1.5s @60fps. */
export const SUSTAIN_FRAMES = 90;
/** Wall-clock pause after a drop so the new tier can settle and the
 *  crossfade can finish — independent of the (degraded) frame rate. */
export const COOLDOWN_MS = 5000;

export class PerfGovernor {
  private ema = 1000 / 60;
  private overCount = 0;
  private cooldownMsLeft = 0;
  tier = 0;

  /**
   * Feed one frame time (ms). Returns the (possibly newly dropped) tier
   * index into PERF_TIERS.
   */
  sample(frameMs: number): number {
    if (!Number.isFinite(frameMs) || frameMs <= 0) return this.tier;
    this.ema = this.ema * 0.9 + frameMs * 0.1;

    if (this.cooldownMsLeft > 0) {
      this.cooldownMsLeft = Math.max(0, this.cooldownMsLeft - frameMs);
      return this.tier;
    }
    if (this.ema > FRAME_BUDGET_MS) {
      this.overCount += 1;
      if (this.overCount >= SUSTAIN_FRAMES && this.tier < PERF_TIERS.length - 1) {
        this.tier += 1;
        this.overCount = 0;
        this.cooldownMsLeft = COOLDOWN_MS;
      }
    } else if (this.overCount > 0) {
      this.overCount -= 1; // brief spikes decay; only sustained load drops a tier
    }
    return this.tier;
  }

  current(): PerfTier {
    return PERF_TIERS[this.tier];
  }
}
