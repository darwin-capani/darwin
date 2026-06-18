//! Gapless TTS playback on a dedicated rodio thread.
//!
//! Design note (per the shared contract): rodio's `OutputStream` is `!Send`,
//! so it can live neither in a tokio task nor directly in a `static`. The
//! `OnceLock` instead holds the sender of a command channel; a dedicated
//! playback thread spawned on first use owns one lazily-created, persistent
//! `OutputStream` for the daemon's life (opening the CoreAudio device is the
//! expensive part — reopening per reply would reintroduce startup gaps).
//!
//! Per spoken reply the thread keeps one `Sink`; clips arrive as full WAV
//! bytes and are appended via `rodio::Decoder` over a `Cursor`, so sentences
//! play back-to-back with no process spawns and no gaps. Every command is
//! acknowledged over a oneshot so the async side never blocks the runtime
//! and can fall back to afplay on any failure.

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use rodio::source::Zero;
use rodio::{Decoder, OutputStream, OutputStreamHandle, Sink};
use tokio::sync::{mpsc, oneshot};
use tracing::warn;

/// Sample rate for generated inter-clip silence. Arbitrary (rodio resamples
/// every source to the device rate); 24 kHz matches the Kokoro WAVs.
const SILENCE_SAMPLE_RATE: u32 = 24_000;

/// Waiting for an append ack: decode+append is instant, but the very first
/// append may have to open the CoreAudio output device.
const APPEND_TIMEOUT: Duration = Duration::from_secs(10);
/// Drain bound margin on top of the total appended audio duration.
const DRAIN_MARGIN: Duration = Duration::from_secs(10);
/// Bound contribution for a clip whose WAV header would not parse.
const UNKNOWN_CLIP_BOUND: Duration = Duration::from_secs(30);

/// Stale-command defense (audit fix): the command queue is FIFO, so when the
/// playback thread wedges (e.g. inside a CoreAudio device open), a Session
/// that times out and falls back to afplay leaves its queued Append/Silence
/// behind — on unwedge those stale clips would play over (or after) the
/// afplay rendition. Every audio command therefore carries the generation of
/// the Session that sent it; a Session that deactivates marks its generation
/// dead via DISCARD_BELOW, which the thread reads OUT of band (an atomic,
/// not a queued message — a queued bump would arrive behind the very
/// commands it is meant to cancel).
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);
static DISCARD_BELOW: AtomicU64 = AtomicU64::new(0);
/// The generation of the reply that is actually SOUNDING — set when a session's
/// first clip reaches the sink (`mark_active`). cancel_all() bounds its
/// stale-discard on THIS, not on NEXT_GENERATION (RC-7): NEXT_GENERATION is the
/// value the next Session::new() will claim, so reading it could mark a
/// freshly-created post-barge reply (a higher generation that has not started)
/// stale, dropping all its audio — the barge would cut the old reply but the
/// NEW reply would then play silent. Bounding on the sounding generation can
/// never reach a later, not-yet-started session.
static ACTIVE_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Whether a queued command from `generation` belongs to a deactivated
/// Session and must be dropped instead of played.
fn is_stale(generation: u64) -> bool {
    generation < DISCARD_BELOW.load(Ordering::Relaxed)
}

enum PlayCmd {
    /// Append one whole WAV (bytes) to the current reply's sink; the ack
    /// reports whether decode+append succeeded.
    Append {
        generation: u64,
        bytes: Vec<u8>,
        ack: oneshot::Sender<bool>,
    },
    /// Append pure silence to the current reply's sink (sentence pacing) —
    /// generated in memory via a rodio Zero source, no files involved.
    Silence {
        generation: u64,
        duration: Duration,
        ack: oneshot::Sender<bool>,
    },
    /// Wait (up to `bound`) for the current sink to drain, then drop it.
    Finish {
        bound: Duration,
        ack: oneshot::Sender<()>,
    },
    /// Barge-in: STOP the currently-sounding sink immediately (no drain) and drop
    /// it. Queued clips from sessions created so far are already marked stale via
    /// DISCARD_BELOW (see [`cancel_all`]), so they are discarded, not played.
    Stop,
}

static PLAYBACK: OnceLock<Option<mpsc::UnboundedSender<PlayCmd>>> = OnceLock::new();

fn sender() -> Option<&'static mpsc::UnboundedSender<PlayCmd>> {
    PLAYBACK
        .get_or_init(|| {
            let (tx, rx) = mpsc::unbounded_channel();
            match std::thread::Builder::new()
                .name("rodio-playback".to_string())
                .spawn(move || run(rx))
            {
                Ok(_) => Some(tx),
                Err(e) => {
                    warn!(error = %e, "failed to spawn the rodio playback thread");
                    None
                }
            }
        })
        .as_ref()
}

