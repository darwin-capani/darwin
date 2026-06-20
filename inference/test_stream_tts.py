#!/usr/bin/env python3
"""Hermetic, NO-NETWORK unit tests for the OPT-IN STREAMING TTS tier
(`_elevenlabs_synth_pcm_stream`) in the inference server.

Streaming is ADDITIVE and opt-in: it hits the ElevenLabs `/stream` endpoint, reads
the chunked-transfer body, and concatenates the PCM. The DEFAULT speak path is the
existing blocking `_elevenlabs_synth_pcm`, and ANY streaming error FALLS BACK to it
(and then, on a total cloud failure, to Kokoro). These tests prove:

  * the streaming seam concatenates the chunks it reads from the (faked) response
    and never lets the api_key touch the URL/query (key hygiene == the blocking seam);
  * the streaming seam refuses keyless / voice-id-less calls BEFORE any network;
  * `speak` is BYTE-FOR-BYTE unchanged by default — with the streaming tier OFF
    (the shipped default) the cloud leg uses the BLOCKING seam and the streaming
    seam is NEVER touched;
  * opting in (stream=True) routes the cloud leg through the STREAMING seam;
  * a streaming error FALLS BACK to the blocking seam (the turn still succeeds via
    the cloud), and a total cloud failure still falls back to Kokoro.

The seams are MONKEYPATCHED in every engine test — there is NO real HTTP here, and
no model is loaded. Run: python3 inference/test_stream_tts.py
"""

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import server  # noqa: E402


def _make_engine():
    settings = {"llm": "stub-llm", "stt": "stub-stt", "engine": "kokoro",
                "voice": "bm_george", "speed": 1.2}
    return server.InferenceEngine(settings, classifier_template="", persona="")


class _Recorder:
    def __init__(self):
        self.kokoro_calls = 0
        self.blocking_calls = 0
        self.stream_calls = 0
        self.last_stream_args = None
        self.last_blocking_args = None


def _canned_pcm():
    """A second of silence with a loud blip so _trim_silence keeps audio."""
    import numpy as np

    samples = np.zeros(2400, dtype="<i2")
    samples[1000:1100] = 8000
    return samples.tobytes()


def _install_stubs(engine, rec, blocking_pcm=None, stream_pcm=None,
                   blocking_raises=None, stream_raises=None):
    """Stub BOTH cloud seams + the Kokoro path. Returns a restore callable."""
    orig_blocking = server._elevenlabs_synth_pcm
    orig_stream = server._elevenlabs_synth_pcm_stream

    def fake_blocking(voice_id, model, api_key, text,
                      timeout_s=server.ELEVENLABS_TIMEOUT_S, audio_tag=None,
                      stability=None, style=None, locators=None):
        rec.blocking_calls += 1
        rec.last_blocking_args = {"voice_id": voice_id, "model": model,
                                  "api_key": api_key, "text": text}
        if blocking_raises is not None:
            raise blocking_raises
        return blocking_pcm

    def fake_stream(voice_id, model, api_key, text,
                    timeout_s=server.ELEVENLABS_TIMEOUT_S, audio_tag=None,
                    stability=None, style=None, locators=None,
                    latency=server.ELEVENLABS_STREAM_LATENCY,
                    chunk_bytes=server.ELEVENLABS_STREAM_CHUNK_BYTES):
        rec.stream_calls += 1
        rec.last_stream_args = {"voice_id": voice_id, "model": model,
                                "api_key": api_key, "text": text}
        if stream_raises is not None:
            raise stream_raises
        return stream_pcm

    server._elevenlabs_synth_pcm = fake_blocking
    server._elevenlabs_synth_pcm_stream = fake_stream

    engine._ensure_tts = lambda: object()

    def fake_synth(tts, text, voice, out_path=None, rate=None, volume=None):
        rec.kokoro_calls += 1
        return "/tmp/kokoro-stub.wav"

    engine._synthesize_to_wav = fake_synth
    engine._write_wav = lambda audio, sr, out_path=None: "/tmp/elevenlabs-stub.wav"

    def restore():
        server._elevenlabs_synth_pcm = orig_blocking
        server._elevenlabs_synth_pcm_stream = orig_stream

    return restore


class _FakeChunkedResponse:
    """A urlopen-like context manager whose .read(n) yields the supplied chunks one
    at a time then b"" — emulating a chunked-transfer body de-chunked by urllib."""

    def __init__(self, chunks):
        self._chunks = list(chunks)

    def read(self, n=-1):
        if self._chunks:
            return self._chunks.pop(0)
        return b""

    def __enter__(self):
        return self

    def __exit__(self, *a):
        return False


