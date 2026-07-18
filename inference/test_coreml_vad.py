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
import sys
import unittest
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parent))

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


if __name__ == "__main__":
    unittest.main()
