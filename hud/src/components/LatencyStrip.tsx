import { queueWaitNote } from "../core/events";
import type { PipelineTimings } from "../core/state";

/**
 * Top strip: stacked stt/classify/route/speak bar with ms labels and a
 * first-audio marker, from the last pipeline.completed event.
 *
 * QUEUE IS NOT A SEGMENT. `queueMs` is VAD-finish -> event-loop pickup, measured
 * BEFORE the daemon's pipeline clock starts, so it is outside `totalMs` and
 * outside every bar segment (all of which are pickup-relative, as is the
 * first-audio marker). Folding it into the track would shift that marker and
 * make the bar claim a total it does not have. It is rendered as its own stat
 * with the honest end-to-end wall time (queue + total) beside it — the number
 * that explains a turn that felt slow but whose TOTAL looks fine.
 */
export default function LatencyStrip({ timings }: { timings: PipelineTimings | null }) {
  if (!timings) {
    return (
      <section className="latency" aria-label="Pipeline latency">
        <span className="idle-note">PIPELINE LATENCY — AWAITING FIRST UTTERANCE</span>
      </section>
    );
  }

  const segs = [
    { key: "stt", label: "STT", ms: timings.sttMs },
    { key: "classify", label: "CLASSIFY", ms: timings.classifyMs },
    { key: "route", label: "ROUTE", ms: timings.routeMs },
    { key: "speak", label: "SPEAK", ms: timings.speakMs },
  ];
  const sum = Math.max(
    1,
    segs.reduce((acc, s) => acc + s.ms, 0),
  );
  const firstAudioPct =
    timings.firstAudioMs !== null && timings.totalMs > 0
      ? Math.min(100, (timings.firstAudioMs / Math.max(sum, timings.totalMs)) * 100)
      : null;

  const queue = queueWaitNote(timings.queueMs, timings.totalMs);

  return (
    <section className="latency" aria-label="Pipeline latency">
      <span>
        PIPELINE — TOTAL <b className="num">{timings.totalMs} ms</b>
        {timings.firstAudioMs !== null && (
          <>
            {" "}
            · FIRST AUDIO <b className="num">{timings.firstAudioMs} ms</b>
          </>
        )}
        {" · "}
        <span
          className={queue.notable ? "queue-wait slow" : "queue-wait"}
          title={
            `Queued ${timings.queueMs} ms behind an in-flight turn before pickup. ` +
            `Not counted in TOTAL — end to end from VAD finish: ${queue.wallMs} ms.`
          }
        >
          QUEUE <b className="num">{queue.text}</b>
          {queue.notable && (
            <>
              {" "}
              — <b className="num">{queue.wallMs} ms</b> end to end
            </>
          )}
        </span>
      </span>
      <div className="track">
        {segs.map((s) => (
          <div
            key={s.key}
            className={`seg ${s.key}`}
            style={{ width: `${(s.ms / sum) * 100}%` }}
            title={`${s.label} ${s.ms}ms`}
          />
        ))}
        {firstAudioPct !== null && (
          <div className="first-audio" style={{ left: `${firstAudioPct}%` }} title="first audio" />
        )}
      </div>
      <div className="legend">
        {segs.map((s) => (
          <span key={s.key}>
            {s.label} <b>{s.ms}ms</b>
          </span>
        ))}
      </div>
    </section>
  );
}
