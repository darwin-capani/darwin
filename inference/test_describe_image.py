#!/usr/bin/env python3
"""Hermetic, NO-MODEL / NO-MLX / NO-NETWORK unit tests for the on-device
vision-language op (op=describe_image) in the inference server.

These tests prove the OP DISPATCH + the lazy-load seam + the honest
UNAVAILABLE-fallback + input validation WITHOUT ever importing mlx-vlm, loading
a model, or touching the network/disk-resident checkpoints:

  * The ONLY place that imports mlx-vlm — the module-level seam
    `server._load_mlx_vlm` — is MONKEYPATCHED in every test. It is replaced with
    either None (the ships-OFF / mlx-vlm-absent state) or a tiny FAKE loader
    whose load/generate are plain Python functions. There is NO real mlx-vlm,
    NO MLX, NO model download, and NO network here.
  * The "available" tests inject a fake VLM that returns canned text, proving
    the op dispatches to describe_image, reads a LOCAL image path, and surfaces
    the text. The "unavailable" tests prove the honest ok:false/reason
    structure (never a fabricated description) the daemon falls back on.

Honesty: this proves the op CONTRACT + the unavailable fallback + the
on-device-only wiring (the image path is read locally; nothing here leaves the
device). It does NOT — and cannot — verify real VLM DESCRIPTION QUALITY: that is
device/runtime-gated (a multi-GB model + RAM) and is never exercised here.

Run: python3 inference/test_describe_image.py   (stdlib only; no pip install)
"""

import asyncio
import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import server  # noqa: E402


def _make_engine(vlm="stub-vlm-repo"):
    """Construct an InferenceEngine without loading any model (all loads are
    lazy). `vlm` is the (fake) checkpoint id; "" disables the op entirely."""
    settings = {
        "llm": "stub-llm",
        "stt": "stub-stt",
        "engine": "kokoro",
        "voice": "bm_george",
        "speed": 1.2,
        "vlm": vlm,
    }
    return server.InferenceEngine(settings, classifier_template="", persona="")


class _FakeVLM:
    """A fake mlx-vlm replacement: records its calls and returns canned text.
    Stands in for the whole {load, generate, apply_chat_template, load_config}
    surface the engine uses — NO MLX, NO model, NO network."""

    def __init__(self, gen_text="a cat sitting on a windowsill in the sun", raise_on=None):
        self.gen_text = gen_text
        self.raise_on = raise_on  # None | "load" | "generate"
        self.load_calls = 0
        self.generate_calls = 0
        self.last_prompt = None
        self.last_images = None

    def load(self, repo):
        self.load_calls += 1
        if self.raise_on == "load":
            raise RuntimeError("checkpoint not downloaded")
        return ("FAKE_MODEL", "FAKE_PROCESSOR")

    def load_config(self, repo):
        return {"model_type": "qwen2_vl"}

    def apply_chat_template(self, processor, config, prompt, num_images=1):
        self.last_prompt = prompt
        return f"<formatted:{num_images}img>{prompt}"

    def generate(self, model, processor, formatted, images, max_tokens=None, verbose=False):
        self.generate_calls += 1
        self.last_images = images
        if self.raise_on == "generate":
            raise RuntimeError("decode failed")
        return self.gen_text

    def as_seam(self):
        """Return the dict server._load_mlx_vlm produces."""
        return {
            "load": self.load,
            "generate": self.generate,
            "apply_chat_template": self.apply_chat_template,
            "load_config": self.load_config,
        }


def _patch_loader(seam_value):
    """Monkeypatch server._load_mlx_vlm to return `seam_value` (None or a fake
    seam dict). Returns a restore callable."""
    orig = server._load_mlx_vlm
    server._load_mlx_vlm = lambda: seam_value
    return lambda: setattr(server, "_load_mlx_vlm", orig)


def _make_image_file():
    """Create a real local temp file to stand in for an image (the op only
    checks existence; the FAKE VLM never decodes it). Returns the path; caller
    deletes it."""
    fd, path = tempfile.mkstemp(suffix=".png")
    os.write(fd, b"\x89PNG\r\n\x1a\n fake pixels (never leave the device)")
    os.close(fd)
    return path


def _dispatch(req):
    """Drive InferenceServer.dispatch for one request with a throwaway engine
    and a dummy writer (describe_image is a single-response op, so the writer is
    never used)."""

    class _DummyWriter:
        def write(self, *_a, **_k):
            pass

    engine = req.pop("_engine")
    srv = server.InferenceServer(engine, preload=False)
    return asyncio.run(srv.dispatch(req, _DummyWriter()))


