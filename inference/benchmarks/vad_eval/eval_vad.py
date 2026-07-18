#!/usr/bin/env python
"""MEASURE-FIRST VAD eval: does a LEARNED VAD (Core ML Silero) beat the daemon's
RMS energy gate on false-accept / false-reject / detect-latency?

This is the ADOPTION gate for replacing the RMS-gate VAD (daemon/src/audio.rs
`Vad`) with a learned one, per docs/ROADMAP.md. Same discipline that dropped
speculative decoding: adopt only on a MEASURED win, never on the idea alone.

TEST SET (on-device, reproducible; NO recorded audio persisted):
  * SPEECH clips — macOS `say` synthesizes a fixed set of short utterances
    (varied installed voices), each converted to 16 kHz mono. Every speech clip
    is embedded as [leading non-speech] + [speech] + [trailing non-speech] so the
    TRUE speech onset frame is known, and variants are produced clean, mixed with
    noise at several SNRs, and scaled quiet — to probe false-REJECT and
    detect-LATENCY.
  * NOISE adversary clips — synthesized (seeded) loud non-speech: white / pink /
    amplitude-modulated noise, keyboard-like transient clicks, an appliance-like
    hum — all at RMS WELL ABOVE the RMS gate's 0.015 floor. These probe false-
    ACCEPT: the RMS gate calls anything loud "speech"; a learned VAD should not.

Both VADs run through the IDENTICAL debounce ported from the daemon
(min_speech_ms / silence_ms over fixed frames); the ONLY thing that differs is
the per-frame voiced verdict — `rms > rms_threshold` vs `prob > threshold` — so
the comparison isolates verdict QUALITY. Both run at the Silero 32 ms frame
cadence so the frame grid is shared.

HONESTY: SYNTHETIC-but-representative (macOS `say` speech + synthesized noise, no
human-labeled corpus), directional build-decision evidence, not a production
guarantee. The `say` voices present depend on the machine and are recorded in
results.json. Run:  .venv/bin/python inference/benchmarks/vad_eval/eval_vad.py
"""
import json
import math
import os
import subprocess
import sys
import tempfile
import time

import numpy as np

HERE = os.path.dirname(os.path.abspath(__file__))
# Import the shipped Core ML VAD backend + its pure geometry helpers.
sys.path.insert(0, os.path.abspath(os.path.join(HERE, "..", "..")))
import coreml_vad as cv  # noqa: E402

SR = cv.SAMPLE_RATE          # 16000
CHUNK = cv.CHUNK             # 512 samples = 32 ms
FRAME_MS = 1000.0 * CHUNK / SR  # 32.0

# Daemon VAD contract (config/darwin.toml defaults).
RMS_THRESHOLD = 0.015
MIN_SPEECH_MS = 250
SILENCE_MS = 350
PROB_THRESHOLD = cv.DEFAULT_THRESHOLD  # 0.5

# Fixed short utterances (commands/questions a user would actually say to DARWIN).
UTTERANCES = [
    "Hello Darwin, what is the weather today?",
    "Set a timer for ten minutes please.",
    "Turn off the kitchen lights.",
    "What time is my first meeting tomorrow?",
    "Play some music in the living room.",
    "Remind me to call the dentist on Friday.",
    "How long will it take to drive downtown?",
    "Add milk and eggs to my shopping list.",
]
# Preferred voices; only those actually installed are used.
PREF_VOICES = ["Samantha", "Alex", "Daniel", "Karen", "Fred", "Tom", "Victoria"]

LEAD_MS = 480      # leading non-speech before speech onset (>= min_speech window)
TRAIL_MS = 480     # trailing non-speech


def installed_voices():
    try:
        out = subprocess.run(["say", "-v", "?"], capture_output=True, text=True,
                             timeout=20).stdout
    except Exception:
        return []
    have = set()
    for line in out.splitlines():
        name = line[:line.find("  ")].strip() if "  " in line else ""
        if name:
            have.add(name)
    return [v for v in PREF_VOICES if v in have]


def say_wav(text, voice, workdir):
    """Synthesize `text` in `voice` to a 16 kHz mono float32 array via `say` +
    `afconvert`. Returns None on any failure (missing voice / tool)."""
    aiff = os.path.join(workdir, "u.aiff")
    wav = os.path.join(workdir, "u.wav")
    try:
        subprocess.run(["say", "-v", voice, "-o", aiff, text], check=True,
                       capture_output=True, timeout=30)
        subprocess.run(["afconvert", aiff, "-o", wav, "-f", "WAVE",
                        "-d", "LEI16@16000", "-c", "1"], check=True,
                       capture_output=True, timeout=30)
    except Exception:
        return None
    import soundfile as sf
    x, sr = sf.read(wav, dtype="float32")
    if x.ndim > 1:
        x = x.mean(axis=1)
    return x.astype(np.float32) if sr == SR else None


