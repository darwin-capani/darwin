use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde_json::json;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::telemetry;
use crate::Event;

const FRAME_MS: u64 = 30;
/// Hard cap on one VAD segment. Continuous above-threshold sound (music, a
/// fan, the TV) must not grow the buffer without bound (~11.5 MB/min at
/// 48kHz) or produce WAVs too long to transcribe inside the request timeout.
const MAX_SEGMENT_SECS: usize = 30;
/// Echo-settle window measured from the moment JARVIS goes QUIET after a barge
/// — i.e. from when `is_speaking()` drops, which itself already trails the last
/// audio by the reply loop's MUTE_TAIL. Room echo + the speaker's draining audio
/// linger a little past that, so the capture gate stays shut for this ADDITIONAL
/// margin before feeding the VAD — JARVIS's own tail can never be segmented into
/// an utterance, transcribed, and re-routed (the echo-feedback / triple-open
/// bug). This is ON TOP of MUTE_TAIL (the clock starts where MUTE_TAIL ends), so
/// the total post-audio cushion is MUTE_TAIL + this. Acoustic length is
/// device-gated; the clock START POINT (quiet-onset, not the barge instant) is
/// the load-bearing part — measuring from the barge instant gave ~zero cushion.
const BARGE_SETTLE_MS: u64 = 350;
/// Minimum spacing between audio.level telemetry events (~15 Hz): plenty for
/// a 60fps HUD waveform that interpolates between samples, while guaranteeing
/// the WS broadcast channel is never flooded by the audio thread.
const LEVEL_INTERVAL: Duration = Duration::from_millis(66);

pub fn spawn_capture(root: PathBuf, cfg: Arc<Config>, tx: UnboundedSender<Event>) {
    // cpal::Stream is !Send, so the stream must live on the thread that owns
    // the capture loop, not in a tokio task.
    std::thread::Builder::new()
        .name("audio-capture".to_string())
        .spawn(move || {
            if let Err(e) = capture_loop(root, cfg, tx) {
                error!(error = %e, "audio capture stopped");
            }
        })
        .expect("spawn audio thread");
}

