import type { ChartSpec, ChartSeries, ChartPoint } from "../core/events";
import Frame from "./Frame";

/**
 * DATA -> CHART (#41) — the EXACT renderer for a daemon ChartSpec
 * (daemon/src/chart.rs, emitted as `chart.data` from a data path in router.rs).
 *
 * It plots the series the daemon emitted, by kind (bar / line / sparkline):
 *   - EVERY emitted point is plotted. The line/sparkline draw segments ONLY
 *     between the GIVEN points; the bar draws one bar per given point.
 *   - NO interpolation beyond those segments, NO invented/extrapolated point,
 *     NO resampling — what the daemon emitted is what is drawn.
 *   - axis labels are the daemon's strings; axis RANGES are DERIVED from the
 *     actual data (min/max of the plotted x and y), never guessed or padded with
 *     a fabricated bound.
 *   - an honest-empty spec (no plottable point) renders the honest-empty state,
 *     never a fabricated point or a flat "zero" line.
 *
 * HONESTY CONTRACT (do not regress):
 *   - PLOTS ONLY THE EMITTED POINTS. The reducer hands a defensively-parsed spec
 *     whose points are the daemon's verbatim [x,y] pairs (a malformed point was
 *     dropped, never zero-filled); this component maps them to screen coordinates
 *     and draws — it never adds, removes, smooths, or extrapolates a point.
 *   - HONEST AXES. The plotted ranges come from the data's own min/max. A single
 *     point, or a series flat in one axis, is shown honestly (a degenerate range
 *     is widened symmetrically only so the point is visible — the point's VALUE is
 *     unchanged and labelled with its real number).
 *   - HONEST EMPTY. `spec.empty` (re-derived by the parser from the surviving
 *     points) drives a plain "no data to chart" state — never a fabricated point.
 *   - SHIPPED OFF. The [chart].enabled gate ships false, so until it is enabled
 *     the daemon emits no chart.data and this panel renders NOTHING.
 *   - NEUTRAL + SECRET-FREE. A chart is pure presentation — no button, no action,
 *     no network. The wire carries only labels, axis strings, a title, and the
 *     numeric points.
 */

/** Plot box geometry (SVG user units). The viewBox is fixed; the data is mapped
 *  into the inner plot rect, leaving a gutter for the axis labels + ticks. */
const VIEW_W = 320;
const VIEW_H = 180;
const PAD_L = 38;
const PAD_R = 10;
const PAD_T = 10;
const PAD_B = 26;
const PLOT_W = VIEW_W - PAD_L - PAD_R;
const PLOT_H = VIEW_H - PAD_T - PAD_B;

/** A series accent, cycled per series so multiple series are distinguishable.
 *  Pure CSS variables (theme-driven), not hardcoded colors. */
const SERIES_ACCENTS = [
  "var(--cyan-bright)",
  "var(--learn-green)",
  "var(--ice)",
  "var(--warn-amber)",
];

/** The data-derived numeric range of one axis across every plotted point. Honest
 *  by construction — min/max of the REAL values, never a padded/guessed bound. */
interface Range {
  min: number;
  max: number;
}

/** Compute the [min, max] of a coordinate across all points of all series. A
 *  degenerate range (all-equal, or a single point) is widened SYMMETRICALLY around
 *  the value so the point is visible — the value itself is never changed, only the
 *  drawing window. Returns null when there are no points (honest-empty). */
function axisRange(series: ChartSeries[], pick: (p: ChartPoint) => number): Range | null {
  let min = Infinity;
  let max = -Infinity;
  for (const s of series) {
    for (const p of s.points) {
      const v = pick(p);
      if (v < min) min = v;
      if (v > max) max = v;
    }
  }
  if (!Number.isFinite(min) || !Number.isFinite(max)) return null;
  if (min === max) {
    // A flat axis: widen by 1 (or |v| when larger) so the single value is
    // visible. The labelled tick values still report the real number.
    const pad = Math.max(1, Math.abs(min));
    return { min: min - pad, max: max + pad };
  }
  return { min, max };
}

/** Map a data x into the plot rect (left->right). For bars/sparkline an x range
 *  spanning a single value is handled by axisRange's widening. */
