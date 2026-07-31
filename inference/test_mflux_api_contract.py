"""op=generate_image must actually be able to call the mflux that is installed.

THE DEFECT THIS PINS. `_load_mlx_diffusion()` did `from mflux import Config, Flux1`.
mflux moved both out of its top level -- Flux1 to mflux.models.flux.variants.txt2img
.flux, Config to mflux.models.common.config.config -- and its __init__ now exports
nothing at all. The import raised ImportError, a bare `except Exception: return None`
swallowed it, and the op was 100% DEAD on every version the installer resolves (it pins
only `mflux>=0.4`; 0.18.0 is what lands today).

Nothing noticed, for three compounding reasons:

  1. The failure is indistinguishable from the ships-OFF state. Both return None, and
     the user-facing message says "image generation is off or its model is not
     downloaded" -- false on both counts, since config ships enabled = true and the
     package IS installed. An operator diagnosing it chases the Hugging Face gate and a
     multi-GB download that cannot change the outcome.
  2. The existing 22-case test_generate_image.py monkeypatches server._load_mlx_diffusion
     -- the exact function that was broken -- so it can never observe the real package.
  3. The docstring claimed the loader "tolerates minor version layout differences". It
     had one import path.

WHY THIS FILE DOES NOT MOCK. A test that substitutes a fake mflux re-creates defect 2.
These tests read the REAL installed package. They do not load weights and do not touch
the GPU: the FLUX checkpoint is a gated multi-GB download and is not present here, so
end-to-end generation is deliberately out of scope. What IS checked is the thing that
actually broke -- that the symbols resolve and that every argument server.py passes
exists in the installed signatures. `inspect.Signature.bind` is the whole point: it
fails on API drift the moment it happens, which is months before anyone asks for a
picture.

  Run: .venv/bin/python inference/test_mflux_api_contract.py   (from the repo root)
"""
import inspect
import pathlib
import unittest

import server as S

REPO = pathlib.Path(__file__).resolve().parent.parent
SRC = (REPO / "inference" / "server.py").read_text(encoding="utf-8")

try:
    import mflux  # noqa: F401
    HAVE_MFLUX = True
except Exception:  # noqa: BLE001 - dep-gated like the other device tests
    HAVE_MFLUX = False


@unittest.skipUnless(HAVE_MFLUX, "mflux not installed (the genuine ships-OFF state)")
class TheInstalledMfluxIsActuallyUsable(unittest.TestCase):
    def setUp(self):
        self.diff = S._load_mlx_diffusion()

    def test_the_loader_finds_the_api(self):
        self.assertIsInstance(
            self.diff, dict,
            "mflux is installed but _load_mlx_diffusion() did not resolve its API. "
            "op=generate_image is dead and the user is told the model is 'off or not "
            "downloaded', which is false on both counts",
        )

    def test_it_did_not_silently_return_the_mismatch_sentinel(self):
        self.assertIsNot(
            self.diff, S._MFLUX_API_MISMATCH,
            "the installed mflux exposes neither known layout -- add the new one",
        )

    def test_the_constructor_accepts_what_server_py_passes(self):
        """The exact construction _ensure_image_model performs, bound against the real
        signature. This is what broke: model_name= no longer exists."""
        sig = inspect.signature(self.diff["Flux1"].__init__)
        if self.diff.get("modern"):
            sig.bind(None, model_config=self.diff["ModelConfig"].from_name("schnell"))
        else:
            sig.bind(None, model_name="schnell")

    def test_the_alias_in_config_resolves(self):
        """DEFAULT_IMAGE_MODEL is an mflux ALIAS ("schnell"), not a repo id. If mflux
        stops recognising it, the op dies with the same invisible signature."""
        if not self.diff.get("modern"):
            self.skipTest("legacy layout resolves the alias inside Flux1")
        self.assertIsNotNone(
            self.diff["ModelConfig"].from_name(S.DEFAULT_IMAGE_MODEL),
            f"mflux no longer resolves the alias {S.DEFAULT_IMAGE_MODEL!r}",
        )

    def test_generate_image_accepts_what_server_py_passes(self):
        """The generation call, bound against the real signature. The old API took a
        Config object; the new one takes the parameters directly."""
        gen = self.diff["Flux1"].generate_image
        sig = inspect.signature(gen)
        if self.diff.get("modern"):
            sig.bind(None, seed=1, prompt="a red cube",
                     num_inference_steps=4, height=512, width=512)
        else:
            cfg_sig = inspect.signature(self.diff["Config"].__init__)
            cfg_sig.bind(None, num_inference_steps=4, height=512, width=512)
            sig.bind(None, seed=1, prompt="a red cube", config=object())

    def test_the_result_object_can_be_saved_the_way_server_py_saves_it(self):
        """server.py calls generated.save(path=str(out_path)). A positional-only or
        renamed parameter here would fail only after a full multi-minute generation."""
        ann = inspect.signature(self.diff["Flux1"].generate_image).return_annotation
        if ann is inspect.Signature.empty or isinstance(ann, str):
            self.skipTest("mflux does not annotate the return type")
        self.assertTrue(hasattr(ann, "save"), f"{ann!r} has no .save()")
        inspect.signature(ann.save).bind(None, path="/tmp/x.png")


