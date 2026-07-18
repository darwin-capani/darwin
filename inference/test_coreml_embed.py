"""Unit tests for the PURE seams of the Core ML op=embed backend.

Runs WITHOUT loading any model and WITHOUT coremltools / torch / transformers:
`import coreml_embed` is import-light (stdlib + numpy) and the tested helpers
take plain Python / numpy inputs, so this exercises the tokenization padding,
the batch chunking, the vector post-processing (scrub), the empty-input guard,
and the embedder-id/dim selection + fallback-decision logic directly. The live
Core ML predict + FP16-vs-torch faithfulness are DEVICE/DEP-gated and exercised
by the once-run smoke (`.venv/bin/python inference/coreml_embed.py`), NOT here.

  Run: .venv/bin/python inference/test_coreml_embed.py   (from the repo root)
"""
import math
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import coreml_embed as ce  # noqa: E402
import server  # noqa: E402


class PadBatchTests(unittest.TestCase):
    def test_shape_padding_and_mask(self):
        ids, mask = ce.pad_batch([[5, 6, 7], [9]], seq=6)
        self.assertEqual(ids.shape, (2, 6))
        self.assertEqual(mask.shape, (2, 6))
        self.assertEqual(ids.dtype.name, "int32")
        self.assertEqual(mask.dtype.name, "int32")
        # Row 0: 3 real tokens then 0-padding; mask 1 on the reals only.
        self.assertEqual(list(ids[0]), [5, 6, 7, 0, 0, 0])
        self.assertEqual(list(mask[0]), [1, 1, 1, 0, 0, 0])
        self.assertEqual(list(ids[1]), [9, 0, 0, 0, 0, 0])
        self.assertEqual(list(mask[1]), [1, 0, 0, 0, 0, 0])

    def test_truncates_to_seq(self):
        ids, mask = ce.pad_batch([list(range(1, 200))], seq=ce.SEQ)
        self.assertEqual(ids.shape, (1, ce.SEQ))
        # Truncated to the FIRST SEQ ids; the whole row is real (mask all 1).
        self.assertEqual(list(ids[0]), list(range(1, ce.SEQ + 1)))
        self.assertTrue(all(m == 1 for m in mask[0]))

    def test_empty_row_is_all_padding(self):
        ids, mask = ce.pad_batch([[]], seq=4)
        self.assertEqual(list(ids[0]), [0, 0, 0, 0])
        self.assertEqual(list(mask[0]), [0, 0, 0, 0])


class PlanChunksTests(unittest.TestCase):
    def _check_cover(self, n, batch):
        plan = ce.plan_coreml_chunks(n, batch)
        # Contiguous, order-preserving, full cover, each chunk <= batch.
        flat = []
        for a, b in plan:
            self.assertLessEqual(b - a, batch)
            self.assertLess(a, b)
            flat.extend(range(a, b))
        self.assertEqual(flat, list(range(n)))
        return plan

    def test_exact_multiple(self):
        self.assertEqual(ce.plan_coreml_chunks(16, 8), [(0, 8), (8, 16)])

    def test_partial_last_chunk(self):
        self.assertEqual(ce.plan_coreml_chunks(10, 8), [(0, 8), (8, 10)])

    def test_single(self):
        self.assertEqual(ce.plan_coreml_chunks(1, 8), [(0, 1)])

    def test_empty(self):
        self.assertEqual(ce.plan_coreml_chunks(0, 8), [])

    def test_various_cover(self):
        for n in (1, 2, 7, 8, 9, 33, 256):
            self._check_cover(n, 8)

    def test_bad_batch_raises(self):
        with self.assertRaises(ValueError):
            ce.plan_coreml_chunks(4, 0)


class ScrubTests(unittest.TestCase):
    def test_non_finite_scrubbed_to_zero(self):
        out = ce.scrub_vector([1.0, float("nan"), float("inf"), float("-inf"), -2.5])
        self.assertEqual(out, [1.0, 0.0, 0.0, 0.0, -2.5])
        self.assertTrue(all(math.isfinite(x) for x in out))

    def test_finite_preserved(self):
        v = [0.1, -0.2, 0.3]
        self.assertEqual(ce.scrub_vector(v), v)


class NormalizeTextTests(unittest.TestCase):
    def test_empty_and_whitespace_become_space(self):
        self.assertEqual(ce.normalize_text(""), " ")
        self.assertEqual(ce.normalize_text("   "), " ")
        self.assertEqual(ce.normalize_text("\n\t"), " ")
        self.assertEqual(ce.normalize_text(None), " ")

    def test_real_text_preserved(self):
        self.assertEqual(ce.normalize_text("hello world"), "hello world")


class EmbedderSelectionTests(unittest.TestCase):
    """The embedder-id/dim selection + fallback-decision logic (server pure seams)."""

    def test_validate_known(self):
        self.assertEqual(
            server.validate_embedder("coreml-bge-small-en-v1.5"),
            server.EMBEDDER_COREML,
        )
        self.assertEqual(
            server.validate_embedder("llm-qwen3-4b-meanpool"),
            server.EMBEDDER_LLM_MEANPOOL,
        )

    def test_validate_unknown_raises(self):
        with self.assertRaises(ValueError):
            server.validate_embedder("nope")

    def test_plan_coreml_available(self):
        eid, dim, fell_back = server.plan_embedder(
            server.EMBEDDER_COREML, coreml_available=True
        )
        self.assertEqual(eid, server.EMBEDDER_COREML)
        self.assertEqual(dim, server.COREML_EMBED_DIM)
        self.assertEqual(dim, ce.DIM)  # server label matches the module's real dim
        self.assertFalse(fell_back)

    def test_plan_coreml_unavailable_falls_back(self):
        eid, dim, fell_back = server.plan_embedder(
            server.EMBEDDER_COREML, coreml_available=False
        )
        self.assertEqual(eid, server.EMBEDDER_LLM_MEANPOOL)
        self.assertIsNone(dim)  # 4B dim known only from a produced vector
        self.assertTrue(fell_back)

    def test_plan_llm_choice_is_not_a_fallback(self):
        eid, dim, fell_back = server.plan_embedder(
            server.EMBEDDER_LLM_MEANPOOL, coreml_available=True
        )
        self.assertEqual(eid, server.EMBEDDER_LLM_MEANPOOL)
        self.assertIsNone(dim)
        self.assertFalse(fell_back)

    def test_contract_ids_are_stable_strings(self):
        # The exact wire ids the Rust docsearch task consumes. Guard against
        # accidental drift.
        self.assertEqual(server.EMBEDDER_COREML, "coreml-bge-small-en-v1.5")
        self.assertEqual(server.EMBEDDER_LLM_MEANPOOL, "llm-qwen3-4b-meanpool")
        self.assertEqual(ce.EMBEDDER_ID, server.EMBEDDER_COREML)
        self.assertEqual(server.DEFAULT_EMBEDDER, server.EMBEDDER_COREML)


@unittest.skipUnless(
    sys.version_info >= (3, 11), "tomllib (py3.11+) required; runtime is 3.11"
)
class ConfigDefaultTests(unittest.TestCase):
    def test_shipped_config_defaults_to_coreml(self):
        settings = server.load_config()
        self.assertEqual(settings["embedder"], server.EMBEDDER_COREML)


if __name__ == "__main__":
    unittest.main(verbosity=2)