/// Barge-in: cancel ALL in-flight playback NOW. Marks every command queued by
/// any session created so far as stale (DISCARD_BELOW), so the playback thread
/// drops them instead of playing, then stops whatever is currently sounding. The
/// NEXT Session (a higher generation) is unaffected, so the reply after the
/// interruption plays normally. Callable from any thread — the audio capture
/// thread invokes it the instant it detects the user talking over JARVIS.
pub fn cancel_all() {
    // RC-7: discard everything up to and INCLUDING the generation that is
    // actually sounding, never up to NEXT_GENERATION. A post-barge reply whose
    // Session::new() already claimed a higher generation (so NEXT_GENERATION
    // advanced past it) is therefore never marked stale — its audio plays.
    let active = ACTIVE_GENERATION.load(Ordering::Relaxed);
    DISCARD_BELOW.fetch_max(active + 1, Ordering::Relaxed);
    if let Some(tx) = sender() {
        let _ = tx.send(PlayCmd::Stop);
    }
}

/// The playback thread: owns the !Send OutputStream and the per-reply Sink.
fn run(mut rx: mpsc::UnboundedReceiver<PlayCmd>) {
    let mut output: Option<(OutputStream, OutputStreamHandle)> = None;
    let mut sink: Option<Sink> = None;
    // The channel sender lives in a static, so this loop runs until exit.
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            PlayCmd::Append { generation, bytes, ack } => {
                if is_stale(generation) {
                    warn!(generation, "discarding stale queued clip from an abandoned reply");
                    let _ = ack.send(false);
                    continue;
                }
                let ok = append_clip(&mut output, &mut sink, bytes);
                let _ = ack.send(ok);
            }
            PlayCmd::Silence { generation, duration, ack } => {
                if is_stale(generation) {
                    let _ = ack.send(false);
                    continue;
                }
                let ok = append_silence(&mut output, &mut sink, duration);
                let _ = ack.send(ok);
            }
            PlayCmd::Finish { bound, ack } => {
                if let Some(s) = sink.take() {
                    // Poll instead of sleep_until_end: a wedged CoreAudio
                    // device must not pin this thread (and the caller's
                    // SPEAKING mute) forever.
                    let deadline = Instant::now() + bound;
                    while !s.empty() && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    if !s.empty() {
                        warn!(bound_s = bound.as_secs(), "rodio sink still playing at its bound; stopping it");
                        s.stop();
                    }
                }
                let _ = ack.send(());
            }
            PlayCmd::Stop => {
                // Barge-in: cut the current reply off mid-clip. No drain, no ack.
                if let Some(s) = sink.take() {
                    s.stop();
                }
            }
        }
    }
}

/// Lazily open the output device and the per-reply sink. Returns None (after
/// logging) when the audio device is unavailable.
fn ensure_sink<'a>(
    output: &mut Option<(OutputStream, OutputStreamHandle)>,
    sink: &'a mut Option<Sink>,
) -> Option<&'a Sink> {
    if output.is_none() {
        match OutputStream::try_default() {
            Ok(pair) => *output = Some(pair),
            Err(e) => {
                warn!(error = %e, "rodio: no default output device");
                return None;
            }
        }
    }
    if sink.is_none() {
        let handle = &output.as_ref().expect("output set above").1;
        match Sink::try_new(handle) {
            Ok(s) => *sink = Some(s),
            Err(e) => {
                warn!(error = %e, "rodio: failed to create sink");
                // The stream may be dead (device unplugged/changed); drop it
                // so the next append rebuilds from scratch.
                *output = None;
                return None;
            }
        }
    }
    sink.as_ref()
}

fn append_clip(
    output: &mut Option<(OutputStream, OutputStreamHandle)>,
    sink: &mut Option<Sink>,
    bytes: Vec<u8>,
) -> bool {
    let Some(sink) = ensure_sink(output, sink) else {
        return false;
    };
    match Decoder::new(Cursor::new(bytes)) {
        Ok(source) => {
            sink.append(source);
            true
        }
        Err(e) => {
            warn!(error = %e, "rodio: failed to decode TTS wav");
            false
        }
    }
}

fn append_silence(
    output: &mut Option<(OutputStream, OutputStreamHandle)>,
    sink: &mut Option<Sink>,
    duration: Duration,
) -> bool {
    let Some(sink) = ensure_sink(output, sink) else {
        return false;
    };
    sink.append(Zero::<f32>::new_samples(
        1,
        SILENCE_SAMPLE_RATE,
        silence_samples(duration),
    ));
    true
}

