#!/usr/bin/env python3
"""Hermetic, NO-NETWORK, NO-MODEL unit tests for the multi-resident LOCAL model
manager (task #17) in the inference server.

These tests prove the PURE keep-warm POLICY + the LocalWarmManager bookkeeping
WITHOUT ever loading a model, importing MLX, or touching the network:

  * estimate_local_model_gib / plan_warm_set are pure arithmetic over SYNTHETIC
    sizes — no MLX, no load.
  * LocalWarmManager.select() is driven with a SYNTHETIC loader callback (a stub
    returning ("model::id", "tok::id")), so keep-warm / LRU-evict / select /
    single-resident-fallback / absent-model-fallback are all exercised with NO
    real model load.
  * config parsing for [models].local_warm / local_budget_gib / local_sizes is
    validated through load_config-shaped dicts via the same validators the server
    uses (no file I/O for the happy/invalid-shape cases that matter).

Honesty: this proves the keep-warm + budget + evict + single-fallback LOGIC and
the config gating only. It does NOT (and cannot) measure the actual model load
or the swap speed benefit — those are runtime/device-gated and never exercised
here. The default is single-resident; multi-resident is opt-in + RAM-bounded.

Run: python3 inference/test_local_warm.py   (stdlib only; no pip install)
"""

import logging
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import server  # noqa: E402

# Synthetic per-model footprints (GiB). NONE of these ids is ever loaded.
SIZES = {"base4b": 3.0, "fast1b": 1.0, "cap8b": 6.0, "tiny": 0.5}


def _loader_factory(record, absent=()):
    """A SYNTHETIC loader: returns canned ("model::id","tok::id"); raises for any
    id in `absent` (simulating a not-downloaded checkpoint). Records every call
    in `record` so tests can assert NO redundant loads happened."""

    def loader(mid):
        record.append(mid)
        if mid in absent:
            raise RuntimeError(f"checkpoint {mid} not downloaded")
        return (f"model::{mid}", f"tok::{mid}")

    return loader


class FootprintEstimate(unittest.TestCase):
    def test_explicit_override_wins(self):
        self.assertEqual(server.estimate_local_model_gib("base4b", SIZES), 3.0)

    def test_bad_override_falls_back_to_default(self):
        self.assertEqual(
            server.estimate_local_model_gib("x", {"x": 0}), server.DEFAULT_LOCAL_MODEL_GIB
        )
        self.assertEqual(
            server.estimate_local_model_gib("x", {"x": "big"}),
            server.DEFAULT_LOCAL_MODEL_GIB,
        )

    def test_quant_heuristic_orders_sizes(self):
        four_4bit = server.estimate_local_model_gib("mlx-community/Qwen3-4B-Instruct-4bit")
        four_bf16 = server.estimate_local_model_gib("mlx-community/Qwen3-4B-Instruct-bf16")
        self.assertLess(four_4bit, four_bf16)

    def test_unknown_id_uses_conservative_default(self):
        self.assertEqual(
            server.estimate_local_model_gib("some/unknown-model"),
            server.DEFAULT_LOCAL_MODEL_GIB,
        )


class WarmSetPolicy(unittest.TestCase):
    def test_zero_budget_is_single_resident(self):
        # The DEFAULT (budget 0): base only, extras ignored.
        self.assertEqual(server.plan_warm_set("base4b", ["fast1b", "cap8b"], 0, SIZES), ["base4b"])

    def test_admits_extras_that_fit_in_config_order(self):
        self.assertEqual(
            server.plan_warm_set("base4b", ["fast1b"], 5.0, SIZES), ["base4b", "fast1b"]
        )
        # 3.0 + 1.0 = 4.0 <= 4.0 admits fast; cap8b (6.0) never fits.
        self.assertEqual(
            server.plan_warm_set("base4b", ["fast1b", "cap8b"], 4.0, SIZES),
            ["base4b", "fast1b"],
        )

    def test_over_budget_extra_skipped_later_one_admitted(self):
        # cap8b (6.0) does not fit at 3.6; tiny (0.5) still does (3.0+0.5=3.5).
        self.assertEqual(
            server.plan_warm_set("base4b", ["cap8b", "tiny"], 3.6, SIZES), ["base4b", "tiny"]
        )

    def test_base_over_budget_is_single_resident(self):
        # The base MUST stay warm (it is the fallback) even if it alone exceeds.
        self.assertEqual(server.plan_warm_set("cap8b", ["fast1b"], 4.0, SIZES), ["cap8b"])

    def test_dedup_admits_each_id_once(self):
        self.assertEqual(
            server.plan_warm_set("base4b", ["fast1b", "fast1b", "base4b"], 9.0, SIZES),
            ["base4b", "fast1b"],
        )

    def test_negative_budget_is_single_resident(self):
        self.assertEqual(server.plan_warm_set("base4b", ["fast1b"], -1, SIZES), ["base4b"])


