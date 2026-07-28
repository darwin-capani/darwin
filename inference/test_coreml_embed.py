"""Unit tests for the PURE seams of the Core ML op=embed backend.

Runs WITHOUT loading any model and WITHOUT coremltools / torch / transformers:
`import coreml_embed` is import-light (stdlib + numpy) and the tested helpers
take plain Python / numpy inputs, so this exercises the tokenization padding /
truncation to SEQ, the vector post-processing (scrub), the empty-input guard,
the model-derived mean-pool space id, and the embedder-id/dim selection +
fallback-decision logic directly. The live Core ML predict + FP16-vs-torch
faithfulness + latency are DEVICE/DEP-gated and exercised by the once-run smoke
(`.venv/bin/python inference/coreml_embed.py`), NOT here.

  Run: .venv/bin/python inference/test_coreml_embed.py   (from the repo root)
"""
import math
import os
import time
import inspect
import importlib.util
import sys
import tempfile
import threading
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
        # Input longer than SEQ -> truncated to the FIRST SEQ ids.
        ids, mask = ce.pad_batch([list(range(1, ce.SEQ + 100))], seq=ce.SEQ)
        self.assertEqual(ids.shape, (1, ce.SEQ))
        self.assertEqual(list(ids[0]), list(range(1, ce.SEQ + 1)))
        self.assertTrue(all(m == 1 for m in mask[0]))

    def test_empty_row_is_all_padding(self):
        ids, mask = ce.pad_batch([[]], seq=4)
        self.assertEqual(list(ids[0]), [0, 0, 0, 0])
        self.assertEqual(list(mask[0]), [0, 0, 0, 0])


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

    _MP = "llm-meanpool:some/model:int4"  # a model-derived mean-pool space id

    def test_plan_coreml_available(self):
        eid, dim, fell_back = server.plan_embedder(
            server.EMBEDDER_COREML, coreml_available=True, meanpool_id=self._MP
        )
        self.assertEqual(eid, server.EMBEDDER_COREML)
        self.assertEqual(dim, server.COREML_EMBED_DIM)
        self.assertEqual(dim, ce.DIM)  # server label matches the module's real dim
        self.assertFalse(fell_back)

    def test_plan_coreml_unavailable_falls_back_to_meanpool_id(self):
        eid, dim, fell_back = server.plan_embedder(
            server.EMBEDDER_COREML, coreml_available=False, meanpool_id=self._MP
        )
        # The fallback emits the MODEL-DERIVED id (not the fixed config value).
        self.assertEqual(eid, self._MP)
        self.assertIsNone(dim)  # mean-pool dim known only from a produced vector
        self.assertTrue(fell_back)

    def test_plan_meanpool_choice_is_not_a_fallback(self):
        eid, dim, fell_back = server.plan_embedder(
            server.EMBEDDER_LLM_MEANPOOL, coreml_available=True, meanpool_id=self._MP
        )
        self.assertEqual(eid, self._MP)
        self.assertIsNone(dim)
        self.assertFalse(fell_back)

    def test_contract_ids(self):
        # The fixed Core ML space id + the CONFIG-SELECTOR value. Guard drift.
        self.assertEqual(server.EMBEDDER_COREML, "coreml-bge-small-en-v1.5")
        self.assertEqual(server.EMBEDDER_LLM_MEANPOOL, "llm-qwen3-4b-meanpool")
        self.assertEqual(ce.EMBEDDER_ID, server.EMBEDDER_COREML)
        self.assertEqual(server.DEFAULT_EMBEDDER, server.EMBEDDER_COREML)

    def test_meanpool_space_id_is_model_derived(self):
        # The EMITTED mean-pool id must reflect the resident model (id + quant),
        # so an LLM/quant swap changes the vector-space stamp. Construct a real
        # engine (no model load) and check the format.
        settings = dict(server.load_config())
        settings["llm"] = "some/model-a-4bit"
        settings["quant"] = "int4"
        eng = server.InferenceEngine(settings, "classify {utterance}", "persona")
        self.assertEqual(
            eng._meanpool_space_id(), "llm-meanpool:some/model-a-4bit:int4"
        )
        # Swapping the model changes the emitted space id.
        settings2 = dict(settings)
        settings2["llm"] = "other/model-b-4bit"
        eng2 = server.InferenceEngine(settings2, "classify {utterance}", "persona")
        self.assertNotEqual(eng._meanpool_space_id(), eng2._meanpool_space_id())