class DescribeImageUnavailableTests(unittest.TestCase):
    """mlx-vlm absent / model missing => honest structured unavailable, NEVER a
    fabricated description."""

    def test_engine_unavailable_when_mlx_vlm_absent(self):
        # _load_mlx_vlm returns None == mlx-vlm not installed (ships-OFF state).
        restore = _patch_loader(None)
        path = _make_image_file()
        try:
            eng = _make_engine()
            res = eng.describe_image(path)
            self.assertFalse(res["ok"])
            self.assertEqual(res["reason"], server.DESCRIBE_IMAGE_UNAVAILABLE_REASON)
            self.assertIn("not available", res["error"].lower())
            self.assertNotIn("text", res)  # no fabricated description
        finally:
            restore()
            os.unlink(path)

    def test_engine_unavailable_when_vlm_id_empty(self):
        # Empty [models].vlm disables the op even if mlx-vlm WERE installed: we
        # must not even reach the loader.
        fake = _FakeVLM()
        restore = _patch_loader(fake.as_seam())
        path = _make_image_file()
        try:
            eng = _make_engine(vlm="")
            res = eng.describe_image(path)
            self.assertFalse(res["ok"])
            self.assertEqual(res["reason"], server.DESCRIBE_IMAGE_UNAVAILABLE_REASON)
            self.assertEqual(fake.load_calls, 0)  # loader never touched
        finally:
            restore()
            os.unlink(path)

    def test_engine_unavailable_when_checkpoint_load_fails(self):
        # mlx-vlm present but the multi-GB checkpoint is not downloaded ->
        # load() raises -> honest unavailable, not a crash.
        fake = _FakeVLM(raise_on="load")
        restore = _patch_loader(fake.as_seam())
        path = _make_image_file()
        try:
            eng = _make_engine()
            res = eng.describe_image(path)
            self.assertFalse(res["ok"])
            self.assertEqual(res["reason"], server.DESCRIBE_IMAGE_UNAVAILABLE_REASON)
            self.assertIsNone(eng._vlm_model)  # never partially cached
        finally:
            restore()
            os.unlink(path)

    def test_dispatch_unavailable_response_shape(self):
        # The DAEMON-FACING dispatch response on the unavailable path.
        restore = _patch_loader(None)
        path = _make_image_file()
        try:
            eng = _make_engine()
            res = _dispatch({"_engine": eng, "id": "r1", "op": "describe_image", "path": path})
            self.assertEqual(res["id"], "r1")
            self.assertFalse(res["ok"])
            self.assertEqual(res["reason"], server.DESCRIBE_IMAGE_UNAVAILABLE_REASON)
            self.assertIn("error", res)
            self.assertIn("latency_ms", res)
            self.assertNotIn("text", res)  # never invents a description
        finally:
            restore()
            os.unlink(path)


class DescribeImageAvailableTests(unittest.TestCase):
    """With a FAKE injected VLM (no real model): the op dispatches, reads the
    LOCAL image path, and surfaces the description."""

    def test_engine_describe_returns_text(self):
        fake = _FakeVLM(gen_text="a desk with two monitors and a coffee mug")
        restore = _patch_loader(fake.as_seam())
        path = _make_image_file()
        try:
            eng = _make_engine()
            res = eng.describe_image(path)
            self.assertTrue(res["ok"])
            self.assertEqual(res["text"], "a desk with two monitors and a coffee mug")
            self.assertEqual(res["model"], "stub-vlm-repo")
            # The image path was passed LOCALLY to the (fake) VLM — nothing left.
            self.assertEqual(fake.last_images, [path])
            self.assertEqual(fake.generate_calls, 1)
        finally:
            restore()
            os.unlink(path)

    def test_default_prompt_when_no_question(self):
        fake = _FakeVLM()
        restore = _patch_loader(fake.as_seam())
        path = _make_image_file()
        try:
            eng = _make_engine()
            eng.describe_image(path)  # no question
            self.assertEqual(fake.last_prompt, server.DESCRIBE_IMAGE_DEFAULT_PROMPT)
        finally:
            restore()
            os.unlink(path)

    def test_question_is_forwarded(self):
        fake = _FakeVLM(gen_text="There are three people.")
        restore = _patch_loader(fake.as_seam())
        path = _make_image_file()
        try:
            eng = _make_engine()
            res = eng.describe_image(path, question="How many people are in the photo?")
            self.assertTrue(res["ok"])
            self.assertEqual(fake.last_prompt, "How many people are in the photo?")
        finally:
            restore()
            os.unlink(path)

    def test_model_loaded_once_then_resident(self):
        fake = _FakeVLM()
        restore = _patch_loader(fake.as_seam())
        path = _make_image_file()
        try:
            eng = _make_engine()
            eng.describe_image(path)
            eng.describe_image(path, question="and now?")
            self.assertEqual(fake.load_calls, 1)  # lazy-loaded once, then cached
            self.assertEqual(fake.generate_calls, 2)
        finally:
            restore()
            os.unlink(path)

    def test_empty_generation_is_unavailable_not_fabricated(self):
        # An empty model output must NOT surface as an ok description.
        fake = _FakeVLM(gen_text="   ")
        restore = _patch_loader(fake.as_seam())
        path = _make_image_file()
        try:
            eng = _make_engine()
            res = eng.describe_image(path)
            self.assertFalse(res["ok"])
            self.assertEqual(res["reason"], server.DESCRIBE_IMAGE_UNAVAILABLE_REASON)
        finally:
            restore()
            os.unlink(path)

    def test_generation_failure_falls_back_honestly(self):
        # A runtime decode failure on a loaded model -> honest unavailable, not
        # a crash and not a fabricated answer.
        fake = _FakeVLM(raise_on="generate")
        restore = _patch_loader(fake.as_seam())
        path = _make_image_file()
        try:
            eng = _make_engine()
            res = eng.describe_image(path)
            self.assertFalse(res["ok"])
            self.assertEqual(res["reason"], server.DESCRIBE_IMAGE_UNAVAILABLE_REASON)
        finally:
            restore()
            os.unlink(path)

    def test_dispatch_available_response_shape(self):
        fake = _FakeVLM(gen_text="a city skyline at dusk")
        restore = _patch_loader(fake.as_seam())
        path = _make_image_file()
        try:
            eng = _make_engine()
            res = _dispatch(
                {
                    "_engine": eng,
                    "id": "r2",
                    "op": "describe_image",
                    "path": path,
                    "question": "what is this?",
                }
            )
            self.assertEqual(res["id"], "r2")
            self.assertTrue(res["ok"])
            self.assertEqual(res["text"], "a city skyline at dusk")
            self.assertEqual(res["model"], "stub-vlm-repo")
            self.assertIn("latency_ms", res)
        finally:
            restore()
            os.unlink(path)

    def test_generate_tuple_return_normalized(self):
        # Some mlx-vlm versions return (text, usage); we normalize to the text.
        fake = _FakeVLM()
        fake.generate = lambda *a, **k: ("a red bicycle leaning on a wall", {"tokens": 9})
        restore = _patch_loader(fake.as_seam())
        path = _make_image_file()
        try:
            eng = _make_engine()
            res = eng.describe_image(path)
            self.assertTrue(res["ok"])
            self.assertEqual(res["text"], "a red bicycle leaning on a wall")
        finally:
            restore()
            os.unlink(path)


