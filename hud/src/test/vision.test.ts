import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import {
  AUDIO_SOUND_MONITOR_EVENT,
  VISION_DESCRIBE_EVENT,
  VISION_TOPIC_DETECTIONS,
  VISION_TOPIC_ERROR,
  VISION_TOPIC_MOTION,
  VISION_TOPIC_PERF,
  VISION_TOPIC_SCREEN,
  VISION_TOPIC_SOUND,
  VISION_TOPIC_STATUS,
  parseAudioSoundMonitor,
  parseVisionDescribe,
  parseVisionDetections,
  parseVisionError,
  parseVisionMotion,
  parseVisionPerf,
  parseVisionScreen,
  parseVisionSound,
  parseVisionStatus,
  screenContextInitial,
  applyScreenContextWatching,
  applyScreenContextConfigured,
  applyScreenContextCommand,
  SCREEN_CONTEXT_WATCHING_EVENT,
  SCREEN_CONTEXT_CONFIGURED_EVENT,
  SCREEN_CONTEXT_COMMAND_EVENT,
  type AudioSoundMonitor,
  type ScreenContext,
  type TelemetryEnvelope,
  type VisionDescribe,
} from "../core/events";
import VisionPanel from "../components/VisionPanel";
import { initialState, reduce, type HudState } from "../core/state";

/* ------------------------------------------------------------------------ *
 * Vision micro-app payload parsers (events.ts). DEFENSIVE-ONLY, ON-DEVICE    *
 * ONLY: the camera/screen capture + Apple Vision/Core ML inference run        *
 * on-device behind a macOS TCC consent gate and are NOT exercised here; these *
 * cover only the telemetry the HUD-side panel renders. Counts/boxes/labels/   *
 * timing ONLY — never pixels, never identity. A malformed/partial payload     *
 * must yield null (or drop the offending sub-item), never throw.              *
 * ------------------------------------------------------------------------ */

describe("parseVisionDetections (vision.detections — DEFAULT topic)", () => {
  it("parses a full per-frame detection summary", () => {
    const d = parseVisionDetections({
      topic: "vision.detections",
      frame: 12,
      ts: 1718200000.5,
      source: "camera",
      count: 2,
      by_kind: { human: 1, animal: 1 },
      detections: [
        {
          kind: "human",
          box: { x: 0.1, y: 0.2, w: 0.3, h: 0.6 },
          confidence: 0.91,
          label: "human",
        },
        {
          kind: "animal",
          box: { x: 0.5, y: 0.1, w: 0.2, h: 0.2 },
          confidence: 0.77,
          label: "cat",
        },
      ],
    });
    expect(d).toEqual({
      frame: 12,
      ts: 1718200000.5,
      source: "camera",
      count: 2,
      byKind: { human: 1, animal: 1 },
      detections: [
        { kind: "human", box: { x: 0.1, y: 0.2, w: 0.3, h: 0.6 }, confidence: 0.91, label: "human" },
        { kind: "animal", box: { x: 0.5, y: 0.1, w: 0.2, h: 0.2 }, confidence: 0.77, label: "cat" },
      ],
    });
  });

  it("returns null when `frame` is missing or non-finite (the structural anchor)", () => {
    expect(parseVisionDetections({ ts: 1, source: "camera", count: 0 })).toBeNull();
    expect(parseVisionDetections({ frame: "x" })).toBeNull();
    expect(parseVisionDetections({ frame: NaN })).toBeNull();
    expect(parseVisionDetections({})).toBeNull();
  });

  it("handles an empty frame (no detections, count 0) and defaults missing fields", () => {
    const d = parseVisionDetections({ frame: 5 });
    expect(d).not.toBeNull();
    expect(d!.frame).toBe(5);
    expect(d!.ts).toBe(0);
    expect(d!.source).toBe("");
    expect(d!.count).toBe(0); // defaults to detections.length when absent
    expect(d!.byKind).toEqual({});
    expect(d!.detections).toEqual([]);
  });

  it("defaults count to the detections length when the count field is absent", () => {
    const d = parseVisionDetections({
      frame: 7,
      detections: [
        { kind: "object", label: "keyboard" },
        { kind: "object", label: "mug" },
      ],
    });
    expect(d!.count).toBe(2);
  });

  it("keeps only finite numeric by_kind counts, dropping junk entries", () => {
    const d = parseVisionDetections({
      frame: 1,
      by_kind: { human: 2, animal: "lots", object: NaN, salientRegion: 3 },
    });
    expect(d!.byKind).toEqual({ human: 2, salientRegion: 3 });
  });

  it("drops malformed detection entries; defaults box/confidence/label on partials", () => {
    const d = parseVisionDetections({
      frame: 9,
      detections: [
        { label: "no kind" }, // missing kind -> dropped
        "junk", // not an object -> dropped
        { kind: "object" }, // partial -> kept with defaults
        { kind: "motion", box: { x: 0.4 }, confidence: 0.5, label: "" }, // partial box
      ],
    });
    expect(d!.detections).toHaveLength(2);
    expect(d!.detections[0]).toEqual({
      kind: "object",
      box: { x: 0, y: 0, w: 0, h: 0 },
      confidence: 0,
      label: "",
    });
    // a partial box defaults its missing components to 0 rather than dropping
    expect(d!.detections[1]).toEqual({
      kind: "motion",
      box: { x: 0.4, y: 0, w: 0, h: 0 },
      confidence: 0.5,
      label: "",
    });
  });

  it("keeps an unknown future detection kind opaque rather than dropping it", () => {
    const d = parseVisionDetections({
      frame: 1,
      detections: [{ kind: "barcode", confidence: 0.6, label: "QR" }],
    });
    expect(d!.detections[0].kind).toBe("barcode");
  });
});

describe("parseVisionStatus (vision.status)", () => {
  it("parses a full watch-lifecycle / capability snapshot", () => {
    const s = parseVisionStatus({
      state: "watching",
      source: "camera",
      sensitivity: 0.7,
      camera_authorized: true,
      screen_authorized: false,
      message: "watching front camera",
    });
    expect(s).toEqual({
      state: "watching",
      source: "camera",
      sensitivity: 0.7,
      cameraAuthorized: true,
      screenAuthorized: false,
      message: "watching front camera",
    });
  });

  it("returns null when `state` is missing or empty (the lifecycle anchor)", () => {
    expect(parseVisionStatus({ source: "camera" })).toBeNull();
    expect(parseVisionStatus({ state: "" })).toBeNull();
    expect(parseVisionStatus({ state: 3 })).toBeNull();
    expect(parseVisionStatus({})).toBeNull();
  });

  it("leaves omitted TCC flags as null — HONEST 'unknown', never a fake grant", () => {
    const s = parseVisionStatus({ state: "idle" });
    expect(s).not.toBeNull();
    expect(s!.cameraAuthorized).toBeNull();
    expect(s!.screenAuthorized).toBeNull();
    expect(s!.source).toBeNull();
    expect(s!.sensitivity).toBeNull();
    expect(s!.message).toBeNull();
  });

  it("keeps an unknown future lifecycle state opaque rather than dropping it", () => {
    expect(parseVisionStatus({ state: "calibrating" })!.state).toBe("calibrating");
  });
});