@unittest.skipUnless(
    sys.version_info >= (3, 11), "tomllib (py3.11+) required; runtime is 3.11"
)
class ConfigDefaultTests(unittest.TestCase):
    def test_shipped_config_defaults_to_coreml(self):
        settings = server.load_config()
        self.assertEqual(settings["embedder"], server.EMBEDDER_COREML)


class TruncationSurfacingTests(unittest.TestCase):
    """SEQ-cap truncation must NOT be silent — `_encode` emits a throttled warn
    naming how many inputs were capped (review-caught: the docstring claimed it
    was surfaced but nothing signalled it at runtime). Pure: stub tokenizer."""

    def _engine_with(self, id_rows):
        import coreml_embed as ce
        e = ce.CoreMLEmbedder.__new__(ce.CoreMLEmbedder)
        e._trunc_seen = 0
        e._trunc_warned_at = 0
        e._tokenizer = lambda norm, **kw: {"input_ids": id_rows}
        return ce, e

    def _capture(self, ce):
        import logging
        msgs = []
        h = logging.Handler()
        h.emit = lambda r: msgs.append(r.getMessage())
        ce.log.addHandler(h)
        ce.log.setLevel(logging.WARNING)
        return msgs

    def test_truncated_input_emits_a_warning(self):
        ce, e = self._engine_with([[1] * ce_SEQ(), [1, 2, 3]])
        msgs = self._capture(ce)
        rows = e._encode(["x" * 5000, "hi"])
        self.assertEqual([len(r) for r in rows], [ce_SEQ(), 3])
        self.assertTrue(any("exceeded the" in m and "cap" in m for m in msgs))
        self.assertEqual(e._trunc_seen, 1)

    def test_no_truncation_is_silent(self):
        ce, e = self._engine_with([[1, 2, 3], [4, 5]])  # both under SEQ
        msgs = self._capture(ce)
        e._encode(["hi", "yo"])
        self.assertFalse(any("cap" in m for m in msgs))
        self.assertEqual(e._trunc_seen, 0)


