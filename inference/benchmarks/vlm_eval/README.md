# VLM latency measurement — "ask about my screen" (op=describe_image / VQA)

Honest, on-device latency measurement for the screen-understanding VLM agent
(feature: pair a screen capture with the on-device VLM so the user can ask a
specific visual question about their screen). Measures what actually gates the
feature; no fabricated numbers.

## Run

    .venv/bin/pip install 'mlx-vlm==0.6.5' pillow   # optional extra (see requirements.txt caveat)
    .venv/bin/python3 inference/benchmarks/vlm_eval/measure.py

`measure.py` renders a representative 1512×982 "screenshot" fixture (a terminal
window with a red `error[E0499]` banner and a blue **Rebuild** button), loads
`mlx-community/Qwen2-VL-2B-Instruct-4bit`, and times a real VQA question
("what error is shown, and which button would rebuild?") over three warm runs.
Writes `results.json`. If mlx-vlm or the checkpoint is absent it prints an
honest NO-GO and exits non-zero (never fabricates a number).

## Measured (M1 Pro, arm64, mlx 0.32.0 / mlx-vlm 0.6.5)

| metric | value |
|---|---|
| cold model load | 17.29 s |
| warm latency / query | **~1.6 s median** (was ~8.2 s before the visual-token cap) |
| peak GPU memory | 2.44 GiB |
| resolution | capped at 380,000 px (~764×496, ~459 visual tokens); 128 max tokens |

**Verdict: GO, with the visual-token cap.** Measured at the SHIPPED configuration over
12 checkable facts x 3 fixtures x 6 samples (72 evaluations, harness in
`compare_models.py`): **54/72 = 75.0%**, 1.63 s median, 2.29 GiB peak, and an even
18/24 on each of the three fixtures. Full numbers and the two losing candidates are in
`results.json` under `MODEL_COMPARISON_2026_07_31`.

> **This paragraph previously said the opposite** — "NOT reliable at reading screens
> ... 2/12 to 4/12 at EVERY resolution" — and was left standing after `results.json`
> had already recorded the correction, so two committed artifacts in this directory
> disagreed about the same shipped configuration. That figure came from an early
> ad-hoc probe at resolutions the server does not use, before the visual-token cap
> existed; it was never a measurement of what ships. Re-measured properly at the cap,
> the number is 75%.

Feeding a FULL-resolution screen IS actively worse and that finding stands: ~1890
visual tokens drive this checkpoint into repetition collapse ("The The The The ..."),
which `DESCRIBE_IMAGE_MAX_PIXELS` now prevents. Two newer candidates were measured
against it on the same harness and both lost on every axis — Qwen3-VL-2B at 25.0% and
Qwen3-VL-4B at 41.7%, the latter also slower and heavier — so the pin stays.

Note also that describe_image no longer asks this model first: an OCR transcript is
read and answered by the resident LLM, and the VLM is consulted only when the
transcript genuinely cannot answer (colour, position, size). Original verdict follows:

~~GO for an ON-DEMAND screen question~~ (a deliberate voice query),
NOT for a continuous/real-time loop. The model answered correctly — it read the
error banner *and* located the Rebuild button (see `results.json:first_answer`),
which is genuine visual reasoning, not OCR. Answer QUALITY is not formally scored
here (a single representative fixture); only latency + a correctness spot-check.

The fixture PNG is regenerated on every run and is git-ignored.