describe("parseVisionMotion (vision.motion)", () => {
  it("parses a motion event (generic magnitude + region only)", () => {
    const m = parseVisionMotion({
      frame: 30,
      ts: 1718200001,
      source: "screen",
      magnitude: 0.42,
      region: { x: 0.2, y: 0.2, w: 0.4, h: 0.5 },
    });
    expect(m).toEqual({
      frame: 30,
      ts: 1718200001,
      source: "screen",
      magnitude: 0.42,
      region: { x: 0.2, y: 0.2, w: 0.4, h: 0.5 },
    });
  });

  it("returns null when `frame` is missing", () => {
    expect(parseVisionMotion({ magnitude: 0.5 })).toBeNull();
    expect(parseVisionMotion({})).toBeNull();
  });

  it("defaults the numeric/region fields on a partial event", () => {
    const m = parseVisionMotion({ frame: 4 });
    expect(m).toEqual({
      frame: 4,
      ts: 0,
      source: "",
      magnitude: 0,
      region: { x: 0, y: 0, w: 0, h: 0 },
    });
  });
});

describe("parseVisionPerf (vision.perf)", () => {
  it("parses a full inference-timing snapshot", () => {
    const p = parseVisionPerf({
      p50_ms: 6.2,
      p95_ms: 11.4,
      fps: 28.5,
      frames: 1402,
      compute_unit: "ane",
    });
    expect(p).toEqual({ p50Ms: 6.2, p95Ms: 11.4, fps: 28.5, frames: 1402, computeUnit: "ane" });
  });

  it("returns null unless all four numeric stats are finite", () => {
    expect(parseVisionPerf({ p50_ms: 6, p95_ms: 11, fps: 28 })).toBeNull();
    expect(parseVisionPerf({ p50_ms: 6, p95_ms: 11, fps: 28, frames: "x" })).toBeNull();
    expect(parseVisionPerf({ p50_ms: 6, p95_ms: 11, fps: Infinity, frames: 10 })).toBeNull();
    expect(parseVisionPerf({})).toBeNull();
  });

  it("defaults compute_unit to '' when absent", () => {
    const p = parseVisionPerf({ p50_ms: 5, p95_ms: 9, fps: 30, frames: 100 });
    expect(p!.computeUnit).toBe("");
  });
});

describe("parseVisionError (vision.error)", () => {
  it("parses a recoverable error (e.g. tcc_denied — the honest TCC gate state)", () => {
    const e = parseVisionError({
      code: "tcc_denied",
      message: "Camera access not granted",
      source: "camera",
    });
    expect(e).toEqual({
      code: "tcc_denied",
      message: "Camera access not granted",
      source: "camera",
    });
  });

  it("returns null when `code` is missing or empty (the machine-code anchor)", () => {
    expect(parseVisionError({ message: "boom" })).toBeNull();
    expect(parseVisionError({ code: "" })).toBeNull();
    expect(parseVisionError({})).toBeNull();
  });

  it("defaults message to '' and leaves source null when omitted", () => {
    const e = parseVisionError({ code: "bad_op" });
    expect(e).toEqual({ code: "bad_op", message: "", source: null });
  });
});

describe("parseVisionScreen (vision.screen — OCR screen read, READ ON REQUEST)", () => {
  it("parses a full screen-read readout: text, blocks, control candidates", () => {
    const s = parseVisionScreen({
      frame: 7,
      ts: 12.5,
      source: "screen",
      block_count: 2,
      text: "Account Settings\nSign Out",
      blocks: [
        { text: "Account Settings", box: { x: 0.1, y: 0.9, w: 0.4, h: 0.04 }, center: { x: 0.3, y: 0.92 }, confidence: 0.97, is_control: false },
        { text: "Sign Out", box: { x: 0.7, y: 0.05, w: 0.15, h: 0.04 }, center: { x: 0.77, y: 0.07 }, confidence: 0.91, is_control: true },
      ],
      controls: [
        { text: "Sign Out", box: { x: 0.7, y: 0.05, w: 0.15, h: 0.04 }, center: { x: 0.77, y: 0.07 }, confidence: 0.91, is_control: true },
      ],
    });
    expect(s).not.toBeNull();
    expect(s!.frame).toBe(7);
    expect(s!.source).toBe("screen");
    expect(s!.blockCount).toBe(2);
    expect(s!.text).toBe("Account Settings\nSign Out");
    expect(s!.blocks).toHaveLength(2);
    expect(s!.controls.map((c) => c.text)).toEqual(["Sign Out"]);
    expect(s!.controls[0].isControl).toBe(true);
    expect(s!.controls[0].center).toEqual({ x: 0.77, y: 0.07 });
    // A plain read carries no where-is query and no located hit.
    expect(s!.query).toBeNull();
    expect(s!.located).toBeNull();
  });

  it("parses a where-is locate result: query + best-matching located block (READ-ONLY, no click)", () => {
    const s = parseVisionScreen({
      frame: 8,
      source: "screen",
      text: "Submit",
      query: "submit",
      blocks: [{ text: "Submit", box: { x: 0.4, y: 0.1, w: 0.1, h: 0.04 }, center: { x: 0.45, y: 0.12 }, confidence: 0.93, is_control: true }],
      controls: [],
      located: { text: "Submit", box: { x: 0.4, y: 0.1, w: 0.1, h: 0.04 }, center: { x: 0.45, y: 0.12 }, confidence: 0.93, is_control: true, score: 0.88 },
    });
    expect(s!.query).toBe("submit");
    expect(s!.located).not.toBeNull();
    expect(s!.located!.text).toBe("Submit");
    expect(s!.located!.center).toEqual({ x: 0.45, y: 0.12 });
    expect(s!.located!.score).toBe(0.88);
  });

  it("returns null when `frame` is missing or non-finite (the structural anchor)", () => {
    expect(parseVisionScreen({ text: "hi", blocks: [] })).toBeNull();
    expect(parseVisionScreen({ frame: "x" })).toBeNull();
    expect(parseVisionScreen({ frame: NaN })).toBeNull();
    expect(parseVisionScreen({})).toBeNull();
  });

  it("defaults missing fields and an empty read (no text, no blocks)", () => {
    const s = parseVisionScreen({ frame: 3 });
    expect(s).toEqual({
      frame: 3,
      ts: 0,
      source: "",
      blockCount: 0,
      text: "",
      blocks: [],
      controls: [],
      query: null,
      located: null,
      // Older payloads omit the #28/#29 fields -> defaults: a plain screen read,
      // no text, length 0, no document-detected bool (N/A for a screen read).
      readKind: "screen",
      textPresent: false,
      textLength: 0,
      documentDetected: null,
    });
  });

  it("parses the #28 handwriting read: read_kind=handwriting, presence/length, NO document bool", () => {
    const s = parseVisionScreen({
      frame: 9,
      source: "camera",
      read_kind: "handwriting",
      text: "Buy milk",
      text_present: true,
      text_length: 8,
      blocks: [{ text: "Buy milk" }],
    });
    expect(s!.readKind).toBe("handwriting");
    expect(s!.textPresent).toBe(true);
    expect(s!.textLength).toBe(8);
    // document detection is N/A for a handwriting read.
    expect(s!.documentDetected).toBeNull();
  });

  it("parses the #29 document scan: read_kind=document carries the HONEST document_detected bool", () => {
    const found = parseVisionScreen({
      frame: 10,
      source: "camera",
      read_kind: "document",
      document_detected: true,
      text: "INVOICE",
      text_present: true,
      text_length: 7,
      blocks: [{ text: "INVOICE" }],
    });
    expect(found!.readKind).toBe("document");
    expect(found!.documentDetected).toBe(true);
    expect(found!.textPresent).toBe(true);
    expect(found!.textLength).toBe(7);

    // No document found -> the honest empty (document_detected=false, no text).
    const none = parseVisionScreen({
      frame: 11,
      source: "camera",
      read_kind: "document",
      document_detected: false,
      text: "",
      text_present: false,
      text_length: 0,
      block_count: 0,
    });
    expect(none!.readKind).toBe("document");
    expect(none!.documentDetected).toBe(false);
    expect(none!.textPresent).toBe(false);
    expect(none!.textLength).toBe(0);
  });

  it("parses the #42 continuous screen-context snapshot: read_kind=context, NO document bool", () => {
    // Normally a context snapshot is routed into the daemon's transient ring and
    // NOT relayed; if one ever surfaces it must be labeled honestly (not mislabeled
    // as a one-shot read), so the parser honors the "context" kind.
    const s = parseVisionScreen({
      frame: 42,
      source: "screen",
      read_kind: "context",
      text: "editor: writing the report",
      text_present: true,
      text_length: 26,
      blocks: [{ text: "editor: writing the report" }],
    });
    expect(s!.readKind).toBe("context");
    expect(s!.documentDetected).toBeNull();
    expect(s!.textPresent).toBe(true);
  });

  it("an unknown read_kind falls back to screen, and document_detected is ignored off a non-document read", () => {
    const s = parseVisionScreen({
      frame: 12,
      read_kind: "bogus",
      document_detected: true, // must be ignored when the read is not a document
      text: "x",
    });
    expect(s!.readKind).toBe("screen");
    expect(s!.documentDetected).toBeNull();
  });

  it("derives text_present/text_length from text when the wire omits them (older payloads)", () => {
    const s = parseVisionScreen({ frame: 13, text: "hello" });
    expect(s!.textPresent).toBe(true);
    expect(s!.textLength).toBe(5);
  });

  it("drops blocks with no usable text glyph string; defaults box/center/confidence on partials", () => {
    const s = parseVisionScreen({
      frame: 4,
      blocks: [
        { box: { x: 0.1, y: 0.1, w: 0.2, h: 0.2 } }, // no text -> dropped
        { text: "OK" }, // partial -> defaults box/center/confidence/is_control
        "junk", // not an object -> dropped
      ],
    });
    expect(s!.blocks).toHaveLength(1);
    expect(s!.blocks[0]).toEqual({
      text: "OK",
      box: { x: 0, y: 0, w: 0, h: 0 },
      center: { x: 0, y: 0 },
      confidence: 0,
      isControl: false,
    });
    // block_count defaults to the kept-blocks length when the field is absent.
    expect(s!.blockCount).toBe(1);
  });

  it("leaves located null when the located block lacks usable text (no phantom hit)", () => {
    const s = parseVisionScreen({
      frame: 5,
      query: "save",
      located: { box: { x: 0.1, y: 0.1, w: 0.1, h: 0.1 }, score: 0.5 }, // no text
    });
    expect(s!.query).toBe("save");
    expect(s!.located).toBeNull();
  });
});

