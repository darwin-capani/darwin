import {
  CANVAS_TOPIC_RENDER_MS,
  CANVAS_TOPIC_SELECTION,
  CANVAS_TOPIC_VIEWPORT,
  parseCanvasRenderMs,
  parseCanvasSelection,
  parseCanvasViewport,
  type CanvasErcFinding,
} from "../core/events";
import type { AppFeed } from "../core/state";
import Frame from "./Frame";

/** Manifest name of the silicon-canvas micro-app
 *  (apps/silicon-canvas/manifest.toml). The panel renders exactly this app's
 *  feed slice; other app names are ignored here (each surface owns its own
 *  component). Silicon Canvas is runtime="binary", gpu=true, surface=
 *  "fullscreen" — the Metal IOSurface composite runs ON-DEVICE; this panel is
 *  the HUD-side telemetry + ERC readout, NOT a re-render of the board. */
export const SILICON_CANVAS_APP = "silicon-canvas";

/** Format a millisecond stat compactly (1 dp under 10ms, else integer). */
function ms(v: number): string {
  return v < 10 ? v.toFixed(1) : Math.round(v).toString();
}

/** Format pixels-per-mm zoom as a "×" multiple, scaled so the schematic's
 *  nominal scale reads near 1× (SPEC §3 zoom range 0.01×–500×). */
function zoom(scale: number): string {
  if (!Number.isFinite(scale) || scale <= 0) return "—";
  if (scale >= 100) return `${Math.round(scale)}×`;
  if (scale >= 10) return `${scale.toFixed(0)}×`;
  return `${scale.toFixed(2)}×`;
}

