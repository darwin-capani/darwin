import { memo, useEffect, useRef } from "react";
import type { CoreState, SystemGauges } from "../core/state";

/**
 * Tactical overlay that wraps the 3D core: concentric range rings + degree
 * ticks, a rotating radar sweep (CSS), two live arc gauges (CPU + memory driven
 * by real telemetry), and four corner readout clusters. Sits ABOVE the opaque
 * WebGL canvas but BELOW the readout panels (z between), pointer-events: none —
 * so it frames the orb without touching the canvas anti-flash invariants or
 * stealing clicks.
 *
 * Honest by construction: every value is the live SystemGauges feed; null
 * (offline / pre-first-sample) renders "——" and an empty arc, never a fake
 * number. It re-renders only when gauges/coreState change (~1Hz), and the arc
 * lengths carry CSS transitions so a new sample eases in (no flash). The radar
 * sweep is a pure CSS animation — no re-render, frozen under reduced-motion.
 */

const R_TICKS = 282;
const CX = 300;
const CY = 300;

// 270°-span gauge: a track + a value arc, both starting at the 7:30 position and
// sweeping clockwise, gap at the bottom. stroke-dasharray on a rotated circle.
function gaugeDasharray(radius: number, frac: number): string {
  const c = 2 * Math.PI * radius;
  const span = 0.75; // 270° of the 360°
  const drawn = c * span * Math.max(0, Math.min(1, frac));
  return `${drawn} ${c}`;
}
const trackDasharray = (radius: number) => {
  const c = 2 * Math.PI * radius;
  return `${c * 0.75} ${c}`;
};

function pct(used: number | null, total: number | null): number | null {
  if (used == null || total == null || total <= 0) return null;
  return Math.max(0, Math.min(1, used / total));
}
function fmtPct(frac: number | null): string {
  return frac == null ? "——" : `${Math.round(frac * 100)}%`;
}
function fmtGB(bytes: number | null): string {
  if (bytes == null) return "——";
  return `${(bytes / 1024 ** 3).toFixed(1)}G`;
}
function fmtUptime(secs: number | null): string {
  if (secs == null) return "——";
  const d = Math.floor(secs / 86400);
  const h = Math.floor((secs % 86400) / 3600);
  const m = Math.floor((secs % 3600) / 60);
  if (d > 0) return `${d}d ${h}h`;
  if (h > 0) return `${h}h ${m}m`;
  return `${m}m`;
}

// Rolling telemetry history for the KPI sparklines (last ~48 samples, ~1/s).
const HIST = 48;
const SPARK_W = 96;
const SPARK_H = 20;
/** Build the line + closed-area path for a 0..1 series across SPARK_W×SPARK_H. */
function sparkPaths(vals: number[]): { line: string; area: string } {
  if (vals.length < 2) return { line: "", area: "" };
  const n = vals.length;
  const pts = vals.map((v, i): [number, number] => [
    (i / (n - 1)) * SPARK_W,
    SPARK_H - Math.max(0, Math.min(1, v)) * SPARK_H,
  ]);
  const line = pts.map((p, i) => `${i ? "L" : "M"} ${p[0].toFixed(1)} ${p[1].toFixed(1)}`).join(" ");
  const area = `${line} L ${SPARK_W} ${SPARK_H} L 0 ${SPARK_H} Z`;
  return { line, area };
}

const STATE_LABEL: Record<CoreState, string> = {
  offline: "OFFLINE",
  idle: "STANDBY",
  listening: "LISTENING",
  processing: "PROCESSING",
  "thinking-local": "COMPUTE · LOCAL",
  "thinking-cloud": "COMPUTE · CLOUD",
  speaking: "RESPONDING",
};

// Degree ticks every 15°, major every 90°.
const TICKS = Array.from({ length: 24 }, (_, i) => {
  const a = (i / 24) * Math.PI * 2 - Math.PI / 2;
  const major = i % 6 === 0;
  const len = major ? 16 : 8;
  return {
    x1: CX + Math.cos(a) * R_TICKS,
    y1: CY + Math.sin(a) * R_TICKS,
    x2: CX + Math.cos(a) * (R_TICKS - len),
    y2: CY + Math.sin(a) * (R_TICKS - len),
    major,
  };
});

