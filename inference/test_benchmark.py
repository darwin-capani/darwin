"""Unit tests for the PURE stat / report-shape helpers in benchmark.py.

These MUST run without loading any model — benchmark.py keeps every mlx/server
import inside its measurement functions, so importing the module here touches no
weights. The model runs are the device-gated part, exercised by actually running
`benchmark.py` on the target Mac (not under this test).

Run: .venv/bin/python inference/test_benchmark.py   (from the repo root)
"""
import ast
import re
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import benchmark  # noqa: E402

SRC = Path(benchmark.__file__).read_text(encoding="utf-8")


class MedianTests(unittest.TestCase):
    def test_odd_count(self):
        self.assertEqual(benchmark.median([3, 1, 2]), 2)

    def test_even_count_averages_middle(self):
        self.assertEqual(benchmark.median([1, 2, 3, 4]), 2.5)

    def test_single(self):
        self.assertEqual(benchmark.median([42.0]), 42.0)

    def test_empty_raises(self):
        with self.assertRaises(ValueError):
            benchmark.median([])


class WarmDiscardTests(unittest.TestCase):
    def test_drops_leading_warmup(self):
        self.assertEqual(benchmark.warm_discard([9, 1, 2, 3], warmup=1), [1, 2, 3])

    def test_drops_multiple_warmups(self):
        self.assertEqual(benchmark.warm_discard([9, 8, 1, 2], warmup=2), [1, 2])

    def test_zero_warmup_keeps_all(self):
        self.assertEqual(benchmark.warm_discard([1, 2], warmup=0), [1, 2])

    def test_not_enough_runs_raises(self):
        with self.assertRaises(ValueError):
            benchmark.warm_discard([1], warmup=1)

    def test_negative_warmup_raises(self):
        with self.assertRaises(ValueError):
            benchmark.warm_discard([1, 2, 3], warmup=-1)

    def test_returns_copy_not_alias(self):
        src = [9, 1, 2]
        out = benchmark.warm_discard(src, warmup=1)
        out.append(999)
        self.assertEqual(src, [9, 1, 2])  # source untouched


class SummarizeMetricTests(unittest.TestCase):
    def test_discards_warmup_then_medians(self):
        # warm-up 100 is dropped; median of [10,20,30] == 20
        s = benchmark.summarize_metric([100, 10, 20, 30], warmup=1)
        self.assertEqual(s["median"], 20)
        self.assertEqual(s["min"], 10)
        self.assertEqual(s["max"], 30)
        self.assertEqual(s["n"], 3)
        self.assertEqual(s["warmup"], 1)
        self.assertEqual(s["runs"], [10, 20, 30])

    def test_none_entries_excluded(self):
        s = benchmark.summarize_metric([100, 10, None, 30], warmup=1)
        self.assertEqual(s["median"], 20)  # median of [10, 30]
        self.assertEqual(s["n"], 2)

    def test_all_none_gives_honest_empty(self):
        s = benchmark.summarize_metric([None, None, None], warmup=1)
        self.assertIsNone(s["median"])
        self.assertEqual(s["n"], 0)
        self.assertEqual(s["runs"], [None, None])


class SummarizeRunsTests(unittest.TestCase):
    def test_transposes_and_summarizes_each_key(self):
        runs = [
            {"a": 100, "b": 1.0},  # warm-up (dropped)
            {"a": 10, "b": 2.0},
            {"a": 20, "b": 4.0},
        ]
        out = benchmark.summarize_runs(runs, ["a", "b"], warmup=1)
        self.assertEqual(out["a"]["median"], 15)
        self.assertEqual(out["b"]["median"], 3.0)

    def test_missing_key_contributes_none(self):
        runs = [{"a": 1}, {"a": 2}, {}]  # last run missing 'a'
        out = benchmark.summarize_runs(runs, ["a"], warmup=1)
        # warm-up drops first; kept = [{a:2}, {}] -> [2, None] -> median 2
        self.assertEqual(out["a"]["median"], 2)
        self.assertEqual(out["a"]["n"], 1)


class ChipSlugTests(unittest.TestCase):
    def test_apple_m1_pro(self):
        self.assertEqual(benchmark.chip_slug("Apple M1 Pro"), "m1_pro")

    def test_apple_m4(self):
        self.assertEqual(benchmark.chip_slug("Apple M4"), "m4")

    def test_m2_max(self):
        self.assertEqual(benchmark.chip_slug("Apple M2 Max"), "m2_max")

    def test_empty_is_unknown(self):
        self.assertEqual(benchmark.chip_slug(""), "unknown")
        self.assertEqual(benchmark.chip_slug(None), "unknown")


