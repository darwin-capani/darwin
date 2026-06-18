import type { ActionEntry, LearnedFact, SystemGauges } from "../core/state";
import Frame from "./Frame";

function fmtBytes(n: number | null): string {
  if (n === null) return "—";
  if (n >= 1e12) return `${(n / 1e12).toFixed(2)} TB`;
  if (n >= 1e9) return `${(n / 1e9).toFixed(1)} GB`;
  if (n >= 1e6) return `${(n / 1e6).toFixed(0)} MB`;
  return `${n} B`;
}

function fmtUptime(secs: number | null): string {
  if (secs === null) return "—";
  const d = Math.floor(secs / 86400);
  const h = Math.floor((secs % 86400) / 3600);
  const m = Math.floor((secs % 3600) / 60);
  return d > 0 ? `${d}d ${h}h ${m}m` : h > 0 ? `${h}h ${m}m` : `${m}m`;
}

/** Discrete equalizer cells (restyle: segmented block gauges). */
const GAUGE_SEGMENTS = 22;

function Gauge({
  label,
  value,
  pct,
  hot,
}: {
  label: string;
  value: string;
  pct: number | null;
  hot?: boolean;
}) {
  const lit =
    pct === null
      ? 0
      : Math.round((Math.min(100, Math.max(0, pct)) / 100) * GAUGE_SEGMENTS);
  return (
    <div className={`gauge ${hot ? "hot" : ""}`}>
      <div className="row">
        <span>{label}</span>
        <span className="val">{value}</span>
      </div>
      <div className="seg-bar">
        {Array.from({ length: GAUGE_SEGMENTS }, (_, i) => (
          <i key={i} className={i < lit ? "on" : ""} />
        ))}
      </div>
    </div>
  );
}

export default function DiagnosticsPanel({
  gauges,
  facts,
  actions,
}: {
  gauges: SystemGauges;
  facts: LearnedFact[];
  actions: ActionEntry[];
}) {
  const memPct =
    gauges.memUsedBytes !== null && gauges.memTotalBytes
      ? (gauges.memUsedBytes / gauges.memTotalBytes) * 100
      : null;

  return (
    <Frame className="diagnostics" title="SYS // DIAGNOSTICS" tag="system.load · 2s">
      <div className="gauges sub">
        <Gauge
          label="CPU"
          value={gauges.cpuPercent === null ? "—" : `${gauges.cpuPercent.toFixed(1)}%`}
          pct={gauges.cpuPercent}
          hot={gauges.cpuPercent !== null && gauges.cpuPercent > 85}
        />
        <Gauge
          label="MEM"
          value={
            memPct === null
              ? "—"
              : `${fmtBytes(gauges.memUsedBytes)} / ${fmtBytes(gauges.memTotalBytes)}`
          }
          pct={memPct}
          hot={memPct !== null && memPct > 90}
        />
        <Gauge label="DISK FREE" value={fmtBytes(gauges.diskFreeBytes)} pct={null} />
        <Gauge label="UPTIME" value={fmtUptime(gauges.uptimeSecs)} pct={null} />
      </div>
      <div className="sub-title">
        MEMORY // ACTIONS <span className="tag">{facts.length + actions.length}</span>
      </div>
      <div className="tickers sub">
        {facts.length === 0 && actions.length === 0 && (
          <div className="tick-entry v">no learned facts or actions yet</div>
        )}
        {facts.map((f) => (
          <div key={`f${f.seq}`} className="tick-entry">
            <span className="k">◈ {f.key}</span> <span className="v">= {f.value}</span>
          </div>
        ))}
        {actions.map((a) => (
          <div key={`a${a.seq}`} className="tick-entry">
            <span className="a">▸ {a.tool}</span> <span className="v">{a.outcome}</span>
          </div>
        ))}
      </div>
    </Frame>
  );
}
