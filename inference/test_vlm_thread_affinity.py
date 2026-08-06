"""Every mlx-vlm model must be loaded and generated on ONE thread.

MLX streams are per-thread. A model loaded on thread A and generated on thread B dies
with:

    RuntimeError: There is no Stream(gpu, 3) in current thread.

Engine ops run on asyncio.to_thread's default pool, which hands back an ARBITRARY
reusable worker. So both describe_image paths -- OCR transcription and the VLM
description -- were one scheduling decision away from failing outright. They appeared
to work only because a lightly loaded pool tends to reuse the same idle thread.

This was not theoretical. Warming the OCR model on its own background thread (a
perfectly reasonable-looking fix for what was then believed to be a 19.5 s lock hold --
that figure has since been RETRACTED: it was a transcription misread as a load, see
server.py's OCR-warm block) made EVERY OCR call fail and silently fall back to the
VLM. It shipped and deployed, because every test written for
it was structural -- they asserted the shape of preload's source, and a source-shape
assertion cannot observe a thread boundary. It was caught by driving the deployed
server over its socket and noticing `path='vlm'` in the reply.

So these tests cross the thread boundary FOR REAL. The cheap ones use a stand-in model
that records which thread touched it; the expensive one (skipped unless
DARWIN_SLOW_TESTS=1) loads the actual OCR checkpoint and transcribes from a different
thread than the loader.

  Run: .venv/bin/python inference/test_vlm_thread_affinity.py   (from the repo root)
"""
import os
import pathlib
import re
import threading
import unittest
from concurrent.futures import ThreadPoolExecutor

import server as S

REPO = pathlib.Path(__file__).resolve().parent.parent
SRC = (REPO / "inference" / "server.py").read_text(encoding="utf-8")
FIXTURE = REPO / "inference" / "benchmarks" / "vlm_eval" / "fixture_settings_12px.png"


def _bare_engine():
    """An engine with only the attributes these paths touch -- __init__ loads models."""
    e = S.InferenceEngine.__new__(S.InferenceEngine)
    e._lock = threading.Lock()
    e.ocr_model_id = S.DEFAULT_OCR_MODEL
    e._ocr_model = e._ocr_processor = e._ocr_config = None
    e._ocr_unavailable = False
    e._ocr_transcripts = {}
    e._vlm_worker = ThreadPoolExecutor(max_workers=1, thread_name_prefix="vlm-worker")
    return e


class AllVlmWorkLandsOnOneThread(unittest.TestCase):
    def setUp(self):
        self.engine = _bare_engine()
        self.addCleanup(self.engine._vlm_worker.shutdown, wait=True)

    def test_the_funnel_pins_every_call_to_a_single_thread(self):
        seen = set()

        def touch():
            seen.add(threading.get_ident())

        # Call from several DIFFERENT caller threads, exactly as asyncio.to_thread does.
        callers = [threading.Thread(target=lambda: self.engine._on_vlm_worker(touch))
                   for _ in range(8)]
        for t in callers:
            t.start()
        for t in callers:
            t.join()
        self.engine._on_vlm_worker(touch)  # and from this thread too

        self.assertEqual(
            len(seen), 1,
            f"mlx-vlm work ran on {len(seen)} different threads. A model loaded on one "
            "and generated on another raises 'There is no Stream(gpu, N) in current "
            "thread' -- which is precisely how the OCR path was shipped broken",
        )

    def test_the_worker_is_not_the_calling_thread(self):
        """If the funnel degenerated into a direct call it would inherit the caller's
        arbitrary pool thread and pin nothing."""
        got = self.engine._on_vlm_worker(threading.get_ident)
        self.assertNotEqual(got, threading.get_ident())

    def test_the_funnel_propagates_exceptions_to_the_caller(self):
        """describe_image relies on catching a failed load/generate to fall back. A
        funnel that swallowed exceptions would turn every failure into a silent
        wrong answer."""
        def boom():
            raise RuntimeError("checkpoint missing")
        with self.assertRaises(RuntimeError):
            self.engine._on_vlm_worker(boom)

    def test_the_funnel_returns_the_value(self):
        self.assertEqual(self.engine._on_vlm_worker(lambda a, b=0: a + b, 2, b=3), 5)

    def test_a_model_touched_only_through_the_funnel_sees_one_thread(self):
        """The property that actually matters, with a stand-in for the model: load and
        every generate must observe the SAME thread id."""
        threads = []

        class FakeModel:
            def load(self):
                threads.append(("load", threading.get_ident()))

            def generate(self):
                threads.append(("generate", threading.get_ident()))

        m = FakeModel()
        self.engine._on_vlm_worker(m.load)
        # Generate from many unrelated caller threads.
        ts = [threading.Thread(target=lambda: self.engine._on_vlm_worker(m.generate))
              for _ in range(5)]
        for t in ts:
            t.start()
        for t in ts:
            t.join()

        ids = {tid for _, tid in threads}
        self.assertEqual(
            len(ids), 1,
            f"load ran on {threads[0][1]} but generates ran on {ids} -- MLX would raise "
            "'There is no Stream(gpu, N) in current thread' on the first mismatch",
        )


