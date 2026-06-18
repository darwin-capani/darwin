import type { CSSProperties } from "react";
import {
  VISION_TOPIC_DETECTIONS,
  VISION_TOPIC_ERROR,
  VISION_TOPIC_MOTION,
  VISION_TOPIC_PERF,
  VISION_TOPIC_SCREEN,
  VISION_TOPIC_SOUND,
  VISION_TOPIC_STATUS,
  parseVisionDetections,
  parseVisionError,
  parseVisionMotion,
  parseVisionPerf,
  parseVisionScreen,
  parseVisionSound,
  parseVisionStatus,
} from "../core/events";
import type { AudioSoundMonitor, ScreenContext, VisionDescribe } from "../core/events";
import type { AppFeed } from "../core/state";
import { agentProfile } from "../core/agents";
import Frame from "./Frame";

/** Manifest name of the vision micro-app (apps/vision/manifest.toml). The panel
 *  renders exactly this app's feed slice; other app names are ignored here
 *  (each surface owns its own component). Vision is runtime="binary",
 *  surface="panel", gpu=true, net_hosts=[] — the camera/screen capture and the
 *  Apple Vision/Core ML INFERENCE run ON-DEVICE behind a macOS TCC consent gate
 *  (NOT grantable by SBPL). This panel is the HUD-side telemetry readout, NOT a
 *  video preview: it shows COUNTS / boxes / labels / timing, never pixels,
 *  never identity. */
export const VISION_APP = "vision";

/** Vision's identity hue from the static roster (config/agents.toml mirror —
 *  vision = 265, a deep violet). Drives the panel accent so the surface reads
 *  as the Vision agent's, distinct from the cyan default. RED stays reserved
 *  for alerts/errors only. */
const VISION_HUE = agentProfile(VISION_APP)?.hue ?? 265;

/** The detection kinds we summarize, in display order, with a short label. An
 *  unknown future kind from the wire still renders in the per-detection list;
 *  this only fixes the order of the by-kind count chips. */
const KIND_ORDER: ReadonlyArray<{ kind: string; label: string }> = [
  { kind: "human", label: "HUMAN" },
  { kind: "animal", label: "ANIMAL" },
  { kind: "object", label: "OBJECT" },
  { kind: "salientRegion", label: "SALIENT" },
  { kind: "motion", label: "MOTION" },
];

/** Format a millisecond stat compactly (1 dp under 10ms, else integer). */
function ms(v: number): string {
  return v < 10 ? v.toFixed(1) : Math.round(v).toString();
}

/** A normalized confidence 0..1 as an integer percent. */
function pct(v: number): string {
  return `${Math.round(Math.max(0, Math.min(1, v)) * 100)}%`;
}

/** A TCC authorization flag -> honest readout. null = the app did not report
 *  it, which we surface as "UNKNOWN" (never a fake "granted"). */
function tcc(v: boolean | null): string {
  if (v === null) return "UNKNOWN";
  return v ? "GRANTED" : "NOT GRANTED";
}