/* ------------------------------------------------------------------------ *
 * Reducer: app.data stashes each vision topic under feed.topics, keyed by the *
 * relay topic — additive, and must not disturb other app surfaces. The panel  *
 * reads + narrows its own slice with the parsers above (mirrors silicon-      *
 * canvas). Vision never carries pixels/identity onto the wire.                *
 * ------------------------------------------------------------------------ */

const V = "vision";

function env(event: string, data: Record<string, unknown>): TelemetryEnvelope {
  return { ts: "2026-06-13T12:00:00.000Z", source: "system", event, data };
}

function tel(state: HudState, e: TelemetryEnvelope, at = 1000): HudState {
  return reduce(state, { type: "telemetry", envelope: e, at });
}

function connected(): HudState {
  return reduce(initialState(), { type: "ws.connected", at: 0 });
}

describe("reducer: app.data vision topic storage", () => {
  it("stores each vision topic payload verbatim under feed.topics[topic]", () => {
    let s = connected();
    s = tel(
      s,
      env("app.data", {
        name: V,
        topic: VISION_TOPIC_DETECTIONS,
        payload: {
          frame: 1,
          ts: 10,
          source: "camera",
          count: 1,
          by_kind: { human: 1 },
          detections: [{ kind: "human", box: { x: 0, y: 0, w: 0.2, h: 0.5 }, confidence: 0.9, label: "human" }],
        },
      }),
    );
    s = tel(
      s,
      env("app.data", {
        name: V,
        topic: VISION_TOPIC_STATUS,
        payload: { state: "watching", source: "camera", sensitivity: 0.5, camera_authorized: true },
      }),
    );
    s = tel(
      s,
      env("app.data", {
        name: V,
        topic: VISION_TOPIC_MOTION,
        payload: { frame: 2, source: "camera", magnitude: 0.3, region: { x: 0, y: 0, w: 1, h: 1 } },
      }),
    );
    s = tel(
      s,
      env("app.data", {
        name: V,
        topic: VISION_TOPIC_PERF,
        payload: { p50_ms: 5, p95_ms: 9, fps: 30, frames: 100, compute_unit: "ane" },
      }),
    );
    s = tel(
      s,
      env("app.data", {
        name: V,
        topic: VISION_TOPIC_ERROR,
        payload: { code: "tcc_denied", message: "no consent", source: "camera" },
      }),
    );
    s = tel(
      s,
      env("app.data", {
        name: V,
        topic: VISION_TOPIC_SCREEN,
        payload: {
          frame: 7,
          ts: 12,
          source: "screen",
          block_count: 2,
          text: "Welcome\nSubmit",
          blocks: [
            { text: "Welcome", box: { x: 0.1, y: 0.8, w: 0.3, h: 0.05 }, center: { x: 0.25, y: 0.82 }, confidence: 0.95, is_control: false },
            { text: "Submit", box: { x: 0.4, y: 0.1, w: 0.12, h: 0.04 }, center: { x: 0.46, y: 0.12 }, confidence: 0.9, is_control: true },
          ],
          controls: [
            { text: "Submit", box: { x: 0.4, y: 0.1, w: 0.12, h: 0.04 }, center: { x: 0.46, y: 0.12 }, confidence: 0.9, is_control: true },
          ],
        },
      }),
    );

    const feed = s.appFeeds[V];
    expect(feed.running).toBe(true);
    expect(s.runningApps.has(V)).toBe(true);

    // Each topic slice round-trips through the matching parser.
    expect(parseVisionDetections(feed.topics[VISION_TOPIC_DETECTIONS])!.count).toBe(1);
    expect(parseVisionStatus(feed.topics[VISION_TOPIC_STATUS])!.cameraAuthorized).toBe(true);
    expect(parseVisionMotion(feed.topics[VISION_TOPIC_MOTION])!.magnitude).toBe(0.3);
    expect(parseVisionPerf(feed.topics[VISION_TOPIC_PERF])!.computeUnit).toBe("ane");
    expect(parseVisionError(feed.topics[VISION_TOPIC_ERROR])!.code).toBe("tcc_denied");
    const screen = parseVisionScreen(feed.topics[VISION_TOPIC_SCREEN])!;
    expect(screen.blockCount).toBe(2);
    expect(screen.controls.map((c) => c.text)).toEqual(["Submit"]);
  });

  it("a newer payload on the SAME topic replaces it; other topics are retained", () => {
    let s = connected();
    s = tel(
      s,
      env("app.data", {
        name: V,
        topic: VISION_TOPIC_DETECTIONS,
        payload: { frame: 1, count: 0, detections: [] },
      }),
    );
    s = tel(
      s,
      env("app.data", {
        name: V,
        topic: VISION_TOPIC_STATUS,
        payload: { state: "watching" },
      }),
    );
    // Newer detections frame on the same topic.
    s = tel(
      s,
      env("app.data", {
        name: V,
        topic: VISION_TOPIC_DETECTIONS,
        payload: { frame: 2, count: 1, detections: [{ kind: "object", label: "mug" }] },
      }),
    );

    const feed = s.appFeeds[V];
    expect(parseVisionDetections(feed.topics[VISION_TOPIC_DETECTIONS])!.frame).toBe(2);
    // The status topic stored earlier survives the detections update.
    expect(parseVisionStatus(feed.topics[VISION_TOPIC_STATUS])!.state).toBe("watching");
  });

  it("does not mutate the prior topics map in place (immutable update)", () => {
    let s = connected();
    s = tel(
      s,
      env("app.data", {
        name: V,
        topic: VISION_TOPIC_DETECTIONS,
        payload: { frame: 1, count: 0, detections: [] },
      }),
    );
    const beforeTopics = s.appFeeds[V].topics;
    s = tel(
      s,
      env("app.data", {
        name: V,
        topic: VISION_TOPIC_STATUS,
        payload: { state: "idle" },
      }),
    );
    expect(VISION_TOPIC_STATUS in beforeTopics).toBe(false);
    expect(VISION_TOPIC_STATUS in s.appFeeds[V].topics).toBe(true);
  });

  it("an app.stopped marks the vision surface offline but keeps the last telemetry", () => {
    let s = connected();
    s = tel(
      s,
      env("app.data", {
        name: V,
        topic: VISION_TOPIC_DETECTIONS,
        payload: { frame: 9, count: 2, detections: [] },
      }),
    );
    s = tel(s, env("app.stopped", { name: V }));
    const feed = s.appFeeds[V];
    expect(feed.running).toBe(false);
    expect(s.runningApps.has(V)).toBe(false);
    // Last telemetry retained so the panel can show it dimmed, not blanked.
    expect(parseVisionDetections(feed.topics[VISION_TOPIC_DETECTIONS])!.frame).toBe(9);
  });

  it("ignores a vision app.data line with no payload object (no churn)", () => {
    let s = connected();
    const before = s;
    s = tel(s, env("app.data", { name: V, topic: VISION_TOPIC_DETECTIONS }));
    // No payload -> reducer returns the same reference (existing contract).
    expect(s).toBe(before);
  });
});