class ReportShapeTests(unittest.TestCase):
    def _synthetic_report(self):
        return benchmark.build_report(
            environment={"chip": "Apple M1 Pro", "mlx": "0.31.2"},
            models={"llm": "x"},
            config={"runs": 5, "warmup": 1},
            results={"llm": {"representative": {}}},
            unavailable={"image_generation": "mflux not installed"},
            methodology=benchmark.METHODOLOGY,
        )

    def test_build_report_has_required_keys(self):
        report = self._synthetic_report()
        for key in benchmark.REQUIRED_TOP_KEYS:
            self.assertIn(key, report)
        self.assertEqual(report["schema"], benchmark.SCHEMA)

    def test_assert_report_shape_passes(self):
        self.assertTrue(benchmark.assert_report_shape(self._synthetic_report()))

    def test_assert_report_shape_rejects_missing_key(self):
        bad = self._synthetic_report()
        del bad["results"]
        with self.assertRaises(AssertionError):
            benchmark.assert_report_shape(bad)

    def test_report_is_json_serializable(self):
        import json

        json.dumps(self._synthetic_report())  # must not raise


class CosineTests(unittest.TestCase):
    """The pure cosine used to RECORD single-vs-batched embed agreement."""

    def test_identical_vectors_are_one(self):
        self.assertAlmostEqual(benchmark.cosine([1.0, 2.0, 3.0], [1.0, 2.0, 3.0]), 1.0)

    def test_orthogonal_vectors_are_zero(self):
        self.assertAlmostEqual(benchmark.cosine([1.0, 0.0], [0.0, 1.0]), 0.0)

    def test_opposite_vectors_are_minus_one(self):
        self.assertAlmostEqual(benchmark.cosine([1.0, 0.0], [-1.0, 0.0]), -1.0)

    def test_scale_invariant(self):
        self.assertAlmostEqual(
            benchmark.cosine([1.0, 2.0], [10.0, 20.0]), 1.0, places=9
        )

    def test_zero_norm_side_is_honest_zero_not_nan(self):
        self.assertEqual(benchmark.cosine([0.0, 0.0], [1.0, 2.0]), 0.0)
        self.assertEqual(benchmark.cosine([1.0, 2.0], [0.0, 0.0]), 0.0)


class HeaderMatchesTheSections(unittest.TestCase):
    """THE HEADER IS THE REFERENCE THE COMMITTED BASELINES ARE READ AGAINST, and it
    drifted from the code twice:
      * the Embeddings bullet named "the 4B-forward mean-pooled op=embed path", but
        `bench_embed` measures whatever [inference].embedder selects and
        server.DEFAULT_EMBEDDER is the Core ML bge embedder — a different model with a
        different dim. baseline_m1_pro.json records coreml-bge-small-en-v1.5 / 384-d,
        so a reader trusting the header read a bge number as a 4B one.
      * `rerank` was registered as a full section in run_all with NO bullet in WHAT IT
        MEASURES and NO entry in the USAGE --skip list, so an operator copying that
        line to skip everything still ran a model-loading section.
    Derived from run_all's own section table, so it cannot drift again."""

    def _sections(self):
        for node in ast.walk(ast.parse(SRC)):
            if not (isinstance(node, ast.FunctionDef) and node.name == "run_all"):
                continue
            for stmt in ast.walk(node):
                if (isinstance(stmt, ast.Assign)
                        and isinstance(stmt.value, ast.Dict)
                        and any(getattr(t, "id", None) == "sections" for t in stmt.targets)):
                    return [k.value for k in stmt.value.keys]
        self.fail("run_all no longer builds a `sections` dict; this test sees nothing")

    def test_the_usage_skip_list_names_every_section(self):
        m = re.search(r"--skip ([a-z,]+)\]", benchmark.__doc__)
        self.assertTrue(m, "the USAGE line no longer shows a --skip list")
        self.assertEqual(
            sorted(m.group(1).split(",")), sorted(self._sections()),
            "USAGE's --skip list and run_all's sections disagree; copying that line "
            "would still run a section it claims to skip",
        )

    def test_the_argparse_skip_help_names_every_section(self):
        m = re.search(r'"--skip".*?help="comma list: ([a-z,]+)"', SRC)
        self.assertTrue(m, "the --skip argument no longer lists its sections")
        self.assertEqual(sorted(m.group(1).split(",")), sorted(self._sections()))

    def _bullets(self):
        """WHAT IT MEASURES as {bullet label (lowercased): bullet body}. Keyed on the
        LABEL, not on the whole block: the word "rerank" occurs in prose there too, so
        a block-wide substring search would still pass with the bullet deleted."""
        doc = benchmark.__doc__
        block = doc[doc.index("WHAT IT MEASURES"):doc.index("METHODOLOGY")]
        bullets, label = {}, None
        for line in block.splitlines():
            m = re.match(r"\s*\* (\S+)", line)
            if m:
                label = m.group(1).rstrip(":").lower()
                bullets[label] = ""
            if label is not None:
                bullets[label] += line + "\n"
        return bullets

    def test_what_it_measures_has_a_bullet_for_every_section(self):
        labels = self._bullets()
        for name in self._sections():
            self.assertTrue(
                any(name in label for label in labels),
                f"the {name} section runs (and loads models) but WHAT IT MEASURES "
                f"has no bullet for it; bullets are {sorted(labels)}",
            )

    def test_the_embed_bullet_names_the_selector_not_one_backend(self):
        """The bullet must describe the ACTIVE backend, not hardcode a path — naming
        one of the two is how it came to describe a model the harness does not run."""
        bullets = self._bullets()
        embed = next(v for k, v in bullets.items() if "embed" in k)
        self.assertIn(
            "[inference].embedder", embed,
            "the Embeddings bullet must name the selector that decides which "
            "backend is measured, not one of the two backends",
        )