fn capture_loop(root: PathBuf, cfg: Arc<Config>, tx: UnboundedSender<Event>) -> Result<()> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| anyhow!("no default input device"))?;
    let supported = device
        .default_input_config()
        .context("querying default input config")?;
    let sample_rate = supported.sample_rate().0;
    let channels = supported.channels() as usize;
    let stream_config: cpal::StreamConfig = supported.into();

    // The cpal callback runs on a realtime audio thread and must not block or
    // allocate heavily: ship raw frames over a std channel and do VAD + WAV
    // writing here instead.
    let (raw_tx, raw_rx) = std_mpsc::channel::<Vec<f32>>();
    let stream = device.build_input_stream(
        &stream_config,
        move |data: &[f32], _: &cpal::InputCallbackInfo| {
            let _ = raw_tx.send(data.to_vec());
        },
        |err| warn!(error = %err, "cpal stream error"),
        None,
    )?;
    stream.play().context("starting input stream")?;
    info!(sample_rate, channels, "audio capture running");

    let tmp_dir = root.join("state").join("tmp");
    let mut vad = Vad::new(&cfg, sample_rate);
    let mut barge = BargeDetector::new(&cfg, sample_rate);
    let mut meter = LevelMeter::new();
    // Barge-in tuning aid: rate-limited log of the mic level DURING playback, so
    // barge_in_rms can be set from real data instead of guessed.
    let mut last_barge_log = Instant::now();
    // Echo-safety state machine (RC-1): set the moment a barge fires; capture
    // stays gated shut until JARVIS is no longer speaking AND BARGE_SETTLE_MS
    // has elapsed since this instant, so the gate can never feed JARVIS's own
    // draining audio / room echo into the VAD. None when no barge is pending.
    let mut barge_armed_at: Option<Instant> = None;
    // A barge fired but JARVIS is still speaking: the echo-settle clock
    // (barge_armed_at) is started only once he goes quiet, so the settle is
    // measured from quiet-onset, not the barge instant (which falls during
    // speech and would give ~zero real post-speech cushion).
    let mut barge_pending = false;
    let settle = Duration::from_millis(BARGE_SETTLE_MS);
    // #34 WHISPER AUTO-ENGAGE energy series. A SMALL ring of recent per-chunk mono
    // RMS values over genuine user-capture windows (never JARVIS's echo). The PURE
    // `prosody::apply_auto_engage_global` heuristic reads it behind BOTH
    // [voice].whisper && [voice].whisper_auto; with EITHER off (the shipped default)
    // the call is a no-op and we never even build the series, so capture is
    // byte-for-byte today's. This NEVER opens the mic — it folds over the RMS the
    // loop already computes for capture frames.
    let mut whisper_energies: std::collections::VecDeque<f32> = std::collections::VecDeque::new();
    const WHISPER_ENERGY_WINDOW: usize = 12;
    // PID in the filename keeps names collision-free across daemon restarts
    // (the counter restarts at 0 every launch and would silently overwrite
    // the previous run's WAVs, invalidating transcript wav_path references).
    let pid = std::process::id();

    while let Ok(chunk) = raw_rx.recv() {
        // LOCKDOWN OVERLAY (task #12 — THE mic kill). While the emergency stop is
        // engaged the capture loop IGNORES every frame: drop it, reset the VAD so
        // no partial utterance survives, and never emit/segment/transcribe. This
        // is checked FIRST — before the level meter, the speaking gate, the
        // barge detector, and the VAD — so panic silences the microphone the very
        // next chunk, with no path that lets audio through. The cpal stream stays
        // open (so unlock resumes instantly without re-acquiring the device), but
        // nothing it captures is processed. With lockdown OFF (the shipped
        // default) this branch is never taken and capture is byte-for-byte today.
        if mic_capture_suppressed(crate::lockdown::is_locked_down()) {
            vad.reset();
            continue;
        }
        // Live mic level for the HUD waveform, rate-limited to one event per
        // LEVEL_INTERVAL. Emitted BEFORE the speaking gate on purpose: while
        // JARVIS talks the mic still hears the room (mostly JARVIS itself)
        // and the HUD waveform must stay alive, flagged speaking=true.
        if let Some(rms) = meter.push_frames(&chunk, channels, Instant::now()) {
            telemetry::emit(
                "audio",
                "audio.level",
                json!({"rms": rms, "speaking": crate::speech::is_speaking()}),
            );
        }
        // Capture gate (RC-1 — the echo-safety invariant). The decision is NOT
        // "is a barge requested": that is what let JARVIS hear himself (the
        // gate reopened while is_speaking() was still true through MUTE_TAIL,
        // so his draining audio + echo was segmented, transcribed and
        // re-routed — the triple-open / "glitching" bug). The rule now:
        //
        //   * While JARVIS is speaking: ALWAYS drop + reset. No exception for a
        //     pending barge — BARGE_IN only means "stop synthesizing the rest
        //     of THIS reply", never "start capturing".
        //   * After a barge, once JARVIS has gone quiet: keep dropping until the
        //     echo-settle window has elapsed, THEN arm capture (reset the VAD so
        //     the user's real utterance starts from a clean onset).
        //   * No barge, not speaking: capture normally.
        let speaking = crate::speech::is_speaking();
        if speaking {
            let rms = chunk_rms(&chunk, channels);
            let frames = chunk.len() / channels.max(1);
            // Tuning aid: when the mic rises above the silence floor WHILE
            // JARVIS speaks (his echo, or you talking over him), log the level
            // at most ~2x/sec. Talk over him and read the peak here, then set
            // barge_in_rms to just under it.
            let now = Instant::now();
            if rms > cfg.audio.rms_threshold as f32
                && now.duration_since(last_barge_log) > Duration::from_millis(500)
            {
                last_barge_log = now;
                info!(
                    mic_rms = round4(rms as f64),
                    barge_in_rms = cfg.audio.barge_in_rms,
                    "barge: mic level during playback (set barge_in_rms just under your interruption level)"
                );
            }
            // Track JARVIS's echo level so the detector's threshold stays
            // adaptive (RC-8): every dropped frame feeds the rolling baseline.
            barge.observe_baseline(rms);
            // Only run the detector while no barge is already pending — once one
            // has fired, the reply is already being cut and re-firing is moot.
            if !crate::speech::barge_in_requested() && barge.observe(rms, frames) {
                info!(rms, "barge-in: user spoke over JARVIS; cutting the reply");
                telemetry::emit("audio", "barge_in", json!({"rms": round4(rms as f64)}));
                crate::speech::request_barge_in();
                barge.reset();
                // Mark the barge pending; the echo-settle clock starts only once
                // JARVIS goes quiet (below), so capture resumes AFTER he stops
                // AND the echo tail clears — never on his own draining audio.
                barge_pending = true;
            }
            // Speaking: drop every frame, no matter the barge state. This is the
            // single rule that makes echo-feedback impossible.
            vad.reset();
            continue;
        }

        // Not speaking. If a barge is pending (it fired while JARVIS was still
        // talking), START the echo-settle clock NOW — this first quiet chunk —
        // so the settle is measured from when he actually went quiet, not from
        // the barge instant (which fell during speech and gave ~zero cushion).
        // The pure `gate_decision` then keeps dropping until the settle elapses.
        if barge_pending {
            barge_armed_at = Some(Instant::now());
            barge_pending = false;
        }
        barge.reset();
        let since_barge = barge_armed_at.map(|t| t.elapsed());
        match gate_decision(false, since_barge, settle) {
            Gate::Drop => {
                vad.reset();
                continue;
            }
            Gate::Capture => {
                // First capturable chunk after a barge: clear the pending state
                // and start the user's utterance from a clean VAD onset.
                if barge_armed_at.take().is_some() {
                    vad.reset();
                }
            }
        }

        // #34 WHISPER AUTO-ENGAGE (mic-gated live site; inert by flag). On a genuine
        // user-capture chunk, fold its mono RMS into the bounded energy ring and let
        // the PURE heuristic decide whether SUSTAINED-quiet input should engage
        // whisper. Both gates ([voice].whisper && [voice].whisper_auto) are honoured
        // INSIDE apply_auto_engage_global, so with the shipped defaults (both OFF)
        // this is a no-op and the capture path is byte-for-byte today's. It NEVER
        // opens the mic (the RMS is computed from `chunk`, which the loop already has)
        // and NEVER auto-DISENGAGES (only the explicit command turns whisper off). The
        // PURE is_sustained_quiet heuristic is unit-tested in prosody.rs; this is the
        // headlessly-untestable live call, wired inert-by-flag.
        if cfg.voice.whisper && cfg.voice.whisper_auto {
            let rms = chunk_rms(&chunk, channels);
            if whisper_energies.len() == WHISPER_ENERGY_WINDOW {
                whisper_energies.pop_front();
            }
            whisper_energies.push_back(rms);
            let series: Vec<f32> = whisper_energies.iter().copied().collect();
            crate::prosody::apply_auto_engage_global(&cfg, &series);
        }

        // Downmix interleaved frames to mono by averaging channels.
        for frame in chunk.chunks(channels.max(1)) {
            let mono = frame.iter().copied().sum::<f32>() / frame.len() as f32;
            if let Some(segment) = vad.push(mono) {
                let path = tmp_dir.join(format!("utterance-{pid}-{}.wav", vad.take_counter()));
                // #30 CONTINUOUS LIVE INTERPRETATION (device-gated segment feed; inert by
                // flag). When [interpret].live is on, THIS finished VAD segment enters the
                // continuous-interpret feed: the segment is transcribed and run through the
                // PURE `interpret::interpret_segment` pipeline downstream in run_pipeline
                // (where the InferenceClient + ReplySession live), which translates it and
                // — when [interpret].speak is on — voices it through the SINGLE echo-safe
                // speech path (mic-mute guard + barge-in + the is_speaking() capture gate
                // all cover it; never a parallel audio path). Here at the segment site we
                // only emit the honest device-gated marker so the HUD shows the live
                // interpret feed is active; the mic loop is DEVICE-GATED and the pipeline
                // wiring is behind the SAME flag. With [interpret].live OFF (the shipped
                // default) this is a no-op and the segment path is byte-for-byte today's
                // (the segment is emitted as an ordinary Event::Utterance and routed
                // normally) — exactly the Batch-C auto-engage / vision runSocketServed
                // inert-by-flag precedent.
                if cfg.interpret.live {
                    telemetry::emit(
                        "audio",
                        "interpret.segment_fed",
                        json!({"target": cfg.interpret.target_lang, "speak": cfg.interpret.speak}),
                    );
                }
                match write_wav(&path, sample_rate, &segment) {
                    Ok(()) => {
                        // Voice-id (round G): compute the on-device speaker
                        // embedding from the SAME raw f32 segment (not the lossy
                        // i16 WAV), only when [voice_id].enabled — when off we
                        // skip the work entirely and pass None (unchanged
                        // behavior; the turn handler treats None as not-enforced
                        // unless enabled+enrolled, where it is the fail-closed
                        // unverified case). The embedding is a feature VECTOR,
                        // never raw audio.
                        let embedding = if cfg.voice_id.enabled {
                            crate::voiceid::embed(&segment, sample_rate)
                        } else {
                            None
                        };
                        if tx.send(Event::Utterance { wav: path, embedding }).is_err() {
                            return Ok(()); // main loop gone; shut down
                        }
                    }
                    Err(e) => warn!(error = %e, "failed to write utterance wav"),
                }
            }
        }
    }
    Ok(())
}