/* ------------------------------------------------------------------------ *
 * parseVisionDescribe (vision.describe — the ON-DEVICE VLM describe event).    *
 * DISTINCT from vision.screen (OCR): the VLM DESCRIBES/REASONS about the scene *
 * (on-device mlx-vlm), OCR transcribes glyph TEXT. CRITICAL: this event is     *
 * METADATA ONLY — NO pixels, NO description text, NO path ever ride telemetry, *
 * so the parser has nothing visual to parse. A malformed payload yields null,  *
 * never a throw. `available` is true ONLY when the on-device VLM actually       *
 * produced a description; false on every gate/confine/unavailable/transport.   *
 * ------------------------------------------------------------------------ */

describe("parseVisionDescribe (vision.describe — on-device VLM, METADATA ONLY)", () => {
  it("parses the available (VLM described) screen outcome", () => {
    const d = parseVisionDescribe({ source: "screen", available: true, vlm: true });
    expect(d).toEqual({ source: "screen", available: true, vlm: true });
  });

  it("parses the available image outcome", () => {
    const d = parseVisionDescribe({ source: "image", available: true, vlm: true });
    expect(d).toEqual({ source: "image", available: true, vlm: true });
  });

  it("parses the honest fall-back outcome (VLM enabled but did not describe)", () => {
    const d = parseVisionDescribe({ source: "screen", available: false, vlm: true });
    expect(d).toEqual({ source: "screen", available: false, vlm: true });
  });

  it("parses the shipped-OFF outcome (model disabled, fell back)", () => {
    const d = parseVisionDescribe({ source: "image", available: false, vlm: false });
    expect(d).toEqual({ source: "image", available: false, vlm: false });
  });

  it("returns null when `source` is missing or empty (the structural anchor)", () => {
    expect(parseVisionDescribe({ available: true, vlm: true })).toBeNull();
    expect(parseVisionDescribe({ source: "" })).toBeNull();
    expect(parseVisionDescribe({ source: 3 })).toBeNull();
    expect(parseVisionDescribe({})).toBeNull();
  });

  it("defaults available/vlm to FALSE when omitted (never a fake 'it described')", () => {
    const d = parseVisionDescribe({ source: "screen" });
    expect(d).toEqual({ source: "screen", available: false, vlm: false });
  });

  it("keeps an unknown future source opaque rather than dropping it", () => {
    expect(parseVisionDescribe({ source: "webcam_still", available: true, vlm: true })!.source).toBe(
      "webcam_still",
    );
  });

  it("carries NO pixels / text / path — only the three metadata fields survive", () => {
    // A hostile payload that tries to smuggle the scene through must NOT leak it:
    // the parser surfaces ONLY {source, available, vlm}.
    const d = parseVisionDescribe({
      source: "screen",
      available: true,
      vlm: true,
      // none of these should ever be present, but prove they never round-trip:
      description: "a person at a desk with a password visible",
      text: "SECRET on screen",
      path: "/Users/me/private.png",
      pixels: [1, 2, 3],
    }) as VisionDescribe & Record<string, unknown>;
    expect(Object.keys(d).sort()).toEqual(["available", "source", "vlm"]);
    expect(d.description).toBeUndefined();
    expect(d.text).toBeUndefined();
    expect(d.path).toBeUndefined();
    expect(d.pixels).toBeUndefined();
  });
});

/* ------------------------------------------------------------------------ *
 * Reducer: vision.describe (channel "local") updates state.visionDescribe.     *
 * It is a top-level telemetry envelope (NOT an app.data topic), so it surfaces  *
 * even while the Vision app feed is offline. METADATA ONLY — nothing visual     *
 * lands in state. The OCR vision.screen app.data path is NOT regressed.         *
 * ------------------------------------------------------------------------ */

describe("reducer: vision.describe (local channel, metadata-only)", () => {
  it("starts null and stores the parsed describe outcome", () => {
    let s = connected();
    expect(s.visionDescribe).toBeNull();
    s = tel(s, { ...env(VISION_DESCRIBE_EVENT, { source: "screen", available: true, vlm: true }), source: "local" });
    expect(s.visionDescribe).toEqual({ source: "screen", available: true, vlm: true });
  });

  it("stores the honest fall-back outcome (available=false) too", () => {
    let s = connected();
    s = tel(s, { ...env(VISION_DESCRIBE_EVENT, { source: "image", available: false, vlm: false }), source: "local" });
    expect(s.visionDescribe).toEqual({ source: "image", available: false, vlm: false });
  });

  it("a newer describe replaces the prior one", () => {
    let s = connected();
    s = tel(s, { ...env(VISION_DESCRIBE_EVENT, { source: "screen", available: false, vlm: true }), source: "local" });
    s = tel(s, { ...env(VISION_DESCRIBE_EVENT, { source: "image", available: true, vlm: true }), source: "local" });
    expect(s.visionDescribe).toEqual({ source: "image", available: true, vlm: true });
  });

  it("drops a malformed describe (no source) without wiping the last posture", () => {
    let s = connected();
    s = tel(s, { ...env(VISION_DESCRIBE_EVENT, { source: "screen", available: true, vlm: true }), source: "local" });
    const before = s.visionDescribe;
    s = tel(s, { ...env(VISION_DESCRIBE_EVENT, { available: false }), source: "local" });
    expect(s.visionDescribe).toEqual(before);
  });

  it("does NOT clear the sticky inference banner (local-channel, not a proof event)", () => {
    // vision.describe rides channel "local"; an offline banner must NOT clear on
    // it (only events that prove the inference server responded clear it).
    let s = connected();
    s = tel(s, env("inference.unavailable", { op: "generate", error: "down" }));
    expect(s.inferenceOffline).toBe(true);
    s = tel(s, { ...env(VISION_DESCRIBE_EVENT, { source: "screen", available: false, vlm: true }), source: "local" });
    expect(s.inferenceOffline).toBe(true);
  });

  it("does not touch the OCR vision.screen app.data path (distinct surfaces)", () => {
    let s = connected();
    s = tel(s, { ...env(VISION_DESCRIBE_EVENT, { source: "screen", available: true, vlm: true }), source: "local" });
    s = tel(
      s,
      env("app.data", {
        name: V,
        topic: VISION_TOPIC_SCREEN,
        payload: { frame: 1, source: "screen", block_count: 1, text: "Submit", blocks: [{ text: "Submit" }] },
      }),
    );
    // Both coexist: the VLM describe posture AND the OCR read are independent.
    expect(s.visionDescribe).toEqual({ source: "screen", available: true, vlm: true });
    expect(parseVisionScreen(s.appFeeds[V].topics[VISION_TOPIC_SCREEN])!.text).toBe("Submit");
  });
});

