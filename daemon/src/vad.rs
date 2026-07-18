//! VAD backend selection + the LIVE learned-VAD path + the PURE speech-debounce
//! decision seam.
//!
//! The capture loop (`audio.rs`) turns a stream of ~30 ms mono frames into
//! utterance segments with a two-stage rule: a per-frame VOICED verdict, then a
//! debounce (a frame must stay voiced for `min_speech_ms` to START a segment, and
//! silent for `silence_ms` to END it). Historically the voiced verdict was a
//! fixed RMS energy gate (`rms > rms_threshold`). The learned VAD (Silero v5,
//! ported to pure Rust in `silero.rs`, weights exported by
//! `inference/coreml_vad.py`) replaces ONLY that per-frame verdict with a learned
//! speech PROBABILITY (`prob > threshold`); the debounce is unchanged. MEASURED
//! adoption evidence (false-accept 0.833 -> 0.0 on loud non-speech, zero
//! false-reject regression) lives in `inference/benchmarks/vad_eval/`.
//!
//! ## The live path ([`LearnedVad`]) — in-process, realtime-safe by construction
//!
//! [`LearnedVad`] runs ON the audio PROCESSING thread (the one draining
//! `raw_rx`), never on the CoreAudio realtime callback (which only ever does
//! `raw_tx.send`): native-rate mono frames are resampled to 16 kHz
//! ([`Resampler16k`]), chunked into Silero's 512-sample windows with the
//! 64-sample look-back context ([`SileroChunker`]), and stepped through the
//! in-process [`crate::silero::SileroModel`] — pure math, sub-millisecond, no
//! IPC. An RPC-per-frame design through the inference server was MEASURED
//! unsafe and rejected: 0.98 ms round-trips idle, but 131 ms median under a
//! concurrent op=embed load — 4x over the ~32 ms frame budget (see `silero.rs`
//! module docs; the numbers are committed in
//! `inference/benchmarks/vad_eval/transport_rtt.json`).
//!
//! ## Honest fallback (armed by default, degraded only when genuinely so)
//!
//! `[audio].vad` ships `"silero"` (the learned VAD measurably wins). The weights
//! file (`state/models/silero_vad_v5_f32.bin`) is exported by the inference
//! server's preload; until it exists — first boot before the server's first
//! preload, or a failed export — the capture loop runs on the RMS gate and SAYS
//! SO (warn + `vad.backend_fallback` telemetry, once per transition), retrying
//! the load every few seconds and announcing `vad.backend_live` when the learned
//! path takes over. `"rms"` is the explicit opt-out. An unknown config value
//! keeps the learned default (a typo never silently disarms the learned VAD —
//! the `sound_monitor` precedent).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::json;
use tracing::{info, warn};

use crate::telemetry;

/// Which per-frame voiced verdict the capture loop uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VadMode {
    /// RMS energy gate: `rms > rms_threshold` (the explicit opt-out).
    Rms,
    /// Learned Silero VAD, in-process (`silero.rs`) — the shipped default.
    Silero,
}

impl VadMode {
    /// Parse the `[audio].vad` config string. `"rms"` is the explicit opt-out;
    /// `"silero"` (and the historical alias `"coreml-silero"`) selects the
    /// learned VAD. Any other value keeps the LEARNED default — a typo'd value
    /// never silently disarms the learned VAD (the `sound_monitor` "a typo'd
    /// opt-out never silently disarms" precedent).
    pub fn from_config_str(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "rms" => VadMode::Rms,
            _ => VadMode::Silero,
        }
    }

}

/// The native weights file the inference server's preload exports (LOCKSTEP with
/// `coreml_vad.NATIVE_WEIGHTS_NAME`), under `<root>/state/models/`.
pub const NATIVE_WEIGHTS_NAME: &str = "silero_vad_v5_f32.bin";

/// Learned-VAD speech-probability threshold. Silero's own recommended operating
/// point; the debounce sits on top of `prob > threshold` exactly as it sat on
/// `rms > rms_threshold`. MUST match `coreml_vad.DEFAULT_THRESHOLD` (0.5) — the
/// committed eval measured at this operating point.
pub const DEFAULT_PROB_THRESHOLD: f32 = 0.5;

