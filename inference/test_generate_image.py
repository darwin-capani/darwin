#!/usr/bin/env python3
"""Hermetic, NO-MODEL / NO-MLX / NO-GENERATION / NO-NETWORK unit tests for the
on-device text->image op (op=generate_image) in the inference server.

These tests prove the OP DISPATCH + the lazy-load seam + the honest
UNAVAILABLE-fallback + input validation WITHOUT ever importing the diffusion
package, loading a model, generating a real image, or touching the network:

  * The ONLY place that imports the MLX diffusion package — the module-level
    seam `server._load_mlx_diffusion` — is MONKEYPATCHED in every test. It is
    replaced with either None (the ships-OFF / package-absent state) or a tiny
    FAKE generator whose Flux1/Config are plain Python objects. There is NO real
    diffusion package, NO MLX, NO model download, NO image generation, and NO
    network here.
  * The "available" tests inject a fake generator that writes a small stub file
    to the on-device path, proving the op dispatches to generate_image, runs
    on-device, saves under state/images/, and surfaces the LOCAL path. The
    "unavailable" tests prove the honest ok:false/reason structure (never a
    fabricated image, NEVER a cloud call) the daemon surfaces honestly.

Honesty: this proves the op CONTRACT + the unavailable fallback + the
on-device-only wiring (the prompt is handed only to the local generator; the
saved path is under state/; nothing here leaves the device). It does NOT — and
cannot — verify real image QUALITY or speed: those are device/runtime-gated (a
multi-GB model + RAM) and are never exercised here.

Run: python3 inference/test_generate_image.py   (stdlib only; no pip install)
"""

import asyncio
import os
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import server  # noqa: E402


def _make_engine(image_model="stub-image-model"):
    """Construct an InferenceEngine without loading any model (all loads are
    lazy). `image_model` is the (fake) checkpoint id; "" disables the op."""
    settings = {
        "llm": "stub-llm",
        "stt": "stub-stt",
        "engine": "kokoro",
        "voice": "bm_george",
        "speed": 1.2,
        "image_model": image_model,
    }
    return server.InferenceEngine(settings, classifier_template="", persona="")


class _FakeGenerated:
    """A fake generation result: its .save(path) writes a tiny stub PNG to the
    on-device path so the engine's post-save existence check passes. NO real
    pixels, NO model — just proof the path-save wiring runs locally."""

    def __init__(self, write=True):
        self.write = write
        self.saved_to = None

    def save(self, path=None):
        self.saved_to = path
        if self.write and path is not None:
            with open(path, "wb") as f:
                f.write(b"\x89PNG\r\n\x1a\n fake on-device pixels (never leave)")


class _FakeConfig:
    """Stands in for mflux Config: records the params the engine forwarded."""

    def __init__(self, num_inference_steps=None, height=None, width=None):
        self.num_inference_steps = num_inference_steps
        self.height = height
        self.width = width


class _FakeFlux1:
    """A fake mflux Flux1 replacement: records its calls and returns a fake
    generated image. Stands in for the whole {Flux1, Config} surface the engine
    uses — NO MLX, NO model, NO real generation, NO network."""

    last_instance = None

    def __init__(self, model_name=None, raise_on_load=False, raise_on_generate=False, write=True):
        self.model_name = model_name
        self.raise_on_generate = raise_on_generate
        self.write = write
        self.generate_calls = 0
        self.last_prompt = None
        self.last_seed = None
        self.last_config = None
        type(self).last_instance = self

    def generate_image(self, seed=None, prompt=None, config=None):
        self.generate_calls += 1
        self.last_seed = seed
        self.last_prompt = prompt
        self.last_config = config
        if self.raise_on_generate:
            raise RuntimeError("decode failed")
        return _FakeGenerated(write=self.write)


def _make_seam(flux_factory=None, config_cls=_FakeConfig):
    """Build the dict server._load_mlx_diffusion produces from a Flux1 factory.
    The factory is called as Flux1(model_name=...) and counts load attempts."""
    state = {"load_calls": 0}

    def Flux1(model_name=None):
        state["load_calls"] += 1
        if flux_factory is None:
            return _FakeFlux1(model_name=model_name)
        return flux_factory(model_name)

    return {"Flux1": Flux1, "Config": config_cls}, state


