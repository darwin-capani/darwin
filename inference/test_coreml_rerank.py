"""Unit tests for the PURE seams of the Core ML op=rerank backend (stage two of
the two-stage retrieval stack).

Runs WITHOUT loading any model and WITHOUT coremltools / torch / transformers:
`import coreml_rerank` is import-light (stdlib + numpy) and the tested helpers take
plain Python / numpy inputs, so this exercises the pair padding / truncation to SEQ,
the score post-processing (scrub), the empty-input guard, the Spearman faithfulness
helper, the reranker-id contract, and the server's rerank id / honest-fallback logic
directly. The live Core ML predict + FP16-vs-torch faithfulness + latency are
DEVICE/DEP-gated and exercised by the once-run smokes
(`.venv/bin/python inference/coreml_rerank.py` and the committed
inference/benchmarks/coreml_rerank_eval/ probe), NOT here.

  Run: .venv/bin/python inference/test_coreml_rerank.py   (from the repo root)
"""
import math
import os
import time
import inspect
import importlib.util
import sys
import tempfile
import threading
import types
import unittest
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parent))

import coreml_rerank as cr  # noqa: E402
import coreml_shared  # noqa: E402
import server  # noqa: E402


class PadPairTests(unittest.TestCase):
    def test_shape_padding_mask_and_types(self):
        ids, types, mask = cr.pad_pair([5, 6, 7], [0, 0, 1], seq=6)
        for arr in (ids, types, mask):
            self.assertEqual(arr.shape, (1, 6))
            self.assertEqual(arr.dtype.name, "int32")
        # 3 real tokens then 0-padding; mask 1 on the reals only.
        self.assertEqual(list(ids[0]), [5, 6, 7, 0, 0, 0])
        self.assertEqual(list(types[0]), [0, 0, 1, 0, 0, 0])  # segment ids preserved
        self.assertEqual(list(mask[0]), [1, 1, 1, 0, 0, 0])

    def test_truncates_to_seq(self):
        n = cr.SEQ + 50
        ids, types, mask = cr.pad_pair(list(range(1, n)), [1] * (n - 1), seq=cr.SEQ)
        self.assertEqual(ids.shape, (1, cr.SEQ))
        self.assertEqual(list(ids[0]), list(range(1, cr.SEQ + 1)))
        self.assertTrue(all(m == 1 for m in mask[0]))

    def test_empty_row_is_all_padding(self):
        ids, types, mask = cr.pad_pair([], [], seq=4)
        self.assertEqual(list(ids[0]), [0, 0, 0, 0])
        self.assertEqual(list(types[0]), [0, 0, 0, 0])
        self.assertEqual(list(mask[0]), [0, 0, 0, 0])


class ScrubScoreTests(unittest.TestCase):
    def test_finite_preserved(self):
        self.assertEqual(cr.scrub_score(6.25), 6.25)
        self.assertEqual(cr.scrub_score(-3.0), -3.0)

    def test_nan_and_neg_inf_sink_to_bottom(self):
        # A degenerate score must be FINITE and sort BELOW any real logit.
        self.assertTrue(math.isfinite(cr.scrub_score(float("nan"))))
        self.assertTrue(math.isfinite(cr.scrub_score(float("-inf"))))
        self.assertLess(cr.scrub_score(float("nan")), -1e29)
        self.assertLess(cr.scrub_score(float("-inf")), -1e29)

    def test_pos_inf_floats_to_top(self):
        self.assertTrue(math.isfinite(cr.scrub_score(float("inf"))))
        self.assertGreater(cr.scrub_score(float("inf")), 1e29)


class NormalizeTextTests(unittest.TestCase):
    def test_empty_and_whitespace_become_space(self):
        self.assertEqual(cr.normalize_text(""), " ")
        self.assertEqual(cr.normalize_text("   "), " ")
        self.assertEqual(cr.normalize_text("\n\t"), " ")
        self.assertEqual(cr.normalize_text(None), " ")

    def test_real_text_preserved(self):
        self.assertEqual(cr.normalize_text("dark roast"), "dark roast")