class FastPathRouting(unittest.TestCase):
    """The SEQ_FAST routing rule (see coreml_embed.SEQ_FAST). Pure/no-device: we
    assert WHICH graph a given token length selects, never that the ANE is used.

    MEASURED on an M1 Pro (2026-07-28) and recorded here so a regression is
    visible: the (1,512) graph costs ~19.65 ms per text no matter how few real
    tokens it carries (masking padding saves nothing), while the (1,128) graph
    costs ~1.84 ms. EQUIVALENCE, re-measured over 22 texts (3..122 tokens): 20 of
    22 come out BIT-identical between the two graphs and the worst normalized
    cosine is 0.999999. In the mixed case production actually hits — a corpus
    already stored from the 512 graph, queried on the 128 graph — that is 0/8
    top-5 order flips and 0/176 changes to the MIN_NEURAL_SIM=0.50 gate decision.
    (An earlier draft here cited 0.999097 as "the 512 graph's own run-to-run
    noise floor". Both halves were wrong: that was an UNNORMALIZED dot product,
    and the 512 graph is exactly deterministic — its run-to-run delta is 0.)"""

    def test_short_text_takes_the_fast_graph_long_text_does_not(self):
        import coreml_embed as ce

        # A recall query / stored fact (3-15 real tokens) fits the fast graph.
        self.assertLessEqual(9, ce.SEQ_FAST)
        # A docsearch chunk (~1200 chars => ~242 WordPiece tokens) does NOT —
        # routing it to the fast graph would TRUNCATE it, the exact file-search
        # regression the SEQ comment records. It must take the 512 graph.
        self.assertGreater(242, ce.SEQ_FAST)
        self.assertGreaterEqual(ce.SEQ, 242)

    def test_fast_graph_is_optional_and_never_load_bearing(self):
        """A cache without the fast graph (an older one, or a failed optional
        conversion) must still embed — just without the speedup."""
        import coreml_embed as ce

        e = ce.CoreMLEmbedder.__new__(ce.CoreMLEmbedder)
        e._lock = __import__("threading").Lock()
        e._loaded = True
        e._model_fast = None            # no fast graph available
        e._trunc_seen = 0
        e._trunc_warned_at = 0
        calls = []

        class Tok:
            def __call__(self, texts, **kw):
                return {"input_ids": [[101, 5, 102] for _ in texts]}

        e._tokenizer = Tok()
        e._encode = lambda texts: [[101, 5, 102] for _ in texts]

        def fake_predict(ids, mask, model=None):
            calls.append((ids.shape[1], model))
            import numpy as _np
            return _np.ones((1, ce.DIM), dtype=_np.float32) / (ce.DIM ** 0.5)

        e._predict = fake_predict
        out = e.embed(["hi"])
        self.assertEqual(len(out), 1)
        # With no fast graph it must use the SEQ shape, not crash.
        self.assertEqual(calls[0][0], ce.SEQ)
        self.assertIsNone(calls[0][1])

    def test_short_text_routes_to_the_fast_graph_when_present(self):
        import coreml_embed as ce

        e = ce.CoreMLEmbedder.__new__(ce.CoreMLEmbedder)
        e._lock = __import__("threading").Lock()
        e._loaded = True
        e._model_fast = object()        # a fast graph IS available
        e._trunc_seen = 0
        e._trunc_warned_at = 0
        calls = []
        # one short row (fits SEQ_FAST) and one long row (does not)
        rows = [[1] * 9, [1] * (ce.SEQ_FAST + 1)]
        e._encode = lambda texts: rows

        def fake_predict(ids, mask, model=None):
            calls.append(ids.shape[1])
            import numpy as _np
            return _np.ones((1, ce.DIM), dtype=_np.float32) / (ce.DIM ** 0.5)

        e._predict = fake_predict
        e.embed(["short", "long"])
        self.assertEqual(
            calls, [ce.SEQ_FAST, ce.SEQ],
            "short text must take the fast graph; long text must NOT be truncated",
        )


def ce_SEQ():
    import coreml_embed as ce
    return ce.SEQ