/* ------------------------------------------------------------------------ *
 * VisionPanel — the VISUAL DESCRIPTION readout (distinct from OCR SCREEN READ). *
 * HONEST copy: on-device VLM, device-gated on a model download + RAM, describes *
 * the visual SCENE, distinct from OCR text, and shows an honest fall-back when  *
 * the model isn't available. The description text + pixels NEVER appear here.   *
 * ------------------------------------------------------------------------ */

function renderPanel(describe: VisionDescribe | null, running = false): string {
  return renderToStaticMarkup(
    createElement(VisionPanel, { feed: undefined, running, describe }),
  );
}

describe("VisionPanel VISUAL DESCRIPTION readout (on-device VLM, honest)", () => {
  it("renders nothing VLM-specific before any describe arrives", () => {
    const html = renderPanel(null);
    expect(html).not.toContain("VISUAL DESCRIPTION");
    // With no describe + offline app, the OFFLINE placeholder shows.
    expect(html).toContain("VISION OFFLINE");
  });

  it("surfaces the VISUAL DESCRIPTION readout even while the Vision app feed is offline", () => {
    // The describe is daemon-driven, so it must render without the app running.
    const html = renderPanel({ source: "screen", available: true, vlm: true });
    expect(html).toContain("VISUAL DESCRIPTION");
    expect(html).not.toContain("VISION OFFLINE");
  });

  it("labels the on-device VLM and the source, distinct from OCR", () => {
    const html = renderPanel({ source: "image", available: true, vlm: true });
    expect(html).toContain("ON-DEVICE VLM");
    expect(html).toContain("IMAGE");
    expect(html).toContain("DESCRIBED");
    // Honest distinction from OCR text glyphs.
    expect(html).toContain("DISTINCT from OCR");
    expect(html).toContain("device-gated");
  });

  it("NEVER renders pixels or any description text on the available readout", () => {
    const html = renderPanel({ source: "screen", available: true, vlm: true });
    // The honest available copy says the scene was described + spoken, transient.
    expect(html.toLowerCase()).toContain("spoken");
    expect(html.toLowerCase()).toContain("transient");
    expect(html.toLowerCase()).toContain("never leave the device");
  });

  it("shows an honest fall-back when the VLM is enabled but did not describe", () => {
    const html = renderPanel({ source: "screen", available: false, vlm: true });
    expect(html).toContain("FALLBACK");
    expect(html.toLowerCase()).toContain("fell back honestly");
    expect(html).toContain("MODEL ON");
    expect(html).not.toContain("DESCRIBED");
  });

  it("shows an honest 'model not enabled / needs download' state when the VLM ships OFF", () => {
    const html = renderPanel({ source: "image", available: false, vlm: false });
    expect(html).toContain("FALLBACK");
    // renderToStaticMarkup HTML-escapes the apostrophe in "isn't".
    expect(html.toLowerCase()).toContain("vision-language model isn&#x27;t enabled");
    expect(html.toLowerCase()).toContain("multi-gb model download");
    expect(html).toContain("MODEL OFF");
  });
});

/* ------------------------------------------------------------------------ *
 * parseVisionSound (vision.sound — the Apple SOUND ANALYSIS class readout).    *
 * The AUDIO analog of vision.detections: the BUILT-IN ~300-class classifier    *
 * (SNClassifierIdentifier.version1) over a supplied audio CLIP -> the top sound *
 * classes {label, confidence}. CRITICAL: each class carries ONLY label +       *
 * confidence — there is NO audio field, NO clip samples, NO path. The audio     *
 * NEVER leaves the device; only the derived LABELS cross the socket. DISTINCT   *
 * from STT (speech). A malformed payload yields null, junk classes drop, never  *
 * a throw.                                                                      *
 * ------------------------------------------------------------------------ */

describe("parseVisionSound (vision.sound — Apple Sound Analysis class readout)", () => {
  it("parses a full sound-class readout (top classes, classifier, compute unit)", () => {
    const s = parseVisionSound({
      topic: "vision.sound",
      ts: 1718200002.5,
      source: "sound",
      count: 2,
      classes: [
        { label: "dog_bark", confidence: 0.88 },
        { label: "doorbell", confidence: 0.41 },
      ],
      classifier: "SNClassifierIdentifier.version1",
      compute_unit: "all",
    });
    expect(s).toEqual({
      ts: 1718200002.5,
      source: "sound",
      count: 2,
      classes: [
        { label: "dog_bark", confidence: 0.88 },
        { label: "doorbell", confidence: 0.41 },
      ],
      classifier: "SNClassifierIdentifier.version1",
      computeUnit: "all",
    });
  });

  it("returns null when `classes` is absent or not an array (the structural anchor)", () => {
    expect(parseVisionSound({ ts: 1, source: "sound", count: 0 })).toBeNull();
    expect(parseVisionSound({ classes: "lots" })).toBeNull();
    expect(parseVisionSound({})).toBeNull();
  });

  it("treats an EMPTY classes array as a valid 'ran, nothing above floor' frame", () => {
    const s = parseVisionSound({ ts: 5, source: "sound", classes: [] });
    expect(s).not.toBeNull();
    expect(s!.classes).toEqual([]);
    expect(s!.count).toBe(0); // defaults to classes.length when absent
    expect(s!.classifier).toBe("");
    expect(s!.computeUnit).toBe("");
  });

  it("defaults count to the kept-classes length when the count field is absent", () => {
    const s = parseVisionSound({
      classes: [{ label: "music" }, { label: "speech" }],
    });
    expect(s!.count).toBe(2);
  });

  it("drops classes with no usable label; defaults confidence on partials", () => {
    const s = parseVisionSound({
      classes: [
        { confidence: 0.9 }, // no label -> dropped
        "junk", // not an object -> dropped
        { label: "alarm" }, // partial -> confidence defaults to 0
        { label: "glass_breaking", confidence: 0.6 },
      ],
    });
    expect(s!.classes).toEqual([
      { label: "alarm", confidence: 0 },
      { label: "glass_breaking", confidence: 0.6 },
    ]);
  });

  it("carries NO audio / samples / path — only label+confidence survive per class", () => {
    // A hostile payload that tries to smuggle the audio through must NOT leak it:
    // the parser surfaces ONLY {label, confidence} per class.
    const s = parseVisionSound({
      ts: 1,
      source: "sound",
      classes: [
        {
          label: "dog_bark",
          confidence: 0.8,
          // none of these should ever be present; prove they never round-trip:
          audio: [1, 2, 3],
          samples: [0.1, 0.2],
          path: "/tmp/clip.wav",
          pcm: "AAAA",
        },
      ],
      classifier: "SNClassifierIdentifier.version1",
    });
    const c = s!.classes[0] as unknown as Record<string, unknown>;
    expect(Object.keys(c).sort()).toEqual(["confidence", "label"]);
    expect(c.audio).toBeUndefined();
    expect(c.samples).toBeUndefined();
    expect(c.path).toBeUndefined();
    expect(c.pcm).toBeUndefined();
  });
});

