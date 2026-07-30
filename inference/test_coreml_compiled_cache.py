"""The compiled-model cache: faster when it works, never a new way to fail.

`ct.models.MLModel(<dir>.mlpackage)` pays a full Core ML program specialization on
EVERY load -- MEASURED at 4.6/2.9/2.3/1.5 s for the four graphs this server loads,
11.24 s of an 18.9 s startup. Core ML caches that specialization per COMPILED model,
but MLModel compiles the package into a throwaway temp each time and so never hits it.
Persisting the .mlmodelc and loading it with CompiledMLModel does: MEASURED 11.24 s ->
2.5 s for both models across fresh processes.

These tests cover the LOGIC and the FALLBACK with stubs. The real load timings are
device-gated and live in the committed benchmark, per this repo's convention (pure
tests here, device smokes separately).

  Run: .venv/bin/python inference/test_coreml_compiled_cache.py   (from the repo root)
"""
import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import coreml_shared as cs  # noqa: E402


class CompiledSiblingPath(unittest.TestCase):
    def test_mlpackage_maps_to_an_mlmodelc_beside_it(self):
        self.assertEqual(
            cs._compiled_sibling("/a/b/emb_b1.mlpackage"), "/a/b/emb_b1.mlmodelc"
        )

    def test_the_fast_graph_gets_its_own_sibling(self):
        self.assertEqual(
            cs._compiled_sibling("/a/b/emb_b1_s128.mlpackage"),
            "/a/b/emb_b1_s128.mlmodelc",
        )

    def test_a_path_without_the_extension_is_not_mangled(self):
        self.assertEqual(cs._compiled_sibling("/a/b/thing"), "/a/b/thing.mlmodelc")


class _FakeCT:
    """A stand-in for coremltools that records what was asked of it."""

    class ComputeUnit:
        ALL = "ALL"

    def __init__(self, compile_ok=True, compiled_load_ok=True):
        self.compile_ok = compile_ok
        self.compiled_load_ok = compiled_load_ok
        self.compiled_dests = []
        self.package_loads = []
        self.compiled_loads = []
        outer = self

        class _Utils:
            @staticmethod
            def compile_model(model, destination_path=None):
                # The REAL compile_model rejects a destination that does not end in
                # .mlmodelc. That exact mistake shipped once and only the fallback
                # saved it, so the stub enforces it too.
                if not str(destination_path).endswith(".mlmodelc"):
                    raise ValueError(
                        '"destination_path" parameter must have ".mlmodelc" '
                        "file extension."
                    )
                if not outer.compile_ok:
                    raise RuntimeError("compile failed")
                os.makedirs(destination_path, exist_ok=True)
                with open(os.path.join(destination_path, "model"), "w") as fh:
                    fh.write("compiled")
                outer.compiled_dests.append(destination_path)
                return destination_path

        class _Models:
            utils = _Utils()

            @staticmethod
            def MLModel(path, compute_units=None):
                outer.package_loads.append(path)
                return ("MLModel", path)

            @staticmethod
            def CompiledMLModel(path, compute_units=None):
                outer.compiled_loads.append(path)
                if not outer.compiled_load_ok:
                    raise RuntimeError("compiled model unusable")
                return ("CompiledMLModel", path)

        self.models = _Models()


class LoadModelFast(unittest.TestCase):
    def setUp(self):
        self.d = tempfile.mkdtemp()
        self.pkg = os.path.join(self.d, "emb_b1.mlpackage")
        os.makedirs(self.pkg)
        self._real = sys.modules.get("coremltools")

    def tearDown(self):
        if self._real is not None:
            sys.modules["coremltools"] = self._real
        else:
            sys.modules.pop("coremltools", None)

    def _install(self, fake):
        sys.modules["coremltools"] = fake

    def test_it_compiles_once_then_loads_the_compiled_model(self):
        fake = _FakeCT()
        self._install(fake)
        kind, _ = cs.load_model_fast(self.pkg)
        self.assertEqual(kind, "CompiledMLModel")
        self.assertTrue(os.path.isdir(cs._compiled_sibling(self.pkg)))
        self.assertEqual(fake.package_loads, [], "it fell back unnecessarily")

    def test_the_second_load_does_not_recompile(self):
        fake = _FakeCT()
        self._install(fake)
        cs.load_model_fast(self.pkg)
        n = len(fake.compiled_dests)
        cs.load_model_fast(self.pkg)
        self.assertEqual(len(fake.compiled_dests), n, "it recompiled a cached model")

    def test_the_temp_destination_ends_in_mlmodelc(self):
        """MUTATION GUARD. A temp name not ending in .mlmodelc makes compile_model
        raise, which silently costs the whole optimization - it shipped that way once
        and only the fallback hid it."""
        fake = _FakeCT()
        self._install(fake)
        cs.load_model_fast(self.pkg)
        self.assertTrue(
            all(d.endswith(".mlmodelc") for d in fake.compiled_dests),
            f"compile destinations must end in .mlmodelc: {fake.compiled_dests}",
        )

    def test_a_failed_compile_falls_back_to_the_package(self):
        fake = _FakeCT(compile_ok=False)
        self._install(fake)
        kind, path = cs.load_model_fast(self.pkg)
        self.assertEqual(kind, "MLModel")
        self.assertEqual(path, self.pkg)

    def test_an_unusable_compiled_model_falls_back_and_is_removed(self):
        """A corrupt .mlmodelc must not be retried forever - it is deleted so the next
        load recompiles it, and this load still returns a working model."""
        fake = _FakeCT(compiled_load_ok=False)
        self._install(fake)
        kind, path = cs.load_model_fast(self.pkg)
        self.assertEqual(kind, "MLModel")
        self.assertFalse(
            os.path.isdir(cs._compiled_sibling(self.pkg)),
            "an unusable compiled model was left in place",
        )

    def test_a_failed_compile_leaves_no_temp_directory_behind(self):
        fake = _FakeCT(compile_ok=False)
        self._install(fake)
        cs.load_model_fast(self.pkg)
        strays = [n for n in os.listdir(self.d) if ".tmp" in n]
        self.assertEqual(strays, [], f"temp dirs left behind: {strays}")

    def test_a_racing_writer_does_not_lose_the_load(self):
        """If another process claims the final name first, we discard our own copy and
        use theirs rather than trying to replace a populated directory."""
        fake = _FakeCT()
        self._install(fake)
        os.makedirs(cs._compiled_sibling(self.pkg))  # the winner got there first
        kind, path = cs.load_model_fast(self.pkg)
        self.assertEqual(kind, "CompiledMLModel")
        strays = [n for n in os.listdir(self.d) if ".tmp" in n]
        self.assertEqual(strays, [], f"temp dirs left behind: {strays}")

    def test_it_never_raises_when_everything_fails(self):
        """The package load is the last resort; only ITS failure may propagate, which
        is exactly what loading the package directly would have done anyway."""
        fake = _FakeCT(compile_ok=False)

        def boom(path, compute_units=None):
            raise RuntimeError("the package itself is broken")
        fake.models.MLModel = staticmethod(boom)
        self._install(fake)
        with self.assertRaises(RuntimeError):
            cs.load_model_fast(self.pkg)


if __name__ == "__main__":
    unittest.main(verbosity=2)