class SpearmanTests(unittest.TestCase):
    def test_identity_is_one(self):
        a = [3.0, 1.0, 2.0, 5.0]
        self.assertAlmostEqual(cr._spearman(a, a), 1.0, places=6)

    def test_reverse_is_minus_one(self):
        a = [1.0, 2.0, 3.0, 4.0]
        self.assertAlmostEqual(cr._spearman(a, a[::-1]), -1.0, places=6)

    def test_monotonic_transform_preserves_rank(self):
        # sigmoid is monotonic, so ranking by raw logit == ranking by probability.
        a = [-2.0, 0.5, 3.0, 1.0]
        b = [1 / (1 + math.exp(-x)) for x in a]
        self.assertAlmostEqual(cr._spearman(a, b), 1.0, places=6)


class RerankerContractTests(unittest.TestCase):
    """The reranker id contract + the server default. Guard drift."""

    def test_contract_ids(self):
        self.assertEqual(server.RERANKER_COREML, "coreml-ms-marco-minilm-l6-v2")
        self.assertEqual(cr.RERANKER_ID, server.RERANKER_COREML)
        self.assertTrue(server.DEFAULT_RERANKER_ENABLED)  # ships ON — measured winner
        # The server cap mirrors the module cap.
        self.assertEqual(server.RERANK_MAX_PASSAGES, cr.MAX_PASSAGES)


@unittest.skipUnless(
    sys.version_info >= (3, 11), "tomllib (py3.11+) required; runtime is 3.11"
)
class ConfigDefaultTests(unittest.TestCase):
    def test_shipped_config_defaults_reranker_on(self):
        settings = server.load_config()
        self.assertTrue(settings["reranker"])

    def test_non_boolean_reranker_keeps_default(self):
        # Parsed like preload/speculative: a non-boolean value keeps the default.
        eng_settings = server.load_config()
        self.assertIsInstance(eng_settings["reranker"], bool)


class RerankFallbackTests(unittest.TestCase):
    """The server's op=rerank id + HONEST-fallback logic (no model load)."""

    def _engine(self):
        return server.InferenceEngine(
            server.load_config(), "classify {utterance}", "persona"
        )

    def test_unavailable_falls_back_to_order_preserving_dense(self):
        eng = self._engine()
        eng._coreml_rerank_unavailable = True  # force the fallback branch
        scores, rid, fell_back = eng.rerank_with_meta("q", ["a", "b", "c"])
        self.assertTrue(fell_back)
        self.assertEqual(rid, "")  # no model produced an order
        # Order-preserving (strictly descending) so a re-sort is the identity.
        self.assertEqual(scores, sorted(scores, reverse=True))
        self.assertEqual(len(scores), 3)

    def test_empty_passages_is_not_a_fallback(self):
        eng = self._engine()
        scores, rid, fell_back = eng.rerank_with_meta("q", [])
        self.assertEqual(scores, [])
        self.assertEqual(rid, server.RERANKER_COREML)
        self.assertFalse(fell_back)

    def test_batch_cap_enforced(self):
        eng = self._engine()
        with self.assertRaises(ValueError):
            eng.rerank_with_meta("q", ["x"] * (server.RERANK_MAX_PASSAGES + 1))

    def test_type_validation(self):
        eng = self._engine()
        with self.assertRaises(ValueError):
            eng.rerank_with_meta(123, ["x"])
        with self.assertRaises(ValueError):
            eng.rerank_with_meta("q", "not a list")
        with self.assertRaises(ValueError):
            eng.rerank_with_meta("q", ["ok", 5])


