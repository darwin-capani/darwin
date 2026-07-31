#!/usr/bin/env python
"""Compare VLM checkpoints on SCREEN-VQA: can it read what is on the screen?

WHY THIS EXISTS. The committed vlm_eval measured ONE fixture with unusually large text
and judged quality by eyeballing a single answer string. It reported a GO. It could not
see how the shipped Qwen2-VL-2B actually scores on checkable facts at the cap it
really runs at (54/72 = 75.0%; an earlier 2/12-to-4/12 figure came from UNCAPPED
resolutions and is retracted in results.json), nor that
feeding a full-resolution screen drove it into repetition collapse.

This harness scores CHECKABLE FACTS — an error code, an IP address, a button label —
across three fixtures with different text sizes, several samples each, so the number
means something. It applies the SHIPPED visual-token cap, so it measures what ships.

  Run: HF_HOME="$HOME/Library/Application Support/DARWIN/models" \
       .venv/bin/python inference/benchmarks/vlm_eval/compare_models.py [model_id ...]

Prints one line per model and a JSON blob for the committed results file.
"""
import json
import os
import statistics
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parents[1]))

DEFAULT_MODELS = ["mlx-community/Qwen2-VL-2B-Instruct-4bit"]

# (fixture, question, predicate) — every fact is VISIBLE in the image and checkable
# without judgement. Predicates are deliberately lenient about phrasing and strict
# about the fact.
def _has(*subs):
    return lambda a: any(s in a.lower() for s in subs)


CASES = [
    ("screenshot_fixture.png", "What is the exact error code shown on screen?", _has("e0499")),
    ("screenshot_fixture.png", "What does the blue button say?", _has("rebuild")),
    ("screenshot_fixture.png", "What command is being run in the terminal?", _has("cargo build")),
    ("screenshot_fixture.png", "What kind of error is it — what cannot be done twice?", _has("borrow")),
    ("fixture_dense_code_13px.png", "What is the exact error code shown on screen?", _has("e0308")),
    ("fixture_dense_code_13px.png", "What does the blue button say?", _has("run test")),
    ("fixture_dense_code_13px.png", "What source file does the error point to?", _has("tally.rs")),
    ("fixture_dense_code_13px.png", "What two types are mismatched in the error?", _has("u64")),
    ("fixture_settings_12px.png", "What is the IPv4 address shown?", _has("192.168.1.47")),
    ("fixture_settings_12px.png", "What does the blue button say?", _has("renew")),
    ("fixture_settings_12px.png", "What DNS server is configured?", _has("1.1.1.1")),
    ("fixture_settings_12px.png", "What wireless security type is shown?", _has("wpa3")),
]
SAMPLES = int(os.environ.get("VLM_EVAL_SAMPLES", "2"))

# ANSWER-SHAPE SUFFIX. Measured to matter MORE than the checkpoint: on the shipped
# model a bare "What is the IPv4 address shown?" scored 2/3 (often emitting the single
# token "The" and stopping), while the same question with this suffix scored 3/3. A
# third phrasing collapsed to "111111...". Set VLM_EVAL_SUFFIX="" to measure bare
# questions, which is what the first comparison unknowingly did.
SUFFIX = os.environ.get("VLM_EVAL_SUFFIX", " Answer in one complete sentence.")


def log(m):
    print(m, file=sys.stderr, flush=True)


def _shipped_path(img_path):
    """Apply the SAME visual-token cap the server applies, so this measures what
    ships rather than an uncapped configuration the user never sees."""
    try:
        import server as _server

        return _server.downscale_for_vlm(str(img_path))
    except Exception as e:  # noqa: BLE001
        log(f"WARNING: shipped cap unavailable ({e}); measuring UNCAPPED")
        return str(img_path)


def evaluate(model_id):
    from mlx_vlm import generate, load
    from mlx_vlm.prompt_utils import apply_chat_template
    import mlx.core as mx

    t0 = time.perf_counter()
    model, processor = load(model_id)
    cfg = model.config
    load_s = time.perf_counter() - t0
    mx.clear_cache()
    mx.reset_peak_memory()

    correct = 0
    total = 0
    lat = []
    per_fixture = {}
    for fixture, question, check in CASES:
        img = _shipped_path(HERE / fixture)
        fm = apply_chat_template(processor, cfg, question + SUFFIX, num_images=1)
        for _ in range(SAMPLES):
            t = time.perf_counter()
            try:
                r = generate(model, processor, fm, [img], max_tokens=96, verbose=False)
                ans = str(r if isinstance(r, str) else getattr(r, "text", r))
            except Exception as e:  # noqa: BLE001
                ans = f"<generate failed: {e}>"
            lat.append(time.perf_counter() - t)
            total += 1
            ok = bool(check(ans))
            correct += ok
            k = fixture.replace(".png", "")
            per_fixture.setdefault(k, [0, 0])
            per_fixture[k][0] += ok
            per_fixture[k][1] += 1
    peak_gib = mx.get_peak_memory() / (1 << 30)
    return {
        "model": model_id,
        "cold_load_s": round(load_s, 2),
        "accuracy": f"{correct}/{total}",
        "accuracy_pct": round(100 * correct / total, 1) if total else 0.0,
        "median_latency_s": round(statistics.median(lat), 2) if lat else None,
        "peak_gib": round(peak_gib, 2),
        "per_fixture": {k: f"{v[0]}/{v[1]}" for k, v in per_fixture.items()},
    }


def main():
    models = sys.argv[1:] or DEFAULT_MODELS
    out = []
    for m in models:
        log(f"=== {m} ===")
        try:
            r = evaluate(m)
        except Exception as e:  # noqa: BLE001 — an unloadable candidate is a result
            log(f"  UNAVAILABLE: {e}")
            out.append({"model": m, "available": False, "reason": str(e)[:200]})
            continue
        out.append(r)
        log(
            f"  {r['accuracy']:>7} ({r['accuracy_pct']:>5.1f}%)  "
            f"median {r['median_latency_s']:>5.2f}s  load {r['cold_load_s']:>5.2f}s  "
            f"peak {r['peak_gib']:.2f} GiB   {r['per_fixture']}"
        )
    print(json.dumps(out, indent=2))


if __name__ == "__main__":
    main()
