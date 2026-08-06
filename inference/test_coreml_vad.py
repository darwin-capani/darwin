"""Unit tests for the PURE seams of the Core ML learned-VAD backend.

Runs WITHOUT loading any model and WITHOUT coremltools / torch / silero_vad:
`import coreml_vad` is import-light (stdlib + numpy) and the tested helpers take
plain Python / numpy inputs, so this exercises the per-frame voiced verdict
(`frame_is_voiced`, incl. the NaN/Inf fail-safe), the streaming geometry
(`next_context`, `build_model_input` with its hard length checks), and the
`StreamingVAD` context-ring + recurrent-state handoff driven by a FAKE backend.
The live Core ML convert/predict + FP16-vs-torch faithfulness + per-frame latency
are DEVICE/DEP-gated and exercised by the once-run smoke
(`.venv/bin/python inference/coreml_vad.py`), NOT here.

  Run: .venv/bin/python inference/test_coreml_vad.py   (from the repo root)
"""
import os
import sys
import tempfile
import threading
import unittest
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parent))

import coreml_shared  # noqa: E402
import coreml_vad as cv  # noqa: E402


class FrameIsVoicedTests(unittest.TestCase):
    def test_threshold_boundary(self):
        # Strictly greater than the threshold, mirroring the RMS gate's `>`.
        self.assertFalse(cv.frame_is_voiced(0.5, 0.5))
        self.assertTrue(cv.frame_is_voiced(0.5001, 0.5))
        self.assertFalse(cv.frame_is_voiced(0.49, 0.5))
        self.assertTrue(cv.frame_is_voiced(0.99, 0.5))

    def test_default_threshold(self):
        self.assertTrue(cv.frame_is_voiced(0.8))
        self.assertFalse(cv.frame_is_voiced(0.2))

    def test_non_finite_is_fail_safe_not_voiced(self):
        # A degenerate frame (NaN/Inf) never opens the mic.
        self.assertFalse(cv.frame_is_voiced(float("nan")))
        self.assertFalse(cv.frame_is_voiced(float("inf")))
        self.assertFalse(cv.frame_is_voiced(float("-inf")))

    def test_returns_plain_bool(self):
        self.assertIsInstance(cv.frame_is_voiced(0.9), bool)


class NextContextTests(unittest.TestCase):
    def test_returns_last_context_samples(self):
        chunk = np.arange(cv.CHUNK, dtype=np.float32)
        ctx = cv.next_context(chunk)
        self.assertEqual(ctx.shape, (cv.CONTEXT,))
        # Exactly the last CONTEXT samples of the chunk.
        np.testing.assert_array_equal(ctx, chunk[-cv.CONTEXT:])

    def test_short_chunk_is_left_zero_padded(self):
        short = np.array([1.0, 2.0, 3.0], dtype=np.float32)
        ctx = cv.next_context(short)
        self.assertEqual(ctx.shape, (cv.CONTEXT,))
        self.assertEqual(list(ctx[-3:]), [1.0, 2.0, 3.0])
        self.assertTrue(np.all(ctx[:-3] == 0.0))

    def test_does_not_alias_input(self):
        chunk = np.ones(cv.CHUNK, dtype=np.float32)
        ctx = cv.next_context(chunk)
        ctx[0] = 999.0
        self.assertEqual(chunk[cv.CHUNK - cv.CONTEXT], 1.0)


class BuildModelInputTests(unittest.TestCase):
    def test_shape_and_concat_order(self):
        ctx = np.full(cv.CONTEXT, 7.0, dtype=np.float32)
        chunk = np.full(cv.CHUNK, 3.0, dtype=np.float32)
        x = cv.build_model_input(ctx, chunk)
        self.assertEqual(x.shape, (1, cv.MODEL_INPUT))
        # context first, then chunk (Silero's cat([context, x])).
        self.assertTrue(np.all(x[0, :cv.CONTEXT] == 7.0))
        self.assertTrue(np.all(x[0, cv.CONTEXT:] == 3.0))

    def test_rejects_wrong_chunk_length(self):
        ctx = np.zeros(cv.CONTEXT, dtype=np.float32)
        with self.assertRaises(ValueError):
            cv.build_model_input(ctx, np.zeros(cv.CHUNK - 1, dtype=np.float32))

    def test_rejects_wrong_context_length(self):
        with self.assertRaises(ValueError):
            cv.build_model_input(np.zeros(cv.CONTEXT + 1, dtype=np.float32),
                                 np.zeros(cv.CHUNK, dtype=np.float32))


