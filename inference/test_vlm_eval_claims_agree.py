"""The committed screen-VQA artifacts must not contradict each other.

Three files in inference/benchmarks/vlm_eval/ describe the SAME shipped configuration:
README.md, results.json and compare_models.py. They disagreed.

results.json recorded, under MODEL_COMPARISON_2026_07_31, a measurement at the shipped
visual-token cap -- 54/72 = 75.0%, evenly 18/24 across all three fixtures -- together
with an explicit CORRECTION_TO_AN_EARLIER_CLAIM saying the older "2/12 to 4/12 at every
resolution" figure came from UNCAPPED resolutions (the regime where the model collapses
into repetition) and a harder question set, and was therefore never a measurement of
what ships.

And then README.md still headlined "NOT reliable at reading screens ... 2/12 to 4/12 at
EVERY resolution"; compare_models.py's own docstring repeated it; and results.json
carried the retracted sentence in a SECOND key a few lines above its own correction.

A number that has been retracted in one artifact and left standing in three is worse
than never having measured it: whoever reads first gets a verdict the project has
already disowned, and the shipped model was on the verge of being swapped out over it.

  Run: .venv/bin/python inference/test_vlm_eval_claims_agree.py   (from the repo root)
"""
import json
import pathlib
import re
import unittest

EVAL = pathlib.Path(__file__).resolve().parent / "benchmarks" / "vlm_eval"
RETRACTED = "2/12 to 4/12"
# A line may still contain the retracted figure if it is visibly retracting it.
RETRACTION_MARKERS = (
    "previously said", "ORIGINAL_TEXT", "CORRECTION_TO", "retracted",
    "SUPERSEDED", "asserted", "left standing",
)


def _measured_accuracy():
    d = json.loads((EVAL / "results.json").read_text(encoding="utf-8"))
    comp = d["MODEL_COMPARISON_2026_07_31"]["results"]
    incumbent = comp["mlx-community/Qwen2-VL-2B-Instruct-4bit"]["accuracy"]
    m = re.search(r"\((\d+(?:\.\d+)?)%\)", incumbent)
    return float(m.group(1)), incumbent


class TheArtifactsAgree(unittest.TestCase):
    def test_results_json_still_records_a_measured_accuracy(self):
        pct, raw = _measured_accuracy()
        self.assertGreater(pct, 0.0, f"unparseable accuracy: {raw!r}")

    def test_the_retracted_figure_never_stands_unqualified(self):
        offenders = []
        for f in sorted(EVAL.rglob("*")):
            if f.is_dir() or f.suffix not in (".md", ".json", ".py"):
                continue
            for i, line in enumerate(f.read_text(encoding="utf-8").splitlines(), 1):
                if RETRACTED in line and not any(m in line for m in RETRACTION_MARKERS):
                    # Allow it when the retraction is on an adjacent line (a wrapped
                    # blockquote), which is how README.md quotes what it disowns.
                    offenders.append(f"{f.relative_to(EVAL)}:{i}: {line.strip()[:110]}")
        # Re-check offenders against their neighbourhood before failing.
        real = []
        for entry in offenders:
            path, lineno = entry.split(":", 2)[0], int(entry.split(":")[1])
            lines = (EVAL / path).read_text(encoding="utf-8").splitlines()
            window = "\n".join(lines[max(0, lineno - 3):lineno + 2])
            if not any(m in window for m in RETRACTION_MARKERS):
                real.append(entry)
        self.assertEqual(
            real, [],
            "the retracted screen-VQA figure is stated as fact. results.json measured "
            f"{_measured_accuracy()[1]} at the SHIPPED cap and explicitly retracted "
            f"this number; whoever reads these first gets a disowned verdict: {real}",
        )

    def test_the_readme_headline_matches_the_measurement(self):
        readme = (EVAL / "README.md").read_text(encoding="utf-8")
        pct, _ = _measured_accuracy()
        verdict = readme[readme.index("**Verdict:"):][:600]
        self.assertIn(
            f"{pct:.1f}%", verdict,
            f"README's verdict does not quote the measured {pct:.1f}% from results.json",
        )
        self.assertNotIn(
            "NOT\nreliable at reading screens", verdict.replace(" ", "\n"),
            "the README verdict still contradicts the measurement",
        )

    def test_the_losing_candidates_are_still_recorded(self):
        """The pin is justified by a comparison; if that vanishes the pin becomes a
        bare assertion."""
        d = json.loads((EVAL / "results.json").read_text(encoding="utf-8"))
        results = d["MODEL_COMPARISON_2026_07_31"]["results"]
        self.assertGreaterEqual(
            len(results), 3,
            "the NO-GO verdict on swapping the VLM rests on measured alternatives",
        )
        incumbent, _ = _measured_accuracy()
        for name, row in results.items():
            if "Qwen2-VL-2B" in name:
                continue
            other = float(re.search(r"\((\d+(?:\.\d+)?)%\)", row["accuracy"]).group(1))
            self.assertLess(
                other, incumbent,
                f"{name} now beats the pinned model ({other}% vs {incumbent}%); the "
                "NO-GO verdict in this file is stale and the pin should be revisited",
            )


if __name__ == "__main__":
    unittest.main(verbosity=2)