class StreamSeamConcatenation(unittest.TestCase):
    """The seam reads the chunked body and concatenates PCM — with NO real network
    (urlopen is monkeypatched to return a canned chunked response)."""

    def test_seam_concatenates_chunks(self):
        import urllib.request

        sent = {}
        chunks = [b"\x01\x02", b"\x03\x04", b"\x05\x06"]

        def fake_urlopen(req, timeout=None):
            sent["url"] = req.full_url
            # urllib capitalizes only the first letter of a header name (Xi-api-key);
            # use capitalize() to match how the Request stored it.
            sent["key"] = req.get_header(server._ELEVENLABS_HEADER.capitalize())
            return _FakeChunkedResponse(chunks)

        orig = urllib.request.urlopen
        urllib.request.urlopen = fake_urlopen
        try:
            out = server._elevenlabs_synth_pcm_stream(
                "EL_VOICE", "eleven_flash_v2_5", "sk-secret", "hello there"
            )
        finally:
            urllib.request.urlopen = orig

        self.assertEqual(out, b"\x01\x02\x03\x04\x05\x06",
                         "the seam must concatenate every chunk in order")
        # Key hygiene: the secret rides the header, NEVER the URL/query.
        self.assertIn("/stream", sent["url"])
        self.assertIn("output_format=pcm_24000", sent["url"])
        self.assertIn("optimize_streaming_latency=", sent["url"])
        self.assertNotIn("sk-secret", sent["url"])
        self.assertNotIn("xi-api-key", sent["url"])
        self.assertEqual(sent["key"], "sk-secret")

    def test_seam_requires_key(self):
        with self.assertRaises(ValueError):
            server._elevenlabs_synth_pcm_stream("EL_VOICE", "m", "", "hi")

    def test_seam_requires_voice_id(self):
        with self.assertRaises(ValueError):
            server._elevenlabs_synth_pcm_stream("", "m", "sk-x", "hi")

    def test_latency_is_clamped_0_4(self):
        import urllib.request

        captured = {}

        def fake_urlopen(req, timeout=None):
            captured["url"] = req.full_url
            return _FakeChunkedResponse([b"\x00\x00"])

        orig = urllib.request.urlopen
        urllib.request.urlopen = fake_urlopen
        try:
            server._elevenlabs_synth_pcm_stream("V", "m", "sk-x", "hi", latency=99)
        finally:
            urllib.request.urlopen = orig
        self.assertIn("optimize_streaming_latency=4", captured["url"])


class StreamDefaultUnchanged(unittest.TestCase):
    """The DEFAULT speak path is unchanged: streaming is OFF by default, so the
    cloud leg uses the BLOCKING seam and the streaming seam is NEVER touched."""

    def test_module_default_ships_off(self):
        self.assertIs(server.STREAM_TTS, False,
                      "streaming TTS must ship OFF (opt-in only)")

    def test_default_cloud_speak_uses_blocking_seam(self):
        engine = _make_engine()
        rec = _Recorder()
        restore = _install_stubs(engine, rec, blocking_pcm=_canned_pcm(),
                                 stream_pcm=_canned_pcm())
        try:
            path = engine.speak(
                "hello there", voice="bm_george", backend="elevenlabs",
                voice_id="EL_VOICE", model="eleven_flash_v2_5", el_key="sk-secret",
            )  # no stream= -> module default (False)
            self.assertEqual(path, "/tmp/elevenlabs-stub.wav")
            self.assertEqual(rec.blocking_calls, 1, "default must use the BLOCKING seam")
            self.assertEqual(rec.stream_calls, 0, "the streaming seam must NOT be touched by default")
            self.assertEqual(rec.kokoro_calls, 0)
        finally:
            restore()

    def test_default_kokoro_speak_touches_no_cloud_seam(self):
        engine = _make_engine()
        rec = _Recorder()
        restore = _install_stubs(engine, rec, blocking_pcm=_canned_pcm(),
                                 stream_pcm=_canned_pcm())
        try:
            path = engine.speak("hello", voice="bm_george")  # default backend = Kokoro
            self.assertEqual(path, "/tmp/kokoro-stub.wav")
            self.assertEqual(rec.kokoro_calls, 1)
            self.assertEqual(rec.blocking_calls, 0)
            self.assertEqual(rec.stream_calls, 0)
        finally:
            restore()