/* ------------------------------------------------------------------------ *
 * parseAudioSoundMonitor (audio.sound_monitor — the OPT-IN monitor STATE).     *
 * Channel "local", emitted once at startup from [audio].sound_monitor (SHIPS    *
 * OFF + pinned). NEVER returns null — a malformed payload yields the honest     *
 * fail-OFF snapshot (enabled:false) so a garbled event can never fake           *
 * "monitoring". LABELS-ONLY posture; consent is device_gated (macOS mic/TCC).   *
 * ------------------------------------------------------------------------ */

describe("parseAudioSoundMonitor (audio.sound_monitor — opt-in monitor state)", () => {
  it("parses the shipped-OFF state (the default — never auto-arms)", () => {
    const m = parseAudioSoundMonitor({
      enabled: false,
      consent: "device_gated",
      labels_only: true,
      audio_left_device: false,
    });
    expect(m).toEqual({
      enabled: false,
      consent: "device_gated",
      labelsOnly: true,
      audioLeftDevice: false,
    });
  });

  it("parses the opted-IN state (operator deliberately enabled it)", () => {
    const m = parseAudioSoundMonitor({
      enabled: true,
      consent: "device_gated",
      labels_only: true,
      audio_left_device: false,
    });
    expect(m.enabled).toBe(true);
    // Even when enabled, consent stays device-gated (macOS mic/TCC is separate).
    expect(m.consent).toBe("device_gated");
  });

  it("NEVER returns null; a malformed payload reads as fail-OFF (never a fake monitoring)", () => {
    const m = parseAudioSoundMonitor({});
    expect(m).toEqual({
      enabled: false, // the monitor never silently arms
      consent: "device_gated",
      labelsOnly: true,
      audioLeftDevice: false,
    });
    // A hostile non-bool `enabled` does not flip the monitor on.
    expect(parseAudioSoundMonitor({ enabled: "yes" }).enabled).toBe(false);
  });
});

/* ------------------------------------------------------------------------ *
 * Reducer: vision.sound rides the app.data relay (topic "vision.sound") and     *
 * lands under feed.topics like the other vision.* topics; audio.sound_monitor   *
 * is a top-level "local" envelope that lands in state.audioSoundMonitor. Only   *
 * LABELS cross the socket — never audio. DISTINCT from the STT path.            *
 * ------------------------------------------------------------------------ */

describe("reducer: vision.sound app.data storage (labels only)", () => {
  it("stores the vision.sound payload under feed.topics and round-trips the parser", () => {
    let s = connected();
    s = tel(
      s,
      env("app.data", {
        name: V,
        topic: VISION_TOPIC_SOUND,
        payload: {
          ts: 10,
          source: "sound",
          count: 1,
          classes: [{ label: "dog_bark", confidence: 0.92 }],
          classifier: "SNClassifierIdentifier.version1",
          compute_unit: "all",
        },
      }),
    );
    const feed = s.appFeeds[V];
    expect(feed.running).toBe(true);
    const sound = parseVisionSound(feed.topics[VISION_TOPIC_SOUND])!;
    expect(sound.classes).toEqual([{ label: "dog_bark", confidence: 0.92 }]);
    expect(sound.classifier).toBe("SNClassifierIdentifier.version1");
    // No audio/path/samples ever reach the stored slice.
    const raw = feed.topics[VISION_TOPIC_SOUND] as Record<string, unknown>;
    expect(raw.audio).toBeUndefined();
    expect(raw.path).toBeUndefined();
  });
});

describe("reducer: audio.sound_monitor (local channel, opt-in monitor state)", () => {
  it("starts null and stores the parsed monitor state", () => {
    let s = connected();
    expect(s.audioSoundMonitor).toBeNull();
    s = tel(s, {
      ...env(AUDIO_SOUND_MONITOR_EVENT, {
        enabled: false,
        consent: "device_gated",
        labels_only: true,
        audio_left_device: false,
      }),
      source: "local",
    });
    expect(s.audioSoundMonitor).toEqual({
      enabled: false,
      consent: "device_gated",
      labelsOnly: true,
      audioLeftDevice: false,
    });
  });

  it("a newer monitor state replaces the prior one (off -> on)", () => {
    let s = connected();
    s = tel(s, { ...env(AUDIO_SOUND_MONITOR_EVENT, { enabled: false }), source: "local" });
    s = tel(s, { ...env(AUDIO_SOUND_MONITOR_EVENT, { enabled: true }), source: "local" });
    expect(s.audioSoundMonitor!.enabled).toBe(true);
  });

  it("a malformed monitor payload reads as fail-OFF, never a fake 'monitoring'", () => {
    let s = connected();
    s = tel(s, { ...env(AUDIO_SOUND_MONITOR_EVENT, {}), source: "local" });
    expect(s.audioSoundMonitor!.enabled).toBe(false);
  });
});

/* ------------------------------------------------------------------------ *
 * VisionPanel — the SOUND-CLASS readout (off vision.sound) + the AMBIENT-       *
 * MONITOR indicator (off audio.sound_monitor.enabled). HONEST copy: on-device   *
 * Apple Sound Analysis; LABELS ONLY (audio never leaves the device); a FIXED    *
 * ~300-class classifier (not arbitrary sounds); opt-in + mic/TCC gated;         *
 * continuous monitoring device-gated; DISTINCT from speech/STT.                 *
 * ------------------------------------------------------------------------ */

/** Render the panel with a vision.sound feed slice (built via the real reducer
 *  so the topics map matches production), plus an optional monitor state. */
function renderSoundPanel(
  soundPayload: Record<string, unknown> | null,
  soundMonitor: AudioSoundMonitor | null = null,
): string {
  let st = connected();
  if (soundPayload !== null) {
    st = tel(st, env("app.data", { name: V, topic: VISION_TOPIC_SOUND, payload: soundPayload }));
  }
  return renderToStaticMarkup(
    createElement(VisionPanel, {
      feed: st.appFeeds[V],
      running: st.runningApps.has(V),
      describe: null,
      soundMonitor,
    }),
  );
}

describe("VisionPanel SOUND-CLASS readout (on-device Apple Sound Analysis, honest)", () => {
  it("renders nothing sound-specific before any vision.sound arrives", () => {
    const html = renderSoundPanel(null);
    expect(html).not.toContain("SOUND CLASSES");
    expect(html).toContain("VISION OFFLINE");
  });

  it("renders the top sound classes with confidence + the honest classifier copy", () => {
    const html = renderSoundPanel({
      ts: 1,
      source: "sound",
      count: 2,
      classes: [
        { label: "dog_bark", confidence: 0.88 },
        { label: "doorbell", confidence: 0.41 },
      ],
      classifier: "SNClassifierIdentifier.version1",
      compute_unit: "all",
    });
    expect(html).toContain("SOUND CLASSES");
    expect(html).toContain("ON-DEVICE APPLE SOUND ANALYSIS");
    expect(html).toContain("dog_bark");
    expect(html).toContain("88%");
    expect(html).toContain("doorbell");
    // Honest: fixed ~300-class classifier, labels only, distinct from STT.
    expect(html).toContain("fixed ~300-class classifier (not arbitrary sounds)");
    expect(html.toLowerCase()).toContain("audio never leaves the device");
    expect(html).toContain("DISTINCT from speech/STT");
  });

  it("shows the honest empty state (ran, nothing above threshold)", () => {
    const html = renderSoundPanel({ ts: 1, source: "sound", count: 0, classes: [] });
    expect(html).toContain("SOUND CLASSES");
    expect(html).toContain("NO SOUND CLASSES ABOVE THRESHOLD");
  });
});