class WarmManagerSelect(unittest.TestCase):
    def test_single_resident_resolves_everything_to_base(self):
        rec = []
        m = server.LocalWarmManager("base4b", ["fast1b"], budget_gib=0, sizes=SIZES)
        self.assertFalse(m.multi_resident())
        loader = _loader_factory(rec)
        self.assertEqual(m.select("fast1b", loader)[0], "base4b")
        # Second request is a warm HIT on the already-resident base: NO reload.
        self.assertEqual(m.select(None, loader)[0], "base4b")
        self.assertEqual(rec, ["base4b"], "base loaded once, then warm-hit")
        self.assertEqual(m.warm_ids(), ["base4b"])

    def test_multi_resident_keeps_both_warm_no_reload_on_hit(self):
        rec = []
        loader = _loader_factory(rec)
        m = server.LocalWarmManager("base4b", ["fast1b"], budget_gib=5.0, sizes=SIZES)
        self.assertTrue(m.multi_resident())
        self.assertEqual(m.capacity, 2)
        self.assertEqual(m.select(None, loader)[0], "base4b")
        self.assertEqual(m.select("fast1b", loader)[0], "fast1b")
        self.assertEqual(sorted(m.warm_ids()), ["base4b", "fast1b"])
        # A re-select is a warm HIT: no second load of fast1b.
        self.assertEqual(m.select("fast1b", loader)[0], "fast1b")
        self.assertEqual(rec, ["base4b", "fast1b"])

    def test_out_of_warm_set_request_resolves_to_base_no_load(self):
        rec = []
        loader = _loader_factory(rec)
        m = server.LocalWarmManager("base4b", ["fast1b"], budget_gib=5.0, sizes=SIZES)
        # cap8b is not in the warm-set -> base, and never loaded.
        self.assertEqual(m.select("cap8b", loader)[0], "base4b")
        self.assertNotIn("cap8b", rec)

    def test_lru_evicts_non_base_base_is_pinned(self):
        rec = []
        loader = _loader_factory(rec)
        m = server.LocalWarmManager("base4b", ["fast1b", "tiny"], budget_gib=9.0, sizes=SIZES)
        self.assertEqual(m.plan, ["base4b", "fast1b", "tiny"])
        m.select(None, loader)
        m.select("fast1b", loader)
        m.select("tiny", loader)
        self.assertEqual(sorted(m.warm_ids()), ["base4b", "fast1b", "tiny"])
        # Cap to 2 and force an eviction with a brand-new resident: the oldest
        # NON-base resident goes; the base survives (pinned).
        m.capacity = 2
        ev = m._insert("newmodel", "m", "t")
        self.assertNotIn("base4b", ev)
        self.assertIn("base4b", m.warm_ids())
        self.assertLessEqual(len(m.warm_ids()), 2)

    def test_absent_model_falls_back_to_base_never_crashes(self):
        rec = []
        loader = _loader_factory(rec, absent={"fast1b"})
        m = server.LocalWarmManager("base4b", ["fast1b"], budget_gib=5.0, sizes=SIZES)
        # Silence the EXPECTED fallback traceback (log.exception) for clean output.
        prev = server.log.level
        server.log.setLevel(logging.CRITICAL)
        try:
            rid, model, _ = m.select("fast1b", loader)
        finally:
            server.log.setLevel(prev)
        self.assertEqual(rid, "base4b")
        self.assertEqual(model, "model::base4b")
        self.assertIn("fast1b", rec)
        self.assertIn("base4b", rec)
        # The failed extra is NOT marked resident.
        self.assertEqual(m.warm_ids(), ["base4b"])

    def test_resolve_id_is_pure(self):
        m = server.LocalWarmManager("base4b", ["fast1b"], budget_gib=5.0, sizes=SIZES)
        self.assertEqual(m.resolve_id("fast1b"), "fast1b")
        self.assertEqual(m.resolve_id("cap8b"), "base4b")
        self.assertEqual(m.resolve_id(None), "base4b")
        self.assertEqual(m.resolve_id(""), "base4b")