class DescribeImageValidationTests(unittest.TestCase):
    """Input validation + the path-existence guard (a bad path is a caller error,
    NOT a VLM-unavailable condition)."""

    def test_missing_path_raises(self):
        eng = _make_engine()
        with self.assertRaises(ValueError):
            eng.describe_image(None)
        with self.assertRaises(ValueError):
            eng.describe_image("")

    def test_nonexistent_path_raises_before_loading_model(self):
        fake = _FakeVLM()
        restore = _patch_loader(fake.as_seam())
        try:
            eng = _make_engine()
            with self.assertRaises(ValueError):
                eng.describe_image("/no/such/image/file.png")
            self.assertEqual(fake.load_calls, 0)  # never reached the model
        finally:
            restore()

    def test_non_string_question_raises(self):
        eng = _make_engine()
        path = _make_image_file()
        try:
            with self.assertRaises(ValueError):
                eng.describe_image(path, question=123)
        finally:
            os.unlink(path)

    def test_bad_max_tokens_raises(self):
        eng = _make_engine()
        path = _make_image_file()
        try:
            with self.assertRaises(ValueError):
                eng.describe_image(path, max_tokens=0)
            with self.assertRaises(ValueError):
                eng.describe_image(path, max_tokens=-5)
        finally:
            os.unlink(path)

    def test_max_tokens_capped(self):
        # An over-large request is capped, not rejected; the fake records the
        # value the engine forwarded.
        seen = {}
        fake = _FakeVLM()

        def gen(model, processor, formatted, images, max_tokens=None, verbose=False):
            seen["max_tokens"] = max_tokens
            return "ok"

        fake.generate = gen
        restore = _patch_loader(fake.as_seam())
        path = _make_image_file()
        try:
            eng = _make_engine()
            eng.describe_image(path, max_tokens=10_000)
            self.assertEqual(seen["max_tokens"], server.DESCRIBE_IMAGE_MAX_TOKENS_CAP)
        finally:
            restore()
            os.unlink(path)

    def test_dispatch_rejects_missing_path(self):
        eng = _make_engine()
        res = _dispatch({"_engine": eng, "id": "r3", "op": "describe_image"})
        self.assertFalse(res["ok"])
        self.assertIn("path", res["error"].lower())


class OpIsolationTests(unittest.TestCase):
    """describe_image is a DISTINCT op; adding it must not regress the others."""

    def test_unknown_op_still_unknown(self):
        eng = _make_engine()
        res = _dispatch({"_engine": eng, "id": "r4", "op": "definitely_not_an_op"})
        self.assertFalse(res["ok"])
        self.assertIn("unknown op", res["error"].lower())

    def test_describe_image_does_not_touch_stt_or_embed_loaders(self):
        # The VLM op uses its OWN lazy-load fields; it must not load the LLM/STT.
        fake = _FakeVLM()
        restore = _patch_loader(fake.as_seam())
        path = _make_image_file()
        try:
            eng = _make_engine()
            eng.describe_image(path)
            self.assertIsNone(eng._model)  # the generate/embed LLM untouched
            self.assertIsNone(eng._tokenizer)
        finally:
            restore()
            os.unlink(path)


if __name__ == "__main__":
    unittest.main(verbosity=2)