/// Mono sample count for a stretch of generated silence.
fn silence_samples(duration: Duration) -> usize {
    (duration.as_secs_f64() * SILENCE_SAMPLE_RATE as f64).round() as usize
}

/// Audio duration parsed from in-memory WAV bytes, for sizing drain bounds
/// (and, in speech.rs, the opener's timed mic-mute release).
pub(crate) fn wav_duration(bytes: &[u8]) -> Option<Duration> {
    let reader = hound::WavReader::new(Cursor::new(bytes)).ok()?;
    let spec = reader.spec();
    if spec.sample_rate == 0 {
        return None;
    }
    Some(Duration::from_secs_f64(
        reader.duration() as f64 / spec.sample_rate as f64,
    ))
}

/// One spoken reply's view of the playback thread: append clips, then wait
/// for the sink to drain. Create a fresh Session per reply; any rodio
/// failure deactivates it so the caller can fall back to afplay.
#[derive(Debug)]
pub struct Session {
    /// Tags every command this session sends; marked dead on deactivation
    /// so the thread discards whatever this session left in the queue.
    generation: u64,
    /// False after any rodio failure — later appends short-circuit.
    active: bool,
    /// Whether any command reached the thread (a Finish is then owed, even
    /// after failures, so a stale sink never leaks into the next reply).
    sent_any: bool,
    /// Total audio appended, the basis of the drain bound.
    appended: Duration,
    first_append: Option<Instant>,
}

impl Session {
    pub fn new() -> Self {
        Self {
            generation: NEXT_GENERATION.fetch_add(1, Ordering::Relaxed),
            active: true,
            sent_any: false,
            appended: Duration::ZERO,
            first_append: None,
        }
    }

    /// Rodio failure: the session goes inert AND its generation is marked
    /// dead, so any Append/Silence it already queued (e.g. one that timed
    /// out against a wedged thread) is discarded when finally dequeued
    /// instead of playing over the afplay fallback.
    fn deactivate(&mut self) {
        self.active = false;
        DISCARD_BELOW.fetch_max(self.generation + 1, Ordering::Relaxed);
    }

    /// When the first clip was handed to the sink (≈ audio start, since the
    /// sink plays immediately).
    pub fn first_append(&self) -> Option<Instant> {
        self.first_append
    }

    /// Whether any clip has reached the sink this reply (silence excluded).
    /// Drives sentence pacing: a pause is inserted only between clips.
    pub fn has_audio(&self) -> bool {
        self.first_append.is_some()
    }

    /// Append one whole WAV to the gapless sink. Returns false on any rodio
    /// failure, after which the session is inert and the caller should use
    /// the afplay fallback for the rest of the reply.
    pub async fn append(&mut self, bytes: Vec<u8>) -> bool {
        if !self.active {
            return false;
        }
        let Some(tx) = sender() else {
            self.deactivate();
            return false;
        };
        let clip = wav_duration(&bytes).unwrap_or(UNKNOWN_CLIP_BOUND);
        let (ack_tx, ack_rx) = oneshot::channel();
        let candidate = Instant::now();
        self.sent_any = true;
        let cmd = PlayCmd::Append {
            generation: self.generation,
            bytes,
            ack: ack_tx,
        };
        if tx.send(cmd).is_err() {
            self.deactivate();
            return false;
        }
        match tokio::time::timeout(APPEND_TIMEOUT, ack_rx).await {
            Ok(Ok(true)) => {
                if self.first_append.is_none() {
                    self.first_append = Some(candidate);
                    // This reply is now the SOUNDING one: a concurrent barge
                    // bounds its discard on this generation (RC-7), so it never
                    // reaches a later, not-yet-started reply. Monotonic — a
                    // stale late append from an older session can't lower it.
                    ACTIVE_GENERATION.fetch_max(self.generation, Ordering::Relaxed);
                }
                self.appended += clip;
                true
            }
            _ => {
                // Decode/device failure, dropped ack, or a wedged thread.
                self.deactivate();
                false
            }
        }
    }

    /// Append pure silence between clips (sentence pacing). Failures are
    /// soft: the session is deactivated like any rodio failure and the next
    /// clip append reports it, so pacing never breaks a reply on its own.
    pub async fn append_silence(&mut self, duration: Duration) -> bool {
        if !self.active || duration.is_zero() {
            return self.active;
        }
        let Some(tx) = sender() else {
            self.deactivate();
            return false;
        };
        let (ack_tx, ack_rx) = oneshot::channel();
        self.sent_any = true;
        let cmd = PlayCmd::Silence {
            generation: self.generation,
            duration,
            ack: ack_tx,
        };
        if tx.send(cmd).is_err() {
            self.deactivate();
            return false;
        }
        match tokio::time::timeout(APPEND_TIMEOUT, ack_rx).await {
            Ok(Ok(true)) => {
                self.appended += duration;
                true
            }
            _ => {
                self.deactivate();
                false
            }
        }
    }

