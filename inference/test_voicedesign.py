#!/usr/bin/env python3
"""Hermetic, NO-NETWORK unit tests for the ElevenLabs VOICE-DESIGN tier
(op=design_voice) in the inference server.

The network seam (`_elevenlabs_design_voice`) is MONKEYPATCHED in every engine
test — it is the ONLY place voice design touches ElevenLabs — so there is NO real
HTTP here. These prove the payload shaping for BOTH steps (design previews +
create-from-preview), the seam's key/description/name guards, and the engine's
honest "no voice on failure" contract (there is NO on-device voice designer to
substitute). Unlike clone, NO audio sample leaves the device.

Run: python3 inference/test_voicedesign.py   (stdlib + numpy only; no pip install)
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


class DesignPayload(unittest.TestCase):
    def test_design_description_only(self):
        self.assertEqual(
            server._build_design_payload("a warm calm British narrator"),
            {"voice_description": "a warm calm British narrator"},
        )

    def test_design_includes_text_when_present(self):
        b = server._build_design_payload("a deep voice", text="Hello there, friend.")
        self.assertEqual(b["voice_description"], "a deep voice")
        self.assertEqual(b["text"], "Hello there, friend.")

    def test_design_omits_blank_or_bad_text(self):
        self.assertNotIn("text", server._build_design_payload("desc", text="   "))
        self.assertNotIn("text", server._build_design_payload("desc", text=None))
        self.assertNotIn("text", server._build_design_payload("desc", text=123))

    def test_create_payload_has_all_three_required_fields(self):
        b = server._build_design_create_payload("Aurora", "a warm voice", "gen-123")
        self.assertEqual(b, {
            "voice_name": "Aurora",
            "voice_description": "a warm voice",
            "generated_voice_id": "gen-123",
        })


class DesignSeamGuards(unittest.TestCase):
    """The seam refuses keyless / descriptionless / nameless calls BEFORE any net."""

    def test_seam_requires_key(self):
        with self.assertRaises(ValueError):
            server._elevenlabs_design_voice("a warm voice", "Aurora", api_key="")

    def test_seam_requires_description(self):
        with self.assertRaises(ValueError):
            server._elevenlabs_design_voice("   ", "Aurora", api_key="sk-x")

    def test_seam_requires_name(self):
        with self.assertRaises(ValueError):
            server._elevenlabs_design_voice("a warm voice", "  ", api_key="sk-x")


class DesignSeamKeyHandling(unittest.TestCase):
    """The key rides ONLY the xi-api-key header — never the URL, query, or body.
    Drive the seam against a fake urlopen so NO real network touch happens."""

    def _run_capture(self, design_resp, create_resp):
        import json
        import urllib.request

        captured = {"urls": [], "headers": [], "bodies": []}
        responses = [design_resp, create_resp]

        class _FakeResp:
            def __init__(self, payload):
                self._payload = json.dumps(payload).encode("utf-8")

            def __enter__(self):
                return self

            def __exit__(self, *a):
                return False

            def read(self):
                return self._payload

        def fake_urlopen(req, timeout=None):
            captured["urls"].append(req.full_url)
            captured["headers"].append(dict(req.header_items()))
            captured["bodies"].append(req.data)
            return _FakeResp(responses.pop(0))

        orig = urllib.request.urlopen
        urllib.request.urlopen = fake_urlopen
        try:
            vid = server._elevenlabs_design_voice(
                "a warm calm narrator voice", "Aurora", api_key="sk-secret-123"
            )
        finally:
            urllib.request.urlopen = orig
        return vid, captured

    def test_key_only_in_header_and_voice_id_returned(self):
        vid, cap = self._run_capture(
            {"previews": [{"generated_voice_id": "gen-xyz"}]},
            {"voice_id": "voice-final-789"},
        )
        self.assertEqual(vid, "voice-final-789")
        # Two POSTs: design then create.
        self.assertEqual(cap["urls"][0], server.ELEVENLABS_DESIGN_URL)
        self.assertEqual(cap["urls"][1], server.ELEVENLABS_DESIGN_CREATE_URL)
        for url in cap["urls"]:
            self.assertNotIn("sk-secret-123", url)
        for hdrs in cap["headers"]:
            # urllib title-cases header names.
            self.assertEqual(hdrs.get("Xi-api-key"), "sk-secret-123")
        for body in cap["bodies"]:
            self.assertNotIn(b"sk-secret-123", body)
        # The step-2 body carries the generated_voice_id from step 1.
        import json
        create_body = json.loads(cap["bodies"][1].decode("utf-8"))
        self.assertEqual(create_body["generated_voice_id"], "gen-xyz")
        self.assertEqual(create_body["voice_name"], "Aurora")

    def test_missing_preview_raises(self):
        with self.assertRaises(ValueError):
            self._run_capture({"previews": []}, {"voice_id": "x"})

    def test_missing_voice_id_raises(self):
        with self.assertRaises(ValueError):
            self._run_capture(
                {"previews": [{"generated_voice_id": "gen-xyz"}]}, {"nope": True}
            )


class DesignEngine(unittest.TestCase):
    def _patch(self, voice_id=None, raises=None):
        orig = server._elevenlabs_design_voice

        def fake(description, name, api_key, text=None,
                 timeout_s=server.ELEVENLABS_TIMEOUT_S):
            if raises is not None:
                raise raises
            return voice_id

        server._elevenlabs_design_voice = fake
        return lambda: setattr(server, "_elevenlabs_design_voice", orig)

    def test_success_returns_voice_id(self):
        engine = _make_engine()
        restore = self._patch(voice_id="voice-abc-123")
        try:
            self.assertEqual(
                engine.design_voice("a warm narrator", "Aurora", el_key="sk-x"),
                "voice-abc-123",
            )
        finally:
            restore()

    def test_failure_returns_none(self):
        engine = _make_engine()
        restore = self._patch(raises=RuntimeError("boom"))
        try:
            self.assertIsNone(engine.design_voice("a warm narrator", "Aurora", el_key="sk-x"))
        finally:
            restore()

    def test_empty_voice_id_returns_none(self):
        engine = _make_engine()
        restore = self._patch(voice_id="")
        try:
            self.assertIsNone(engine.design_voice("a warm narrator", "Aurora", el_key="sk-x"))
        finally:
            restore()

    def test_no_key_falls_to_none(self):
        # No key -> the REAL seam raises ValueError before any net -> engine None.
        engine = _make_engine()
        self.assertIsNone(engine.design_voice("a warm narrator", "Aurora", el_key=""))


if __name__ == "__main__":
    unittest.main()