class _FakeBackend:
    """Records the (x, state) it is fed and returns a deterministic prob + a
    state advanced by +1, so StreamingVAD's context/state threading is observable
    without any model."""

    def __init__(self):
        self.calls = []

    def ensure_loaded(self):
        pass

    def step(self, x, state):
        x = np.asarray(x, dtype=np.float32).reshape(1, cv.MODEL_INPUT)
        state = np.asarray(state, dtype=np.float32).reshape(*cv.STATE_SHAPE)
        self.calls.append((x.copy(), state.copy()))
        # prob = mean of the CHUNK region (lets a test drive prob from input)
        prob = float(x[0, cv.CONTEXT:].mean())
        return prob, state + 1.0


class StreamingVADTests(unittest.TestCase):
    def test_first_chunk_has_zero_context_and_zero_state(self):
        fake = _FakeBackend()
        sv = cv.StreamingVAD(backend=fake)
        chunk = np.full(cv.CHUNK, 0.9, dtype=np.float32)
        sv.push_chunk(chunk)
        x, state = fake.calls[0]
        self.assertTrue(np.all(x[0, :cv.CONTEXT] == 0.0), "initial context is zero")
        self.assertTrue(np.all(state == 0.0), "initial recurrent state is zero")

    def test_context_and_state_thread_between_chunks(self):
        fake = _FakeBackend()
        sv = cv.StreamingVAD(backend=fake)
        c1 = np.arange(cv.CHUNK, dtype=np.float32)
        c2 = np.full(cv.CHUNK, 5.0, dtype=np.float32)
        sv.push_chunk(c1)
        sv.push_chunk(c2)
        x2, state2 = fake.calls[1]
        # 2nd input's context is the last CONTEXT samples of chunk 1.
        np.testing.assert_array_equal(x2[0, :cv.CONTEXT], c1[-cv.CONTEXT:])
        # 2nd input's chunk region is chunk 2.
        np.testing.assert_array_equal(x2[0, cv.CONTEXT:], c2)
        # state advanced by the fake backend (was zeros, +1 after first step).
        self.assertTrue(np.all(state2 == 1.0), "state threaded from step 1")

    def test_reset_clears_context_and_state(self):
        fake = _FakeBackend()
        sv = cv.StreamingVAD(backend=fake)
        sv.push_chunk(np.full(cv.CHUNK, 2.0, dtype=np.float32))
        sv.reset()
        sv.push_chunk(np.full(cv.CHUNK, 2.0, dtype=np.float32))
        x, state = fake.calls[-1]
        self.assertTrue(np.all(x[0, :cv.CONTEXT] == 0.0), "context cleared by reset")
        self.assertTrue(np.all(state == 0.0), "state cleared by reset")

    def test_push_returns_prob(self):
        fake = _FakeBackend()
        sv = cv.StreamingVAD(backend=fake)
        p = sv.push_chunk(np.full(cv.CHUNK, 0.42, dtype=np.float32))
        self.assertAlmostEqual(p, 0.42, places=5)


class GeometryConstantsTests(unittest.TestCase):
    def test_model_input_is_context_plus_chunk(self):
        self.assertEqual(cv.MODEL_INPUT, cv.CONTEXT + cv.CHUNK)

    def test_16khz_frame_is_32ms(self):
        self.assertEqual(cv.CHUNK, 512)
        self.assertEqual(cv.SAMPLE_RATE, 16000)
        self.assertAlmostEqual(1000.0 * cv.CHUNK / cv.SAMPLE_RATE, 32.0)