    /// Wait until everything appended has played (sink empty), bounded by
    /// total appended duration + margin. Always called — also after
    /// failures — so the thread's per-reply sink is dropped.
    pub async fn finish(&mut self) {
        if !self.sent_any {
            return;
        }
        let Some(tx) = sender() else { return };
        let bound = self.appended + DRAIN_MARGIN;
        let (ack_tx, ack_rx) = oneshot::channel();
        if tx.send(PlayCmd::Finish { bound, ack: ack_tx }).is_err() {
            return;
        }
        // The thread enforces `bound` itself; the extra margin here is only
        // a backstop against the thread being wedged inside CoreAudio.
        if tokio::time::timeout(bound + Duration::from_secs(5), ack_rx)
            .await
            .is_err()
        {
            warn!("playback thread did not acknowledge drain in time");
        }
        self.sent_any = false;
        self.appended = Duration::ZERO;
    }
}

#[cfg(test)]
mod tests {
    use super::wav_duration;
    use std::io::Cursor;
    use std::time::Duration;

    /// Header math only — no audio device is opened in tests.
    #[test]
    fn wav_duration_reads_in_memory_header() {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut writer = hound::WavWriter::new(&mut cursor, spec).unwrap();
            for _ in 0..16_000 {
                writer.write_sample(0i16).unwrap();
            }
            writer.finalize().unwrap();
        }
        let dur = wav_duration(cursor.get_ref()).unwrap();
        assert_eq!(dur, Duration::from_secs(1));
    }

    #[test]
    fn wav_duration_rejects_garbage() {
        assert!(wav_duration(b"not a wav at all").is_none());
    }

    /// Generation / stale-discard machinery, in ONE test so the shared globals
    /// (NEXT_GENERATION, DISCARD_BELOW, ACTIVE_GENERATION) are never mutated by
    /// two tests at once on the parallel runner — `is_stale` reads DISCARD_BELOW,
    /// which a concurrent deactivate/cancel_all would push out from under an
    /// assertion. No audio device or thread is involved (these calls only touch
    /// atomics; no command is sent). Covers both the original deactivation
    /// audit fix AND the RC-7 cancel_all generation race.
    #[test]
    fn generation_stale_discard_machinery() {
        use super::{ACTIVE_GENERATION, NEXT_GENERATION};
        use std::sync::atomic::Ordering;

        // --- Audit fix: deactivate marks only older generations stale. ---
        let mut abandoned = super::Session::new();
        let survivor = super::Session::new();
        assert!(!super::is_stale(abandoned.generation));
        abandoned.deactivate();
        assert!(super::is_stale(abandoned.generation));
        assert!(!super::is_stale(survivor.generation));
        // A session created after the deactivation is unaffected too.
        assert!(!super::is_stale(super::Session::new().generation));

        // --- RC-7: cancel_all() bounds its discard on the SOUNDING generation,
        // never NEXT_GENERATION — so a post-barge reply whose Session::new()
        // already claimed a HIGHER generation never gets every Append dropped
        // (which would play it silent). Drive the exact race: ---
        // The sounding reply claims a generation and marks itself active (what
        // Session::append does on its first successful clip).
        let sounding = super::Session::new();
        ACTIVE_GENERATION.fetch_max(sounding.generation, Ordering::Relaxed);

        // The user barges; the pipeline begins the NEXT reply, which claims a
        // higher generation (NEXT_GENERATION advances past it) BEFORE the barge
        // thread runs cancel_all.
        let next = super::Session::new();
        assert!(next.generation > sounding.generation);
        assert!(NEXT_GENERATION.load(Ordering::Relaxed) > next.generation);

        // Barge fires: discard is bounded on the SOUNDING generation (+1).
        super::cancel_all();

        // The sounding reply is stale (cut); the fresh post-barge reply is NOT
        // — its audio will play. The old NEXT_GENERATION-based bound would have
        // marked `next` stale here (next.generation < NEXT_GENERATION).
        assert!(super::is_stale(sounding.generation), "the interrupted reply is cut");
        assert!(
            !super::is_stale(next.generation),
            "the fresh post-barge reply must NOT be marked stale"
        );
    }

    /// Pure math — no audio device, no playback thread.
    #[test]
    fn silence_sample_count_matches_duration() {
        assert_eq!(
            super::silence_samples(Duration::from_millis(250)),
            super::SILENCE_SAMPLE_RATE as usize / 4
        );
        assert_eq!(super::silence_samples(Duration::ZERO), 0);
    }
}
