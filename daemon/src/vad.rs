//! VAD backend selection + the PURE speech-debounce decision seam.
//!
//! The capture loop (`audio.rs`) turns a stream of ~30 ms mono frames into
//! utterance segments with a two-stage rule: a per-frame VOICED verdict, then a
//! debounce (a frame must stay voiced for `min_speech_ms` to START a segment, and
//! silent for `silence_ms` to END it). Historically the voiced verdict was a
//! fixed RMS energy gate (`rms > rms_threshold`). A learned VAD (Core ML Silero,
//! `inference/coreml_vad.py`) replaces ONLY that per-frame verdict with a learned
//! speech PROBABILITY (`prob > threshold`); the debounce is unchanged.
//!
//! This module holds the parts of that decision that are pure and headlessly
//! testable — the backend selection ([`VadMode`]), the per-frame verdict
//! ([`prob_is_voiced`]), and the debounce state machine ([`SpeechDebounce`]) — so
//! the "probability -> speech start/end with the debounce windows" logic the
//! learned VAD drives is unit-tested with synthetic probability streams, exactly
//! as the RMS path is exercised live. The live Core ML inference itself is
//! device/dep-gated (like the Core ML embedder's) and runs Python-side.
//!
//! ## Honest availability (armed but inert without its dependency)
//!
//! `[audio].vad` selects the backend: `"rms"` (the shipped default) or
//! `"coreml-silero"`. The Core ML VAD's inference runs as a Core ML model, and
//! the daemon has NO in-process Core ML runtime (its native audio ML is the pure-
//! Rust filterbank in `voiceid.rs`; Core ML lives Python/Swift-side). So
//! selecting `"coreml-silero"` today RESOLVES to the RMS gate with a SURFACED
//! reason (warn + telemetry) — never a silent no-op. This mirrors the project's
//! posture elsewhere ("ON but inert without its dependency"): the learned backend
//! is built, measured (see `inference/benchmarks/vad_eval/`), and selectable; the
//! only missing piece is an in-process Core ML runtime (or a per-frame VAD
//! sidecar) to execute it inside the realtime capture loop.

/// Which per-frame voiced verdict the capture loop uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VadMode {
    /// RMS energy gate: `rms > rms_threshold` (the shipped default).
    Rms,
    /// Learned Core ML Silero VAD: `prob > threshold`.
    CoremlSilero,
}

impl VadMode {
    /// Parse the `[audio].vad` config string. An unknown / empty value is the
    /// SAFE default (`Rms`) — a typo never silently selects a different backend,
    /// and never leaves the capture loop without a verdict source.
    pub fn from_config_str(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "coreml-silero" | "coreml_silero" | "silero" => VadMode::CoremlSilero,
            _ => VadMode::Rms,
        }
    }

    /// A short stable token for status/telemetry.
    pub fn as_str(self) -> &'static str {
        match self {
            VadMode::Rms => "rms",
            VadMode::CoremlSilero => "coreml-silero",
        }
    }
}

/// Default learned-VAD speech-probability threshold. Silero's own recommended
/// operating point; the debounce sits on top of `prob > threshold` exactly as it
/// sat on `rms > rms_threshold`. MUST match `coreml_vad.DEFAULT_THRESHOLD`.
///
/// Scoped to test builds: this is the LEARNED-verdict seam, and the daemon has no
/// live speech-probability source yet (the Core ML backend resolves to RMS — see
/// `resolve_backend`). It is exercised now by the synthetic-probability-stream
/// unit tests; when an in-process Core ML runtime lands it is promoted to drive
/// `audio.rs::Vad::step`'s verdict in place of `rms > threshold`, with the
/// `SpeechDebounce` below unchanged.
#[cfg(test)]
pub const DEFAULT_PROB_THRESHOLD: f32 = 0.5;

/// PURE per-frame verdict for the learned path: is this frame's speech
/// probability above the operating threshold? The learned analogue of the RMS
/// gate's `rms > rms_threshold`. A non-finite probability (NaN/Inf — a degenerate
/// frame) is NOT voiced (fail-safe: a bad frame never opens the mic). Test-scoped
/// for the same reason as `DEFAULT_PROB_THRESHOLD` (no live prob source yet).
#[cfg(test)]
pub fn prob_is_voiced(prob: f32, threshold: f32) -> bool {
    prob.is_finite() && prob > threshold
}

/// The outcome of resolving a requested [`VadMode`] against what can actually run
/// in-process today.
#[derive(Debug, Clone)]
pub struct ResolvedVad {
    /// The backend that will actually drive the capture loop.
    pub active: VadMode,
    /// `Some(reason)` when the requested backend could not be honored and the
    /// capture loop fell back — surfaced to the operator, never silent.
    pub fallback_reason: Option<String>,
}

/// Resolve the requested backend against in-process capability. `coreml-silero`
/// has no in-process Core ML runtime in the daemon, so it resolves to the RMS
/// gate with an HONEST reason (the caller logs it + emits telemetry). `rms`
/// resolves to itself with no fallback.
pub fn resolve_backend(requested: VadMode) -> ResolvedVad {
    match requested {
        VadMode::Rms => ResolvedVad { active: VadMode::Rms, fallback_reason: None },
        VadMode::CoremlSilero => ResolvedVad {
            active: VadMode::Rms,
            fallback_reason: Some(
                "coreml-silero VAD is selected but the daemon has no in-process \
                 Core ML runtime to execute it in the realtime capture loop; \
                 falling back to the RMS energy gate"
                    .to_string(),
            ),
        },
    }
}

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
///
/// This is the headlessly-testable core the live loop's segmentation is built
/// on; `audio.rs` keeps the sample buffering, this decides start/end.
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
/// (`inference/benchmarks/vad_eval/`). Test-scoped until a live prob source
/// exists (see `prob_is_voiced`).
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

    #[test]
    fn mode_parse_is_safe_and_case_insensitive() {
        assert_eq!(VadMode::from_config_str("rms"), VadMode::Rms);
        assert_eq!(VadMode::from_config_str("coreml-silero"), VadMode::CoremlSilero);
        assert_eq!(VadMode::from_config_str("Coreml-Silero"), VadMode::CoremlSilero);
        assert_eq!(VadMode::from_config_str("silero"), VadMode::CoremlSilero);
        // Unknown / empty / typo -> the SAFE default, never a silent third mode.
        assert_eq!(VadMode::from_config_str(""), VadMode::Rms);
        assert_eq!(VadMode::from_config_str("coreml"), VadMode::Rms);
        assert_eq!(VadMode::from_config_str("energy"), VadMode::Rms);
    }

    #[test]
    fn coreml_selection_resolves_to_rms_with_a_surfaced_reason() {
        let r = resolve_backend(VadMode::CoremlSilero);
        assert_eq!(r.active, VadMode::Rms, "no in-process Core ML runtime -> RMS");
        assert!(r.fallback_reason.is_some(), "the fallback must be surfaced, never silent");
        assert!(r.fallback_reason.unwrap().contains("coreml-silero"));

        let r = resolve_backend(VadMode::Rms);
        assert_eq!(r.active, VadMode::Rms);
        assert!(r.fallback_reason.is_none(), "RMS is native; no fallback reason");
    }

    #[test]
    fn default_prob_threshold_matches_the_python_backend() {
        // The learned VAD's operating point MUST match coreml_vad.DEFAULT_THRESHOLD
        // (0.5) — the daemon and the Python backend share it so the config's
        // debounce sits on the same verdict boundary on either side.
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