export default function VisionPanel({
  feed,
  running,
  describe,
  soundMonitor,
  screenContext,
}: {
  /** The vision app's feed slice, or undefined if it never reported. */
  feed: AppFeed | undefined;
  /** Tracked-running flag from state.runningApps (authoritative over feed). */
  running: boolean;
  /** The last ON-DEVICE VLM describe outcome (vision.describe, channel "local"),
   *  or null until the daemon emits one. METADATA ONLY — source kind +
   *  availability + the model-enabled flag; NEVER pixels / description text /
   *  path (the visual content never rides telemetry). DISTINCT from the OCR
   *  screen-read readout below. Daemon-driven, so it surfaces even when the
   *  Vision app feed itself is offline. */
  describe?: VisionDescribe | null;
  /** The OPT-IN ambient sound-monitor STATE (audio.sound_monitor, channel
   *  "local"), or null until the daemon emits it at startup. Daemon-driven (NOT
   *  part of the Vision app feed), so the indicator surfaces even when the app
   *  surface is offline. SHIPS OFF: `enabled` is the operator's config opt-in;
   *  even when enabled, continuous ambient capture is DEVICE-GATED behind macOS
   *  mic/TCC. LABELS ONLY — the audio never leaves the device. */
  soundMonitor?: AudioSoundMonitor | null;
  /** The CONTINUOUS SCREEN-CONTEXT posture (#42, folded from the secret-free
   *  screen_context.* system envelopes), or null/undefined until the daemon
   *  emits the startup config. Daemon-driven (NOT part of the Vision app feed),
   *  so the PROMINENT WATCHING indicator surfaces even when the app surface is
   *  offline. SHIPS OFF: the continuous loop never runs by default; the live
   *  capture is TCC-DEVICE-GATED (Screen Recording). SECRET-FREE — only the
   *  loop-active bit + bounded counts ride this surface, NEVER the recognized
   *  glyphs / recalled text; glyph-only (never a person id), transient, bounded,
   *  forgettable, read-only. */
  screenContext?: ScreenContext | null;
}) {
  // Live (running) OR a feed that has reported => online. A stopped app that
  // previously reported keeps showing its last telemetry, dimmed.
  const online = running || feed?.running === true;
  const topics = feed?.topics ?? {};

  const detections = topics[VISION_TOPIC_DETECTIONS]
    ? parseVisionDetections(topics[VISION_TOPIC_DETECTIONS])
    : null;
  const status = topics[VISION_TOPIC_STATUS]
    ? parseVisionStatus(topics[VISION_TOPIC_STATUS])
    : null;
  const motion = topics[VISION_TOPIC_MOTION]
    ? parseVisionMotion(topics[VISION_TOPIC_MOTION])
    : null;
  const perf = topics[VISION_TOPIC_PERF]
    ? parseVisionPerf(topics[VISION_TOPIC_PERF])
    : null;
  const error = topics[VISION_TOPIC_ERROR]
    ? parseVisionError(topics[VISION_TOPIC_ERROR])
    : null;
  const screen = topics[VISION_TOPIC_SCREEN]
    ? parseVisionScreen(topics[VISION_TOPIC_SCREEN])
    : null;
  const sound = topics[VISION_TOPIC_SOUND]
    ? parseVisionSound(topics[VISION_TOPIC_SOUND])
    : null;

  // The describe outcome is daemon-driven (not part of the Vision app feed), so
  // it can be present while the app surface is otherwise offline. It counts as
  // data so the panel renders its body (with the VISUAL DESCRIPTION readout)
  // rather than the OFFLINE placeholder.
  // The sound-class readout (vision.sound) and the ambient-monitor indicator
  // (audio.sound_monitor) are daemon-driven, so — like `describe` — they count
  // as data and render the panel body even while the Vision app surface is
  // otherwise offline (e.g. the monitor-state indicator at startup).
  const hasData = !!(
    detections ||
    status ||
    motion ||
    perf ||
    error ||
    screen ||
    sound ||
    describe ||
    soundMonitor ||
    screenContext
  );

  // Header tag: an active error wins (alert), then the CONTINUOUS-SCREEN-CONTEXT
  // WATCHING signal (alert-grade — the screen is being read right now, so it
  // must be unmistakable even before the body), then a live detection count, a
  // screen-read block count, the watch state, else OFFLINE/ON DEVICE. The
  // WATCHING tag is daemon-driven so it shows even when the app surface is
  // offline.
  const tag = error
    ? error.code.toUpperCase()
    : screenContext?.watching
      ? "WATCHING SCREEN"
      : !online
        ? "OFFLINE"
        : detections
          ? `${detections.count} DET`
          : screen
            ? `${screen.blockCount} BLOCKS`
            : sound
              ? `${sound.count} SOUND`
              : status
                ? status.state.toUpperCase()
                : "ON DEVICE";

  // The whole panel carries the Vision hue as a CSS custom property so the
  // accents read as the Vision agent's violet (consumed by .vision-* in
  // styles.css). RED stays reserved for the error row only.
  const style = { ["--vision-hue" as string]: String(VISION_HUE) } as CSSProperties;

  return (
    <Frame
      className={`vision ${online ? "" : "offline"}`}
      title="VISION // ON-DEVICE SIGHT"
      tag={tag}
    >
      {!online && !hasData ? (
        <div className="vi-placeholder" style={style}>
          <div className="vi-ph-big">VISION OFFLINE</div>
          <div className="vi-ph-small">say "open vision"</div>
        </div>
      ) : (
        <div className="vi-body" style={style}>
          {/* On-device honesty banner. Live capture + inference run on the
              device behind a macOS TCC consent gate; this panel is the
              telemetry readout, never a video preview, and never leaves the
              device. */}
          <div className="vi-surface">
            <span className="vi-surface-dot" aria-hidden="true" />
            <span className="vi-surface-text">CAPTURE + INFERENCE ON DEVICE</span>
            <span className="vi-surface-sub">TCC-consented · offline · no identity</span>
          </div>

          {/* screen_context.* — CONTINUOUS SCREEN CONTEXT (#42). The MOST
              privacy-sensitive read feature, so this surface is placed FIRST in
              the body + carries a PROMINENT, amber/alert-styled WATCHING SCREEN
              indicator whenever the continuous loop is active, so the user can
              NEVER miss that the screen is being read. When the loop is OFF (the
              shipped default) the indicator is dim/inert ("NOT WATCHING").
              SECRET-FREE: ONLY the loop-active bit + the bounded ring counts
              (held N / cap M) ride this surface — NEVER the recognized glyphs or
              the recalled redacted text, which stay TRANSIENT in the daemon's
              in-RAM ring and are spoken (persona-voiced) only. HONEST copy: OFF
              by default, TCC-device-gated, glyph-only (never a person id),
              transient + bounded + forgettable, read-only. Daemon-driven, so the
              WATCHING indicator surfaces even while the Vision app feed is
              offline. */}
          {screenContext ? (
            <div className={`vi-sctx ${screenContext.watching ? "watching" : "off"}`}>
              <div className="vi-sctx-head">
                <span className="vi-sctx-label">SCREEN CONTEXT</span>
                <span
                  className={`vi-sctx-state ${screenContext.watching ? "watching" : "off"}`}
                  role={screenContext.watching ? "status" : undefined}
                >
                  {screenContext.watching ? (
                    <>
                      <span className="vi-sctx-pulse" aria-hidden="true" />
                      WATCHING SCREEN
                    </>
                  ) : (
                    "NOT WATCHING"
                  )}
                </span>
              </div>
              {/* The bounded, SECRET-FREE recent-context summary: the redacted
                  entry count + the hard cap. NEVER raw/sensitive text — the
                  glyphs never cross the wire. An empty ring reads the honest
                  "no recent screen context", never a fabricated preview. */}
              <div className="vi-sctx-ring">
                {screenContext.held > 0 ? (
                  <span className="vi-sctx-held">
                    {screenContext.held} redacted {screenContext.held === 1 ? "entry" : "entries"}
                    {screenContext.cap > 0 ? ` · cap ${screenContext.cap}` : ""} held in the
                    bounded ring
                  </span>
                ) : (
                  <span className="vi-sctx-empty">
                    No recent screen context
                    {screenContext.cap > 0 ? ` · cap ${screenContext.cap}` : ""}
                  </span>
                )}
                {screenContext.lastVerb ? (
                  <span className={`vi-sctx-verb ${screenContext.lastVerb}`}>
                    {screenContext.lastVerb === "forget"
                      ? "FORGOTTEN — ring wiped"
                      : screenContext.lastVerb === "recall"
                        ? "RECALLED — read-only"
                        : screenContext.lastVerb.toUpperCase()}
                  </span>
                ) : null}
              </div>
              <div className="vi-sctx-copy">
                {screenContext.watching
                  ? "The continuous screen-read loop is ACTIVE — it periodically OCRs one frame into a bounded, redacted, transient in-RAM ring (TCC-device-gated: it requires Screen Recording consent). Glyph-only — never a face or person id; pixels never leave the device. Say “forget my screen context” to wipe it."
                  : screenContext.enabled
                    ? "The continuous screen-read loop is enabled but NOT watching right now — even enabled, live capture is TCC-device-gated behind macOS Screen Recording consent the daemon cannot grant. Nothing is being read."
                    : "OFF by default — the continuous screen-read loop never auto-starts. Even when enabled, live capture is TCC-device-gated (Screen Recording). Nothing is being read."}
              </div>
              <div className="vi-sctx-note">
                OFF by default · TCC-device-gated · glyph-only, never a person id ·
                transient + bounded (held {screenContext.held}/cap {screenContext.cap}) ·
                forgettable (“forget my screen context”) · read-only
              </div>
            </div>
          ) : null}

          {/* vision.error — recoverable error (e.g. tcc_denied). The ONLY place
              this panel uses --alert-red. tcc_denied is the honest, expected
              state until the user grants Camera / Screen Recording on-device. */}
          {error ? (
            <div className="vi-error">
              <span className="vi-error-badge" aria-hidden="true" />
              <span className="vi-error-code">{error.code}</span>
              {error.message ? (
                <span className="vi-error-msg">{error.message}</span>
              ) : null}
              {error.source ? (
                <span className="vi-error-src">{error.source}</span>
              ) : null}
            </div>
          ) : null}

          {/* vision.status — watch lifecycle + TCC capability snapshot. The TCC
              flags read honestly: GRANTED / NOT GRANTED / UNKNOWN — the panel
              never implies consent it cannot see. */}
          {status ? (
            <div className="vi-status">
              <div className="vi-status-head">
                <span className={`vi-state ${status.state}`}>{status.state.toUpperCase()}</span>
                {status.source ? (
                  <span className="vi-status-src">{status.source.toUpperCase()}</span>
                ) : null}
                {status.sensitivity !== null ? (
                  <span className="vi-status-sens">SENS {pct(status.sensitivity)}</span>
                ) : null}
              </div>
              <div className="vi-tcc">
                <span className={`vi-tcc-chip ${status.cameraAuthorized ? "on" : ""}`}>
                  CAM {tcc(status.cameraAuthorized)}
                </span>
                <span className={`vi-tcc-chip ${status.screenAuthorized ? "on" : ""}`}>
                  SCREEN {tcc(status.screenAuthorized)}
                </span>
              </div>
              {status.message ? (
                <div className="vi-status-msg">{status.message}</div>
              ) : null}
            </div>
          ) : null}

          {/* vision.detections — per-frame summary: by-kind count chips + the
              per-box detection list. Counts/boxes/labels ONLY. Humans are
              generic rectangles, NOT named people. */}
          {detections ? (
            <div className="vi-detect">
              <div className="vi-detect-head">
                <span className="vi-detect-label">DETECTIONS</span>
                <span className="vi-detect-frame">
                  FRAME {detections.frame}
                  {detections.source ? ` · ${detections.source.toUpperCase()}` : ""}
                </span>
                <span className="vi-detect-count">{detections.count}</span>
              </div>
              <div className="vi-kinds">
                {KIND_ORDER.filter((k) => (detections.byKind[k.kind] ?? 0) > 0).map((k) => (
                  <span key={k.kind} className={`vi-kind ${k.kind}`}>
                    {k.label} {detections.byKind[k.kind]}
                  </span>
                ))}
                {detections.count === 0 ? (
                  <span className="vi-kind-empty">NO DETECTIONS THIS FRAME</span>
                ) : null}
              </div>
              {detections.detections.length > 0 ? (
                <div className="vi-rows">
                  {detections.detections.map((d, i) => (
                    <div key={`${d.kind}-${i}`} className={`vi-row ${d.kind}`}>
                      <span className="vi-row-kind">{d.kind.toUpperCase()}</span>
                      <span className="vi-row-label">{d.label || "—"}</span>
                      <span className="vi-row-conf">{pct(d.confidence)}</span>
                    </div>
                  ))}
                </div>
              ) : null}
            </div>
          ) : null}

          {/* vision.describe — the ON-DEVICE VISION-LANGUAGE-MODEL (VLM) readout.
              DISTINCT from the OCR SCREEN READ below: OCR transcribes the TEXT
              GLYPHS, the VLM DESCRIBES + reasons about the visual SCENE (an
              on-device mlx-vlm model). HONESTY: the description itself NEVER rides
              telemetry (no pixels, no text, no path here) — it is spoken in the
              persona-voiced reply and kept transient; this readout shows only the
              honest POSTURE: which source, whether the on-device VLM actually
              described it (`available`), and whether the model is enabled (`vlm`).
              DEVICE-GATED: the VLM needs a multi-GB model download + RAM, so it
              ships OFF; when it isn't downloaded / enabled the readout shows an
              honest "model not downloaded" fallback state, never a fake scene. */}
          {describe ? (
            <div className={`vi-vlm ${describe.available ? "available" : "fallback"}`}>
              <div className="vi-vlm-head">
                <span className="vi-vlm-label">VISUAL DESCRIPTION</span>
                <span className="vi-vlm-tag">
                  ON-DEVICE VLM · {describe.source.toUpperCase()}
                </span>
                <span className={`vi-vlm-state ${describe.available ? "on" : "off"}`}>
                  {describe.available ? "DESCRIBED" : "FALLBACK"}
                </span>
              </div>
              {describe.available ? (
                // The on-device VLM produced a description. The text is spoken
                // (persona-voiced) + kept transient — it is NEVER carried on
                // telemetry, so it is honestly NOT shown here.
                <div className="vi-vlm-spoken">
                  Scene described on-device — spoken aloud. The description reasons
                  about the visual scene (distinct from OCR text) and is kept
                  transient; pixels never leave the device, never on this readout.
                </div>
              ) : (
                // Honest fallback: the VLM did not produce a description (the model
                // isn't downloaded / is turned off, the image was out of bounds, or
                // the model/transport was unavailable). The daemon falls back
                // honestly (e.g. to OCR) — never a fabricated scene.
                <div className="vi-vlm-fallback">
                  {describe.vlm
                    ? "The on-device vision-language model couldn't describe this — it fell back honestly (e.g. to OCR text). No scene is invented."
                    : "The on-device vision-language model isn't enabled — it needs a multi-GB model download + RAM, so it ships OFF. Fell back honestly (e.g. to OCR text)."}
                </div>
              )}
              <div className="vi-vlm-note">
                VLM = reasons about the visual scene · DISTINCT from OCR text ·
                device-gated (model download + RAM) · MODEL {describe.vlm ? "ON" : "OFF"}
              </div>
            </div>
          ) : null}

          {/* vision.screen — OCR screen-read readout (READ ON REQUEST). The full
              readable text (reading order), the control-candidate labels, and —
              for a "where is <X>" request — the located control's box/center.
              READ-ONLY: this LOCATES/DESCRIBES a control, it never clicks.
              PRIVACY: the recognized text is sensitive + TRANSIENT — the daemon
              keeps it off lifelong memory / optimizer traces; it shows here only
              while the read is live and is never persisted by the HUD. Live
              capture is DEVICE-GATED behind macOS TCC (Screen Recording). */}
          {screen ? (
            <div className="vi-screen">
              <div className="vi-screen-head">
                <span className="vi-screen-label">
                  {screen.readKind === "handwriting"
                    ? "HANDWRITING READ"
                    : screen.readKind === "document"
                      ? "DOCUMENT SCAN"
                      : screen.readKind === "context"
                        ? "CONTEXT SNAPSHOT"
                        : "SCREEN READ"}
                </span>
                <span className="vi-screen-tag">READ ON REQUEST · TRANSIENT</span>
                <span className="vi-screen-count">{screen.blockCount} BLK</span>
              </div>
              {/* NON-RAW-TEXT signal: read length + (for #29) the HONEST
                  document-detected bool. Shows "read N chars" / "no document
                  found" WITHOUT rendering the sensitive glyphs — distinct from the
                  full text below, which is transient + live-only. */}
              <div className="vi-screen-meta">
                {screen.readKind === "document" ? (
                  <span
                    className={`vi-screen-doc ${screen.documentDetected ? "found" : "none"}`}
                  >
                    {screen.documentDetected ? "DOCUMENT FOUND" : "NO DOCUMENT FOUND"}
                  </span>
                ) : null}
                <span className="vi-screen-len">
                  {screen.textPresent ? `${screen.textLength} CHARS READ` : "NO TEXT READ"}
                </span>
              </div>
              {/* Where-is locate result (READ-ONLY: box/center, never a click). */}
              {screen.query ? (
                <div className="vi-screen-locate">
                  <span className="vi-screen-q">WHERE IS “{screen.query}”</span>
                  {screen.located ? (
                    <span className="vi-screen-hit">
                      “{screen.located.text}” @ {pct(screen.located.center.x)},{" "}
                      {pct(screen.located.center.y)}
                    </span>
                  ) : (
                    <span className="vi-screen-miss">NOT FOUND ON SCREEN</span>
                  )}
                </div>
              ) : null}
              {/* Control-candidate labels (short button-ish strings). LOCATE-only. */}
              {screen.controls.length > 0 ? (
                <div className="vi-screen-controls">
                  {screen.controls.slice(0, 12).map((c, i) => (
                    <span key={`ctl-${i}`} className="vi-screen-ctl">
                      {c.text}
                    </span>
                  ))}
                </div>
              ) : null}
              {/* The full readable text (reading order). Sensitive + transient —
                  rendered live only, never persisted. */}
              {screen.text ? (
                <pre className="vi-screen-text">{screen.text}</pre>
              ) : (
                <div className="vi-screen-empty">NO TEXT RECOGNIZED THIS READ</div>
              )}
            </div>
          ) : null}

          {/* audio.sound_monitor — the OPT-IN ambient sound-monitor STATE
              indicator. SHIPS OFF: `enabled` is the operator's config opt-in (no
              tool/agent/model can flip it, no auto-arm). Even when MONITORING,
              continuous ambient capture is DEVICE-GATED behind macOS mic/TCC
              consent (consent="device_gated"). LABELS ONLY — only the sound-class
              labels would ever surface; the audio never leaves the device. Daemon-
              driven, so it renders even while the Vision app feed is offline. */}
          {soundMonitor ? (
            <div className={`vi-monitor ${soundMonitor.enabled ? "on" : "off"}`}>
              <div className="vi-monitor-head">
                <span className="vi-monitor-label">AMBIENT SOUND MONITOR</span>
                <span className={`vi-monitor-state ${soundMonitor.enabled ? "on" : "off"}`}>
                  {soundMonitor.enabled ? "MONITORING" : "OFF"}
                </span>
              </div>
              <div className="vi-monitor-copy">
                {soundMonitor.enabled
                  ? "OPT-IN monitor is enabled — periodic on-device Apple Sound Analysis runs ONCE macOS mic/TCC consent is granted (continuous monitoring is device-gated). Only the sound-class labels surface; the audio never leaves the device."
                  : "OPT-IN monitor ships OFF and never auto-starts — the mic stays closed for ambient classification. Distinct from speech/STT. Only labels would ever surface; the audio never leaves the device."}
              </div>
              <div className="vi-monitor-note">
                opt-in + mic/TCC gated · consent {soundMonitor.consent} ·
                labels only · audio never leaves the device
              </div>
            </div>
          ) : null}

          {/* vision.sound — the Apple SOUND ANALYSIS class readout (the audio
              analog of vision.detections). The top sound classes {label,
              confidence} from the BUILT-IN classifier
              (SNClassifierIdentifier.version1 — a FIXED ~300-class vocabulary,
              NOT arbitrary sounds), over a supplied audio CLIP. HONESTY: on-device
              Apple Sound Analysis; LABELS ONLY — the audio never leaves the device
              (no audio rides this readout); audio SCENE understanding (dog bark /
              doorbell / alarm), DISTINCT from speech/STT (no transcript). An empty
              readout is the honest "ran, nothing above the floor" state (a too-
              short / undecodable clip yields a no_sound_classes error instead). */}
          {sound ? (
            <div className="vi-sound">
              <div className="vi-sound-head">
                <span className="vi-sound-label">SOUND CLASSES</span>
                <span className="vi-sound-tag">
                  ON-DEVICE APPLE SOUND ANALYSIS
                  {sound.source ? ` · ${sound.source.toUpperCase()}` : ""}
                </span>
                <span className="vi-sound-count">{sound.count}</span>
              </div>
              {sound.classes.length > 0 ? (
                <div className="vi-sound-rows">
                  {sound.classes.map((c, i) => (
                    <div key={`${c.label}-${i}`} className="vi-sound-row">
                      <span className="vi-sound-name">{c.label}</span>
                      <span className="vi-sound-bar" aria-hidden="true">
                        <span
                          className="vi-sound-fill"
                          style={{
                            width: `${Math.round(Math.max(0, Math.min(1, c.confidence)) * 100)}%`,
                          }}
                        />
                      </span>
                      <span className="vi-sound-conf">{pct(c.confidence)}</span>
                    </div>
                  ))}
                </div>
              ) : (
                <div className="vi-sound-empty">NO SOUND CLASSES ABOVE THRESHOLD</div>
              )}
              <div className="vi-sound-note">
                fixed ~300-class classifier (not arbitrary sounds) · labels only,
                audio never leaves the device · DISTINCT from speech/STT
                {sound.computeUnit ? ` · ${sound.computeUnit.toUpperCase()}` : ""}
              </div>
            </div>
          ) : null}

          {/* vision.motion — last motion event (generic: magnitude + region,
              never what moved). */}
          {motion ? (
            <div className="vi-motion">
              <span className="vi-motion-label">MOTION</span>
              <span className="vi-motion-bar" aria-hidden="true">
                <span
                  className="vi-motion-fill"
                  style={{ width: `${Math.round(Math.max(0, Math.min(1, motion.magnitude)) * 100)}%` }}
                />
              </span>
              <span className="vi-motion-mag">{pct(motion.magnitude)}</span>
              <span className="vi-motion-frame">F{motion.frame}</span>
            </div>
          ) : null}

          {/* vision.perf — inference timing. INFER FPS is the inference-bound
              ceiling (1000/p50), NOT a capture/camera rate; UNIT is the requested
              compute eligibility (ane/gpu/all), not measured execution residency. */}
          {perf ? (
            <div className="vi-perf">
              <div className="vi-perf-stat">
                <span className="vi-perf-label">P50</span>
                <span className="vi-perf-val">{ms(perf.p50Ms)}ms</span>
              </div>
              <div className="vi-perf-stat">
                <span className="vi-perf-label">P95</span>
                <span className="vi-perf-val">{ms(perf.p95Ms)}ms</span>
              </div>
              <div className="vi-perf-stat">
                <span className="vi-perf-label">INFER FPS</span>
                <span className="vi-perf-val">{perf.fps.toFixed(1)}</span>
              </div>
              <div className="vi-perf-stat">
                <span className="vi-perf-label">UNIT</span>
                <span className="vi-perf-val">
                  {perf.computeUnit ? perf.computeUnit.toUpperCase() : "—"}
                </span>
              </div>
            </div>
          ) : null}
        </div>
      )}
    </Frame>
  );
}
