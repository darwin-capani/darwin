import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import LatencyStrip from "../components/LatencyStrip";
import {
  queueWaitNote,
  QUEUE_NOTABLE_MIN_MS,
  type TelemetryEnvelope,
} from "../core/events";
import { initialState, reduce, type HudState } from "../core/state";

/**
 * DEAD FIELD: `queue_ms` on pipeline.completed.
 *
 * The daemon measures VAD-finish -> event-loop pickup and emits it on every
 * completed turn. It was added by an audit fix precisely so a wait behind an
 * in-flight turn would be VISIBLE ("the clock used to start at dequeue, hiding
 * it", main.rs) — and the HUD reducer then dropped it, so the wait stayed
 * hidden. A turn that felt slow showed a healthy TOTAL and nothing else.
 *
 * ARITHMETIC (the reason it is not a bar segment): the daemon's pipeline clock
 * starts at PICKUP, so queue_ms is outside total_ms and outside every stacked
 * segment. End-to-end wall time from VAD finish is queue + total.
 */

const envelope = (data: Record<string, unknown>): TelemetryEnvelope => ({
  ts: "2026-08-10T00:00:00Z",
  source: "system",
  event: "pipeline.completed",
  data,
});

const apply = (s: HudState, data: Record<string, unknown>): HudState =>
  reduce(s, { type: "telemetry", envelope: envelope(data), at: 1_000 });

const FRAME = {
  queue_ms: 4_200,
  stt_ms: 640,
  classify_ms: 210,
  route_ms: 980,
  first_audio_ms: 310,
  speak_ms: 2_150,
  total_ms: 3_980,
};

describe("queueWaitNote (pure)", () => {
  it("adds queue to total for the honest end-to-end wall time", () => {
    // 4200 queued + 3980 pipeline = 8180 ms from VAD finish. TOTAL alone (3980)
    // is less than half of what the person actually waited.
    expect(queueWaitNote(4_200, 3_980).wallMs).toBe(8_180);
  });

  it("calls a dominant wait notable", () => {
    const n = queueWaitNote(4_200, 3_980);
    expect(n.notable).toBe(true); // 4200/8180 = 51%
    expect(n.text).toContain("4200 ms QUEUED");
  });

  it("does not call a small absolute wait notable, however big its share", () => {
    // 200 ms of 400 ms is 50% — but 200 ms is below the absolute floor, so it is
    // a big fraction of nothing.
    expect(QUEUE_NOTABLE_MIN_MS).toBe(250);
    expect(queueWaitNote(200, 200).notable).toBe(false);
  });

  it("does not call a small SHARE notable, however big its absolute value", () => {
    // 300 ms behind a 60 s turn: real, but not what made the turn slow.
    expect(queueWaitNote(300, 60_000).notable).toBe(false);
  });

  it("no wait at all is still reported honestly as 0", () => {
    const n = queueWaitNote(0, 3_980);
    expect(n.notable).toBe(false);
    expect(n.text).toBe("0 ms queued");
    expect(n.wallMs).toBe(3_980);
  });

  it("a malformed value degrades to 0 rather than NaN", () => {
    expect(queueWaitNote(NaN, 100).wallMs).toBe(100);
    expect(queueWaitNote(-5, 100).text).toBe("0 ms queued");
  });
});

describe("queue_ms reaches state and pixels", () => {
  it("the reducer keeps queue_ms", () => {
    const s = apply(initialState(), FRAME);
    expect(s.lastTimings?.queueMs).toBe(4_200);
  });

  it("an older daemon that omits it degrades to 0, never NaN", () => {
    const { queue_ms: _drop, ...noQueue } = FRAME;
    const s = apply(initialState(), noQueue);
    expect(s.lastTimings?.queueMs).toBe(0);
  });

  it("LatencyStrip RENDERS the queue wait and the end-to-end wall time", () => {
    const s = apply(initialState(), FRAME);
    const html = renderToStaticMarkup(
      createElement(LatencyStrip, { timings: s.lastTimings }),
    );
    expect(html).toContain("QUEUE");
    expect(html).toContain("4200 ms QUEUED");
    expect(html).toContain("8180 ms"); // queue + total, stated for the reader
  });

  it("LatencyStrip does NOT fold the queue into the bar or the TOTAL", () => {
    const s = apply(initialState(), FRAME);
    const html = renderToStaticMarkup(
      createElement(LatencyStrip, { timings: s.lastTimings }),
    );
    // TOTAL is still the daemon's pickup-relative total, unchanged by the queue.
    expect(html).toContain("3980 ms");
    // The stacked track still has exactly the four pickup-relative segments.
    expect(html.match(/class="seg /g) ?? []).toHaveLength(4);
    expect(html).not.toContain("seg queue");
  });

  it("a turn with no wait still shows the field (0 is a measurement)", () => {
    const s = apply(initialState(), { ...FRAME, queue_ms: 0 });
    const html = renderToStaticMarkup(
      createElement(LatencyStrip, { timings: s.lastTimings }),
    );
    expect(html).toContain("0 ms queued");
  });
});