fn write_wav(path: &std::path::Path, sample_rate: u32, samples: &[f32]) -> Result<()> {
    // i16 PCM, not f32: the inference server decodes these with Python's
    // stdlib `wave`, which cannot read IEEE-float WAVs.
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)?;
    for &s in samples {
        writer.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)?;
    }
    writer.finalize()?;
    Ok(())
}

/// Energy-based VAD over ~30ms frames. Speech begins once RMS stays above the
/// threshold for min_speech_ms (the run-up is buffered so the segment keeps
/// its onset); it ends after silence_ms below the threshold.
struct Vad {
    threshold: f32,
    frame_len: usize,
    min_speech_samples: usize,
    silence_limit_samples: usize,
    max_segment_samples: usize,
    frame: Vec<f32>,
    in_speech: bool,
    voiced_run: usize,
    silent_run: usize,
    pending: Vec<f32>,
    segment: Vec<f32>,
    counter: u64,
}

impl Vad {
    fn new(cfg: &Config, sample_rate: u32) -> Self {
        let per_ms = sample_rate as usize / 1000;
        Self {
            threshold: cfg.audio.rms_threshold as f32,
            frame_len: (per_ms * FRAME_MS as usize).max(1),
            min_speech_samples: (per_ms * cfg.audio.min_speech_ms as usize).max(1),
            silence_limit_samples: (per_ms * cfg.audio.silence_ms as usize).max(1),
            max_segment_samples: (sample_rate as usize * MAX_SEGMENT_SECS).max(1),
            frame: Vec::new(),
            in_speech: false,
            voiced_run: 0,
            silent_run: 0,
            pending: Vec::new(),
            segment: Vec::new(),
            counter: 0,
        }
    }