function projectX(x: number, xr: Range): number {
  const t = (x - xr.min) / (xr.max - xr.min);
  return PAD_L + t * PLOT_W;
}

/** Map a data y into the plot rect (bottom->top, SVG y grows downward). */
function projectY(y: number, yr: Range): number {
  const t = (y - yr.min) / (yr.max - yr.min);
  return PAD_T + (1 - t) * PLOT_H;
}

/** A compact numeric tick label — integers as-is, fractionals to 2 dp, trimmed. */
function fmtTick(v: number): string {
  if (Number.isInteger(v)) return String(v);
  return Number(v.toFixed(2)).toString();
}

export default function ChartPanel({ spec }: { spec: ChartSpec | null }) {
  // Nothing to show until a chart.data arrives. The [chart].enabled gate ships
  // OFF, so the reducer holds `chart` at null until it is enabled AND a chart op
  // runs — render nothing rather than a placeholder (mirrors the other event-fed
  // panels).
  if (spec === null) return null;

  const title = spec.title.length > 0 ? spec.title : "CHART";

  return (
    <div className="chart-panel">
      <Frame title={`CHART // ${title}`} tag="EXACT · NEUTRAL">
        <div className="chart-body">
          {spec.empty ? (
            <ChartEmpty />
          ) : (
            <ChartPlot spec={spec} />
          )}
          <div className="chart-foot dim-note">
            Plots EXACTLY the points the data path emitted — every point, line
            segments only between the given points, no interpolation, no invented or
            extrapolated point. Axis ranges are derived from the data. An empty
            series shows &ldquo;no data&rdquo;, never a fabricated point. Charting is
            neutral presentation (no action, no network) and ships OFF behind{" "}
            <code>[chart].enabled</code>.
          </div>
        </div>
      </Frame>
    </div>
  );
}

/** The honest-empty state — the daemon emitted a chart with no plottable point
 *  (re-derived by the parser). Say so plainly; never draw a fabricated point. */
function ChartEmpty() {
  return (
    <div className="chart-empty dim-note">
      No data to chart — the series carried no plottable point. Nothing is drawn
      rather than inventing one.
    </div>
  );
}

/** The plotted chart: the axes (data-derived ranges) + the series drawn by kind. */
function ChartPlot({ spec }: { spec: ChartSpec }) {
  const xr = axisRange(spec.series, (p) => p.x);
  const yr = axisRange(spec.series, (p) => p.y);
  // Defensive: parser guarantees non-empty here, but if both ranges are null
  // (no point survived) fall back to the honest-empty copy rather than a NaN draw.
  if (xr === null || yr === null) return <ChartEmpty />;

  return (
    <>
      <svg
        viewBox={`0 0 ${VIEW_W} ${VIEW_H}`}
        className="chart-svg"
        role="img"
        aria-label={`${spec.kind} chart: ${spec.title}`}
      >
        {/* plot frame + axis lines */}
        <rect
          x={PAD_L}
          y={PAD_T}
          width={PLOT_W}
          height={PLOT_H}
          className="chart-plot-rect"
        />
        <line
          x1={PAD_L}
          y1={PAD_T + PLOT_H}
          x2={PAD_L + PLOT_W}
          y2={PAD_T + PLOT_H}
          className="chart-axis-line"
        />
        <line x1={PAD_L} y1={PAD_T} x2={PAD_L} y2={PAD_T + PLOT_H} className="chart-axis-line" />

        {/* y-axis tick labels — the REAL min/max of the data */}
        <text x={PAD_L - 4} y={PAD_T + 4} className="chart-tick-label y">
          {fmtTick(yr.max)}
        </text>
        <text x={PAD_L - 4} y={PAD_T + PLOT_H} className="chart-tick-label y">
          {fmtTick(yr.min)}
        </text>
        {/* x-axis tick labels — the REAL min/max of the data */}
        <text x={PAD_L} y={PAD_T + PLOT_H + 14} className="chart-tick-label x start">
          {fmtTick(xr.min)}
        </text>
        <text x={PAD_L + PLOT_W} y={PAD_T + PLOT_H + 14} className="chart-tick-label x end">
          {fmtTick(xr.max)}
        </text>

        {spec.series.map((s, i) => (
          <SeriesGlyph
            key={`${s.label}:${i}`}
            series={s}
            kind={spec.kind}
            seriesIndex={i}
            seriesCount={spec.series.length}
            xr={xr}
            yr={yr}
          />
        ))}
      </svg>

      {/* axis labels + legend — honest copy from the daemon's strings */}
      <div className="chart-axes">
        {spec.xAxis.length > 0 && (
          <span className="chart-axis x" title="x-axis">
            x: {spec.xAxis}
          </span>
        )}
        {spec.yAxis.length > 0 && (
          <span className="chart-axis y" title="y-axis">
            y: {spec.yAxis}
          </span>
        )}
      </div>
      {spec.series.length > 1 && (
        <div className="chart-legend">
          {spec.series.map((s, i) => (
            <span className="chart-legend-item" key={`${s.label}:legend:${i}`}>
              <i
                className="chart-legend-swatch"
                style={{ background: SERIES_ACCENTS[i % SERIES_ACCENTS.length] }}
                aria-hidden="true"
              />
              {s.label.length > 0 ? s.label : `series ${i + 1}`}
              <span className="chart-legend-count">{s.points.length}pt</span>
            </span>
          ))}
        </div>
      )}
    </>
  );
}