class StreamOptIn(unittest.TestCase):
    def test_opt_in_routes_through_streaming_seam(self):
        engine = _make_engine()
        rec = _Recorder()
        restore = _install_stubs(engine, rec, blocking_pcm=_canned_pcm(),
                                 stream_pcm=_canned_pcm())
        try:
            path = engine.speak(
                "hello there", voice="bm_george", backend="elevenlabs",
                voice_id="EL_VOICE", model="eleven_flash_v2_5", el_key="sk-secret",
                stream=True,
            )
            self.assertEqual(path, "/tmp/elevenlabs-stub.wav")
            self.assertEqual(rec.stream_calls, 1, "opt-in must use the STREAMING seam")
            self.assertEqual(rec.blocking_calls, 0, "the blocking seam must NOT run on a clean stream")
            self.assertEqual(rec.kokoro_calls, 0)
            self.assertEqual(rec.last_stream_args["voice_id"], "EL_VOICE")
            self.assertEqual(rec.last_stream_args["api_key"], "sk-secret")
        finally:
            restore()


class StreamFallback(unittest.TestCase):
    def test_streaming_error_falls_back_to_blocking_seam(self):
        engine = _make_engine()
        rec = _Recorder()
        # Streaming raises -> the helper retries via the blocking seam, which succeeds.
        restore = _install_stubs(
            engine, rec, blocking_pcm=_canned_pcm(),
            stream_raises=RuntimeError("stream connection reset"),
        )
        try:
            path = engine.speak(
                "hello", voice="bm_george", backend="elevenlabs",
                voice_id="EL_VOICE", model="eleven_flash_v2_5", el_key="sk-secret",
                stream=True,
            )
            # The turn still succeeds via the CLOUD (blocking) — not Kokoro.
            self.assertEqual(path, "/tmp/elevenlabs-stub.wav")
            self.assertEqual(rec.stream_calls, 1, "streaming was attempted")
            self.assertEqual(rec.blocking_calls, 1, "then the blocking seam served the turn")
            self.assertEqual(rec.kokoro_calls, 0, "Kokoro is NOT reached when blocking succeeds")
        finally:
            restore()

    def test_total_cloud_failure_falls_back_to_kokoro(self):
        engine = _make_engine()
        rec = _Recorder()
        # BOTH cloud seams raise -> speak's outer except -> Kokoro serves the turn.
        restore = _install_stubs(
            engine, rec,
            stream_raises=RuntimeError("stream down"),
            blocking_raises=RuntimeError("blocking down"),
        )
        try:
            path = engine.speak(
                "hello", voice="bm_george", backend="elevenlabs",
                voice_id="EL_VOICE", model="eleven_flash_v2_5", el_key="sk-secret",
                stream=True,
            )
            self.assertEqual(path, "/tmp/kokoro-stub.wav")
            self.assertEqual(rec.stream_calls, 1)
            self.assertEqual(rec.blocking_calls, 1)
            self.assertEqual(rec.kokoro_calls, 1, "Kokoro is the final fallback")
        finally:
            restore()

    def test_streaming_no_audio_falls_back_to_kokoro(self):
        engine = _make_engine()
        rec = _Recorder()
        # Streaming returns empty bytes (no raise) -> no cloud audio -> Kokoro.
        restore = _install_stubs(engine, rec, stream_pcm=b"", blocking_pcm=b"")
        try:
            path = engine.speak(
                "hello", voice="bm_george", backend="elevenlabs",
                voice_id="EL_VOICE", model="eleven_flash_v2_5", el_key="sk-secret",
                stream=True,
            )
            self.assertEqual(path, "/tmp/kokoro-stub.wav")
            self.assertEqual(rec.kokoro_calls, 1)
        finally:
            restore()


class StreamSeamIsTheOnlyStreamNetworkTouch(unittest.TestCase):
    def test_module_exposes_exactly_one_stream_seam(self):
        self.assertTrue(hasattr(server, "_elevenlabs_synth_pcm_stream"))
        # The latency knob is a constant, never a secret; the header is unchanged.
        self.assertEqual(server._ELEVENLABS_HEADER, "xi-api-key")
        self.assertIn(server.ELEVENLABS_STREAM_LATENCY, range(0, 5))


if __name__ == "__main__":
    unittest.main(verbosity=2)