def _patch_loader(seam_value):
    """Monkeypatch server._load_mlx_diffusion to return `seam_value` (None or a
    fake seam dict). Returns a restore callable."""
    orig = server._load_mlx_diffusion
    server._load_mlx_diffusion = lambda: seam_value
    return lambda: setattr(server, "_load_mlx_diffusion", orig)


def _dispatch(req):
    """Drive InferenceServer.dispatch for one request with a given engine and a
    dummy writer (generate_image is a single-response op, writer unused)."""

    class _DummyWriter:
        def write(self, *_a, **_k):
            pass

    engine = req.pop("_engine")
    srv = server.InferenceServer(engine, preload=False)
    return asyncio.run(srv.dispatch(req, _DummyWriter()))


def _cleanup(res):
    """Delete a generated stub file if the result reported an on-device path."""
    if isinstance(res, dict) and res.get("ok") and res.get("path"):
        try:
            os.unlink(res["path"])
        except OSError:
            pass


class GenerateImageUnavailableTests(unittest.TestCase):
    """Diffusion package absent / model missing => honest structured
    unavailable, NEVER a fabricated image and NEVER a cloud call."""

    def test_engine_unavailable_when_package_absent(self):
        # _load_mlx_diffusion returns None == the diffusion package is not
        # installed (the normal ships-OFF state).
        restore = _patch_loader(None)
        try:
            eng = _make_engine()
            res = eng.generate_image("a red bicycle on a beach")
            self.assertFalse(res["ok"])
            self.assertEqual(res["reason"], server.GENERATE_IMAGE_UNAVAILABLE_REASON)
            self.assertIn("not available", res["error"].lower())
            self.assertNotIn("path", res)  # no fabricated image path
        finally:
            restore()

    def test_engine_unavailable_when_model_id_empty(self):
        # Empty [image] model id disables the op even if the package WERE
        # installed: we must not even reach the loader.
        seam, state = _make_seam()
        restore = _patch_loader(seam)
        try:
            eng = _make_engine(image_model="")
            res = eng.generate_image("a sunset")
            self.assertFalse(res["ok"])
            self.assertEqual(res["reason"], server.GENERATE_IMAGE_UNAVAILABLE_REASON)
            self.assertEqual(state["load_calls"], 0)  # loader never touched
        finally:
            restore()

    def test_engine_unavailable_when_checkpoint_load_fails(self):
        # Package present but the multi-GB checkpoint is not downloaded ->
        # Flux1(...) raises -> honest unavailable, not a crash.
        def boom(model_name):
            raise RuntimeError("checkpoint not downloaded")

        seam, _state = _make_seam(flux_factory=boom)
        restore = _patch_loader(seam)
        try:
            eng = _make_engine()
            res = eng.generate_image("a forest")
            self.assertFalse(res["ok"])
            self.assertEqual(res["reason"], server.GENERATE_IMAGE_UNAVAILABLE_REASON)
            self.assertIsNone(eng._image_model)  # never partially cached
        finally:
            restore()

    def test_dispatch_unavailable_response_shape(self):
        # The DAEMON-FACING dispatch response on the unavailable path. NO cloud
        # fallback — just an honest ok:false + machine-keyable reason.
        restore = _patch_loader(None)
        try:
            eng = _make_engine()
            res = _dispatch(
                {"_engine": eng, "id": "g1", "op": "generate_image", "prompt": "a kite"}
            )
            self.assertEqual(res["id"], "g1")
            self.assertFalse(res["ok"])
            self.assertEqual(res["reason"], server.GENERATE_IMAGE_UNAVAILABLE_REASON)
            self.assertIn("error", res)
            self.assertIn("latency_ms", res)
            self.assertNotIn("path", res)  # never invents an image
        finally:
            restore()

    def test_generation_failure_falls_back_honestly(self):
        # A runtime decode failure on a loaded model -> honest unavailable, not
        # a crash, not a fabricated image, NOT a cloud call.
        def factory(model_name):
            return _FakeFlux1(model_name=model_name, raise_on_generate=True)

        seam, _state = _make_seam(flux_factory=factory)
        restore = _patch_loader(seam)
        try:
            eng = _make_engine()
            res = eng.generate_image("a mountain")
            self.assertFalse(res["ok"])
            self.assertEqual(res["reason"], server.GENERATE_IMAGE_UNAVAILABLE_REASON)
        finally:
            restore()

    def test_missing_output_file_is_unavailable_not_fabricated(self):
        # The generator claims success but writes no file -> unavailable, not a
        # path to nothing.
        def factory(model_name):
            return _FakeFlux1(model_name=model_name, write=False)

        seam, _state = _make_seam(flux_factory=factory)
        restore = _patch_loader(seam)
        try:
            eng = _make_engine()
            res = eng.generate_image("a teapot")
            self.assertFalse(res["ok"])
            self.assertEqual(res["reason"], server.GENERATE_IMAGE_UNAVAILABLE_REASON)
        finally:
            restore()