describe("VisionPanel AMBIENT-MONITOR indicator (OPT-IN, ships OFF, honest)", () => {
  it("renders nothing monitor-specific before audio.sound_monitor arrives", () => {
    const html = renderSoundPanel(null, null);
    expect(html).not.toContain("AMBIENT SOUND MONITOR");
  });

  it("shows OFF (the shipped default) — never auto-starts, honest copy", () => {
    const html = renderSoundPanel(null, {
      enabled: false,
      consent: "device_gated",
      labelsOnly: true,
      audioLeftDevice: false,
    });
    expect(html).toContain("AMBIENT SOUND MONITOR");
    expect(html).toContain(">OFF<");
    expect(html).not.toContain("MONITORING");
    expect(html.toLowerCase()).toContain("ships off and never auto-starts");
    expect(html.toLowerCase()).toContain("opt-in + mic/tcc gated");
    expect(html.toLowerCase()).toContain("audio never leaves the device");
  });

  it("shows MONITORING when opted in, but states continuous capture is device-gated", () => {
    const html = renderSoundPanel(null, {
      enabled: true,
      consent: "device_gated",
      labelsOnly: true,
      audioLeftDevice: false,
    });
    expect(html).toContain("MONITORING");
    expect(html.toLowerCase()).toContain("continuous monitoring is device-gated");
    expect(html.toLowerCase()).toContain("macos mic/tcc consent");
    expect(html.toLowerCase()).toContain("opt-in");
  });

  it("surfaces the monitor indicator even while the Vision app feed is offline", () => {
    // The monitor state is daemon-driven, so it must render without the app running.
    const html = renderSoundPanel(null, {
      enabled: false,
      consent: "device_gated",
      labelsOnly: true,
      audioLeftDevice: false,
    });
    expect(html).toContain("AMBIENT SOUND MONITOR");
    expect(html).not.toContain("VISION OFFLINE");
  });
});

/* ------------------------------------------------------------------------ *
 * screen_context.* — CONTINUOUS SCREEN CONTEXT (#42). The MOST privacy-        *
 * sensitive READ feature. The wire carries ONLY whether the continuous        *
 * capture loop is ACTIVE + the BOUNDED ring counts (held / cap) + the startup  *
 * config + the recall/forget verb — NEVER the recognized glyphs, NEVER the     *
 * recalled redacted text (those live ONLY in the daemon's transient in-RAM     *
 * ring). The parsers FAIL OFF (a malformed payload reads as NOT watching /     *
 * NOT enabled, never a fake "watching"); counts floor to >= 0. Never throws.   *
 * ------------------------------------------------------------------------ */

describe("screen_context parsers (continuous screen context — secret-free posture)", () => {
  it("seeds the honest OFF-default resting state", () => {
    expect(screenContextInitial()).toEqual({
      enabled: false,
      watching: false,
      held: 0,
      cap: 0,
      ingested: false,
      intervalSecs: null,
      lastVerb: null,
    });
  });

  it("folds the startup config (enabled / cap / interval_secs); enabled ships FALSE", () => {
    const sc = applyScreenContextConfigured(screenContextInitial(), {
      enabled: false,
      cap: 32,
      interval_secs: 5,
    });
    expect(sc.enabled).toBe(false);
    expect(sc.cap).toBe(32);
    expect(sc.intervalSecs).toBe(5);
    // The live watching/held counts are untouched by the config envelope.
    expect(sc.watching).toBe(false);
    expect(sc.held).toBe(0);
  });

  it("folds a watching snapshot: the loop-active bit + the bounded ring counts", () => {
    const sc = applyScreenContextWatching(screenContextInitial(), {
      watching: true,
      ingested: true,
      held: 7,
      cap: 32,
    });
    expect(sc.watching).toBe(true);
    expect(sc.ingested).toBe(true);
    expect(sc.held).toBe(7);
    expect(sc.cap).toBe(32);
  });

  it("a watching snapshot NEVER carries glyphs — only counts land in the posture", () => {
    // Even if a (hostile) payload smuggles a text-ish field, it is NEVER read
    // into the posture — the parser only ever reads watching/held/cap/ingested.
    const sc = applyScreenContextWatching(screenContextInitial(), {
      watching: true,
      held: 1,
      cap: 8,
      // none of these are part of the contract; they must be ignored:
      text: "my-secret-password",
      redacted_text: "still-secret",
      blocks: [{ text: "secret" }],
    });
    expect(Object.values(sc).join("|")).not.toContain("secret");
    expect(Object.values(sc).join("|")).not.toContain("password");
    expect(sc.held).toBe(1);
  });

  it("FAILS OFF — a malformed watching payload reads as NOT watching (never a fake 'watching')", () => {
    const sc = applyScreenContextWatching({ ...screenContextInitial(), held: 4, cap: 8 }, {});
    expect(sc.watching).toBe(false);
    expect(sc.ingested).toBe(false);
    // The prior bounded counts are preserved when a count is absent.
    expect(sc.held).toBe(4);
    expect(sc.cap).toBe(8);
    // A hostile non-bool `watching` does not flip the loop on.
    expect(applyScreenContextWatching(screenContextInitial(), { watching: "yes" }).watching).toBe(
      false,
    );
  });

  it("floors junk counts to >= 0 (never a negative held/cap)", () => {
    const sc = applyScreenContextWatching(screenContextInitial(), {
      watching: true,
      held: -5,
      cap: -1,
    });
    expect(sc.held).toBe(0);
    expect(sc.cap).toBe(0);
  });

  it("folds a recall/forget command verb (verb ONLY — never the recalled text)", () => {
    let sc = applyScreenContextCommand(screenContextInitial(), { verb: "recall", enabled: true });
    expect(sc.lastVerb).toBe("recall");
    expect(sc.enabled).toBe(true);
    sc = applyScreenContextCommand(sc, { verb: "forget", enabled: true });
    expect(sc.lastVerb).toBe("forget");
  });

  it("a malformed command leaves the prior verb untouched (no fake command)", () => {
    const base = applyScreenContextCommand(screenContextInitial(), { verb: "recall" });
    const sc = applyScreenContextCommand(base, {});
    expect(sc.lastVerb).toBe("recall");
  });
});

/* ------------------------------------------------------------------------ *
 * Reducer: the three screen_context.* envelopes ride source "system" and fold  *
 * into state.screenContext (seeded with the OFF-default resting state). SECRET- *
 * FREE — only the loop-active bit + bounded counts + verb cross the socket.     *
 * ------------------------------------------------------------------------ */

