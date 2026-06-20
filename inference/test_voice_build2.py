#!/usr/bin/env python3
"""Hermetic, NO-NETWORK unit tests for the build-2/2 ElevenLabs seams in the
inference server: voice CLONING (consent-gated), gated cloud Scribe STT (with
the mlx_whisper fallback), and Babel MULTILINGUAL model selection on the TTS leg.

These tests prove the WIRING + the gating + the on-device fallbacks WITHOUT ever
touching the network or loading a model:

  * Every ElevenLabs network seam (`_elevenlabs_clone_voice`,
    `_elevenlabs_scribe_transcribe`, `_elevenlabs_synth_pcm`) is MONKEYPATCHED —
    each is the ONLY place that would touch ElevenLabs, and each is replaced with
    a stub returning canned data (or raising). There is NO real HTTP here.
  * The on-device fallbacks (mlx_whisper transcribe, Kokoro synth) are likewise
    stubbed so no MLX model is loaded; the tests assert WHICH path was taken.

Honesty: this proves backend selection + the on-device-fallback contracts + key
hygiene + the consent surface only. It does NOT (and cannot) verify live
ElevenLabs transcription/clone/voice quality — that is device + credential gated
and is never exercised here. Cloning + cloud STT send AUDIO to the cloud when
enabled (more sensitive than TTS text); whisper/Kokoro stay the offline defaults.

Run: python3 inference/test_voice_build2.py   (stdlib + numpy only; no pip install)
"""

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import server  # noqa: E402


def _make_engine():
    """Construct an InferenceEngine without loading any model (all model loads
    are lazy)."""
    settings = {
        "llm": "stub-llm",
        "stt": "stub-stt",
        "engine": "kokoro",
        "voice": "bm_george",
        "speed": 1.2,
    }
    return server.InferenceEngine(settings, classifier_template="", persona="")


# --------------------------------------------------------------------------
# Babel multilingual model selection (pure; no engine, no network).
# --------------------------------------------------------------------------
class MultilingualModelSelection(unittest.TestCase):
    def test_non_english_lang_selects_multilingual(self):
        for lang in ("Spanish", "French", "es", "fr-FR", "de", "ja", "zh-CN"):
            self.assertEqual(
                server.InferenceEngine._resolve_elevenlabs_model(None, lang),
                server.ELEVENLABS_MULTILINGUAL_MODEL,
                f"{lang!r} must select the multilingual model",
            )

    def test_english_or_absent_keeps_default(self):
        for lang in (None, "", "English", "en", "en-US", "en_GB", "eng"):
            self.assertEqual(
                server.InferenceEngine._resolve_elevenlabs_model(None, lang),
                server.ELEVENLABS_DEFAULT_MODEL,
                f"{lang!r} must keep the English-centric default model",
            )

    def test_explicit_model_is_always_honored(self):
        # The daemon's explicit model wins even for a non-English lang.
        self.assertEqual(
            server.InferenceEngine._resolve_elevenlabs_model("eleven_v3", "Spanish"),
            "eleven_v3",
        )
        self.assertEqual(
            server.InferenceEngine._resolve_elevenlabs_model("custom_model", None),
            "custom_model",
        )

    def test_is_non_english_lang_is_conservative(self):
        self.assertFalse(server._is_non_english_lang(None))
        self.assertFalse(server._is_non_english_lang(123))  # non-string
        self.assertFalse(server._is_non_english_lang("   "))
        self.assertFalse(server._is_non_english_lang("EN"))
        self.assertTrue(server._is_non_english_lang("Spanish"))

    def test_lang_threads_through_speak_to_the_seam(self):
        # End-to-end (minus the net): a non-English lang on a speak request reaches
        # the EL synth seam as a multilingual model_id; English keeps the default.
        import numpy as np

        engine = _make_engine()
        seen_models = []
        orig = server._elevenlabs_synth_pcm

        def fake_seam(voice_id, model, api_key, text, timeout_s=server.ELEVENLABS_TIMEOUT_S,
                      audio_tag=None, stability=None, style=None, locators=None):
            seen_models.append(model)
            samples = np.zeros(2400, dtype="<i2")
            samples[1000:1100] = 8000
            return samples.tobytes()

        server._elevenlabs_synth_pcm = fake_seam
        engine._write_wav = lambda audio, sr, out_path=None: "/tmp/el.wav"
        try:
            engine.speak(
                "hola", voice="bm_george", backend="elevenlabs",
                voice_id="V", el_key="sk", lang="Spanish",
            )
            engine.speak(
                "hello", voice="bm_george", backend="elevenlabs",
                voice_id="V", el_key="sk", lang="English",
            )
            engine.speak(
                "hello", voice="bm_george", backend="elevenlabs",
                voice_id="V", el_key="sk",  # lang absent
            )
        finally:
            server._elevenlabs_synth_pcm = orig
        self.assertEqual(seen_models[0], server.ELEVENLABS_MULTILINGUAL_MODEL)
        self.assertEqual(seen_models[1], server.ELEVENLABS_DEFAULT_MODEL)
        self.assertEqual(seen_models[2], server.ELEVENLABS_DEFAULT_MODEL)