/// How long to wait between weights-load attempts while the file is absent or
/// invalid (first boot before the server's first preload export).
const WEIGHTS_RETRY: Duration = Duration::from_secs(5);

/// PURE per-frame verdict for the learned path: is this frame's speech
/// probability above the operating threshold? The learned analogue of the RMS
/// gate's `rms > rms_threshold`. A non-finite probability (NaN/Inf — a
/// degenerate frame) is NOT voiced (fail-safe: a bad frame never opens the mic).
pub fn prob_is_voiced(prob: f32, threshold: f32) -> bool {
    prob.is_finite() && prob > threshold
}

// ===========================================================================
// Resampling + chunking (pure, unit-tested)
// ===========================================================================

/// Stateful LINEAR resampler from an arbitrary input rate to 16 kHz (Silero's
/// only supported rate). Linear interpolation is ample for a VAD front-end (the
/// model is robust to far worse than first-order interpolation error). Output
/// sample k sits at time `k * in_rate / 16000` in input-sample units, computed
/// from the integer output index each time (no accumulated float drift).
/// Passthrough when the input already runs at 16 kHz.
pub struct Resampler16k {
    step: f64,
    passthrough: bool,
    /// Absolute index of the next INPUT sample to arrive.
    next_in: u64,
    /// Absolute index of the next OUTPUT sample to emit.
    next_out: u64,
    /// The input sample at `next_in - 1` (for interpolation across a push
    /// boundary).
    prev: f32,
}

impl Resampler16k {
    pub fn new(in_rate: u32) -> Self {
        Self {
            step: f64::from(in_rate.max(1)) / f64::from(crate::silero::SAMPLE_RATE),
            passthrough: in_rate == crate::silero::SAMPLE_RATE,
            next_in: 0,
            next_out: 0,
            prev: 0.0,
        }
    }

    /// Feed input samples; append the resampled 16 kHz stream to `out`.
    pub fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.passthrough {
            out.extend_from_slice(input);
            return;
        }
        for &s in input {
            let n = self.next_in as f64;
            loop {
                let t = self.next_out as f64 * self.step;
                if t > n {
                    break;
                }
                // t lies in (n-1, n]: interpolate prev..s (t == n -> exactly s;
                // the very first sample has no prev, and t == 0 == n there).
                let y = if self.next_in == 0 || t >= n {
                    s
                } else {
                    let frac = (t - (n - 1.0)) as f32;
                    self.prev + (s - self.prev) * frac
                };
                out.push(y);
                self.next_out += 1;
            }
            self.prev = s;
            self.next_in += 1;
        }
    }

    /// Drop all continuity state (a capture-gate reset — the stream restarts).
    pub fn reset(&mut self) {
        self.next_in = 0;
        self.next_out = 0;
        self.prev = 0.0;
    }
}

/// Accumulates a 16 kHz mono stream into Silero's fixed 512-sample chunks and
/// assembles each model input as `[64-sample context | 512-sample chunk]`,
/// threading the context exactly like Silero's own wrapper (the last 64 samples
/// of each chunk become the next chunk's look-back). Pure, unit-tested.
pub struct SileroChunker {
    buf: Vec<f32>,
    context: [f32; crate::silero::CONTEXT],
}

impl Default for SileroChunker {
    fn default() -> Self {
        Self::new()
    }
}

impl SileroChunker {
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(2 * crate::silero::CHUNK),
            context: [0.0; crate::silero::CONTEXT],
        }
    }

    /// Append resampled samples.
    pub fn push(&mut self, samples: &[f32]) {
        self.buf.extend_from_slice(samples);
    }

    /// If a full 512-sample chunk is buffered, write the 576-sample model input
    /// into `out`, advance the context, consume the chunk, and return true.
    pub fn pop_input(&mut self, out: &mut [f32; crate::silero::MODEL_INPUT]) -> bool {
        const CHUNK: usize = crate::silero::CHUNK;
        const CONTEXT: usize = crate::silero::CONTEXT;
        if self.buf.len() < CHUNK {
            return false;
        }
        out[..CONTEXT].copy_from_slice(&self.context);
        out[CONTEXT..].copy_from_slice(&self.buf[..CHUNK]);
        self.context.copy_from_slice(&self.buf[CHUNK - CONTEXT..CHUNK]);
        self.buf.drain(..CHUNK);
        true
    }

    /// Drop buffered samples + the look-back context (stream restart).
    pub fn reset(&mut self) {
        self.buf.clear();
        self.context = [0.0; crate::silero::CONTEXT];
    }
}