    /// Feed one mono sample; returns a finished segment when an utterance ends.
    fn push(&mut self, sample: f32) -> Option<Vec<f32>> {
        self.frame.push(sample);
        if self.frame.len() < self.frame_len {
            return None;
        }
        let rms = (self.frame.iter().map(|s| s * s).sum::<f32>() / self.frame.len() as f32).sqrt();
        let frame = std::mem::take(&mut self.frame);
        self.step(rms, frame)
    }

    fn step(&mut self, rms: f32, frame: Vec<f32>) -> Option<Vec<f32>> {
        if !self.in_speech {
            if rms > self.threshold {
                self.voiced_run += frame.len();
                self.pending.extend_from_slice(&frame);
                if self.voiced_run >= self.min_speech_samples {
                    self.in_speech = true;
                    self.silent_run = 0;
                    self.segment = std::mem::take(&mut self.pending);
                }
            } else {
                self.voiced_run = 0;
                self.pending.clear();
            }
            return None;
        }

        self.segment.extend_from_slice(&frame);
        // Force-emit at the cap: better to transcribe the first 30s than to
        // buffer forever waiting for silence that may be minutes away.
        if self.segment.len() >= self.max_segment_samples {
            warn!(
                samples = self.segment.len(),
                "VAD segment hit the {MAX_SEGMENT_SECS}s cap; force-emitting"
            );
            telemetry::emit(
                "audio",
                "vad.segment_capped",
                json!({"samples": self.segment.len(), "cap_secs": MAX_SEGMENT_SECS}),
            );
            return Some(self.finish_segment());
        }
        if rms > self.threshold {
            self.silent_run = 0;
            return None;
        }
        self.silent_run += frame.len();
        if self.silent_run < self.silence_limit_samples {
            return None;
        }
        Some(self.finish_segment())
    }

    fn finish_segment(&mut self) -> Vec<f32> {
        self.in_speech = false;
        self.voiced_run = 0;
        self.silent_run = 0;
        self.counter += 1;
        std::mem::take(&mut self.segment)
    }

    fn take_counter(&self) -> u64 {
        self.counter
    }

    /// Discard any in-progress capture state (used while JARVIS speaks).
    fn reset(&mut self) {
        self.frame.clear();
        self.pending.clear();
        self.segment.clear();
        self.in_speech = false;
        self.voiced_run = 0;
        self.silent_run = 0;
    }
}

/// The capture gate's verdict for one mic chunk.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Gate {
    /// Discard the chunk (and reset the VAD): JARVIS is speaking, or his echo
    /// has not yet settled after a barge.
    Drop,
    /// Feed the chunk to the VAD: a genuine user-capture window.
    Capture,
}

/// PURE capture-gate decision (RC-1), factored out of the realtime loop so the
/// echo-safety invariant is unit-testable without a mic.
///
/// The ONLY way to reach `Capture` is: JARVIS is NOT speaking, AND either no
/// barge is pending, OR a barge fired and the echo-settle window has fully
/// elapsed since it armed. A barge alone NEVER opens the gate — that was the
/// echo-feedback bug (the gate reopened while is_speaking() was still true
/// through the reply's MUTE_TAIL, so JARVIS's own draining audio + room echo
/// was captured, transcribed, and re-routed, re-running the action).
///
/// `since_barge` is `None` when no barge is pending, else the time elapsed
/// since the barge armed. `settle` is BARGE_SETTLE_MS as a Duration.
fn gate_decision(speaking: bool, since_barge: Option<Duration>, settle: Duration) -> Gate {
    if speaking {
        return Gate::Drop;
    }
    match since_barge {
        Some(elapsed) if elapsed < settle => Gate::Drop,
        _ => Gate::Capture,
    }
}

/// PURE mic-suppression decision for the LOCKDOWN overlay (task #12), factored
/// out of the realtime loop so the "panic silences the mic" invariant is
/// unit-testable without a microphone. `locked` is
/// [`crate::lockdown::is_locked_down`] at the top of the loop.
///
/// Returns `true` when the chunk must be DROPPED (and the VAD reset) because the
/// emergency stop is engaged — checked FIRST in the loop, before every other gate,
/// so no audio is metered, segmented, or transcribed while locked. Returns
/// `false` (capture proceeds normally) when unlocked — the shipped default, so
/// capture is byte-for-byte today.
fn mic_capture_suppressed(locked: bool) -> bool {
    locked
}