class ConversionSerializationTests(unittest.TestCase):
    """coremltools keeps global MIL state, so two overlapping conversions corrupt each
    other — reproduced on the real converters, and it is what broke the first deploy of
    the short-graph fast path. coreml_shared's contract is that EVERY conversion in the
    process takes CONVERT_LOCK; this backend was the one that did not, so warming the
    VAD beside the embedder/reranker (both of which start background rebuild threads at
    preload) would have reproduced that crash with no guard."""

    def test_conversions_are_serialized_process_wide(self):
        self.assertIs(cv.CONVERT_LOCK, coreml_shared.CONVERT_LOCK)
        # Behavioural, not textual: a COMMENT naming CONVERT_LOCK must not satisfy
        # this. Hold the lock, then prove _convert_atomic actually blocks on it.
        entered = threading.Event()
        o = cv.CoreMLVAD.__new__(cv.CoreMLVAD)
        with tempfile.TemporaryDirectory() as tmp:
            o._dir = os.path.join(tmp, "vad")
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


def _fake_native_blob():
    """A structurally-valid native weights blob (zeros) per NATIVE_TENSORS —
    the exact byte layout daemon/src/silero.rs parses (lockstep contract)."""
    import struct

    out = [cv.NATIVE_WEIGHTS_MAGIC, struct.pack("<I", len(cv.NATIVE_TENSORS))]
    for _name, shape in cv.NATIVE_TENSORS:
        n = int(np.prod(shape))
        out.append(struct.pack("<I", n))
        out.append(b"\x00" * (4 * n))
    return b"".join(out)


class NativeWeightsFormatTests(unittest.TestCase):
    """The export-file contract the daemon's Rust loader (silero.rs) depends on.
    These run WITHOUT torch/silero_vad — pure file-format logic."""

    def test_valid_blob_passes_validation(self):
        import tempfile

        with tempfile.NamedTemporaryFile(suffix=".bin") as f:
            f.write(_fake_native_blob())
            f.flush()
            self.assertTrue(cv.native_weights_valid(f.name))

    def test_truncated_and_corrupt_blobs_are_rejected(self):
        import tempfile

        blob = _fake_native_blob()
        cases = [
            ("truncated", blob[:-10]),
            ("bad magic", b"XXXXXXX\n" + blob[8:]),
            ("trailing garbage", blob + b"\x00" * 8),
            ("empty", b""),
        ]
        for name, data in cases:
            with tempfile.NamedTemporaryFile(suffix=".bin") as f:
                f.write(data)
                f.flush()
                self.assertFalse(cv.native_weights_valid(f.name), name)

    def test_missing_file_is_invalid(self):
        self.assertFalse(cv.native_weights_valid("/nonexistent/vad/weights.bin"))

    def test_tensor_table_matches_the_documented_geometry(self):
        # The lockstep numbers silero.rs mirrors: total parameter count and the
        # LSTM/STFT dimensions the architecture fixes.
        sizes = {name: int(np.prod(shape)) for name, shape in cv.NATIVE_TENSORS}
        self.assertEqual(sizes["stft_basis"], 258 * 256)
        self.assertEqual(sizes["lstm_wih"], 4 * 128 * 128)
        self.assertEqual(sizes["lstm_bih"], 4 * 128)
        self.assertEqual(sizes["dec_w"], 128)
        self.assertEqual(sizes["dec_b"], 1)
        total = sum(sizes.values())
        # 309,633 fp32 values -> the exported file is exactly 1,238,604 bytes
        # (8 magic + 4 count + 15*4 length prefixes + 4*total data).
        self.assertEqual(total, 309_633, "total fp32 parameter count is pinned")