class GenerateImageAvailableTests(unittest.TestCase):
    """With a FAKE injected generator (no real model): the op dispatches, runs
    on-device, saves under state/images/, and surfaces the LOCAL path."""

    def test_engine_generate_returns_local_path(self):
        seam, _state = _make_seam()
        restore = _patch_loader(seam)
        res = None
        try:
            eng = _make_engine()
            res = eng.generate_image("a city skyline at dusk")
            self.assertTrue(res["ok"])
            self.assertEqual(res["model"], "stub-image-model")
            # The saved path is ON-DEVICE under state/images/ (nothing left).
            self.assertTrue(res["path"].endswith(".png"))
            self.assertTrue(
                str(server.IMAGES_DIR) in res["path"],
                f"path not under state/images/: {res['path']}",
            )
            self.assertTrue(os.path.isfile(res["path"]))
            # The prompt was handed to the LOCAL (fake) generator only.
            self.assertEqual(_FakeFlux1.last_instance.last_prompt, "a city skyline at dusk")
            self.assertEqual(_FakeFlux1.last_instance.generate_calls, 1)
        finally:
            restore()
            _cleanup(res)

    def test_default_size_and_steps_forwarded(self):
        seam, _state = _make_seam()
        restore = _patch_loader(seam)
        res = None
        try:
            eng = _make_engine()
            res = eng.generate_image("a lighthouse")
            self.assertEqual(res["size"], server.GENERATE_IMAGE_DEFAULT_SIZE)
            self.assertEqual(res["steps"], server.GENERATE_IMAGE_DEFAULT_STEPS)
            cfg = _FakeFlux1.last_instance.last_config
            self.assertEqual(cfg.height, server.GENERATE_IMAGE_DEFAULT_SIZE)
            self.assertEqual(cfg.width, server.GENERATE_IMAGE_DEFAULT_SIZE)
            self.assertEqual(cfg.num_inference_steps, server.GENERATE_IMAGE_DEFAULT_STEPS)
        finally:
            restore()
            _cleanup(res)

    def test_seed_is_forwarded_and_echoed(self):
        seam, _state = _make_seam()
        restore = _patch_loader(seam)
        res = None
        try:
            eng = _make_engine()
            res = eng.generate_image("a robot", seed=42)
            self.assertTrue(res["ok"])
            self.assertEqual(res["seed"], 42)
            self.assertEqual(_FakeFlux1.last_instance.last_seed, 42)
        finally:
            restore()
            _cleanup(res)

    def test_model_loaded_once_then_resident(self):
        seam, state = _make_seam()
        restore = _patch_loader(seam)
        results = []
        try:
            eng = _make_engine()
            results.append(eng.generate_image("a cat"))
            results.append(eng.generate_image("a dog"))
            self.assertEqual(state["load_calls"], 1)  # lazy-loaded once, cached
            self.assertEqual(_FakeFlux1.last_instance.generate_calls, 2)
        finally:
            restore()
            for r in results:
                _cleanup(r)

    def test_dispatch_available_response_shape(self):
        seam, _state = _make_seam()
        restore = _patch_loader(seam)
        res = None
        try:
            eng = _make_engine()
            res = _dispatch(
                {
                    "_engine": eng,
                    "id": "g2",
                    "op": "generate_image",
                    "prompt": "a paper airplane",
                    "size": 256,
                    "steps": 6,
                    "seed": 7,
                }
            )
            self.assertEqual(res["id"], "g2")
            self.assertTrue(res["ok"])
            self.assertEqual(res["model"], "stub-image-model")
            self.assertEqual(res["size"], 256)
            self.assertEqual(res["steps"], 6)
            self.assertEqual(res["seed"], 7)
            self.assertTrue(os.path.isfile(res["path"]))
            self.assertIn("latency_ms", res)
        finally:
            restore()
            _cleanup(res)


