import type { AudioIoStatus } from "../core/events";
import {
  interpretLabel,
  interpretDirection,
  interpretTone,
  diarizationLabel,
  diarizationTone,
  diarizationDetail,
} from "../core/events";
import Frame from "./Frame";

/**
 * AUDIO // I/O — the read-only indicator for the three audio-INPUT features, all
 * shipping OFF / neutral and surfaced HONESTLY here. It reads only the secret-free
 * `audioIo` status the reducer folds from daemon telemetry — never the transcript
 * text, a translation, a fabricated speaker, or the captured wav path.
 *
 *   #30 LIVE INTERPRET — source→target language + render-only vs spoken + the
 *       count of REAL translations rendered. ACTIVE only once the DEVICE-GATED mic
 *       loop has fed a segment (`interpret.segment_fed`); a real translation
 *       (`interpret.segment` with translated:true) bumps the count. The copy is
 *       explicit that the continuous always-listening live loop is DEVICE-GATED
 *       (mic) and that an offline/unavailable segment DEGRADES honestly — never a
 *       fabricated translation.
 *   #31 DIARIZATION — speaker labelling is ElevenLabs-Scribe-ONLY. The badge reads
 *       the GROUND-TRUTH `backendCanDiarize` bit: ON-DEVICE: NO DIARIZATION (whisper
 *       has no diarization model — a single honest stream, never a fabricated
 *       speaker), SINGLE STREAM (EL Scribe, one speaker), or MULTI-SPEAKER (EL
 *       Scribe reported >1 distinct speaker — the backend's labels, never invented).
 *   #32 WAKE WORD — the ACTIVE configured wake phrase (default "jarvis"). The copy
 *       is honest that the always-listening loop that consults the matcher is
 *       DEVICE-GATED (mic); the matcher itself is conservative + pure.
 *
 * HONESTY CONTRACT (do not regress):
 *   - DEVICE-GATED. Live interpretation + the always-listening / wake loop run on
 *     the MIC and are device-gated; this panel only reflects what the daemon
 *     reported it did, never claims a live mic capture happened headlessly.
 *   - NEVER FABRICATED. No fabricated speaker, no fabricated translation, no
 *     fabricated wake trigger reaches this surface — diarization honesty is the
 *     backend's own `backend_can_diarize`, a translation count is only the real
 *     translated:true frames, and the wake phrase is the configured one.
 *   - SHIPS OFF / NEUTRAL. All three flags ship OFF; before any telemetry the
 *     surface rests in the honest INTERPRET OFF / NOT SEEN / default-"jarvis"
 *     state and the panel still renders that resting posture (it is informative —
 *     it names the configured wake word and the diarization posture).
 *   - SECRET-FREE. Only languages / booleans / counts / the wake phrase — never
 *     the transcript text, the translation, or the wav path.
 */
export default function AudioIoPanel({
  audio,
}: {
  audio: AudioIoStatus;
}) {
  const i = audio.interpret;
  const d = audio.diarization;
  const w = audio.wake;

  const iTone = interpretTone(i);
  const dTone = diarizationTone(d);

  return (
    <div className="audioio-panel">
      <Frame title="AUDIO // I/O" tag="HONEST · READ ONLY">
        <div className="verify-body">
          {/* #30 LIVE INTERPRET ----------------------------------------- */}
          <div className="verify-row">
            <span className="verify-head">INTERPRET</span>
            <span
              className={`verify-pill audioio-${iTone}`}
              title={
                "Continuous live interpretation (#30). The always-listening live loop " +
                "is DEVICE-GATED (mic) — this only reflects what the daemon reported. " +
                "An offline/unavailable segment DEGRADES honestly; a fabricated " +
                "translation is never produced. Ships OFF ([interpret].live)."
              }
            >
              <span className={`dot ${iTone}`} />
              {interpretLabel(i)}
            </span>
            <span className="verify-meaning">
              {i.active
                ? `${interpretDirection(i)} · ${i.spoke ? "spoken" : "render-only"}`
                : "off — device-gated mic loop not running"}
            </span>
          </div>
          {i.active && (
            <div className="audioio-sub dim-note">
              {i.translations > 0
                ? `${i.translations} real translation${i.translations === 1 ? "" : "s"} rendered`
                : "0 real translations yet (a degrade renders nothing — never a fake translation)"}
            </div>
          )}

          {/* #31 DIARIZATION -------------------------------------------- */}
          <div className="verify-row">
            <span className="verify-head">DIARIZATION</span>
            <span
              className={`verify-pill audioio-${dTone}`}
              title={diarizationDetail(d)}
            >
              <span className={`dot ${dTone}`} />
              {diarizationLabel(d)}
            </span>
            <span className="verify-meaning">
              {!d.seen
                ? "ElevenLabs-Scribe-only ([voice].diarize ships OFF)"
                : d.backendCanDiarize
                  ? `${d.turns} turn${d.turns === 1 ? "" : "s"}`
                  : "single honest stream — no on-device diarization model"}
            </span>
          </div>

          {/* #32 WAKE WORD ---------------------------------------------- */}
          <div className="verify-row">
            <span className="verify-head">WAKE WORD</span>
            <span
              className="verify-pill audioio-good"
              title={
                'The configured wake phrase that gates "is this for JARVIS" (#32). ' +
                'Defaults to "jarvis", preserving today\'s behavior. The matcher is ' +
                "conservative + pure; the always-listening loop that consults it is " +
                "DEVICE-GATED (mic). Ships OFF ([wake].enabled)."
              }
            >
              <span className="dot good" />“{w.phrase}”
            </span>
            <span className="verify-meaning">
              {w.lastDropped
                ? "active — an utterance was dropped for lacking the wake word"
                : "active wake phrase (default preserves today's behavior)"}
            </span>
          </div>

          <div className="verify-foot dim-note">
            All three audio-input features ship <b>OFF / neutral</b>. Live
            interpretation and the always-listening wake loop are{" "}
            <b>DEVICE-GATED (mic)</b> — this panel reflects only what the daemon
            reported, never a fabricated translation or wake trigger. Diarization is{" "}
            <b>ElevenLabs-Scribe-only</b>: on-device whisper has no diarization model
            and reads as a single honest stream — <b>never a fabricated speaker</b>.
            Secret-free — only languages, counts, and the wake phrase; never the
            transcript, the translation, or the audio.
          </div>
        </div>
      </Frame>
    </div>
  );
}