/// Mono RMS of one interleaved chunk (channels averaged) — for barge-in
/// detection. One cheap pass, no allocation.
fn chunk_rms(chunk: &[f32], channels: usize) -> f32 {
    let ch = channels.max(1);
    let mut sum = 0.0f64;
    let mut n = 0usize;
    for frame in chunk.chunks(ch) {
        let mono = frame.iter().copied().sum::<f32>() / frame.len() as f32;
        sum += f64::from(mono) * f64::from(mono);
        n += 1;
    }
    if n == 0 {
        0.0
    } else {
        (sum / n as f64).sqrt() as f32
    }
}

/// Watches the mic DURING JARVIS's playback for the user barging in. Fires once
/// the ACCUMULATED loud time (above the effective threshold) reaches
/// `dwell_samples`. Two echo-safety guards (RC-8) keep JARVIS's OWN voice from
/// tripping it, since his echo through the speakers has syllable gaps shorter
/// than the dip-tolerance window and would otherwise slowly integrate to dwell:
///
///   1. ADAPTIVE THRESHOLD. While dropped frames stream by, the detector tracks
///      a rolling baseline of the mic RMS it sees (JARVIS's echo level) and
///      requires barge frames to exceed `baseline + margin`, not just the fixed
///      configured floor — so a louder-than-expected reply cannot creep over a
///      static threshold. The effective threshold is `max(configured, baseline
///      + margin)`.
///   2. CONTIGUOUS ARMING BURST. Dip tolerance only engages AFTER one
///      un-bridged run of `arm_samples` over-threshold audio. Steady-state echo
///      (loud/quiet/loud at syllable cadence) never produces that contiguous
///      burst, so it can never reach the dip-tolerant accumulation phase. A real
///      interruption — a person actually talking over JARVIS — clears the short
///      arming burst easily, after which the original late-fire-fixing dip
///      tolerance applies unchanged.
///
/// `threshold`/dwell/quiet-reset constants stay device-gated; only the state
/// machine around them is changed here.
struct BargeDetector {
    enabled: bool,
    threshold: f32,
    dwell_samples: usize,
    quiet_reset_samples: usize,
    arm_samples: usize,
    /// Rolling baseline of observed echo RMS while JARVIS speaks; barge frames
    /// must clear `baseline + BASELINE_MARGIN` as well as the fixed threshold.
    baseline: f32,
    /// Whether the contiguous arming burst has been seen this run; until then
    /// only contiguous loud counts (no dip tolerance).
    armed: bool,
    /// Contiguous over-threshold run toward the arming burst (reset by any dip).
    arm_run: usize,
    loud_run: usize,
    quiet_run: usize,
}

impl BargeDetector {
    /// Contiguous quiet that counts as "the user stopped" and resets the loud
    /// accumulator. Longer than an inter-word gap, shorter than a real pause.
    const RESET_QUIET_MS: usize = 220;
    /// Contiguous over-threshold audio required to ARM dip-tolerant
    /// accumulation. Long enough that JARVIS's syllable-gapped echo never
    /// sustains it, short enough that a real interruption clears it instantly.
    const ARM_BURST_MS: usize = 120;
    /// How much a barge frame must exceed the rolling echo baseline. Keeps the
    /// detector adaptive when JARVIS is louder than the static threshold.
    const BASELINE_MARGIN: f32 = 0.02;
    /// EMA weight for the echo baseline (per chunk). Small: a slow average so a
    /// single loud transient does not yank the baseline up and self-suppress.
    const BASELINE_ALPHA: f32 = 0.05;

    fn new(cfg: &Config, sample_rate: u32) -> Self {
        let per_ms = sample_rate as usize / 1000;
        Self {
            enabled: cfg.audio.barge_in,
            threshold: cfg.audio.barge_in_rms as f32,
            dwell_samples: (per_ms * cfg.audio.barge_in_ms as usize).max(1),
            quiet_reset_samples: (per_ms * Self::RESET_QUIET_MS).max(1),
            arm_samples: (per_ms * Self::ARM_BURST_MS).max(1),
            baseline: 0.0,
            armed: false,
            arm_run: 0,
            loud_run: 0,
            quiet_run: 0,
        }
    }

    /// Update the rolling echo baseline with one observed RMS. Called for every
    /// dropped frame while JARVIS speaks, so the baseline tracks his echo level.
    fn observe_baseline(&mut self, rms: f32) {
        self.baseline += Self::BASELINE_ALPHA * (rms - self.baseline);
    }

    /// The threshold a frame must clear to count as loud: the larger of the
    /// configured floor and the adaptive `baseline + margin`.
    fn effective_threshold(&self) -> f32 {
        self.threshold.max(self.baseline + Self::BASELINE_MARGIN)
    }