class TruncationSurfacingTests(unittest.TestCase):
    """SEQ-cap truncation must NOT be silent — `_encode` emits a throttled warn
    naming how many pairs were capped. Pure: stub tokenizer (no model)."""

    def _reranker_with(self, id_rows, type_rows):
        e = cr.CoreMLReranker.__new__(cr.CoreMLReranker)
        e._trunc_seen = 0
        e._trunc_warned_at = 0
        e._tokenizer = lambda qs, ps, **kw: {
            "input_ids": id_rows, "token_type_ids": type_rows
        }
        return e

    def _capture(self):
        import logging
        msgs = []
        h = logging.Handler()
        h.emit = lambda r: msgs.append(r.getMessage())
        cr.log.addHandler(h)
        cr.log.setLevel(logging.WARNING)
        return msgs

    def test_truncated_pair_emits_a_warning(self):
        e = self._reranker_with([[1] * cr.SEQ, [1, 2, 3]], [[0] * cr.SEQ, [0, 0, 1]])
        msgs = self._capture()
        ids, types = e._encode("q", ["x" * 5000, "hi"])
        self.assertEqual([len(r) for r in ids], [cr.SEQ, 3])
        self.assertTrue(any("exceeded the" in m and "cap" in m for m in msgs))
        self.assertEqual(e._trunc_seen, 1)

    def test_no_truncation_is_silent(self):
        e = self._reranker_with([[1, 2, 3], [4, 5]], [[0, 0, 1], [0, 1]])
        msgs = self._capture()
        e._encode("q", ["hi", "yo"])
        self.assertFalse(any("cap" in m for m in msgs))
        self.assertEqual(e._trunc_seen, 0)


class FastPathRouting(unittest.TestCase):
    """A pair that already fits in SEQ_FAST is scored on the SHORT graph; anything
    longer stays on the SEQ graph. Routing is a speed decision ONLY — it must never
    truncate a pair the SEQ graph would have kept, and it must vanish entirely when
    the optional short graph is absent. Pure: stub models, no Core ML."""

    def _reranker(self, id_rows, type_rows, fast):
        r = cr.CoreMLReranker.__new__(cr.CoreMLReranker)
        r._lock = threading.Lock()
        r._loaded = True
        r._trunc_seen = 0
        r._trunc_warned_at = 0
        r._tokenizer = lambda qs, ps, **kw: {
            "input_ids": id_rows, "token_type_ids": type_rows
        }
        r.calls = []

        def graph(tag):
            def predict(d):
                r.calls.append((tag, d["input_ids"].shape[1]))
                return {"score": np.array([[1.0]], dtype=np.float32)}
            return types.SimpleNamespace(predict=predict)

        r._model = graph("seq")
        r._model_fast = graph("fast") if fast else None
        r.ensure_loaded = lambda: None
        return r

    def test_short_pair_routes_to_the_fast_graph_when_present(self):
        short = [1] * (cr.SEQ_FAST - 4)
        r = self._reranker([short], [[0] * len(short)], fast=True)
        r.rerank("q", ["hi"])
        # MUTATION GUARD: dropping the routing branch makes this ("seq", 512).
        self.assertEqual(r.calls, [("fast", cr.SEQ_FAST)])

    def test_a_pair_longer_than_the_fast_seq_stays_on_the_full_graph(self):
        long_row = [1] * (cr.SEQ_FAST + 1)
        r = self._reranker([long_row], [[0] * len(long_row)], fast=True)
        r.rerank("q", ["x"])
        # Routing this to the short graph would silently DROP a real token.
        self.assertEqual(r.calls, [("seq", cr.SEQ)])

    def test_a_mixed_batch_routes_each_pair_independently(self):
        rows = [[1] * 30, [1] * (cr.SEQ_FAST + 50), [1] * cr.SEQ_FAST]
        r = self._reranker(rows, [[0] * len(x) for x in rows], fast=True)
        r.rerank("q", ["a", "b", "c"])
        self.assertEqual(
            r.calls,
            [("fast", cr.SEQ_FAST), ("seq", cr.SEQ), ("fast", cr.SEQ_FAST)],
        )

    def test_without_the_short_graph_every_pair_runs_at_seq(self):
        r = self._reranker([[1] * 10], [[0] * 10], fast=False)
        r.rerank("q", ["hi"])
        self.assertEqual(r.calls, [("seq", cr.SEQ)])

    def test_the_boundary_length_still_fits_the_fast_graph(self):
        exact = [1] * cr.SEQ_FAST
        r = self._reranker([exact], [[0] * cr.SEQ_FAST], fast=True)
        r.rerank("q", ["hi"])
        # <= SEQ_FAST, so pad_pair does not truncate at exactly the boundary.
        self.assertEqual(r.calls, [("fast", cr.SEQ_FAST)])

    def test_the_fast_seq_is_shorter_than_the_full_seq(self):
        self.assertLess(cr.SEQ_FAST, cr.SEQ)