// ===========================================================================
// The LIVE learned-VAD path (in-process model on the processing thread)
// ===========================================================================

/// The live learned-VAD verdict source the capture loop's `Vad` consults per
/// ~30 ms native frame. Owns the resample -> chunk -> model pipeline and the
/// recurrent state; produces `Some(voiced)` while the learned path is live and
/// `None` when it is not (weights not yet exported / unreadable) — the caller
/// then uses the RMS verdict for that frame, and the degradation is SURFACED
/// here (warn + telemetry, once per transition, with a bounded retry).
pub struct LearnedVad {
    weights_path: PathBuf,
    model: Option<crate::silero::SileroModel>,
    resampler: Resampler16k,
    chunker: SileroChunker,
    state: [f32; crate::silero::STATE_LEN],
    xbuf: [f32; crate::silero::MODEL_INPUT],
    buf16k: Vec<f32>,
    last_prob: Option<f32>,
    next_load_attempt: Instant,
    warned_unavailable: bool,
}

impl LearnedVad {
    /// `input_rate` is the native capture rate (the resampler feeds the model's
    /// fixed 16 kHz). Tries the weights immediately; a miss is surfaced and
    /// retried every [`WEIGHTS_RETRY`] from `push_frame`.
    pub fn new(weights_path: PathBuf, input_rate: u32) -> Self {
        let mut this = Self {
            weights_path,
            model: None,
            resampler: Resampler16k::new(input_rate),
            chunker: SileroChunker::new(),
            state: [0.0; crate::silero::STATE_LEN],
            xbuf: [0.0; crate::silero::MODEL_INPUT],
            buf16k: Vec::with_capacity(1024),
            last_prob: None,
            next_load_attempt: Instant::now(),
            warned_unavailable: false,
        };
        this.try_load();
        this
    }

    /// Whether the learned model is currently live (for honest status logs).
    pub fn is_live(&self) -> bool {
        self.model.is_some()
    }

    fn try_load(&mut self) {
        if self.model.is_some() || Instant::now() < self.next_load_attempt {
            return;
        }
        match crate::silero::SileroModel::load(&self.weights_path) {
            Ok(m) => {
                self.model = Some(m);
                self.warned_unavailable = false;
                info!(
                    weights = %self.weights_path.display(),
                    "learned VAD live: in-process Silero (rust port) now decides speech frames"
                );
                telemetry::emit(
                    "audio",
                    "vad.backend_live",
                    json!({"vad": "silero", "weights": self.weights_path.display().to_string()}),
                );
            }
            Err(e) => {
                self.next_load_attempt = Instant::now() + WEIGHTS_RETRY;
                if !self.warned_unavailable {
                    self.warned_unavailable = true;
                    warn!(
                        weights = %self.weights_path.display(),
                        error = %e,
                        "learned VAD weights unavailable; capture runs on the RMS gate until the \
                         inference server's preload exports them (retrying)"
                    );
                    telemetry::emit(
                        "audio",
                        "vad.backend_fallback",
                        json!({
                            "requested": "silero",
                            "active": "rms",
                            "reason": format!("weights unavailable: {e}"),
                        }),
                    );
                }
            }
        }
    }