export default function SiliconCanvasPanel({
  feed,
  running,
}: {
  /** The silicon-canvas app's feed slice, or undefined if it never reported. */
  feed: AppFeed | undefined;
  /** Tracked-running flag from state.runningApps (authoritative over feed). */
  running: boolean;
}) {
  // Live (running) OR a feed that has reported => online. A stopped app that
  // previously reported keeps showing its last telemetry, dimmed.
  const online = running || feed?.running === true;
  const topics = feed?.topics ?? {};

  const render = topics[CANVAS_TOPIC_RENDER_MS]
    ? parseCanvasRenderMs(topics[CANVAS_TOPIC_RENDER_MS])
    : null;
  const viewport = topics[CANVAS_TOPIC_VIEWPORT]
    ? parseCanvasViewport(topics[CANVAS_TOPIC_VIEWPORT])
    : null;
  const selection = topics[CANVAS_TOPIC_SELECTION]
    ? parseCanvasSelection(topics[CANVAS_TOPIC_SELECTION])
    : null;

  const erc: CanvasErcFinding[] = selection?.erc ?? [];
  const errors = erc.filter((f) => f.severity === "error").length;
  const warnings = erc.filter((f) => f.severity === "warning").length;

  // Header tag: ERC summary when an ERC result is present, else the frame
  // draw-call count, else OFFLINE.
  const tag = !online
    ? "OFFLINE"
    : selection?.erc
      ? `${errors} ERR · ${warnings} WARN`
      : render
        ? `${render.draws} DRAWS`
        : "RENDER ON DEVICE";

  return (
    <Frame
      className={`silicon-canvas ${online ? "" : "offline"}`}
      title="SILICON-CANVAS // GPU SCHEMATIC"
      tag={tag}
    >
      {!online && !render && !viewport && !selection ? (
        <div className="sc-placeholder">
          <div className="sc-ph-big">SILICON-CANVAS OFFLINE</div>
          <div className="sc-ph-small">say "open silicon canvas"</div>
        </div>
      ) : (
        <div className="sc-body">
          {/* On-device composite affordance — when the app runs, the fullscreen
              Metal IOSurface renders on the device GPU. This banner reflects that
              the app is running on-device; it is NOT a measured render from the
              HUD, and the HUD never fakes a rendered board. */}
          <div className="sc-surface">
            <span className="sc-surface-dot" aria-hidden="true" />
            <span className="sc-surface-text">
              FULLSCREEN RENDER RUNS ON DEVICE
            </span>
            <span className="sc-surface-sub">Metal IOSurface · GPU-resident · on-device only</span>
          </div>

          {/* canvas.render_ms — frame stats strip (SPEC §2). */}
          <div className="sc-stats">
            <div className="sc-stat">
              <span className="sc-stat-label">P50</span>
              <span className="sc-stat-val">{render ? `${ms(render.p50)}ms` : "—"}</span>
            </div>
            <div className="sc-stat">
              <span className="sc-stat-label">P95</span>
              <span className="sc-stat-val">{render ? `${ms(render.p95)}ms` : "—"}</span>
            </div>
            <div className="sc-stat">
              <span className="sc-stat-label">DRAWS</span>
              <span className="sc-stat-val">{render ? render.draws : "—"}</span>
            </div>
            <div className="sc-stat">
              <span className="sc-stat-label">CULLED</span>
              <span className="sc-stat-val">
                {render ? `${Math.round(render.culledPct)}%` : "—"}
              </span>
            </div>
          </div>

          {/* canvas.viewport — minimap affordance (SPEC §3). The HUD mirrors
              the camera pose; the board itself stays on the device surface. */}
          <div className="sc-viewport">
            <div className="sc-mini" aria-hidden="true">
              <span className="sc-mini-frame" />
            </div>
            <div className="sc-vp-readout">
              <div className="sc-vp-row">
                <span className="sc-vp-label">VIEW</span>
                <span className="sc-vp-val">
                  {viewport
                    ? `${viewport.x.toFixed(1)}, ${viewport.y.toFixed(1)} mm`
                    : "—"}
                </span>
              </div>
              <div className="sc-vp-row">
                <span className="sc-vp-label">ZOOM</span>
                <span className="sc-vp-val">{viewport ? zoom(viewport.scale) : "—"}</span>
              </div>
              {viewport && viewport.layerVisibility.length > 0 ? (
                <div className="sc-layers">
                  {viewport.layerVisibility.map((l) => (
                    <span
                      key={l.layer}
                      className={`sc-layer ${l.visible ? "on" : "off"}`}
                    >
                      {l.layer}
                    </span>
                  ))}
                </div>
              ) : null}
            </div>
          </div>

          {/* canvas.selection — selected net summary (SPEC §4). */}
          {selection?.net ? (
            <div className="sc-net">
              <span className="sc-net-label">NET</span>
              <span className="sc-net-name">{selection.net.name}</span>
              <span className="sc-net-counts">
                {selection.net.entityCount} ENT · {selection.net.pinCount} PIN
              </span>
            </div>
          ) : null}
          {selection?.component ? (
            <div className="sc-net">
              <span className="sc-net-label">CMP</span>
              <span className="sc-net-name">
                {selection.component.reference}
                {selection.component.value ? ` · ${selection.component.value}` : ""}
              </span>
              <span className="sc-net-counts">{selection.component.pinCount} PIN</span>
            </div>
          ) : null}

          {/* canvas.selection.trace — net-walk progress affordance (SPEC §4).
              Present only while a trace is walking. The actual via-flash render
              is on-device; this is the telemetry-driven readout of the front:
              "step k of n", the entity now at the front, BFS distance from the
              seed, and a CROSS-LAYER indicator on a via / cross-copper step. */}
          {selection?.trace ? (
            <div
              className={`sc-trace${selection.trace.crossesLayer ? " cross-layer" : ""}${
                selection.trace.atEnd ? " at-end" : ""
              }`}
            >
              <div className="sc-trace-head">
                <span className="sc-trace-label">TRACE</span>
                <span className="sc-trace-step">
                  step {selection.trace.step} of {selection.trace.of}
                </span>
                {selection.trace.crossesLayer ? (
                  <span className="sc-trace-xlayer">CROSS-LAYER</span>
                ) : null}
                {selection.trace.atEnd ? (
                  <span className="sc-trace-end">END</span>
                ) : null}
              </div>
              <div className="sc-trace-front">
                <span className="sc-trace-at">
                  {selection.trace.at.kind.toUpperCase()} #{selection.trace.at.index}
                </span>
                <span className="sc-trace-dist">
                  dist {selection.trace.distance}
                </span>
              </div>
            </div>
          ) : null}

          {/* ERC findings list — amber warning / red error badges (SPEC §5).
              An ERC result line with no findings reads "ERC CLEAN". */}
          {selection?.erc ? (
            <div className="sc-erc">
              <div className="sc-erc-head">
                <span className="sc-erc-title">ERC</span>
                {erc.length === 0 ? (
                  <span className="sc-erc-clean">CLEAN</span>
                ) : (
                  <span className="sc-erc-count">
                    {errors} ERROR · {warnings} WARNING
                  </span>
                )}
              </div>
              {erc.length > 0 ? (
                <div className="sc-erc-list">
                  {erc.map((f, i) => (
                    <div key={`${f.code}-${i}`} className={`sc-erc-row ${f.severity}`}>
                      <span className={`sc-erc-badge ${f.severity}`}>
                        {f.severity === "error" ? "ERR" : "WARN"}
                      </span>
                      <span className="sc-erc-code">{f.code}</span>
                      {f.message ? <span className="sc-erc-msg">{f.message}</span> : null}
                    </div>
                  ))}
                </div>
              ) : null}
            </div>
          ) : null}
        </div>
      )}
    </Frame>
  );
}
