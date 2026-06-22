#!/usr/bin/env python3
"""Hermetic, NO-NETWORK unit tests for the ElevenLabs MUSIC (compose) tier
(op=compose_music) in the inference server.

The network seam (`_elevenlabs_music`) is MONKEYPATCHED in every engine test — it
is the ONLY place compose-music touches ElevenLabs — so there is NO real HTTP here.
These prove the payload shaping (prompt guards + length clamping into EL's
[3000,600000] ms window), the seam's key/prompt guards, the MONO channel-layout
handling (pcm_24000 is mono; an odd-length PCM16 stream is rejected, never read as
skewed mono), and the engine's honest "no track on failure" contract (there is NO
on-device music fallback to substitute).

Run: python3 inference/test_music.py   (stdlib + numpy only; no pip install)
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


def _canned_pcm():
    import numpy as np

    # 2400 int16 mono samples -> 4800 bytes (an EVEN, valid mono PCM16 length).
    return np.zeros(2400, dtype="<i2").tobytes()


class MusicPayload(unittest.TestCase):
    def test_prompt_and_default_length(self):
        b = server._build_music_payload("a calm lofi beat")
        self.assertEqual(b["prompt"], "a calm lofi beat")
        self.assertEqual(b["music_length_ms"], server.ELEVENLABS_MUSIC_LENGTH_DEFAULT_MS)

    def test_length_passed_through_when_in_window(self):
        b = server._build_music_payload("x", length_ms=45000)
        self.assertEqual(b["music_length_ms"], 45000)

    def test_length_clamped_into_el_window(self):
        self.assertEqual(
            server._build_music_payload("x", length_ms=1)["music_length_ms"],
            server.ELEVENLABS_MUSIC_LENGTH_MIN_MS,
        )
        self.assertEqual(
            server._build_music_payload("x", length_ms=9_999_999)["music_length_ms"],
            server.ELEVENLABS_MUSIC_LENGTH_MAX_MS,
        )

    def test_bad_length_folds_to_default(self):
        for bad in (None, "nope", float("nan"), float("inf"), True):
            self.assertEqual(
                server._build_music_payload("x", length_ms=bad)["music_length_ms"],
                server.ELEVENLABS_MUSIC_LENGTH_DEFAULT_MS,
            )

    def test_empty_prompt_rejected(self):
        for bad in ("", "   ", None, 123):
            with self.assertRaises(ValueError):
                server._build_music_payload(bad)

    def test_overlong_prompt_rejected(self):
        with self.assertRaises(ValueError):
            server._build_music_payload("a" * (server.ELEVENLABS_MUSIC_PROMPT_MAX + 1))

    def test_max_length_prompt_accepted(self):
        b = server._build_music_payload("a" * server.ELEVENLABS_MUSIC_PROMPT_MAX)
        self.assertEqual(len(b["prompt"]), server.ELEVENLABS_MUSIC_PROMPT_MAX)


class MusicSeamGuards(unittest.TestCase):
    """The seam refuses keyless / promptless calls BEFORE any network touch."""

    def test_seam_requires_key(self):
        with self.assertRaises(ValueError):
            server._elevenlabs_music("a beat", api_key="")

    def test_seam_requires_prompt(self):
        with self.assertRaises(ValueError):
            server._elevenlabs_music("   ", api_key="sk-x")


class MusicChannelLayout(unittest.TestCase):
    """CHANNEL LAYOUT: pcm_24000 is MONO. A valid mono PCM16 body is an even number
    of bytes and decodes cleanly; an odd-length stream is rejected so a truncated /
    skewed body is never read as mono. (A pinned mono output_format never yields
    interleaved stereo, so we never misread stereo as mono.)"""

    def test_even_mono_pcm_decodes_to_expected_sample_count(self):
        pcm = _canned_pcm()  # 4800 bytes -> 2400 mono int16 samples
        audio = server._pcm16_to_float32(pcm)
        self.assertEqual(audio.shape[0], 2400)
        self.assertEqual(audio.ndim, 1)  # MONO: a single (flat) channel

    def test_seam_rejects_odd_length_pcm(self):
        # The REAL seam asserts the body is a whole number of int16 samples. We drive
        # it with a mocked urlopen (NO network) that returns an ODD-length body; the
        # seam must RAISE rather than hand truncated bytes to the (lossy) mono decoder
        # — that decoder would silently drop the trailing byte, so the seam is where a
        # truncated/skewed stream gets caught. This is the channel-layout safety net.
        import urllib.request

        class _Resp:
            def __enter__(self_inner):
                return self_inner

            def __exit__(self_inner, *a):
                return False

            def read(self_inner):
                return b"\x00\x01\x02"  # 3 bytes: NOT a whole number of int16 samples

        orig_urlopen = urllib.request.urlopen
        urllib.request.urlopen = lambda req, timeout=None: _Resp()
        try:
            with self.assertRaises(ValueError):
                server._elevenlabs_music("a beat", api_key="sk-x")
        finally:
            urllib.request.urlopen = orig_urlopen

    def test_seam_accepts_even_length_pcm(self):
        # The mirror case: an EVEN-length (valid mono PCM16) body passes the guard and
        # is returned verbatim for the caller to decode through the mono path.
        import urllib.request

        body = _canned_pcm()

        class _Resp:
            def __enter__(self_inner):
                return self_inner

            def __exit__(self_inner, *a):
                return False

            def read(self_inner):
                return body

        orig_urlopen = urllib.request.urlopen
        urllib.request.urlopen = lambda req, timeout=None: _Resp()
        try:
            self.assertEqual(server._elevenlabs_music("a beat", api_key="sk-x"), body)
        finally:
            urllib.request.urlopen = orig_urlopen


class MusicEngine(unittest.TestCase):
    def _patch(self, pcm=None, raises=None, capture=None):
        orig = server._elevenlabs_music

        def fake(prompt, api_key, length_ms=server.ELEVENLABS_MUSIC_LENGTH_DEFAULT_MS,
                 timeout_s=server.ELEVENLABS_TIMEOUT_S):
            if capture is not None:
                capture["length_ms"] = length_ms
            if raises is not None:
                raise raises
            return pcm

        server._elevenlabs_music = fake
        return lambda: setattr(server, "_elevenlabs_music", orig)

    def test_success_returns_wav_path(self):
        engine = _make_engine()
        engine._write_wav = lambda audio, sr, out_path=None: "/tmp/music-stub.wav"
        restore = self._patch(pcm=_canned_pcm())
        try:
            self.assertEqual(
                engine.compose_music("a calm lofi beat", el_key="sk-x"),
                "/tmp/music-stub.wav",
            )
        finally:
            restore()

    def test_none_length_uses_default(self):
        engine = _make_engine()
        engine._write_wav = lambda audio, sr, out_path=None: "/tmp/music-stub.wav"
        capture = {}
        restore = self._patch(pcm=_canned_pcm(), capture=capture)
        try:
            engine.compose_music("a beat", el_key="sk-x", length_ms=None)
            self.assertEqual(capture["length_ms"], server.ELEVENLABS_MUSIC_LENGTH_DEFAULT_MS)
        finally:
            restore()

    def test_explicit_length_threaded_through(self):
        engine = _make_engine()
        engine._write_wav = lambda audio, sr, out_path=None: "/tmp/music-stub.wav"
        capture = {}
        restore = self._patch(pcm=_canned_pcm(), capture=capture)
        try:
            engine.compose_music("a beat", el_key="sk-x", length_ms=60000)
            self.assertEqual(capture["length_ms"], 60000)
        finally:
            restore()

    def test_failure_returns_none(self):
        engine = _make_engine()
        restore = self._patch(raises=RuntimeError("boom"))
        try:
            self.assertIsNone(engine.compose_music("a beat", el_key="sk-x"))
        finally:
            restore()

    def test_no_audio_returns_none(self):
        engine = _make_engine()
        restore = self._patch(pcm=b"")
        try:
            self.assertIsNone(engine.compose_music("a beat", el_key="sk-x"))
        finally:
            restore()

    def test_no_key_falls_to_none(self):
        # No key -> the REAL seam raises ValueError before any net -> engine returns None.
        engine = _make_engine()
        self.assertIsNone(engine.compose_music("a beat", el_key=""))


if __name__ == "__main__":
    unittest.main()