    /// Feed one native-rate mono frame; returns the learned voiced verdict for
    /// this frame, or `None` when the learned path is not live (caller falls
    /// back to RMS for THIS frame). Runs entirely on the calling (processing)
    /// thread: resample + chunk + in-process model steps, sub-millisecond.
    pub fn push_frame(&mut self, frame: &[f32]) -> Option<bool> {
        self.try_load();
        // Resample into the scratch buffer, then hand it to the chunker.
        // (take/put-back keeps the borrow checker happy without reallocating.)
        let mut samples = std::mem::take(&mut self.buf16k);
        samples.clear();
        self.resampler.push(frame, &mut samples);
        self.chunker.push(&samples);
        self.buf16k = samples;
        // Completed 512-sample chunks step the model (typically 0 or 1 per
        // ~30 ms frame). Chunks are consumed even while the model is absent so
        // the buffers stay bounded.
        while self.chunker.pop_input(&mut self.xbuf) {
            if let Some(m) = self.model.as_mut() {
                let p = m.step(&self.xbuf, &mut self.state);
                if p.is_finite() {
                    self.last_prob = Some(p);
                } else {
                    // Fail-safe: a degenerate output is never voiced, and a
                    // poisoned recurrence is reset rather than propagated.
                    self.last_prob = None;
                    self.state = [0.0; crate::silero::STATE_LEN];
                }
            }
        }
        // Not live -> None (the caller's RMS fallback for this frame).
        self.model.as_ref()?;
        // The verdict holds the latest chunk probability (staleness is bounded
        // structurally: a chunk completes at least every ~32 ms of audio). Before
        // the first chunk of a stream there is no probability yet -> RMS frame.
        self.last_prob
            .map(|p| prob_is_voiced(p, DEFAULT_PROB_THRESHOLD))
    }

    /// Discard all stream state (the capture gate reset — DARWIN speaking /
    /// barge settle / lockdown). The loaded model itself is kept.
    pub fn reset(&mut self) {
        self.resampler.reset();
        self.chunker.reset();
        self.state = [0.0; crate::silero::STATE_LEN];
        self.last_prob = None;
    }
}

// ===========================================================================
// The PURE debounce seam (verdict-source-agnostic)
// ===========================================================================

/// The boundary a [`SpeechDebounce::push`] crossed on the frame just fed. The
/// daemon keeps the buffered run-up separately (`audio.rs`'s `pending`), so the
/// event carries no frame index — it is purely "a segment started / ended on this
/// frame".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebounceEvent {
    /// The voiced run reached `min_speech_frames`: a segment starts on this frame.
    SpeechStart,
    /// The silence run reached `silence_frames`: the segment ends on this frame.
    SpeechEnd,
}

/// PURE speech-onset/offset debounce state machine, frame-quantized to mirror
/// `audio.rs::Vad`'s sample-counted logic: a voiced run of `min_speech_frames`
/// STARTS a segment (onset = where the run began); a silent run of
/// `silence_frames` (while in speech) ENDS it. Verdict-source-agnostic — feed it
/// the RMS gate's `rms > threshold` or the learned VAD's `prob > threshold`; the
/// debounce is identical, which is the whole point of the seam.
#[derive(Debug, Clone)]
pub struct SpeechDebounce {
    min_speech_frames: usize,
    silence_frames: usize,
    in_speech: bool,
    voiced_run: usize,
    silent_run: usize,
}

impl SpeechDebounce {
    /// `min_speech_frames` / `silence_frames` are the debounce windows in frames
    /// (the caller derives them from `min_speech_ms` / `silence_ms` and the frame
    /// duration, e.g. `ceil(ms / frame_ms)`); both are clamped to at least 1.
    pub fn new(min_speech_frames: usize, silence_frames: usize) -> Self {
        Self {
            min_speech_frames: min_speech_frames.max(1),
            silence_frames: silence_frames.max(1),
            in_speech: false,
            voiced_run: 0,
            silent_run: 0,
        }
    }

    /// Feed one frame's voiced verdict; returns a start/end event when the
    /// debounce crosses a boundary, else `None`.
    pub fn push(&mut self, voiced: bool) -> Option<DebounceEvent> {
        if !self.in_speech {
            if voiced {
                self.voiced_run += 1;
                if self.voiced_run >= self.min_speech_frames {
                    self.in_speech = true;
                    self.silent_run = 0;
                    return Some(DebounceEvent::SpeechStart);
                }
            } else {
                self.voiced_run = 0;
            }
            None
        } else if voiced {
            self.silent_run = 0;
            None
        } else {
            self.silent_run += 1;
            if self.silent_run >= self.silence_frames {
                self.in_speech = false;
                self.voiced_run = 0;
                return Some(DebounceEvent::SpeechEnd);
            }
            None
        }
    }