class PerFrameConfusionCoversEveryLabeledRegion(unittest.TestCase):
    """The committed VAD eval's per-frame confusion must label every frame it says it
    labels. inference/benchmarks/vad_eval/eval_vad.py documents it in its own comment —
    "speech clips' body frames = speech; noise clips' frames + speech clips' LEAD/TRAIL
    frames = non-speech" — but the loop under it had no arm for `idx >= body_end`, so
    the whole TRAILING non-speech region was counted nowhere: 40 clips x 15 frames =
    600 labeled frames, roughly HALF the non-speech denominator, and exactly the region
    where a learned VAD's probability HANGOVER shows up. A verdict source with a 100%
    false-accept rate on the trail scored 0.0.

    `evaluate()` takes a per-frame verdict function and plain clip dicts, so this runs
    with NO model and NO audio. (The clip-level false_accept_rate_clips that
    daemon/src/vad.rs cites as adoption evidence is computed separately and was never
    affected.)"""

    @classmethod
    def setUpClass(cls):
        sys.path.insert(0, str(Path(__file__).resolve().parent / "benchmarks" / "vad_eval"))
        import eval_vad

        cls.ev = eval_vad
        cls.FRAMES = 50
        cls.ONSET = 15                                              # leading non-speech
        cls.TRAIL = int(eval_vad.SR * eval_vad.TRAIL_MS / 1000) // eval_vad.CHUNK
        cls.BODY_END = cls.FRAMES - cls.TRAIL

    def setUp(self):
        self.assertGreater(self.TRAIL, 0, "the trailing region must be non-empty or "
                                          "this class proves nothing")
        self.assertLess(self.ONSET, self.BODY_END, "the body region must be non-empty")

    def _clip(self):
        """One speech clip: [0, ONSET) lead, [ONSET, BODY_END) speech, then trail. The
        signal is never inspected by `evaluate` — only its length and our verdicts."""
        return {
            "signal": np.zeros(self.FRAMES * self.ev.CHUNK, dtype=np.float32),
            "onset_frame": self.ONSET,
            "category": "clean",
        }

    def _verdict(self, voiced_idx):
        want = set(voiced_idx)
        return lambda sig: [i in want for i in range(len(sig) // self.ev.CHUNK)]

    def test_the_trailing_region_is_scored_as_non_speech(self):
        """THE REGRESSION. Voiced on EVERY trailing frame and nothing else — a 100%
        false-accept rate over that region, reported as 0.0."""
        got = self.ev.evaluate(
            self._verdict(range(self.BODY_END, self.FRAMES)), [self._clip()], []
        )
        self.assertEqual(
            got["labeled_non_speech_frames"], self.ONSET + self.TRAIL,
            "the trailing non-speech frames are missing from the denominator",
        )
        self.assertAlmostEqual(
            got["frame_false_accept_rate"],
            self.TRAIL / (self.ONSET + self.TRAIL), places=4,
            msg="every trailing frame was voiced and none of it reached the metric",
        )

    def test_the_leading_region_is_still_scored_as_non_speech(self):
        """The arm that always worked — the fix must not have moved it."""
        got = self.ev.evaluate(
            self._verdict(range(0, self.ONSET)), [self._clip()], []
        )
        self.assertAlmostEqual(
            got["frame_false_accept_rate"],
            self.ONSET / (self.ONSET + self.TRAIL), places=4,
        )

    def test_the_body_is_scored_as_speech(self):
        """Silent through the body -> every body frame is a false reject, and the body
        must NOT leak into the non-speech denominator."""
        got = self.ev.evaluate(self._verdict([]), [self._clip()], [])
        self.assertEqual(got["labeled_speech_frames"], self.BODY_END - self.ONSET)
        self.assertAlmostEqual(got["frame_false_reject_rate"], 1.0, places=4)
        self.assertAlmostEqual(got["frame_false_accept_rate"], 0.0, places=4)

    def test_a_perfect_verdict_source_scores_zero_on_both(self):
        got = self.ev.evaluate(
            self._verdict(range(self.ONSET, self.BODY_END)), [self._clip()], []
        )
        self.assertAlmostEqual(got["frame_false_accept_rate"], 0.0, places=4)
        self.assertAlmostEqual(got["frame_false_reject_rate"], 0.0, places=4)

    def test_every_labeled_frame_of_a_clip_is_accounted_for(self):
        """lead + body + trail must be the whole clip: no frame counted twice, none
        dropped. This is the invariant whose violation was invisible before."""
        got = self.ev.evaluate(self._verdict([]), [self._clip()], [])
        self.assertEqual(
            got["labeled_non_speech_frames"] + got["labeled_speech_frames"],
            self.FRAMES,
            "the confusion covers fewer frames than the clip has",
        )


if __name__ == "__main__":
    unittest.main()