def rms(x):
    x = np.asarray(x, dtype=np.float64)
    return float(np.sqrt(np.mean(x * x))) if x.size else 0.0


def pink_noise(n, rng):
    """Approximate pink noise via FFT 1/sqrt(f) shaping of white noise."""
    white = rng.standard_normal(n)
    f = np.fft.rfft(white)
    k = np.arange(len(f))
    k[0] = 1
    f = f / np.sqrt(k)
    out = np.fft.irfft(f, n)
    return (out / (np.std(out) + 1e-9)).astype(np.float32)


def scale_to_rms(x, target):
    r = rms(x)
    return (x * (target / r)).astype(np.float32) if r > 1e-9 else x.astype(np.float32)


def mix_snr(speech, noise, snr_db):
    """Mix speech + noise at a target SNR (dB), noise tiled/truncated to length."""
    n = len(speech)
    if len(noise) < n:
        noise = np.tile(noise, n // len(noise) + 1)
    noise = noise[:n]
    ps, pn = rms(speech) ** 2, rms(noise) ** 2
    if pn < 1e-12 or ps < 1e-12:
        return speech.astype(np.float32)
    g = math.sqrt(ps / (pn * (10 ** (snr_db / 10.0))))
    return (speech + g * noise).astype(np.float32)


def build_speech_clips(rng, workdir):
    """Build labeled speech clips: (name, signal, onset_frame, category). Onset
    frame = LEAD_MS boundary. Categories: clean / snr10 / snr5 / snr0 / quiet."""
    voices = installed_voices()
    if not voices:
        return [], []
    lead = np.zeros(int(SR * LEAD_MS / 1000), dtype=np.float32)
    trail = np.zeros(int(SR * TRAIL_MS / 1000), dtype=np.float32)
    # a quiet room-tone under the RMS floor, so leading/trailing regions are
    # unambiguously non-speech for BOTH gates.
    room = scale_to_rms(rng.standard_normal(len(lead)), 0.004)
    room_t = scale_to_rms(rng.standard_normal(len(trail)), 0.004)
    onset_frame = len(lead) // CHUNK
    clips = []
    for i, text in enumerate(UTTERANCES):
        voice = voices[i % len(voices)]
        sp = say_wav(text, voice, workdir)
        if sp is None or len(sp) < CHUNK * 4:
            continue
        # normalize speech to a realistic RMS (~0.12) for a fair, level baseline
        sp = scale_to_rms(sp, 0.12)
        variants = [("clean", sp)]
        wn = pink_noise(len(sp), rng)
        variants.append(("snr10", mix_snr(sp, wn, 10)))
        variants.append(("snr5", mix_snr(sp, wn, 5)))
        variants.append(("snr0", mix_snr(sp, wn, 0)))
        variants.append(("quiet", scale_to_rms(sp, 0.03)))  # quiet speech
        for cat, body in variants:
            sig = np.concatenate([lead + room, body, trail + room_t]).astype(np.float32)
            clips.append({
                "name": f"speech_{i:02d}_{voice}_{cat}",
                "signal": sig, "onset_frame": onset_frame, "category": cat,
                "voice": voice, "text": text,
            })
    return clips, voices


def build_noise_clips(rng):
    """Build noise-only adversary clips (no speech): loud non-speech that the RMS
    gate would flag. (name, signal, category)."""
    dur = 3.0
    n = int(SR * dur)
    t = np.arange(n) / SR
    clips = []

    def add(name, sig, target_rms):
        clips.append({"name": name, "signal": scale_to_rms(sig, target_rms),
                      "category": "noise"})

    add("white_0.03", rng.standard_normal(n), 0.03)
    add("white_0.06", rng.standard_normal(n), 0.06)
    add("pink_0.05", pink_noise(n, rng), 0.05)
    # amplitude-modulated noise (rhythmic non-speech ~ fan wobble / rustling)
    add("ammod_0.05", pink_noise(n, rng) * (0.5 + 0.5 * np.sin(2 * np.pi * 3 * t)), 0.05)
    # keyboard-like transient clicks: short high-energy bursts on a quiet bed
    clicks = scale_to_rms(rng.standard_normal(n), 0.004)
    for start in range(0, n, int(SR * 0.18)):
        w = int(SR * 0.01)
        clicks[start:start + w] += rng.standard_normal(min(w, n - start)) * 0.4
    add("clicks_kbd", clicks, 0.05)
    # appliance-like hum: 60 Hz + harmonics + a little broadband
    hum = (np.sin(2 * np.pi * 60 * t) + 0.5 * np.sin(2 * np.pi * 120 * t)
           + 0.3 * np.sin(2 * np.pi * 180 * t) + 0.2 * rng.standard_normal(n))
    add("hum_60hz", hum, 0.05)
    return clips


# ---- per-frame verdicts + shared debounce (ported from daemon audio.rs Vad) ---

def rms_frames(sig):
    n = len(sig) // CHUNK
    return [rms(sig[i * CHUNK:(i + 1) * CHUNK]) for i in range(n)]


def rms_voiced(sig):
    return [r > RMS_THRESHOLD for r in rms_frames(sig)]


def silero_probs(sig, streamer):
    streamer.reset()
    n = len(sig) // CHUNK
    return [streamer.push_chunk(sig[i * CHUNK:(i + 1) * CHUNK]) for i in range(n)]


def debounce_segments(voiced, min_speech_frames, silence_frames):
    """PURE port of daemon `Vad::step`: voiced-frame run must reach
    min_speech_frames to START (segment onset = where the run began, the daemon's
    buffered `pending`); silence run must reach silence_frames to END. Returns a
    list of {seg_start, decision, end} frame indices."""
    segs = []
    in_speech = False
    voiced_run = 0
    silent_run = 0
    run_start = None
    for idx, v in enumerate(voiced):
        if not in_speech:
            if v:
                if voiced_run == 0:
                    run_start = idx
                voiced_run += 1
                if voiced_run >= min_speech_frames:
                    in_speech = True
                    silent_run = 0
                    segs.append({"seg_start": run_start, "decision": idx, "end": None})
            else:
                voiced_run = 0
        else:
            if v:
                silent_run = 0
            else:
                silent_run += 1
                if silent_run >= silence_frames:
                    in_speech = False
                    voiced_run = 0
                    segs[-1]["end"] = idx
    return segs


def ceil_frames(ms):
    return max(1, math.ceil(ms / FRAME_MS))


def evaluate(voiced_fn, speech_clips, noise_clips):
    """Run a per-frame verdict function through the shared debounce over all clips
    and return aggregate FA / FR / detect-latency + per-frame confusion."""
    minf = ceil_frames(MIN_SPEECH_MS)
    silf = ceil_frames(SILENCE_MS)

    # NOISE clips -> false accept if ANY segment fires.
    fa_hits = 0
    for c in noise_clips:
        segs = debounce_segments(voiced_fn(c["signal"]), minf, silf)
        if segs:
            fa_hits += 1
    fa_rate = fa_hits / len(noise_clips) if noise_clips else 0.0

    # SPEECH clips -> false reject if NO segment starts within/at the speech
    # region; detect latency = (decision - onset) * frame_ms for the first
    # qualifying segment.
    misses = 0
    latencies = []
    per_cat = {}
    for c in speech_clips:
        segs = debounce_segments(voiced_fn(c["signal"]), minf, silf)
        onset = c["onset_frame"]
        detected = None
        for s in segs:
            # a segment whose decision lands at/after true onset and whose onset is
            # not spuriously deep in the leading region
            if s["decision"] >= onset - 1 and s["seg_start"] >= onset - minf:
                detected = s
                break
        cat = c["category"]
        d = per_cat.setdefault(cat, {"n": 0, "miss": 0, "lat": []})
        d["n"] += 1
        if detected is None:
            misses += 1
            d["miss"] += 1
        else:
            lat = (detected["decision"] - onset) * FRAME_MS
            latencies.append(lat)
            d["lat"].append(lat)
    fr_rate = misses / len(speech_clips) if speech_clips else 0.0

    # Per-frame verdict confusion (debounce-independent), over labeled frames:
    # speech clips' body frames = speech; noise clips' frames + speech clips'
    # lead/trail frames = non-speech.
    fp = tot_non = fn = tot_sp = 0
    for c in speech_clips:
        v = voiced_fn(c["signal"])
        onset = c["onset_frame"]
        # body region = [onset, len - trail_frames)
        trail_frames = int(SR * TRAIL_MS / 1000) // CHUNK
        body_end = len(v) - trail_frames
        for idx, vv in enumerate(v):
            if idx < onset:  # leading non-speech
                tot_non += 1
                fp += int(vv)
            elif idx < body_end:  # speech
                tot_sp += 1
                fn += int(not vv)
    for c in noise_clips:
        v = voiced_fn(c["signal"])
        tot_non += len(v)
        fp += sum(int(x) for x in v)

    def summ(cat):
        d = per_cat.get(cat, {"n": 0, "miss": 0, "lat": []})
        return {
            "n": d["n"], "miss": d["miss"],
            "median_latency_ms": (round(float(np.median(d["lat"])), 1)
                                  if d["lat"] else None),
        }

    return {
        "false_accept_rate_clips": round(fa_rate, 4),
        "false_accept_clips": f"{fa_hits}/{len(noise_clips)}",
        "false_reject_rate_clips": round(fr_rate, 4),
        "false_reject_clips": f"{misses}/{len(speech_clips)}",
        "detect_latency_ms_median": (round(float(np.median(latencies)), 1)
                                     if latencies else None),
        "detect_latency_ms_p90": (round(float(np.percentile(latencies, 90)), 1)
                                  if latencies else None),
        "frame_false_accept_rate": round(fp / tot_non, 4) if tot_non else None,
        "frame_false_reject_rate": round(fn / tot_sp, 4) if tot_sp else None,
        "by_category": {k: summ(k) for k in ("clean", "snr10", "snr5", "snr0", "quiet")},
    }


def main():
    rng = np.random.default_rng(1234)
    workdir = tempfile.mkdtemp(prefix="vad_eval_")
    print(f"synthesizing test set in {workdir} ...", flush=True)
    speech_clips, voices = build_speech_clips(rng, workdir)
    noise_clips = build_noise_clips(rng)
    if not speech_clips:
        print("NO speech clips (no `say` voices?) — cannot run the speech half.")
    print(f"speech clips: {len(speech_clips)} (voices: {voices})")
    print(f"noise adversary clips: {len(noise_clips)} "
          f"(RMS {[round(rms(c['signal']), 3) for c in noise_clips]})")

    # Learned VAD (Core ML). Build/load once; honest fallback message if absent.
    try:
        backend = cv.CoreMLVAD()
        backend.ensure_loaded()
        streamer = cv.StreamingVAD(backend=backend)
        per_frame_lat = measure_frame_latency(backend)
    except cv.CoreMLVADUnavailable as e:
        print(f"Core ML VAD unavailable ({e}); cannot run the learned half.")
        return 1

    def silero_voiced(sig):
        return [p > PROB_THRESHOLD for p in silero_probs(sig, streamer)]

    print("evaluating RMS energy gate ...", flush=True)
    rms_res = evaluate(rms_voiced, speech_clips, noise_clips)
    print("evaluating Core ML Silero VAD ...", flush=True)
    silero_res = evaluate(silero_voiced, speech_clips, noise_clips)

    results = {
        "meta": {
            "generated": time.strftime("%Y-%m-%dT%H:%M:%S"),
            "sample_rate": SR, "frame_ms": FRAME_MS,
            "rms_threshold": RMS_THRESHOLD, "prob_threshold": PROB_THRESHOLD,
            "min_speech_ms": MIN_SPEECH_MS, "silence_ms": SILENCE_MS,
            "speech_clips": len(speech_clips), "noise_clips": len(noise_clips),
            "say_voices": voices,
            "synthetic_note": ("macOS `say` speech + synthesized noise adversaries; "
                               "SYNTHETIC-but-representative, directional build "
                               "evidence, NOT ground-truth production data."),
            "coreml_per_frame_latency_ms": per_frame_lat,
        },
        "rms_gate": rms_res,
        "coreml_silero": silero_res,
    }
    out = os.path.join(HERE, "results.json")
    with open(out, "w") as f:
        json.dump(results, f, indent=2)
    print("\n==== RESULTS ====")
    print(json.dumps({"rms_gate": rms_res, "coreml_silero": silero_res,
                      "coreml_per_frame_latency_ms": per_frame_lat}, indent=2))
    print(f"\nwrote {out}")
    return 0


def measure_frame_latency(backend):
    import statistics
    x = np.zeros((1, cv.MODEL_INPUT), dtype=np.float32)
    st = np.zeros(cv.STATE_SHAPE, dtype=np.float32)
    for _ in range(5):
        backend.step(x, st)
    runs = []
    for _ in range(200):
        t0 = time.perf_counter()
        backend.step(x, st)
        runs.append((time.perf_counter() - t0) * 1000.0)
    return {"median": round(statistics.median(runs), 3),
            "p90": round(float(np.percentile(runs, 90)), 3),
            "frame_budget_ms": round(FRAME_MS, 1)}


if __name__ == "__main__":
    raise SystemExit(main())