# --------------------------------------------------------------------------
# Voice cloning (consent-gated): stub seam returns a voice_id, or fails clean.
# --------------------------------------------------------------------------
class VoiceCloning(unittest.TestCase):
    def test_clone_returns_stub_voice_id(self):
        engine = _make_engine()
        seen = {}
        orig = server._elevenlabs_clone_voice

        def fake_clone(name, sample_path, api_key, timeout_s=server.ELEVENLABS_TIMEOUT_S):
            seen["name"] = name
            seen["path"] = sample_path
            seen["key"] = api_key
            return "VOICE_CLONE_123"

        server._elevenlabs_clone_voice = fake_clone
        try:
            vid = engine.clone_voice("/tmp/owner.wav", "Dar's Voice", "sk-secret")
        finally:
            server._elevenlabs_clone_voice = orig
        self.assertEqual(vid, "VOICE_CLONE_123")
        self.assertEqual(seen["name"], "Dar's Voice")
        self.assertEqual(seen["path"], "/tmp/owner.wav")
        self.assertEqual(seen["key"], "sk-secret")

    def test_clone_failure_returns_none_clean(self):
        # Any seam error -> None (clean no-clone); the daemon keeps Kokoro / the
        # user's existing voice. NEVER raises out of clone_voice.
        engine = _make_engine()
        orig = server._elevenlabs_clone_voice

        def boom(name, sample_path, api_key, timeout_s=server.ELEVENLABS_TIMEOUT_S):
            raise RuntimeError("connection refused")

        server._elevenlabs_clone_voice = boom
        try:
            vid = engine.clone_voice("/tmp/owner.wav", "Dar", "sk-secret")
        finally:
            server._elevenlabs_clone_voice = orig
        self.assertIsNone(vid)

    def test_clone_seam_refuses_without_key_pre_network(self):
        # Defense in depth: the REAL seam refuses (raises) before any POST when
        # there is no key, and clone_voice turns that into a clean None.
        engine = _make_engine()
        vid = engine.clone_voice("/tmp/owner.wav", "Dar", "")  # NO key
        self.assertIsNone(vid)

    def test_clone_seam_refuses_empty_name_pre_network(self):
        engine = _make_engine()
        # Real seam runs (no monkeypatch); empty name -> ValueError -> clean None.
        vid = engine.clone_voice("/tmp/owner.wav", "   ", "sk-secret")
        self.assertIsNone(vid)