class EngineWiring(unittest.TestCase):
    """The engine constructs the manager from settings; verify default is
    single-resident and the status snapshot is honest. No model is loaded."""

    def _engine(self, **overrides):
        settings = {
            "llm": "base4b",
            "stt": "stub-stt",
            "engine": "kokoro",
            "voice": "bm_george",
            "speed": 1.2,
        }
        settings.update(overrides)
        return server.InferenceEngine(settings, classifier_template="", persona="")

    def test_default_engine_is_single_resident(self):
        eng = self._engine()
        st = eng.local_warm_status()
        self.assertEqual(st["base"], "base4b")
        self.assertEqual(st["planned"], ["base4b"])
        self.assertFalse(st["multi_resident"])
        # Nothing loaded yet -> nothing resident.
        self.assertEqual(st["resident"], [])

    def test_configured_warm_set_is_multi_resident(self):
        eng = self._engine(
            local_warm=["fast1b"], local_budget_gib=5.0, local_sizes=SIZES
        )
        st = eng.local_warm_status()
        self.assertEqual(st["planned"], ["base4b", "fast1b"])
        self.assertTrue(st["multi_resident"])


try:
    import tomllib as _tomllib  # noqa: F401  (3.11+ stdlib; the server's runtime)

    _HAVE_TOML = True
except ModuleNotFoundError:
    _HAVE_TOML = False


@unittest.skipUnless(_HAVE_TOML, "tomllib (py3.11+) required; server runtime is 3.11")
class ConfigParsing(unittest.TestCase):
    """[models].local_* validators degrade to single-resident on a bad shape
    (conservative default) instead of half-applying. Exercised by writing a
    temp TOML and calling load_config so the real parser runs end-to-end.
    Skipped under <3.11 (no tomllib) — the server itself requires 3.11."""

    def _load(self, toml_text):
        import tempfile

        with tempfile.NamedTemporaryFile("w", suffix=".toml", delete=False) as f:
            f.write(toml_text)
            path = Path(f.name)
        orig = server.CONFIG_PATH
        server.CONFIG_PATH = path
        try:
            prev = server.log.level
            server.log.setLevel(logging.CRITICAL)  # silence expected warnings
            try:
                return server.load_config()
            finally:
                server.log.setLevel(prev)
        finally:
            server.CONFIG_PATH = orig
            path.unlink()

    def test_defaults_when_section_absent(self):
        s = self._load("[models]\nllm = 'base4b'\n")
        self.assertEqual(s["local_warm"], [])
        self.assertEqual(s["local_budget_gib"], 0.0)
        self.assertEqual(s["local_sizes"], {})

    def test_valid_warm_set_and_budget(self):
        s = self._load(
            "[models]\nllm = 'base4b'\nlocal_warm = ['fast1b','cap8b']\n"
            "local_budget_gib = 5.0\nlocal_sizes = {fast1b = 1.0}\n"
        )
        self.assertEqual(s["local_warm"], ["fast1b", "cap8b"])
        self.assertEqual(s["local_budget_gib"], 5.0)
        self.assertEqual(s["local_sizes"], {"fast1b": 1.0})

    def test_invalid_warm_set_disables_extras(self):
        # A non-string member -> the whole extra warm-set is dropped.
        s = self._load("[models]\nllm = 'base4b'\nlocal_warm = ['fast1b', 3]\n")
        self.assertEqual(s["local_warm"], [])

    def test_invalid_budget_stays_single_resident(self):
        s = self._load("[models]\nllm = 'base4b'\nlocal_budget_gib = -2\n")
        self.assertEqual(s["local_budget_gib"], 0.0)
        s2 = self._load("[models]\nllm = 'base4b'\nlocal_budget_gib = true\n")
        self.assertEqual(s2["local_budget_gib"], 0.0)

    def test_bad_size_entry_dropped_individually(self):
        s = self._load(
            "[models]\nllm = 'base4b'\nlocal_sizes = {fast1b = 1.0, bad = -3}\n"
        )
        self.assertEqual(s["local_sizes"], {"fast1b": 1.0})


if __name__ == "__main__":
    unittest.main(verbosity=2)