    /// Feed a chunk's mono RMS + its frame count; returns true once the user has
    /// accumulated `dwell_samples` of loud audio. Dip tolerance applies ONLY
    /// after a contiguous arming burst (guard 2); the effective threshold is
    /// adaptive (guard 1). A disabled detector never fires.
    fn observe(&mut self, rms: f32, frames: usize) -> bool {
        if !self.enabled {
            return false;
        }
        let loud = rms > self.effective_threshold();
        if loud {
            // Arming phase: require ONE contiguous over-threshold burst before
            // dip-tolerant accumulation engages, so syllable-gapped echo never
            // integrates to dwell.
            if !self.armed {
                self.arm_run += frames;
                if self.arm_run >= self.arm_samples {
                    self.armed = true;
                    // Seed the dwell accumulator with the arming burst so a long
                    // single shout still fires at the same total dwell.
                    self.loud_run = self.arm_run;
                }
                self.quiet_run = 0;
                return self.armed && self.loud_run >= self.dwell_samples;
            }
            self.loud_run += frames;
            self.quiet_run = 0;
            self.loud_run >= self.dwell_samples
        } else {
            // A dip below threshold. Before arming, ANY dip breaks the
            // contiguous burst (this is what rejects syllable-gapped echo).
            // After arming, only a SUSTAINED quiet resets the run, so genuine
            // inter-word gaps don't keep zeroing it (the late-fire fix).
            if !self.armed {
                self.arm_run = 0;
                return false;
            }
            self.quiet_run += frames;
            if self.quiet_run >= self.quiet_reset_samples {
                self.loud_run = 0;
                self.armed = false;
                self.arm_run = 0;
            }
            false
        }
    }

    fn reset(&mut self) {
        self.loud_run = 0;
        self.quiet_run = 0;
        self.arm_run = 0;
        self.armed = false;
        // Baseline is intentionally NOT reset: it is a property of the room/
        // device echo, persisting across replies for a stable adaptive floor.
    }
}

/// Accumulates mono RMS between rate-limited audio.level emissions. The
/// capture loop feeds every raw chunk through `push_frames`; the meter only
/// surfaces a value once LEVEL_INTERVAL has elapsed since the last one, so
/// the telemetry WS sees at most ~15 events/s no matter the device's chunk
/// cadence. Time is an explicit parameter so the rate limit is unit-testable
/// without sleeping.
struct LevelMeter {
    last_emit: Instant,
    sum_squares: f64,
    samples: usize,
}

impl LevelMeter {
    fn new() -> Self {
        Self {
            last_emit: Instant::now(),
            sum_squares: 0.0,
            samples: 0,
        }
    }

    /// Feed one interleaved chunk (downmixed to mono internally, matching the
    /// VAD's averaging downmix). Returns the RMS over everything accumulated
    /// since the last emission — rounded to 4 decimal places — once at least
    /// LEVEL_INTERVAL has elapsed; None otherwise.
    fn push_frames(&mut self, chunk: &[f32], channels: usize, now: Instant) -> Option<f64> {
        for frame in chunk.chunks(channels.max(1)) {
            let mono = frame.iter().copied().sum::<f32>() / frame.len() as f32;
            self.sum_squares += f64::from(mono) * f64::from(mono);
            self.samples += 1;
        }
        if self.samples == 0 || now.duration_since(self.last_emit) < LEVEL_INTERVAL {
            return None;
        }
        let rms = (self.sum_squares / self.samples as f64).sqrt();
        self.last_emit = now;
        self.sum_squares = 0.0;
        self.samples = 0;
        Some(round4(rms))
    }
}

/// Round to 4 decimal places. Done in f64 so the JSON wire value is the
/// clean shortest representation (an f32 routed through serde_json's f64
/// conversion would serialize as e.g. 0.012299999594688416).
fn round4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

#[cfg(test)]
mod tests {
    use super::{round4, LevelMeter, LEVEL_INTERVAL};
    use std::time::{Duration, Instant};

    fn meter_at(origin: Instant) -> LevelMeter {
        LevelMeter {
            last_emit: origin,
            sum_squares: 0.0,
            samples: 0,
        }
    }

    #[test]
    fn level_meter_rate_limits_to_the_interval() {
        let t0 = Instant::now();
        let mut meter = meter_at(t0);
        let chunk = [0.5f32; 480];
        // Inside the interval: accumulate silently.
        assert_eq!(meter.push_frames(&chunk, 1, t0 + Duration::from_millis(10)), None);
        assert_eq!(meter.push_frames(&chunk, 1, t0 + Duration::from_millis(65)), None);
        // Interval elapsed: the accumulated window is emitted...
        let rms = meter
            .push_frames(&chunk, 1, t0 + Duration::from_millis(66))
            .expect("emit at the interval boundary");
        assert_eq!(rms, 0.5);
        // ...and the clock restarts: the very next chunk is silent again.
        assert_eq!(meter.push_frames(&chunk, 1, t0 + Duration::from_millis(70)), None);
        assert!(meter
            .push_frames(&chunk, 1, t0 + Duration::from_millis(66) + LEVEL_INTERVAL)
            .is_some());
    }