REPO = Path(__file__).resolve().parent.parent
LORA_SMOKE_DIR = REPO / "inference" / "benchmarks" / "lora_eval"
DISTILL_RS = REPO / "daemon" / "src" / "distill.rs"


def _lora_smoke():
    """The sibling harness under inference/benchmarks/ (this file's other resident
    harness tests live here too). Import-light: no training, no model."""
    sys.path.insert(0, str(LORA_SMOKE_DIR))
    import smoke

    return smoke


def _daemon_argv_flags(fn_name):
    """The `--flag` literals, IN ORDER, from a distill.rs command builder's
    `args: vec![...]`. Flags only — the values beside them are Rust variables
    (base_model, data_dir, ...) with no counterpart on the Python side."""
    src = DISTILL_RS.read_text(encoding="utf-8")
    start = src.index(f"pub fn {fn_name}(")
    body = src[start:src.index("\n}\n", start)]
    vec = body[body.index("args: vec!["):]
    return re.findall(r'"(--[a-z-]+)"', vec)


@unittest.skipUnless(DISTILL_RS.is_file(), "daemon/src/distill.rs is not in this tree")
class LoraSmokeArgvMirrorsTheDaemon(unittest.TestCase):
    """inference/benchmarks/lora_eval/smoke.py says it evaluates BASE vs ADAPTER with
    "the SAME `mlx_lm.lora --test [...]` argv" as the daemon, and its README says the
    harness "mirrors the daemon exactly". The TRAIN argv did match
    distill.rs::train_command byte for byte; the EVAL argv was MISSING
    `--batch-size 1`, which is what made the omission look deliberate.

    Not cosmetic: mlx_lm 0.31.3's CONFIG_DEFAULTS batch_size is 4, and
    `tuner.trainer.iterate_batches` walks `range(0, len(idx) - batch_size + 1,
    batch_size)` over a length-sorted index — so the smoke's 6-row held-out split gave
    ONE batch and scored 4 of the 6 rows (the shortest four), where the daemon's
    `--batch-size 1` gives six batches and scores all six. The harness measured a
    different batching path from the shipped `eval_command` and could not catch a
    regression that only appears at 1."""

    def test_the_eval_argv_matches_eval_command(self):
        """THE REGRESSION: --batch-size was absent here and present in the daemon."""
        smoke = _lora_smoke()
        self.assertEqual(
            [a for a in smoke.eval_argv("") if a.startswith("--")],
            _daemon_argv_flags("eval_command"),
            "the smoke's eval argv and distill.rs::eval_command have drifted; the "
            "harness's entire claim is that it reproduces the daemon's measurement",
        )

    def test_the_train_argv_matches_train_command(self):
        smoke = _lora_smoke()
        self.assertEqual(
            [a for a in smoke.train_argv(smoke.RUN, 120) if a.startswith("--")],
            _daemon_argv_flags("train_command"),
            "the smoke's train argv and distill.rs::train_command have drifted",
        )

    def test_both_evals_pin_batch_size_one(self):
        """Explicit, because this is the value whose ABSENCE was invisible: at the
        mlx_lm default of 4 the 6-row held-out split scores only 4 rows."""
        smoke = _lora_smoke()
        for adapter in ("", smoke.RUN):
            argv = smoke.eval_argv(adapter)
            self.assertIn("--batch-size", argv)
            self.assertEqual(
                argv[argv.index("--batch-size") + 1], "1",
                "the daemon evaluates at --batch-size 1; anything else scores a "
                "different subset of the held-out split",
            )

    def test_the_base_eval_uses_the_empty_adapter_path(self):
        """mlx_lm's "test without LoRA layers". An OMITTED flag defaults to the dir
        `adapters` and fails, which is how an earlier run produced
        reject:unmeasurable."""
        argv = _lora_smoke().eval_argv("")
        self.assertEqual(argv[argv.index("--adapter-path") + 1], "")

    def test_the_smoke_builds_its_argv_through_the_shared_builders(self):
        """A builder nothing calls proves nothing: main() must go through them, or the
        two can drift again with these tests still green."""
        src = (LORA_SMOKE_DIR / "smoke.py").read_text(encoding="utf-8")
        body = src[src.index("def main("):]
        self.assertIn("eval_argv(", body)
        self.assertIn("train_argv(", body)
        self.assertNotIn(
            '"--test"', body,
            "main() is hand-rolling an mlx_lm argv again instead of using eval_argv",
        )


if __name__ == "__main__":
    unittest.main()