    /// Discard any in-progress state (used while DARWIN speaks — the exact point
    /// `audio.rs::Vad::reset` is called). Clears the run/speech state in place.
    pub fn reset(&mut self) {
        self.in_speech = false;
        self.voiced_run = 0;
        self.silent_run = 0;
    }

    pub fn in_speech(&self) -> bool {
        self.in_speech
    }
}

/// PURE: run a whole probability stream through `prob > threshold` then the
/// debounce, returning `(onset_frame, end_frame_or_none)` for each detected
/// segment. `end` is `None` for a segment still open at the end of the stream.
/// This is the exact "synthetic probability stream -> speech start/end" path the
/// learned VAD drives — the Rust mirror of the committed Python eval
/// (`inference/benchmarks/vad_eval/`). Test-scoped (the live path composes
/// `LearnedVad` + `SpeechDebounce` instead).
#[cfg(test)]
pub fn segments_from_probs(
    probs: &[f32],
    threshold: f32,
    min_speech_frames: usize,
    silence_frames: usize,
) -> Vec<(usize, Option<usize>)> {
    let mut deb = SpeechDebounce::new(min_speech_frames, silence_frames);
    let mut segs: Vec<(usize, Option<usize>)> = Vec::new();
    for (idx, &p) in probs.iter().enumerate() {
        match deb.push(prob_is_voiced(p, threshold)) {
            // `idx` is the DECISION frame (where the run/silence window completed).
            Some(DebounceEvent::SpeechStart) => segs.push((idx, None)),
            Some(DebounceEvent::SpeechEnd) => {
                if let Some(last) = segs.last_mut() {
                    last.1 = Some(idx);
                }
            }
            None => {}
        }
    }
    segs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::silero::{synthetic_weights, CHUNK, CONTEXT, MODEL_INPUT};

    // ---- mode selection ----------------------------------------------------

    #[test]
    fn mode_parse_learned_default_with_rms_opt_out() {
        assert_eq!(VadMode::from_config_str("rms"), VadMode::Rms, "explicit opt-out");
        assert_eq!(VadMode::from_config_str("RMS"), VadMode::Rms);
        assert_eq!(VadMode::from_config_str("silero"), VadMode::Silero);
        assert_eq!(
            VadMode::from_config_str("coreml-silero"),
            VadMode::Silero,
            "historical alias"
        );
        // Unknown / empty keeps the LEARNED default — a typo'd value never
        // silently disarms the learned VAD (sound_monitor precedent).
        assert_eq!(VadMode::from_config_str(""), VadMode::Silero);
        assert_eq!(VadMode::from_config_str("energy"), VadMode::Silero);
    }

    #[test]
    fn default_prob_threshold_matches_the_python_backend() {
        // The learned VAD's operating point MUST match coreml_vad.DEFAULT_THRESHOLD
        // (0.5) — the committed eval measured at this operating point.
        assert_eq!(DEFAULT_PROB_THRESHOLD, 0.5);
        assert!(prob_is_voiced(0.6, DEFAULT_PROB_THRESHOLD));
        assert!(!prob_is_voiced(0.4, DEFAULT_PROB_THRESHOLD));
    }

    #[test]
    fn prob_verdict_threshold_and_fail_safe() {
        // Strictly greater than, mirroring the RMS gate's `>`.
        assert!(!prob_is_voiced(0.5, 0.5));
        assert!(prob_is_voiced(0.5001, 0.5));
        assert!(prob_is_voiced(0.99, 0.5));
        assert!(!prob_is_voiced(0.1, 0.5));
        // Non-finite frame is fail-safe not-voiced (never opens the mic).
        assert!(!prob_is_voiced(f32::NAN, 0.5));
        assert!(!prob_is_voiced(f32::INFINITY, 0.5));
        assert!(!prob_is_voiced(f32::NEG_INFINITY, 0.5));
    }

    // ---- resampler ---------------------------------------------------------

    #[test]
    fn resampler_16k_input_is_passthrough() {
        let mut r = Resampler16k::new(16_000);
        let mut out = Vec::new();
        let input: Vec<f32> = (0..100).map(|i| i as f32).collect();
        r.push(&input, &mut out);
        assert_eq!(out, input, "16 kHz input passes through untouched");
    }

    #[test]
    fn resampler_48k_downsamples_3_to_1_on_grid_points() {
        // 48k -> 16k: output k sits exactly on input sample 3k, so a ramp's
        // outputs are the every-3rd inputs exactly (linear interp at integer
        // positions is exact).
        let mut r = Resampler16k::new(48_000);
        let mut out = Vec::new();
        let input: Vec<f32> = (0..300).map(|i| i as f32).collect();
        r.push(&input, &mut out);
        assert_eq!(out.len(), 100);
        for (k, &y) in out.iter().enumerate() {
            assert_eq!(y, (3 * k) as f32, "output {k} is input {}", 3 * k);
        }
    }

    #[test]
    fn resampler_interpolates_between_samples_and_is_split_invariant() {
        // 44.1k -> 16k: fractional positions, so outputs interpolate. Feeding
        // the same signal in one push vs many small pushes must agree exactly
        // (the continuity state carries across push boundaries).
        let input: Vec<f32> = (0..441).map(|i| (i as f32 * 0.7).sin()).collect();
        let mut one = Vec::new();
        let mut r1 = Resampler16k::new(44_100);
        r1.push(&input, &mut one);
        assert_eq!(one.len(), 160, "441 samples @44.1k -> 160 @16k");

        let mut many = Vec::new();
        let mut r2 = Resampler16k::new(44_100);
        for chunk in input.chunks(7) {
            r2.push(chunk, &mut many);
        }
        assert_eq!(one, many, "split boundaries must not change the output");

        // Spot-check an interpolated value: output 1 sits at t = 44100/16000 =
        // 2.75625 -> between inputs 2 and 3.
        let t = 44_100.0f64 / 16_000.0;
        let frac = (t - 2.0) as f32;
        let expect = input[2] + (input[3] - input[2]) * frac;
        assert!((one[1] - expect).abs() < 1e-6);
    }

    // ---- chunker -----------------------------------------------------------

    #[test]
    fn chunker_yields_at_512_boundaries_with_context_threading() {
        let mut c = SileroChunker::new();
        let mut x = [0.0f32; MODEL_INPUT];
        // 400 samples: not enough.
        c.push(&vec![1.0; 400]);
        assert!(!c.pop_input(&mut x));
        // 200 more (600 total): one chunk pops, 88 remain buffered.
        c.push(&vec![2.0; 200]);
        assert!(c.pop_input(&mut x));
        assert!(x[..CONTEXT].iter().all(|&v| v == 0.0), "first chunk: zero context");
        assert_eq!(x[CONTEXT], 1.0, "chunk body starts with the first samples");
        assert_eq!(x[MODEL_INPUT - 1], 2.0, "chunk body ends with the later samples");
        assert!(!c.pop_input(&mut x), "only 88 buffered");
        // Fill to the next chunk; its context must be the previous chunk's tail.
        c.push(&vec![3.0; 424]);
        assert!(c.pop_input(&mut x));
        assert!(
            x[..CONTEXT].iter().all(|&v| v == 2.0),
            "context = last 64 samples of the previous chunk (which were 2.0)"
        );
    }

    #[test]
    fn chunker_reset_clears_buffer_and_context() {
        let mut c = SileroChunker::new();
        let mut x = [0.0f32; MODEL_INPUT];
        c.push(&vec![5.0; CHUNK]);
        assert!(c.pop_input(&mut x));
        c.push(&vec![6.0; 100]);
        c.reset();
        c.push(&vec![7.0; CHUNK]);
        assert!(c.pop_input(&mut x));
        assert!(x[..CONTEXT].iter().all(|&v| v == 0.0), "reset cleared the context");
        assert!(x[CONTEXT..].iter().all(|&v| v == 7.0), "reset dropped the 6.0 remainder");
    }

    // ---- the LIVE learned path (synthetic weights; headless) ---------------

    fn temp_weights_file(dec_bias: f32, tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "darwin-vad-test-{}-{tag}.bin",
            std::process::id()
        ));
        std::fs::write(&path, synthetic_weights(dec_bias)).unwrap();
        path
    }

    #[test]
    fn learned_vad_produces_the_learned_verdict_not_the_rms_one() {
        // dec_bias +10 -> the model says VOICED for any input, even digital
        // silence (where the RMS gate would say silent): the learned verdict is
        // load-bearing, not decorative. And dec_bias -10 -> NOT voiced even for
        // loud input (the false-accept fix the eval measured).
        for (bias, expect_voiced, tag) in [(10.0f32, true, "hi"), (-10.0, false, "lo")] {
            let path = temp_weights_file(bias, tag);
            let mut lv = LearnedVad::new(path.clone(), 16_000);
            assert!(lv.is_live(), "synthetic weights load immediately");
            // Loud 30 ms frames (RMS 0.5, far above the 0.015 gate) — enough
            // frames that chunks complete.
            let frame = vec![0.5f32; 480];
            let mut verdicts = Vec::new();
            for _ in 0..3 {
                if let Some(v) = lv.push_frame(&frame) {
                    verdicts.push(v);
                }
            }
            assert!(!verdicts.is_empty(), "learned path live -> verdicts flow");
            assert!(
                verdicts.iter().all(|&v| v == expect_voiced),
                "bias {bias}: learned verdict must be {expect_voiced} regardless of loudness"
            );
            std::fs::remove_file(&path).ok();
        }
    }

    #[test]
    fn learned_vad_missing_weights_falls_back_none_then_recovers() {
        let path = std::env::temp_dir().join(format!(
            "darwin-vad-test-{}-recover.bin",
            std::process::id()
        ));
        std::fs::remove_file(&path).ok();
        let mut lv = LearnedVad::new(path.clone(), 16_000);
        assert!(!lv.is_live(), "no weights yet");
        let frame = vec![0.5f32; 480];
        assert_eq!(lv.push_frame(&frame), None, "not live -> None (caller uses RMS)");
        // The weights appear (the server's preload export lands)...
        std::fs::write(&path, synthetic_weights(10.0)).unwrap();
        // ...but the retry timer gates the reload: force it due now.
        lv.next_load_attempt = Instant::now();
        let mut got = None;
        for _ in 0..3 {
            if let Some(v) = lv.push_frame(&frame) {
                got = Some(v);
            }
        }
        assert!(lv.is_live(), "weights picked up on retry");
        assert_eq!(got, Some(true), "learned verdicts flow after recovery");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn learned_vad_reset_clears_stream_state_but_keeps_the_model() {
        let path = temp_weights_file(10.0, "reset");
        let mut lv = LearnedVad::new(path.clone(), 16_000);
        let frame = vec![0.5f32; 480];
        for _ in 0..3 {
            lv.push_frame(&frame);
        }
        assert!(lv.last_prob.is_some(), "a chunk was stepped");
        lv.reset();
        assert!(lv.is_live(), "reset keeps the loaded model");
        assert!(lv.last_prob.is_none(), "reset drops the last verdict");
        assert!(lv.state.iter().all(|&v| v == 0.0), "reset zeroes the recurrence");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn learned_vad_resamples_native_rate_before_chunking() {
        // At 48 kHz a 30 ms frame is 1440 samples -> 480 @16k, so a Silero
        // chunk completes every ~1.07 frames; verdicts must still flow.
        let path = temp_weights_file(10.0, "48k");
        let mut lv = LearnedVad::new(path.clone(), 48_000);
        let frame = vec![0.25f32; 1440];
        let mut n_verdicts = 0;
        for _ in 0..4 {
            if lv.push_frame(&frame).is_some() {
                n_verdicts += 1;
            }
        }
        assert!(n_verdicts >= 3, "verdicts flow at 48 kHz native rate");
        std::fs::remove_file(&path).ok();
    }

    // ---- debounce ----------------------------------------------------------

    #[test]
    fn debounce_requires_min_speech_run_to_start() {
        // min 3 voiced frames to start, 2 silent to end.
        let mut d = SpeechDebounce::new(3, 2);
        // Two voiced frames: not enough yet.
        assert_eq!(d.push(true), None);
        assert_eq!(d.push(true), None);
        assert!(!d.in_speech());
        // Third voiced frame crosses the threshold: the segment starts here.
        assert_eq!(d.push(true), Some(DebounceEvent::SpeechStart));
        assert!(d.in_speech());
    }

    #[test]
    fn debounce_a_short_blip_never_starts_and_resets_the_run() {
        let mut d = SpeechDebounce::new(3, 2);
        assert_eq!(d.push(true), None); // run = 1
        assert_eq!(d.push(true), None); // run = 2
        assert_eq!(d.push(false), None); // blip ends: run resets, no start
        assert!(!d.in_speech());
        // A fresh run must accumulate from scratch — the start fires only after a
        // NEW full run of 3, proving the blip reset the run (else it would have
        // started one frame sooner).
        assert_eq!(d.push(true), None); // run = 1
        assert_eq!(d.push(true), None); // run = 2
        assert_eq!(d.push(true), Some(DebounceEvent::SpeechStart)); // run = 3
    }

    #[test]
    fn debounce_ends_only_after_full_silence_run() {
        let mut d = SpeechDebounce::new(2, 3);
        d.push(true);
        assert_eq!(d.push(true), Some(DebounceEvent::SpeechStart));
        // A single silent frame then voiced again must NOT end the segment.
        assert_eq!(d.push(false), None);
        assert_eq!(d.push(true), None);
        assert!(d.in_speech(), "one dip below is bridged");
        // Now a full silence run of 3 ends it, on the 3rd silent frame.
        assert_eq!(d.push(false), None);
        assert_eq!(d.push(false), None);
        assert_eq!(d.push(false), Some(DebounceEvent::SpeechEnd));
        assert!(!d.in_speech());
    }

    #[test]
    fn segments_from_a_synthetic_probability_stream() {
        // A stream: noise (low prob), a speech burst (high), silence, a 2nd burst.
        // threshold 0.5, min 3 voiced frames, 2 silent to end. Onset/end are the
        // DECISION frames (where the run/silence window completed).
        let probs = [
            0.10, 0.20, 0.05, // noise (0..2)
            0.90, 0.95, 0.99, 0.80, // burst 1 (3..6): 3rd voiced -> start at frame 5
            0.10, 0.05, // 2 silent (7,8) -> end at frame 8
            0.30, // noise (9)
            0.99, 0.99, 0.99, // burst 2 (10..12): start at frame 12
            0.01, 0.01, // (13,14) -> end at frame 14
        ];
        let segs = segments_from_probs(&probs, 0.5, 3, 2);
        assert_eq!(segs, vec![(5, Some(8)), (12, Some(14))]);
    }

    #[test]
    fn a_pure_noise_stream_yields_no_segments() {
        // Loud-but-non-speech would trip an RMS gate; a learned VAD keeps prob low,
        // so the SAME debounce yields nothing. (The low-prob stream stands in for
        // the learned verdict on noise; see inference/benchmarks/vad_eval for the
        // measured false-accept numbers.)
        let probs: Vec<f32> = (0..200).map(|i| 0.05 + 0.1 * ((i % 7) as f32) / 7.0).collect();
        let segs = segments_from_probs(&probs, 0.5, 8, 11);
        assert!(segs.is_empty(), "no frame exceeds threshold -> no false-accept segment");
    }

    #[test]
    fn segment_open_at_stream_end_has_no_end_frame() {
        let probs = [0.9, 0.9, 0.9, 0.9];
        let segs = segments_from_probs(&probs, 0.5, 2, 3);
        // start decision on the 2nd voiced frame (index 1); never ends.
        assert_eq!(segs, vec![(1, None)], "still in speech at end -> end is None");
    }

    #[test]
    fn reset_clears_in_progress_speech() {
        let mut d = SpeechDebounce::new(2, 3);
        d.push(true);
        d.push(true);
        assert!(d.in_speech());
        d.reset();
        assert!(!d.in_speech(), "reset drops the in-progress capture (DARWIN speaking)");
    }
}
