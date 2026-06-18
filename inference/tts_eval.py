"""TTS engine evaluation (contract item 6). One candidate per process.

Usage: .venv/bin/python inference/tts_eval.py <engine> <repo> <voice>
Measures warm RTF = synth_seconds / audio_seconds on the shared audition
line, runs the server's silence-trim path, and writes the audition sample to
state/voice-samples/<engine>-<voice>.wav. Prints one JSON result line
prefixed RESULT::. Never plays audio.
"""
import json
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(Path(__file__).resolve().parent))
import numpy as np  # noqa: E402

import server  # noqa: E402

AUDITION = (
    "Good evening, sir. All systems are running at full capacity. "
    "Shall I begin the diagnostic?"
)
WARMUP = "Systems check."


def main():
    engine, repo, voice = sys.argv[1], sys.argv[2], sys.argv[3]
    out = {"engine": engine, "repo": repo, "voice": voice}
    settings = server.load_config()
    settings["engine"] = engine
    settings["tts_model"] = repo
    settings["voice"] = voice
    eng = server.InferenceEngine(settings, "t {utterance}", "")

    t0 = time.perf_counter()
    tts = eng._ensure_tts()
    out["load_s"] = round(time.perf_counter() - t0, 2)

    synth = eng._tts_synth_fn()
    rate = eng._tts_sample_rate(tts)
    out["sample_rate"] = rate

    # Warm-up (kernel compile, voice prompt fetch) — untimed.
    warm = synth(tts, WARMUP, voice)
    out["warmup_samples"] = int(sum(len(c) for c in warm))

    # Two timed warm runs; report both, gate on the better one.
    rtfs = []
    audio = None
    for _ in range(2):
        t0 = time.perf_counter()
        chunks = synth(tts, AUDITION, voice)
        synth_s = time.perf_counter() - t0
        audio = np.concatenate(chunks) if chunks else np.zeros(0, dtype=np.float32)
        audio_s = len(audio) / rate if rate else 0.0
        rtfs.append(round(synth_s / audio_s, 3) if audio_s else None)
    out["rtf_runs"] = rtfs
    out["rtf_warm_best"] = min(r for r in rtfs if r is not None) if any(rtfs) else None
    out["audio_s"] = round(len(audio) / rate, 2)

    # Silence-trim path (same as speak/converse/openers).
    trimmed = eng._trim_silence(audio, rate)
    loud = int(np.flatnonzero(np.abs(trimmed) >= server.TRIM_AMPLITUDE).size)
    out["trim_ok"] = bool(trimmed.size > 0 and loud > 0)
    out["trimmed_s"] = round(len(trimmed) / rate, 2)

    sample = ROOT / "state" / "voice-samples" / f"{engine}-{voice}.wav"
    eng._write_wav(trimmed, rate, out_path=sample)
    out["sample_path"] = str(sample)

    gate = (
        out["rtf_warm_best"] is not None
        and out["rtf_warm_best"] <= 0.5
        and out["trim_ok"]
    )
    out["gate_pass"] = bool(gate)
    print("RESULT::" + json.dumps(out))


if __name__ == "__main__":
    try:
        main()
    except Exception as exc:  # report load/synth failures as a gate fail
        print(
            "RESULT::"
            + json.dumps(
                {
                    "engine": sys.argv[1] if len(sys.argv) > 1 else "?",
                    "error": f"{type(exc).__name__}: {exc}",
                    "gate_pass": False,
                }
            )
        )
        raise