class TheCallSitesActuallyUseTheFunnel(unittest.TestCase):
    """A funnel nothing routes through is the bug, not the fix."""

    def test_transcription_goes_through_the_worker(self):
        self.assertIn("self._on_vlm_worker(self._transcribe_screen, path)", SRC)

    def test_the_vlm_load_goes_through_the_worker(self):
        self.assertIn("self._on_vlm_worker(self._ensure_vlm)", SRC)

    def test_the_vlm_generate_goes_through_the_worker(self):
        self.assertRegex(
            SRC, r"self\._on_vlm_worker\(\s*vlm\[\"generate\"\]",
            "generating on a thread other than the loader's raises; the load being "
            "funnelled is not enough on its own",
        )

    def test_the_preload_warm_goes_through_the_worker(self):
        self.assertIn("self._on_vlm_worker(self._ensure_ocr)", SRC)

    # Bodies that ALREADY run on the worker, so they must call inline -- re-submitting
    # from the worker self-deadlocks (max_workers=1). Each one has to say so, so the
    # exemption is a declared contract rather than a hole in this test.
    WORKER_ONLY = ("_transcribe_screen",)

    def test_worker_only_bodies_declare_themselves(self):
        for name in self.WORKER_ONLY:
            body = SRC[SRC.index(f"def {name}("):]
            self.assertIn(
                "RUNS ON THE VLM WORKER", body[:body.index("\n    def ")],
                f"{name} is exempted from the funnel check, so it must document that "
                "it runs on the worker and why it calls the model inline",
            )

    def test_no_call_site_touches_a_vlm_model_outside_the_funnel(self):
        """The regression that shipped: a NEW thread calling _ensure_ocr directly."""
        # Map every line to its enclosing def so exemptions are per-function.
        enclosing, current = {}, ""
        for i, line in enumerate(SRC.splitlines(), 1):
            m = re.match(r"\s*def (\w+)\(", line)
            if m:
                current = m.group(1)
            enclosing[i] = current

        offenders = []
        for i, line in enumerate(SRC.splitlines(), 1):
            body = line.strip()
            if body.startswith("#") or "_on_vlm_worker" in body:
                continue
            if enclosing[i] in self.WORKER_ONLY:
                continue
            for bad in ("self._ensure_ocr()", "self._ensure_vlm()",
                        'vlm["generate"](', 'ocr["generate"]('):
                if bad in body:
                    offenders.append(f"line {i} in {enclosing[i]}(): {body}")
        self.assertEqual(
            offenders, [],
            "these load or run an mlx-vlm model outside the single-thread funnel, so "
            f"MLX's per-thread streams can break them: {offenders}",
        )


@unittest.skipUnless(os.environ.get("DARWIN_SLOW_TESTS") == "1",
                     "loads the real OCR checkpoint; set DARWIN_SLOW_TESTS=1")
class TheRealModelSurvivesTheThreadBoundary(unittest.TestCase):
    """The end-to-end version. Without this, only the socket-level check catches it."""

    def test_transcribe_from_a_different_thread_than_the_loader(self):
        engine = _bare_engine()
        self.addCleanup(engine._vlm_worker.shutdown, wait=True)
        self.assertIsNotNone(engine._on_vlm_worker(engine._ensure_ocr),
                             "OCR checkpoint not available")

        out = {}

        def caller():
            engine._ocr_transcripts.clear()   # defeat the content cache; do real work
            out["text"] = engine._on_vlm_worker(engine._transcribe_screen, str(FIXTURE))

        t = threading.Thread(target=caller)
        t.start()
        t.join()
        self.assertTrue(
            (out.get("text") or "").strip(),
            "transcription from a non-loading thread produced nothing -- this is the "
            "shipped bug: it fails, is caught, and silently falls back to the VLM",
        )
        self.assertIn("192.168.1.47", out["text"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