    #[test]
    fn level_meter_rms_covers_the_whole_window_and_resets() {
        let t0 = Instant::now();
        let mut meter = meter_at(t0);
        // Half the window silent, half at 0.8: rms = sqrt(0.8^2 / 2).
        assert_eq!(meter.push_frames(&[0.0f32; 100], 1, t0), None);
        let rms = meter
            .push_frames(&[0.8f32; 100], 1, t0 + LEVEL_INTERVAL)
            .expect("emit");
        let expected = round4((0.8f64 * 0.8 * 100.0 / 200.0).sqrt());
        assert_eq!(rms, expected);
        // Accumulators were reset: a silent follow-up window reads 0.
        let rms = meter
            .push_frames(&[0.0f32; 100], 1, t0 + LEVEL_INTERVAL * 2)
            .expect("emit");
        assert_eq!(rms, 0.0);
    }

    #[test]
    fn level_meter_downmixes_interleaved_channels_by_averaging() {
        let t0 = Instant::now();
        let mut meter = meter_at(t0);
        // Stereo frames [1.0, 0.0] average to mono 0.5 -> rms 0.5.
        let chunk = [1.0f32, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0];
        let rms = meter
            .push_frames(&chunk, 2, t0 + LEVEL_INTERVAL)
            .expect("emit");
        assert_eq!(rms, 0.5);
    }

    #[test]
    fn level_meter_emits_nothing_for_an_empty_window() {
        let t0 = Instant::now();
        let mut meter = meter_at(t0);
        assert_eq!(meter.push_frames(&[], 1, t0 + LEVEL_INTERVAL * 10), None);
    }

    #[test]
    fn level_meter_treats_zero_channels_as_mono() {
        // channels=0 must not panic (chunks(0) would); it clamps to 1.
        let t0 = Instant::now();
        let mut meter = meter_at(t0);
        let rms = meter
            .push_frames(&[0.25f32; 16], 0, t0 + LEVEL_INTERVAL)
            .expect("emit");
        assert_eq!(rms, 0.25);
    }

    #[test]
    fn round4_rounds_to_four_decimal_places() {
        assert_eq!(round4(0.123456), 0.1235);
        assert_eq!(round4(0.00004), 0.0);
        assert_eq!(round4(0.00005), 0.0001);
        assert_eq!(round4(1.0), 1.0);
        assert_eq!(round4(0.0), 0.0);
    }

    #[test]
    fn chunk_rms_downmixes_and_measures() {
        use super::chunk_rms;
        // Mono full-scale -> rms 1.0.
        assert!((chunk_rms(&[1.0, 1.0, 1.0], 1) - 1.0).abs() < 1e-6);
        // Stereo [1,0] frames average to mono 0.5 -> rms 0.5.
        assert!((chunk_rms(&[1.0, 0.0, 1.0, 0.0], 2) - 0.5).abs() < 1e-6);
        // Empty chunk -> 0.0, no divide-by-zero panic.
        assert_eq!(chunk_rms(&[], 1), 0.0);
    }

    /// Build a detector with explicit timing samples for tests, bypassing the
    /// Config-derived `new()`. `arm` is the contiguous arming burst in samples.
    fn detector(enabled: bool, threshold: f32, dwell: usize, quiet_reset: usize, arm: usize) -> super::BargeDetector {
        super::BargeDetector {
            enabled,
            threshold,
            dwell_samples: dwell,
            quiet_reset_samples: quiet_reset,
            arm_samples: arm,
            baseline: 0.0,
            armed: false,
            arm_run: 0,
            loud_run: 0,
            quiet_run: 0,
        }
    }

    #[test]
    fn barge_detector_accumulates_loud_and_tolerates_inter_word_dips() {
        // dwell 1000, quiet-reset 500, threshold 0.06 (above JARVIS's ~0.04
        // echo), arm 200 (a short contiguous burst arms dip tolerance).
        let mut d = detector(true, 0.06, 1000, 500, 200);
        // Echo-level frames (below threshold) never accumulate or arm.
        assert!(!d.observe(0.04, 480));
        assert!(!d.observe(0.05, 480));
        // First contiguous loud burst (>= arm) ARMS dip tolerance and seeds the
        // dwell accumulator; loud_run = 400 < dwell, so no fire yet.
        assert!(!d.observe(0.12, 400)); // armed, loud_run = 400
        assert!(!d.observe(0.03, 300)); // brief dip (300 < 500): run kept (armed)
        assert!(d.observe(0.12, 600)); // loud_run = 1000 -> FIRES
        // A SUSTAINED quiet (>= quiet-reset) means the user stopped: reset +
        // disarm. The next loud run must re-arm before it can fire.
        d.reset();
        assert!(!d.observe(0.12, 400)); // armed, loud_run = 400
        assert!(!d.observe(0.02, 500)); // sustained quiet -> reset + disarm
        assert!(!d.observe(0.12, 600)); // re-armed, loud_run = 600 < 1000
        // Disabled never fires.
        let mut off = detector(false, 0.0, 1, 1, 1);
        assert!(!off.observe(1.0, 1_000_000));
    }