describe("reducer: screen_context.* (continuous screen context, secret-free)", () => {
  it("starts at the OFF-default resting posture (no loop, empty ring)", () => {
    const s = connected();
    expect(s.screenContext).toEqual(screenContextInitial());
    expect(s.screenContext.watching).toBe(false);
  });

  it("folds the startup configured snapshot (OFF-default: enabled false)", () => {
    let s = connected();
    s = tel(s, {
      ...env(SCREEN_CONTEXT_CONFIGURED_EVENT, { enabled: false, cap: 32, interval_secs: 5 }),
      source: "system",
    });
    expect(s.screenContext.enabled).toBe(false);
    expect(s.screenContext.cap).toBe(32);
    expect(s.screenContext.intervalSecs).toBe(5);
    expect(s.screenContext.watching).toBe(false);
  });

  it("folds a watching snapshot into the loop-active bit + bounded counts", () => {
    let s = connected();
    s = tel(s, {
      ...env(SCREEN_CONTEXT_WATCHING_EVENT, { watching: true, ingested: true, held: 3, cap: 32 }),
      source: "system",
    });
    expect(s.screenContext.watching).toBe(true);
    expect(s.screenContext.held).toBe(3);
    expect(s.screenContext.cap).toBe(32);
  });

  it("a watching=false exit honestly flips the indicator off (the loop stopped)", () => {
    let s = connected();
    s = tel(s, {
      ...env(SCREEN_CONTEXT_WATCHING_EVENT, { watching: true, held: 2, cap: 8 }),
      source: "system",
    });
    expect(s.screenContext.watching).toBe(true);
    s = tel(s, {
      ...env(SCREEN_CONTEXT_WATCHING_EVENT, { watching: false, held: 2, cap: 8 }),
      source: "system",
    });
    expect(s.screenContext.watching).toBe(false);
    // The bounded counts persist across the stop (the ring still holds entries).
    expect(s.screenContext.held).toBe(2);
  });

  it("a malformed watching payload reads as NOT watching, never a fake 'watching'", () => {
    let s = connected();
    s = tel(s, { ...env(SCREEN_CONTEXT_WATCHING_EVENT, {}), source: "system" });
    expect(s.screenContext.watching).toBe(false);
  });

  it("config + watching fold together (config bounds + live loop state coexist)", () => {
    let s = connected();
    s = tel(s, {
      ...env(SCREEN_CONTEXT_CONFIGURED_EVENT, { enabled: true, cap: 16, interval_secs: 4 }),
      source: "system",
    });
    s = tel(s, {
      ...env(SCREEN_CONTEXT_WATCHING_EVENT, { watching: true, held: 9, cap: 16 }),
      source: "system",
    });
    expect(s.screenContext.enabled).toBe(true);
    expect(s.screenContext.intervalSecs).toBe(4);
    expect(s.screenContext.watching).toBe(true);
    expect(s.screenContext.held).toBe(9);
  });

  it("a recall/forget command folds the verb ONLY (never the recalled text)", () => {
    let s = connected();
    s = tel(s, {
      ...env(SCREEN_CONTEXT_COMMAND_EVENT, { verb: "recall", enabled: false }),
      source: "system",
    });
    expect(s.screenContext.lastVerb).toBe("recall");
    s = tel(s, {
      ...env(SCREEN_CONTEXT_COMMAND_EVENT, { verb: "forget", enabled: false }),
      source: "system",
    });
    expect(s.screenContext.lastVerb).toBe("forget");
  });
});

/* ------------------------------------------------------------------------ *
 * VisionPanel — the SCREEN CONTEXT readout (#42). The PROMINENT amber WATCHING  *
 * SCREEN indicator whenever the continuous loop is active (absent/dim when      *
 * OFF), the bounded recent-context summary (held N / cap M — NEVER raw text),   *
 * and the honest copy: OFF by default, TCC-device-gated, glyph-only never a     *
 * person id, transient + bounded + forgettable, read-only.                     *
 * ------------------------------------------------------------------------ */

function renderScreenContextPanel(
  screenContext: ScreenContext | null,
  running = false,
): string {
  return renderToStaticMarkup(
    createElement(VisionPanel, {
      feed: undefined,
      running,
      describe: null,
      soundMonitor: null,
      screenContext,
    }),
  );
}

describe("VisionPanel SCREEN CONTEXT readout (#42 — prominent WATCHING, honest)", () => {
  it("renders nothing screen-context-specific before any envelope arrives", () => {
    const html = renderScreenContextPanel(null);
    expect(html).not.toContain("SCREEN CONTEXT");
    expect(html).toContain("VISION OFFLINE");
  });

  it("shows the OFF-default resting state — NOT WATCHING, honest copy, no amber alert", () => {
    const html = renderScreenContextPanel(screenContextInitial());
    expect(html).toContain("SCREEN CONTEXT");
    expect(html).toContain("NOT WATCHING");
    expect(html).not.toContain("WATCHING SCREEN");
    expect(html.toLowerCase()).toContain("off by default");
    expect(html.toLowerCase()).toContain("never auto-starts");
    // Honest empty ring.
    expect(html).toContain("No recent screen context");
  });

  it("shows the PROMINENT WATCHING SCREEN indicator when the continuous loop is active", () => {
    const html = renderScreenContextPanel({
      enabled: true,
      watching: true,
      held: 4,
      cap: 32,
      ingested: true,
      intervalSecs: 5,
      lastVerb: null,
    });
    expect(html).toContain("WATCHING SCREEN");
    // The amber alert-state class is applied so the indicator is unmistakable.
    expect(html).toContain("vi-sctx watching");
    expect(html).toContain("vi-sctx-state watching");
    // The pulsing dot marks the live loop.
    expect(html).toContain("vi-sctx-pulse");
    // Surfaces even while the Vision app feed is offline (daemon-driven).
    expect(html).not.toContain("VISION OFFLINE");
  });

  it("surfaces the bounded recent-context counts (held / cap) — NEVER raw text", () => {
    const html = renderScreenContextPanel({
      enabled: true,
      watching: true,
      held: 5,
      cap: 32,
      ingested: true,
      intervalSecs: 5,
      lastVerb: null,
    });
    expect(html).toContain("5 redacted entries");
    expect(html).toContain("cap 32");
    // The bounded counts ride the note line too (held N/cap M).
    expect(html).toContain("held 5/cap 32");
  });

  it("the empty ring reads the honest 'no recent screen context', never a preview", () => {
    const html = renderScreenContextPanel({
      enabled: true,
      watching: true,
      held: 0,
      cap: 32,
      ingested: false,
      intervalSecs: 5,
      lastVerb: null,
    });
    expect(html).toContain("No recent screen context");
    expect(html).not.toContain("redacted entries");
  });

  it("holds the privacy copy verbatim: TCC-device-gated, glyph-only, transient, forgettable, read-only", () => {
    const html = renderScreenContextPanel({
      enabled: true,
      watching: true,
      held: 1,
      cap: 8,
      ingested: true,
      intervalSecs: 5,
      lastVerb: null,
    }).toLowerCase();
    expect(html).toContain("tcc-device-gated");
    expect(html).toContain("glyph-only, never a person id");
    expect(html).toContain("transient");
    expect(html).toContain("forgettable");
    expect(html).toContain("read-only");
    // Pixels never leave the device.
    expect(html).toContain("pixels never leave the device");
  });

  it("a forget command surfaces the wiped-ring affordance (read-only / forgettable)", () => {
    const html = renderScreenContextPanel({
      enabled: false,
      watching: false,
      held: 0,
      cap: 32,
      ingested: false,
      intervalSecs: 5,
      lastVerb: "forget",
    });
    expect(html).toContain("FORGOTTEN");
  });

  it("shows the enabled-but-not-watching honest state (TCC consent still required)", () => {
    const html = renderScreenContextPanel({
      enabled: true,
      watching: false,
      held: 0,
      cap: 32,
      ingested: false,
      intervalSecs: 5,
      lastVerb: null,
    });
    expect(html).toContain("NOT WATCHING");
    expect(html.toLowerCase()).toContain("nothing is being read");
    expect(html.toLowerCase()).toContain("tcc-device-gated");
  });
});
