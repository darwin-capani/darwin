import { useCallback, useEffect, useState } from "react";
import type { AudioIoStatus } from "../core/events";
import {
  interpretLabel,
  interpretDirection,
  interpretTone,
  diarizationLabel,
  diarizationTone,
  diarizationDetail,
} from "../core/events";
import {
  CUE_CATALOG,
  buildPlayCueRequest,
  cueDisabledReason,
  cueGateCopy,
  cueOutcomeCopy,
  type CuePlayOutcome,
} from "../core/sfxCue";
import { inTauri, keychainStatus, playSfxCue } from "../tauri/bridge";
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

          {/* SFX CUES — read-only catalog + honest per-cue test triggers ---- */}
          <SfxCueTrigger />

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

/**
 * SFX CUES (Phase-2) — a small, HONEST way to TEST the built-in sound-effect
 * cues from the HUD. It surfaces the READ-ONLY cue catalog (the daemon's
 * code-level palette) and a per-cue "Play" button that sends the `play_sfx_cue`
 * command through the existing token-injecting command seam.
 *
 * HONESTY CONTRACT (do not regress):
 *   - Cues require the ElevenLabs key + cloud SFX ON. The buttons are DISABLED
 *     (with copy that names exactly what's missing) until the gate is open; with
 *     the switch off / no key / offline, NOTHING plays — never a fabricated cue.
 *   - The control adds NO authority and NO new tier. The daemon makes the real,
 *     gated call (`voice_tier::sfx_enabled`) and returns an honest silent no-op
 *     when closed; this surface only reflects that outcome, never claims a cue
 *     played when it didn't.
 *   - SECRET-FREE. It reads only two booleans (switch on, key present) and sends
 *     the cue NAME — never the key value, never the produced WAV path.
 *
 * The gate inputs are probed ONCE on mount (config `voice.cloud_sfx` + key
 * presence); the pure catalog/gate/outcome logic lives in core/sfxCue.ts and is
 * unit-tested headlessly.
 */
function SfxCueTrigger() {
  const shell = inTauri();
  const [cloudSfxOn, setCloudSfxOn] = useState(false);
  const [keyPresent, setKeyPresent] = useState(false);
  const [playing, setPlaying] = useState<string | null>(null);
  const [note, setNote] = useState("");

  // Probe the secret-free gate inputs once: the cloud-SFX switch (from config)
  // and whether an ElevenLabs key is on file (presence only — never the value).
  // In a plain browser both stay false and the controls render disabled.
  useEffect(() => {
    if (!shell) return;
    let live = true;
    void (async () => {
      try {
        const { configGet } = await import("../tauri/configSettings");
        const settings = await configGet();
        const sfx = settings.find((s) => s.key === "voice.cloud_sfx");
        if (live) setCloudSfxOn(sfx?.value === true);
      } catch {
        if (live) setCloudSfxOn(false);
      }
    })();
    void keychainStatus("elevenlabs_api_key")
      .then((present) => {
        if (live) setKeyPresent(present);
      })
      .catch(() => {
        if (live) setKeyPresent(false);
      });
    return () => {
      live = false;
    };
  }, [shell]);

  const reason = cueDisabledReason(shell, { cloudSfxOn, keyPresent });
  const gateOpen = reason === null;

  const play = useCallback(
    async (name: string) => {
      if (playing) return;
      const req = buildPlayCueRequest(name);
      if (!req) return; // unknown name — never fabricated (defense-in-depth)
      setPlaying(name);
      setNote("");
      try {
        const r = await playSfxCue(req.cue);
        // Map the bounded daemon outcome to honest prose (never claims a play
        // that didn't happen). `detail` is the daemon's own secret-free line; we
        // prefer it, falling back to the contract copy.
        setNote(r.detail || cueOutcomeCopy(r.outcome as CuePlayOutcome, req.cue));
      } catch {
        setNote(cueOutcomeCopy("failed", req.cue));
      } finally {
        setPlaying(null);
      }
    },
    [playing],
  );

  return (
    <div className="audioio-sfx" aria-label="Sound-effect cues">
      <div className="verify-row">
        <span className="verify-head">SFX CUES</span>
        <span
          className={`verify-pill audioio-${gateOpen ? "good" : "idle"}`}
          title={
            "Built-in sound-effect cues you can test from here. They require the " +
            "ElevenLabs key + cloud SFX ([voice].cloud_sfx) ON; with either off (or " +
            "offline) nothing plays — never a fabricated cue. The cue NAME is the " +
            "only thing sent; no key value or audio path crosses this surface."
          }
        >
          <span className={`dot ${gateOpen ? "good" : "idle"}`} />
          {gateOpen ? "READY" : "OFF"}
        </span>
        <span className="verify-meaning">{cueGateCopy(reason)}</span>
      </div>

      <ul className="audioio-cue-list">
        {CUE_CATALOG.map((cue) => (
          <li className="audioio-cue-row" key={cue.name}>
            <button
              type="button"
              className="icon-btn audioio-cue-play"
              onClick={() => void play(cue.name)}
              disabled={!gateOpen || playing !== null}
              title={
                gateOpen
                  ? `Play the “${cue.label}” cue — ${cue.blurb}`
                  : `“${cue.label}” — ${cueGateCopy(reason)}`
              }
            >
              {playing === cue.name ? "Playing…" : "Play"}
            </button>
            <span className="audioio-cue-name">{cue.label}</span>
            <span className="audioio-cue-blurb dim-note">{cue.blurb}</span>
          </li>
        ))}
      </ul>

      <div className="audioio-sub dim-note" role="status">
        {note ||
          "Built-in cues (read-only): " +
            CUE_CATALOG.map((c) => c.name).join(", ") +
            ". They generate once via ElevenLabs, then play from cache. " +
            "Cues require the key + cloud SFX on — else nothing plays."}
      </div>
    </div>
  );
}