class AnInstalledPackageWeCannotCallIsReportedNotSwallowed(unittest.TestCase):
    """The reason this hid for so long: total failure looked exactly like 'switched
    off'. These do not need mflux present."""

    def test_a_missing_package_is_still_the_silent_ships_off_state(self):
        real = S.importlib.import_module

        def no_mflux(name, *a, **k):
            if name == "mflux" or name.startswith("mflux."):
                raise ImportError("No module named 'mflux'")
            return real(name, *a, **k)

        S.importlib.import_module = no_mflux
        try:
            import builtins
            real_import = builtins.__import__

            def blocked(name, *a, **k):
                if name == "mflux" or name.startswith("mflux."):
                    raise ImportError("No module named 'mflux'")
                return real_import(name, *a, **k)

            builtins.__import__ = blocked
            try:
                self.assertIsNone(
                    S._load_mlx_diffusion(),
                    "an absent package must stay the quiet, expected ships-OFF state",
                )
            finally:
                builtins.__import__ = real_import
        finally:
            S.importlib.import_module = real

    def test_an_unusable_installed_package_is_distinguishable(self):
        """Present but no known layout must NOT return the same None as 'not
        installed' -- that equivalence is what made a dead feature look intentional."""
        real = S.importlib.import_module

        def moved(name, *a, **k):
            if name.startswith("mflux"):
                raise ImportError("layout moved again")
            return real(name, *a, **k)

        S.importlib.import_module = moved
        try:
            if not HAVE_MFLUX:
                self.skipTest("needs the top-level package to import")
            self.assertIs(
                S._load_mlx_diffusion(), S._MFLUX_API_MISMATCH,
                "an installed-but-uncallable mflux must be reported separately from "
                "the ships-OFF state, and logged, or the next API move hides the same "
                "way this one did",
            )
        finally:
            S.importlib.import_module = real

    def test_the_engine_treats_the_sentinel_as_unavailable(self):
        """The sentinel is a truthy string; if _ensure_image_model forgot it, the code
        would subscript a str and raise instead of reporting unavailable."""
        self.assertIn("diff is _MFLUX_API_MISMATCH", SRC)

    def test_the_docstring_no_longer_claims_tolerance_it_lacks(self):
        body = SRC[SRC.index("def _load_mlx_diffusion("):]
        body = body[:body.index("\n_MFLUX_API_MISMATCH")]
        self.assertNotIn("tolerate minor version layout differences", body)
        self.assertIn("layouts = (", body,
                      "the loader must actually try more than one layout")


class TheBaselineNeverSilentlyOmitsACapability(unittest.TestCase):
    """detect_unavailable()'s stated contract. It recorded a reason ONLY when a
    package was missing -- so in the normal deployed state, where both are installed
    and this harness measures neither, both capabilities vanished from the baseline
    with no number and no explanation."""

    def _detect(self):
        import benchmark
        return benchmark.detect_unavailable()

    def test_both_capabilities_are_always_accounted_for(self):
        got = self._detect()
        for key in ("image_generation", "vlm_describe_image"):
            self.assertIn(
                key, got,
                f"{key} is neither measured nor explained; it silently disappears "
                "from the baseline, which is exactly what this function exists to "
                "prevent",
            )

    def test_the_reason_says_which_state_we_are_in(self):
        """'not benchmarked' alone is useless: an operator needs to know whether the
        package is missing, broken, or simply unmeasured."""
        for key, reason in self._detect().items():
            self.assertRegex(
                reason, r"not installed|unusable|installed and usable|installed —",
                f"{key}'s reason does not identify the state: {reason!r}",
            )

    @unittest.skipUnless(HAVE_MFLUX, "needs mflux installed")
    def test_a_broken_api_is_reported_as_broken_not_as_available(self):
        """The regression: presence was mistaken for usability."""
        import benchmark
        real = S._load_mlx_diffusion
        S._load_mlx_diffusion = lambda: None
        try:
            reason = benchmark.detect_unavailable()["image_generation"]
        finally:
            S._load_mlx_diffusion = real
        self.assertIn("unusable", reason,
                      f"an unusable mflux was reported as fine: {reason!r}")


if __name__ == "__main__":
    unittest.main(verbosity=2)