function CoreHud({ gauges, coreState }: { gauges: SystemGauges; coreState: CoreState }) {
  const cpu = gauges.cpuPercent == null ? null : Math.max(0, Math.min(1, gauges.cpuPercent / 100));
  const mem = pct(gauges.memUsedBytes, gauges.memTotalBytes);
  const cpuR = 248;
  const memR = 264;

  // Rolling history for the KPI sparklines. Appended in an effect on each new
  // sample (gauges changes ~1/s); the one-sample draw lag is imperceptible, and
  // because it updates only on real data it needs no continuous animation
  // (inherently reduced-motion-friendly).
  const cpuHist = useRef<number[]>([]);
  const memHist = useRef<number[]>([]);
  useEffect(() => {
    cpuHist.current = cpu == null ? [] : [...cpuHist.current, cpu].slice(-HIST);
    memHist.current = mem == null ? [] : [...memHist.current, mem].slice(-HIST);
  }, [cpu, mem]);
  const cpuSpark = sparkPaths(cpuHist.current);
  const memSpark = sparkPaths(memHist.current);

  return (
    <div className="core-hud" aria-hidden="true">
      <div className="core-radar" />
      <svg className="core-hud-svg" viewBox="0 0 600 600" preserveAspectRatio="xMidYMid meet">
        {/* single faint framing ring (the ticks sit on it) — the inner range
            rings were removed to declutter the concentric stack */}
        <g className="ch-rings">
          <circle cx={CX} cy={CY} r={R_TICKS} />
        </g>
        {/* degree ticks */}
        <g className="ch-ticks">
          {TICKS.map((t, i) => (
            <line key={i} x1={t.x1} y1={t.y1} x2={t.x2} y2={t.y2} className={t.major ? "major" : ""} />
          ))}
        </g>
        {/* gauge arcs — rotated so the 270° span opens at the bottom */}
        <g transform={`rotate(135 ${CX} ${CY})`}>
          <circle className="ch-gauge-track" cx={CX} cy={CY} r={cpuR} strokeDasharray={trackDasharray(cpuR)} />
          <circle
            className="ch-gauge-val ch-cpu"
            cx={CX}
            cy={CY}
            r={cpuR}
            strokeDasharray={gaugeDasharray(cpuR, cpu ?? 0)}
          />
          <circle className="ch-gauge-track" cx={CX} cy={CY} r={memR} strokeDasharray={trackDasharray(memR)} />
          <circle
            className="ch-gauge-val ch-mem"
            cx={CX}
            cy={CY}
            r={memR}
            strokeDasharray={gaugeDasharray(memR, mem ?? 0)}
          />
        </g>
      </svg>

      {/* corner readout clusters — real telemetry, honest "——" when absent */}
      <div className="ch-readout tl">
        <span className="ch-k">CPU LOAD</span>
        <span className="ch-v ch-cpu-c">{fmtPct(cpu)}</span>
        {cpuSpark.line && (
          <svg
            className="ch-spark ch-cpu-spark"
            viewBox={`0 0 ${SPARK_W} ${SPARK_H}`}
            preserveAspectRatio="none"
            aria-hidden="true"
          >
            <path className="sp-area" d={cpuSpark.area} />
            <path className="sp-line" d={cpuSpark.line} />
          </svg>
        )}
      </div>
      <div className="ch-readout tr">
        <span className="ch-k">MEMORY</span>
        <span className="ch-v ch-mem-c">{fmtPct(mem)}</span>
        <span className="ch-sub">
          {fmtGB(gauges.memUsedBytes)} / {fmtGB(gauges.memTotalBytes)}
        </span>
        {memSpark.line && (
          <svg
            className="ch-spark ch-mem-spark"
            viewBox={`0 0 ${SPARK_W} ${SPARK_H}`}
            preserveAspectRatio="none"
            aria-hidden="true"
          >
            <path className="sp-area" d={memSpark.area} />
            <path className="sp-line" d={memSpark.line} />
          </svg>
        )}
      </div>
      <div className="ch-readout bl">
        <span className="ch-k">UPTIME</span>
        <span className="ch-v">{fmtUptime(gauges.uptimeSecs)}</span>
        <span className="ch-sub">{fmtGB(gauges.diskFreeBytes)} FREE</span>
      </div>
      <div className="ch-readout br">
        <span className="ch-k">CORE STATE</span>
        <span className="ch-v ch-state">{STATE_LABEL[coreState]}</span>
      </div>
    </div>
  );
}

export default memo(CoreHud);