@unittest.skipUnless(
    importlib.util.find_spec("coremltools") is not None,
    "drives ensure_loaded(), which requires coremltools",
)
class FastGraphUpgrade(unittest.TestCase):
    """A cache published BEFORE the fast graph existed still validate-loads, so
    without an upgrade an already-deployed machine would keep it forever and never
    pick the speedup up. The upgrade must therefore fire - but it must fire in the
    BACKGROUND (a rebuild measured 43.8 s and the daemon's inference timeout is 30 s
    with no retry), exactly once per process, and it must leave a sentinel on EVERY
    outcome that yields no usable fast graph, or the next start rebuilds again."""

    def _obj(self, tmp, sequence):
        """`sequence` is the list of _try_load_final results, consumed in order."""
        o = ce.CoreMLEmbedder.__new__(ce.CoreMLEmbedder)
        o._dir = tmp
        o._lock = threading.Lock()
        o._loaded = False
        o._upgrade_started = False
        o._tokenizer = o._model = o._model_fast = None
        o.converts = 0
        pending = list(sequence)
        o._try_load_final = lambda: pending.pop(0) if pending else None

        def convert():
            o.converts += 1
        o._convert_atomic = convert
        return o

    def _load_and_settle(self, o):
        """ensure_loaded() plus a bounded wait for the background rebuild, so the
        assertions below see the finished state rather than a race."""
        o.ensure_loaded()
        for _ in range(400):
            if not o._upgrade_started or o.converts:
                break
            time.sleep(0.005)
        for t in threading.enumerate():
            if t.name.endswith("-fast-upgrade") and t is not threading.current_thread():
                t.join(timeout=5)

    def _sentinel(self, tmp):
        return os.path.exists(os.path.join(tmp, ce._FAST_ABSENT_MARK))

    def test_a_cache_without_the_short_graph_is_rebuilt(self):
        with tempfile.TemporaryDirectory() as tmp:
            o = self._obj(tmp, [("t", "m", None), ("t", "m", "FAST")])
            self._load_and_settle(o)
            # MUTATION GUARD: dropping the upgrade leaves converts at 0.
            self.assertEqual(o.converts, 1)
            self.assertEqual(o._model_fast, "FAST")

    def test_the_rebuild_does_not_block_the_caller(self):
        """The whole point of the thread: ensure_loaded must RETURN while the rebuild
        is still running, or the first op after a deploy blows the 30 s timeout."""
        with tempfile.TemporaryDirectory() as tmp:
            o = self._obj(tmp, [("t", "m", None), ("t", "m", "FAST")])
            release = threading.Event()

            def slow_convert():
                release.wait(timeout=5)
                o.converts += 1
            o._convert_atomic = slow_convert
            started = time.perf_counter()
            o.ensure_loaded()
            elapsed = time.perf_counter() - started
            self.assertLess(elapsed, 1.0, "ensure_loaded blocked on the rebuild")
            self.assertEqual(o.converts, 0)   # still in flight
            self.assertEqual(o._model, "m")   # and already serving
            release.set()

    def test_a_cache_that_already_has_the_short_graph_is_not_rebuilt(self):
        with tempfile.TemporaryDirectory() as tmp:
            o = self._obj(tmp, [("t", "m", "FAST")])
            self._load_and_settle(o)
            self.assertEqual(o.converts, 0)

    def test_the_sentinel_stops_the_upgrade_from_running_every_start(self):
        with tempfile.TemporaryDirectory() as tmp:
            open(os.path.join(tmp, ce._FAST_ABSENT_MARK), "w").close()
            o = self._obj(tmp, [("t", "m", None)])
            self._load_and_settle(o)
            self.assertEqual(o.converts, 0)
            self.assertIsNone(o._model_fast)

    def test_a_rebuild_that_still_yields_no_short_graph_is_not_repeated_next_start(self):
        """THE REGRESSION THIS EXISTS FOR. A fast graph that CONVERTS but fails to
        validate leaves _load_from returning fast=None with no sentinel. Before the
        fix that meant four process starts cost four full rebuilds - measured 43.8 s
        each - forever. One instance cannot show this: `_loaded` latches, so a single
        ensure_loaded() calls convert once no matter what. It takes FRESH instances
        against the SAME on-disk state, which is what a process restart is."""
        with tempfile.TemporaryDirectory() as tmp:
            total = 0
            for start in range(4):
                o = self._obj(tmp, [("t", "m", None), ("t", "m", None)])
                self._load_and_settle(o)
                total += o.converts
            self.assertTrue(self._sentinel(tmp), "no sentinel was ever written")
            self.assertEqual(
                total, 1, f"{total} rebuilds across 4 starts; expected exactly 1"
            )

    def test_a_fresh_conversion_with_no_short_graph_also_leaves_a_sentinel(self):
        """The other uncovered path: the cache did not exist at all, we converted it,
        and the fast graph still is not usable. Rebuilding cannot help, so the next
        start must not try."""
        with tempfile.TemporaryDirectory() as tmp:
            o = self._obj(tmp, [None, ("t", "m", None)])
            self._load_and_settle(o)
            self.assertEqual(o.converts, 1)
            self.assertTrue(self._sentinel(tmp))
            o2 = self._obj(tmp, [("t", "m", None)])
            self._load_and_settle(o2)
            self.assertEqual(o2.converts, 0, "the sentinel did not stop the retry")

    def test_a_failed_rebuild_keeps_the_working_cache(self):
        with tempfile.TemporaryDirectory() as tmp:
            o = self._obj(tmp, [("t", "m", None)])

            def boom():
                o.converts += 1
                raise RuntimeError("disk full")
            o._convert_atomic = boom
            self._load_and_settle(o)   # must NOT raise: the SEQ cache still works
            self.assertEqual(o.converts, 1)
            self.assertEqual((o._tokenizer, o._model), ("t", "m"))
            self.assertTrue(o._loaded)
            self.assertTrue(self._sentinel(tmp))

    def test_the_upgrade_starts_at_most_once_per_process(self):
        with tempfile.TemporaryDirectory() as tmp:
            o = self._obj(tmp, [("t", "m", None), ("t", "m", None)])
            self._load_and_settle(o)
            before = o.converts
            o._schedule_fast_upgrade()   # a second call must be inert
            self.assertEqual(o.converts, before)

    def test_a_thread_that_cannot_start_never_reaches_the_caller(self):
        """The optional block sits OUTSIDE ensure_loaded's exception funnel, so an
        unguarded raise there escapes as a NON-Unavailable error. The server's preload
        catches that with a bare `except Exception` and latches the backend onto a
        DIFFERENT vector space for the whole process life - an optimization that could
        not spawn a thread costing the real embedder. threading.Thread.start() raises
        RuntimeError once the per-task thread ceiling is reached, so this is reachable."""
        with tempfile.TemporaryDirectory() as tmp:
            o = self._obj(tmp, [("t", "m", None)])
            real = threading.Thread.start
            threading.Thread.start = lambda s: (_ for _ in ()).throw(
                RuntimeError("can't start new thread")
            )
            try:
                o.ensure_loaded()   # MUTATION GUARD: unguarded, this raises
            finally:
                threading.Thread.start = real
            self.assertTrue(o._loaded)
            self.assertEqual((o._tokenizer, o._model), ("t", "m"))

    def test_the_live_swap_leaves_the_tokenizer_alone(self):
        """`_encode` reads self._tokenizer OUTSIDE the load lock while the models are
        read inside it, so rebinding the tokenizer in the background swap would be
        observable mid-call. It is byte-identical across generations anyway."""
        with tempfile.TemporaryDirectory() as tmp:
            o = self._obj(tmp, [("OLD_TOK", "OLD_M", None), ("NEW_TOK", "NEW_M", "FAST")])
            self._load_and_settle(o)
            self.assertEqual(o._model, "NEW_M")
            self.assertEqual(o._model_fast, "FAST")
            self.assertEqual(o._tokenizer, "OLD_TOK", "the swap rebound the tokenizer")

    def test_the_sentinel_write_never_raises(self):
        """mark_fast_absent runs where the fast graph is already lost; if it could
        throw (a full or read-only cache root) it would take the whole backend with
        it - which is how the optional optimization became fatal once already."""
        self.assertFalse(ce.mark_fast_absent("/nonexistent/read-only/path", "why"))


class SentinelWriteIsNeverFatal(unittest.TestCase):
    """An OPTIONAL fast-graph failure must never cost the REQUIRED artifacts. The
    tokenizer is written before the optional graph is attempted, so a cache can never
    be published complete-but-tokenizer-less."""

    def test_the_tokenizer_is_saved_before_the_optional_graph_is_attempted(self):
        src = inspect.getsource(ce.CoreMLEmbedder._convert_into)
        tok_at = src.index("tok.save_pretrained")
        fast_at = src.index("_MODEL_FAST_NAME")
        self.assertLess(
            tok_at, fast_at,
            "the tokenizer must be written BEFORE the optional graph is attempted",
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