# --------------------------------------------------------------------------
# Gated cloud Scribe STT: backend selection matrix + whisper fallback.
# --------------------------------------------------------------------------
class ScribeSttSelection(unittest.TestCase):
    def _stub_whisper(self, engine, rec):
        def fake_whisper(path):
            rec["whisper"] = rec.get("whisper", 0) + 1
            return "whisper transcript"

        engine._transcribe_whisper = fake_whisper

    def test_no_backend_uses_whisper(self):
        engine = _make_engine()
        rec = {}
        self._stub_whisper(engine, rec)
        orig = server._elevenlabs_scribe_transcribe
        scribe_calls = {"n": 0}

        def fake_scribe(path, key, model=server.ELEVENLABS_SCRIBE_MODEL,
                        timeout_s=server.ELEVENLABS_TIMEOUT_S):
            scribe_calls["n"] += 1
            return "scribe transcript", None

        server._elevenlabs_scribe_transcribe = fake_scribe
        try:
            text, words = engine.transcribe("/tmp/a.wav")
        finally:
            server._elevenlabs_scribe_transcribe = orig
        self.assertEqual(text, "whisper transcript")
        self.assertIsNone(words, "on-device whisper has no diarization -> no words")
        self.assertEqual(rec["whisper"], 1)
        self.assertEqual(scribe_calls["n"], 0, "the cloud STT seam must NOT be touched")

    def test_explicit_whisper_backend_uses_whisper(self):
        engine = _make_engine()
        rec = {}
        self._stub_whisper(engine, rec)
        text, words = engine.transcribe("/tmp/a.wav", backend="whisper", el_key="sk")
        self.assertEqual(text, "whisper transcript")
        self.assertIsNone(words)
        self.assertEqual(rec["whisper"], 1)

    def test_scribe_backend_with_key_uses_scribe(self):
        engine = _make_engine()
        rec = {}
        self._stub_whisper(engine, rec)
        orig = server._elevenlabs_scribe_transcribe
        seen = {}

        def fake_scribe(path, key, model=server.ELEVENLABS_SCRIBE_MODEL,
                        timeout_s=server.ELEVENLABS_TIMEOUT_S):
            seen["path"] = path
            seen["key"] = key
            seen["model"] = model
            return "scribe transcript", None

        server._elevenlabs_scribe_transcribe = fake_scribe
        try:
            text, _words = engine.transcribe(
                "/tmp/a.wav", backend="elevenlabs_scribe", el_key="sk-secret"
            )
        finally:
            server._elevenlabs_scribe_transcribe = orig
        self.assertEqual(text, "scribe transcript")
        self.assertEqual(rec.get("whisper", 0), 0, "whisper must NOT run on the cloud hit")
        self.assertEqual(seen["path"], "/tmp/a.wav")
        self.assertEqual(seen["key"], "sk-secret")
        self.assertEqual(seen["model"], server.ELEVENLABS_SCRIBE_MODEL)

    def test_scribe_error_falls_back_to_whisper(self):
        engine = _make_engine()
        rec = {}
        self._stub_whisper(engine, rec)
        orig = server._elevenlabs_scribe_transcribe

        def boom(path, key, model=server.ELEVENLABS_SCRIBE_MODEL,
                 timeout_s=server.ELEVENLABS_TIMEOUT_S):
            raise RuntimeError("503 from cloud")

        server._elevenlabs_scribe_transcribe = boom
        try:
            text, words = engine.transcribe(
                "/tmp/a.wav", backend="elevenlabs_scribe", el_key="sk"
            )
        finally:
            server._elevenlabs_scribe_transcribe = orig
        # The turn is NEVER failed: whisper serves it.
        self.assertEqual(text, "whisper transcript")
        self.assertIsNone(words, "the whisper fallback carries no diarization")
        self.assertEqual(rec["whisper"], 1)

    def test_scribe_backend_without_key_falls_back_to_whisper(self):
        # The REAL seam refuses (raises) pre-network on a missing key, and
        # transcribe falls back to whisper. No monkeypatch of the seam here.
        engine = _make_engine()
        rec = {}
        self._stub_whisper(engine, rec)
        text, words = engine.transcribe(
            "/tmp/a.wav", backend="elevenlabs_scribe", el_key=""  # NO key
        )
        self.assertEqual(text, "whisper transcript")
        self.assertIsNone(words)
        self.assertEqual(rec["whisper"], 1)


# --------------------------------------------------------------------------
# Key hygiene + seam-surface invariants.
# --------------------------------------------------------------------------
class KeyHygieneAndSeams(unittest.TestCase):
    def test_all_three_network_seams_exist(self):
        self.assertTrue(hasattr(server, "_elevenlabs_synth_pcm"))
        self.assertTrue(hasattr(server, "_elevenlabs_clone_voice"))
        self.assertTrue(hasattr(server, "_elevenlabs_scribe_transcribe"))
        self.assertTrue(hasattr(server.InferenceEngine, "clone_voice"))

    def test_key_rides_only_the_header_constant(self):
        # The auth header is the standard ElevenLabs header, never a query param.
        self.assertEqual(server._ELEVENLABS_HEADER, "xi-api-key")
        self.assertNotIn("?", server._ELEVENLABS_HEADER)
        # The endpoints carry no key/query string baked in.
        for url in (server.ELEVENLABS_CLONE_URL, server.ELEVENLABS_SCRIBE_URL):
            self.assertNotIn("?", url)
            self.assertNotIn("xi-api-key", url)

    def test_redactor_scrubs_header_name_from_logs(self):
        scrubbed = server._redact_elevenlabs("boom near xi-api-key header")
        self.assertNotIn("xi-api-key", scrubbed)

    def test_multipart_body_carries_fields_and_file(self):
        # Pure builder: the body contains the named text fields + the file part,
        # and the returned content-type names the boundary it actually used.
        ct, body = server._multipart_body(
            {"name": "Dar"}, "files", "owner.wav", b"RIFFDATA", content_type="audio/wav"
        )
        self.assertIn("multipart/form-data; boundary=", ct)
        boundary = ct.split("boundary=", 1)[1]
        self.assertIn(boundary.encode("ascii"), body)
        self.assertIn(b'name="name"', body)
        self.assertIn(b"Dar", body)
        self.assertIn(b'filename="owner.wav"', body)
        self.assertIn(b"RIFFDATA", body)


if __name__ == "__main__":
    unittest.main(verbosity=2)