class GenerateImageValidationTests(unittest.TestCase):
    """Input validation + the param bounds (a bad prompt is a caller error, NOT
    a model-unavailable condition)."""

    def test_missing_prompt_raises(self):
        eng = _make_engine()
        with self.assertRaises(ValueError):
            eng.generate_image(None)
        with self.assertRaises(ValueError):
            eng.generate_image("")
        with self.assertRaises(ValueError):
            eng.generate_image("   ")

    def test_non_string_prompt_raises(self):
        eng = _make_engine()
        with self.assertRaises(ValueError):
            eng.generate_image(123)

    def test_bad_size_raises_before_loading_model(self):
        seam, state = _make_seam()
        restore = _patch_loader(seam)
        try:
            eng = _make_engine()
            with self.assertRaises(ValueError):
                eng.generate_image("x", size=0)
            with self.assertRaises(ValueError):
                eng.generate_image("x", size=-10)
            self.assertEqual(state["load_calls"], 0)  # never reached the model
        finally:
            restore()

    def test_bad_steps_raises(self):
        eng = _make_engine()
        with self.assertRaises(ValueError):
            eng.generate_image("x", steps=0)
        with self.assertRaises(ValueError):
            eng.generate_image("x", steps=-3)

    def test_bad_seed_raises(self):
        eng = _make_engine()
        with self.assertRaises(ValueError):
            eng.generate_image("x", seed="not-an-int")

    def test_size_and_steps_capped(self):
        seam, _state = _make_seam()
        restore = _patch_loader(seam)
        res = None
        try:
            eng = _make_engine()
            res = eng.generate_image("x", size=99_999, steps=10_000)
            self.assertEqual(res["size"], server.GENERATE_IMAGE_MAX_SIZE)
            self.assertEqual(res["steps"], server.GENERATE_IMAGE_MAX_STEPS_CAP)
        finally:
            restore()
            _cleanup(res)

    def test_size_floored_to_min(self):
        seam, _state = _make_seam()
        restore = _patch_loader(seam)
        res = None
        try:
            eng = _make_engine()
            res = eng.generate_image("x", size=1)
            self.assertEqual(res["size"], server.GENERATE_IMAGE_MIN_SIZE)
        finally:
            restore()
            _cleanup(res)

    def test_dispatch_rejects_missing_prompt(self):
        eng = _make_engine()
        res = _dispatch({"_engine": eng, "id": "g3", "op": "generate_image"})
        self.assertFalse(res["ok"])
        self.assertIn("prompt", res["error"].lower())

    def test_dispatch_rejects_bad_size(self):
        eng = _make_engine()
        res = _dispatch(
            {"_engine": eng, "id": "g4", "op": "generate_image", "prompt": "x", "size": -1}
        )
        self.assertFalse(res["ok"])
        self.assertIn("size", res["error"].lower())


class OpIsolationTests(unittest.TestCase):
    """generate_image is a DISTINCT op; adding it must not regress the others."""

    def test_unknown_op_still_unknown(self):
        eng = _make_engine()
        res = _dispatch({"_engine": eng, "id": "g5", "op": "definitely_not_an_op"})
        self.assertFalse(res["ok"])
        self.assertIn("unknown op", res["error"].lower())

    def test_generate_image_does_not_touch_llm_or_vlm_loaders(self):
        # The image op uses its OWN lazy-load field; it must not load the
        # LLM/STT or the VLM.
        seam, _state = _make_seam()
        restore = _patch_loader(seam)
        res = None
        try:
            eng = _make_engine()
            res = eng.generate_image("a balloon")
            self.assertIsNone(eng._model)  # the generate/embed LLM untouched
            self.assertIsNone(eng._tokenizer)
            self.assertIsNone(eng._vlm_model)  # the VLM untouched
        finally:
            restore()
            _cleanup(res)


if __name__ == "__main__":
    unittest.main(verbosity=2)