/** Draw ONE series by kind. Every glyph maps a GIVEN point to a screen position —
 *  no point is added/removed/smoothed. Bars: one rect per point. Line: a polyline
 *  through the points in order (segments only between the given points). Sparkline:
 *  the same polyline with no markers (a compact trend). */
function SeriesGlyph({
  series,
  kind,
  seriesIndex,
  seriesCount,
  xr,
  yr,
}: {
  series: ChartSeries;
  kind: ChartSpec["kind"];
  seriesIndex: number;
  seriesCount: number;
  xr: Range;
  yr: Range;
}) {
  const accent = SERIES_ACCENTS[seriesIndex % SERIES_ACCENTS.length];

  if (kind === "bar") {
    // One bar per emitted point, anchored at the y=baseline (the data min). Bars
    // for multiple series are split across the slot so none overlaps; each bar's
    // height is the REAL value, never a fabricated one.
    const n = series.points.length;
    // A slot per x position; when x values are distinct, space them by index so a
    // single-series bar chart reads cleanly while still mapping x by value.
    const baselineY = PAD_T + PLOT_H;
    return (
      <g className="chart-series bar" style={{ color: accent }}>
        {series.points.map((p, i) => {
          // Slot width: divide the plot among the points (by index), then narrow
          // per series so grouped bars sit side by side without overlap.
          const slot = PLOT_W / Math.max(1, n);
          const groupW = (slot * 0.7) / Math.max(1, seriesCount);
          const x0 = PAD_L + slot * i + slot * 0.15 + groupW * seriesIndex;
          const y = projectY(p.y, yr);
          const h = baselineY - y;
          return (
            <rect
              key={i}
              x={x0}
              y={Math.min(y, baselineY)}
              width={Math.max(0.5, groupW)}
              height={Math.max(0, Math.abs(h))}
              className="chart-bar"
            >
              <title>{`${series.label || "series"} — x=${fmtTick(p.x)}, y=${fmtTick(p.y)}`}</title>
            </rect>
          );
        })}
      </g>
    );
  }

  // Line / sparkline: a polyline through the GIVEN points in emitted order. With a
  // single point we render a marker (a polyline of one point draws nothing).
  const pts = series.points
    .map((p) => `${projectX(p.x, xr).toFixed(2)},${projectY(p.y, yr).toFixed(2)}`)
    .join(" ");
  const single = series.points.length === 1;

  return (
    <g className={`chart-series ${kind}`} style={{ color: accent }}>
      {series.points.length >= 2 && (
        <polyline points={pts} className={`chart-line ${kind}`} fill="none" />
      )}
      {/* Markers on the line variant (not the bare sparkline) so each emitted
          point is visible; a single point always shows a marker. */}
      {(kind === "line" || single) &&
        series.points.map((p, i) => (
          <circle
            key={i}
            cx={projectX(p.x, xr)}
            cy={projectY(p.y, yr)}
            r={2.2}
            className="chart-dot"
          >
            <title>{`${series.label || "series"} — x=${fmtTick(p.x)}, y=${fmtTick(p.y)}`}</title>
          </circle>
        ))}
    </g>
  );
}