    /// RC-8: JARVIS's own echo at syllable cadence (loud/quiet/loud, each gap
    /// shorter than the dip-reset but the loud runs shorter than the arming
    /// burst) must NEVER arm, so it can never integrate to a false barge — the
    /// mechanism that fired the detector on JARVIS himself and caused the
    /// echo-feedback triple-open. A sustained run DOES fire.
    #[test]
    fn barge_detector_rejects_syllable_cadence_echo_but_fires_on_a_sustained_run() {
        // arm 200: needs a 200-sample CONTIGUOUS loud burst to engage dip
        // tolerance. dwell 1000, quiet-reset 500, threshold 0.06.
        let mut d = detector(true, 0.06, 1000, 500, 200);
        // Syllable cadence: 100-sample loud bursts (each < arm) split by 150ms
        // dips. Each dip breaks the (unarmed) contiguous run, so it never arms
        // and never accumulates — no matter how many cycles.
        for _ in 0..50 {
            assert!(!d.observe(0.12, 100), "loud burst < arm must not fire");
            assert!(!d.observe(0.01, 150), "dip resets the unarmed burst");
        }
        assert!(!d.armed, "syllable-cadence echo must never arm the detector");

        // A genuine sustained interruption: one long contiguous loud run arms
        // (>= 200) and reaches dwell (>= 1000), firing as it should.
        let mut g = detector(true, 0.06, 1000, 500, 200);
        assert!(!g.observe(0.2, 300)); // arms (300 >= 200), loud_run = 300
        assert!(g.observe(0.2, 700)); // loud_run = 1000 -> FIRES
    }

    /// RC-8 guard 1: the threshold is adaptive — a frame must clear both the
    /// fixed floor and `baseline + margin`. A loud reply that drives the
    /// baseline up must not let same-level echo trip the detector.
    #[test]
    fn barge_detector_threshold_adapts_to_the_echo_baseline() {
        let mut d = detector(true, 0.06, 1000, 500, 200);
        // Drive the rolling baseline up to ~0.15 (a loud reply through the
        // speakers) by feeding the echo level repeatedly.
        for _ in 0..200 {
            d.observe_baseline(0.15);
        }
        // effective_threshold is now max(0.06, baseline + 0.02) > 0.16, so a
        // 0.15 echo frame is BELOW it and cannot accumulate.
        assert!(d.effective_threshold() > 0.15, "baseline must raise the floor");
        assert!(!d.observe(0.15, 1_000_000), "echo at the baseline must not fire");
        // A frame well above the adaptive floor still works normally.
        assert!(d.observe(0.30, 1000), "a frame above the adaptive floor fires");
    }
}

#[cfg(test)]
mod gate_tests {
    use super::{gate_decision, mic_capture_suppressed, Gate};
    use std::time::Duration;

    /// RC-1, the echo-safety invariant. The capture gate Drops every frame
    /// while JARVIS speaks AND through the echo-settle window after a barge;
    /// only a clean non-speaking, settled window Captures. A barge alone never
    /// opens the gate.
    #[test]
    fn gate_drops_while_speaking_and_through_the_settle_window() {
        let settle = Duration::from_millis(300);

        // Speaking: ALWAYS Drop, regardless of barge state. This is what makes
        // JARVIS's own draining audio impossible to re-capture.
        assert_eq!(gate_decision(true, None, settle), Gate::Drop);
        assert_eq!(gate_decision(true, Some(Duration::ZERO), settle), Gate::Drop);
        assert_eq!(
            gate_decision(true, Some(Duration::from_secs(10)), settle),
            Gate::Drop,
            "even long after a barge, a speaking JARVIS is never captured"
        );

        // Not speaking, no barge pending: a normal capture window.
        assert_eq!(gate_decision(false, None, settle), Gate::Capture);

        // Not speaking, barge pending, INSIDE the settle window: still Drop —
        // this is exactly the residual-echo span that re-ran the action.
        assert_eq!(
            gate_decision(false, Some(Duration::from_millis(0)), settle),
            Gate::Drop
        );
        assert_eq!(
            gate_decision(false, Some(Duration::from_millis(299)), settle),
            Gate::Drop
        );

        // Not speaking, barge pending, settle window ELAPSED: now a genuine
        // user-capture window.
        assert_eq!(
            gate_decision(false, Some(Duration::from_millis(300)), settle),
            Gate::Capture
        );
        assert_eq!(
            gate_decision(false, Some(Duration::from_millis(900)), settle),
            Gate::Capture
        );
    }

    /// LOCKDOWN overlay (task #12): the mic-suppression decision drops every
    /// chunk while locked and captures normally when unlocked — the pure half of
    /// the capture loop's "panic silences the mic" check.
    #[test]
    fn mic_suppressed_only_while_locked() {
        assert!(mic_capture_suppressed(true), "locked => drop the chunk (mic silenced)");
        assert!(
            !mic_capture_suppressed(false),
            "unlocked (shipped default) => capture proceeds byte-for-byte today"
        );
    }
}