@unittest.skipUnless(
    importlib.util.find_spec("coremltools") is not None,
    "drives ensure_loaded(), which requires coremltools",
)
class FastGraphUpgrade(unittest.TestCase):
    """A cache published BEFORE the short-sequence graph existed still validate-loads, so
    without an upgrade an already-deployed machine would keep it forever and never
    pick the speedup up. The upgrade must therefore fire - but it must fire in the
    BACKGROUND (a rebuild measured 43.8 s and the daemon's inference timeout is 30 s
    with no retry), exactly once per process, and it must leave a sentinel on EVERY
    outcome that yields no usable short-sequence graph, or the next start rebuilds again."""

    def _obj(self, tmp, sequence):
        """`sequence` is the list of _try_load_final results, consumed in order."""
        o = cr.CoreMLReranker.__new__(cr.CoreMLReranker)
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
        return os.path.exists(os.path.join(tmp, coreml_shared.FAST_ABSENT_MARK))

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
            for _ in range(coreml_shared.MAX_FAST_ATTEMPTS):
                coreml_shared.note_fast_attempt(tmp, "exhausted in the test")
            o = self._obj(tmp, [("t", "m", None)])
            self._load_and_settle(o)
            self.assertEqual(o.converts, 0)
            self.assertIsNone(o._model_fast)

    def test_a_fresh_conversion_with_no_short_graph_charges_the_budget(self):
        """The other uncovered path: the cache did not exist at all, we converted it,
        and the short-sequence graph still is not usable. Rebuilding cannot help, so the next
        start must not try."""
        with tempfile.TemporaryDirectory() as tmp:
            o = self._obj(tmp, [None, ("t", "m", None)])
            self._load_and_settle(o)
            self.assertEqual(o.converts, 1)
            self.assertEqual(coreml_shared.fast_attempts(tmp), 1, "budget not charged")
            # Spend the rest of the budget, then the retry must stop.
            while not coreml_shared.fast_upgrade_exhausted(tmp):
                coreml_shared.note_fast_attempt(tmp, "spending the budget")
            o2 = self._obj(tmp, [("t", "m", None)])
            self._load_and_settle(o2)
            self.assertEqual(o2.converts, 0, "an exhausted budget did not stop the retry")

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

    def test_the_budget_allows_at_least_one_retry(self):
        """MUTATION GUARD. Every other budget test reads MAX_FAST_ATTEMPTS
        symbolically, so they all still pass at a budget of 1 - which IS the broken
        permanent-sentinel behaviour this replaced. The point of a budget is that a
        transient failure gets another go, so it must exceed 1."""
        self.assertGreater(
            coreml_shared.MAX_FAST_ATTEMPTS, 1,
            "a budget of 1 is the permanent sentinel that pinned a machine to the "
            "slow path over a transient concurrency fault",
        )

    def test_a_transient_failure_is_retried_not_recorded_as_permanent(self):
        """THE REGRESSION THIS EXISTS FOR. The first real deploy hit a TRANSIENT
        failure - two Core ML conversions ran concurrently and corrupted each other -
        and a single binary sentinel recorded it as PERMANENT, pinning the machine to
        the slow path for good. A retry budget lets a transient fault heal on the next
        start while still capping a deterministic one."""
        with tempfile.TemporaryDirectory() as tmp:
            attempts = 0
            for start in range(coreml_shared.MAX_FAST_ATTEMPTS - 1):
                o = self._obj(tmp, [("t", "m", None), ("t", "m", None)])
                self._load_and_settle(o)
                attempts += o.converts
            # Budget not spent yet, so a transient failure still gets another go.
            self.assertEqual(attempts, coreml_shared.MAX_FAST_ATTEMPTS - 1)
            self.assertFalse(coreml_shared.fast_upgrade_exhausted(tmp))
            # And a start that finally succeeds picks the speedup up.
            o = self._obj(tmp, [("t", "m", None), ("t", "m", "FAST")])
            self._load_and_settle(o)
            self.assertEqual(o._model_fast, "FAST")

    def test_the_budget_stops_a_deterministic_failure(self):
        with tempfile.TemporaryDirectory() as tmp:
            total = 0
            for start in range(coreml_shared.MAX_FAST_ATTEMPTS + 3):
                o = self._obj(tmp, [("t", "m", None), ("t", "m", None)])
                self._load_and_settle(o)
                total += o.converts
            self.assertEqual(
                total, coreml_shared.MAX_FAST_ATTEMPTS,
                f"{{total}} rebuilds; the budget is coreml_shared.MAX_FAST_ATTEMPTS",
            )
            self.assertTrue(coreml_shared.fast_upgrade_exhausted(tmp))

    def test_conversions_are_serialized_process_wide(self):
        """coremltools keeps global MIL state, so two overlapping conversions corrupt
        each other - reproduced on the real converters, and it is what broke the first
        deploy. Every backend must take the SAME lock."""
        self.assertIs(cr.CONVERT_LOCK, coreml_shared.CONVERT_LOCK)
        # Behavioural, not textual: a COMMENT naming CONVERT_LOCK must not satisfy
        # this. Hold the lock, then prove _convert_atomic actually blocks on it.
        entered = threading.Event()
        o = cr.CoreMLReranker.__new__(cr.CoreMLReranker)
        with tempfile.TemporaryDirectory() as tmp:
            o._dir = tmp
            o._convert_into = lambda d: entered.set()
            coreml_shared.CONVERT_LOCK.acquire()
            try:
                t = threading.Thread(target=lambda: o._convert_atomic(), daemon=True)
                t.start()
                t.join(timeout=0.5)
                self.assertFalse(
                    entered.is_set(),
                    "_convert_atomic converted while CONVERT_LOCK was held - "
                    "concurrent coremltools conversions corrupt each other",
                )
            finally:
                coreml_shared.CONVERT_LOCK.release()
            t.join(timeout=5)
            self.assertTrue(entered.is_set(), "it never converted after release")

    def test_the_sentinel_write_never_raises(self):
        """note_fast_attempt runs where the short graph is already lost; if it could
        throw (a full or read-only cache root) it would take the whole backend with
        it - which is how the optional optimization became fatal once already."""
        self.assertEqual(
            coreml_shared.note_fast_attempt("/nonexistent/read-only/path", "why"), 0
        )


class SentinelWriteIsNeverFatal(unittest.TestCase):
    """An OPTIONAL short-sequence-graph failure must never cost the REQUIRED artifacts. The
    tokenizer is written before the optional graph is attempted, so a cache can never
    be published complete-but-tokenizer-less."""

    def test_the_tokenizer_is_saved_before_the_optional_graph_is_attempted(self):
        src = inspect.getsource(cr.CoreMLReranker._convert_into)
        tok_at = src.index("tok.save_pretrained")
        fast_at = src.index("_MODEL_FAST_NAME")
        self.assertLess(
            tok_at, fast_at,
            "the tokenizer must be written BEFORE the optional graph is attempted",
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
