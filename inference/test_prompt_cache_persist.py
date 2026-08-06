"""Persisted prompt caches: fast when valid, and NEVER wrong.

The persona and classifier KV prefixes were recomputed from scratch on every boot --
MEASURED at 7.3 s and 3.1 s on a quiet M1 Pro, a third of a 31 s startup spent
reproducing a pure function of (model, quant, prefix text, mlx_lm version). mlx_lm
0.31.3 can persist a prompt cache and a loaded one is still trimmable, which the
classifier's turn-to-turn reuse depends on.

THE HAZARD these tests exist for: a cache silently paired with a CHANGED persona would
make the assistant behave as the OLD persona, with no error and no log line. That is a
correctness failure wearing a performance win's clothes, and it is strictly worse than
being slow. So every test below is about the fingerprint REFUSING a cache, not about
the happy path.

  Run: .venv/bin/python inference/test_prompt_cache_persist.py   (from the repo root)
"""
import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import server  # noqa: E402

try:
    import mlx.core as mx
    from mlx_lm.models.cache import KVCache, can_trim_prompt_cache, trim_prompt_cache
    HAVE_MLX = True
except Exception:  # noqa: BLE001
    HAVE_MLX = False


def _cache(layers=3, tokens=17, dim=8):
    cache = [KVCache() for _ in range(layers)]
    for c in cache:
        k = mx.random.normal((1, 2, tokens, dim))
        c.update_and_fetch(k, k)
    mx.eval([c.state for c in cache])
    return cache


@unittest.skipUnless(HAVE_MLX, "needs mlx + mlx_lm")
class FingerprintRefusesAStaleCache(unittest.TestCase):
    """Each test changes exactly ONE fingerprint input and asserts the cache is
    refused. A cache accepted after any of these changes would be silently wrong."""

    def setUp(self):
        self.root = tempfile.mkdtemp()
        self.args = ("m/x", "4bit", "PERSONA TEXT v1")
        cache = _cache()
        ok = server.save_prompt_cache_fingerprinted(
            self.root, "persona", cache, *self.args
        )
        self.assertTrue(ok, "setUp could not persist a cache")
        self.cache = cache

    def _load(self, model_id=None, quant=None, prefix=None, expect=None):
        return server.load_prompt_cache_fingerprinted(
            self.root, "persona",
            model_id if model_id is not None else self.args[0],
            quant if quant is not None else self.args[1],
            prefix if prefix is not None else self.args[2],
            expect if expect is not None else self.cache,
        )

    def test_an_unchanged_fingerprint_loads(self):
        got = self._load()
        self.assertIsNotNone(got, "an identical fingerprint must load")
        self.assertEqual([c.offset for c in got], [c.offset for c in self.cache])

    def test_a_changed_prefix_is_refused(self):
        """THE ONE THAT MATTERS. Editing the persona must invalidate the cache, or the
        assistant keeps answering as the persona you replaced."""
        self.assertIsNone(self._load(prefix="PERSONA TEXT v2"))

    def test_a_prefix_of_the_same_length_is_still_refused(self):
        """The fingerprint hashes the text; it must not be fooled by an edit that
        preserves the character count."""
        same_len = "PERSONA TEXT v9"
        self.assertEqual(len(same_len), len(self.args[2]))
        self.assertIsNone(self._load(prefix=same_len))

    def test_a_changed_model_is_refused(self):
        self.assertIsNone(self._load(model_id="other/model"))

    def test_a_changed_quant_is_refused(self):
        self.assertIsNone(self._load(quant="8bit"))

    def test_a_changed_layer_count_is_refused(self):
        self.assertIsNone(self._load(expect=_cache(layers=5)))

    def test_an_absent_file_is_refused_not_raised(self):
        self.assertIsNone(
            server.load_prompt_cache_fingerprinted(
                tempfile.mkdtemp(), "persona", *self.args, self.cache
            )
        )

    def test_a_truncated_file_is_refused_not_raised(self):
        """A crash mid-write, a disk-full, a concurrent writer: presence is not
        integrity, and a corrupt file must fall back to a real prefill."""
        path = server._prompt_cache_path(self.root, "persona")
        with open(path, "r+b") as fh:
            fh.truncate(os.path.getsize(path) // 3)
        self.assertIsNone(self._load())

    def test_a_garbage_file_is_refused_not_raised(self):
        path = server._prompt_cache_path(self.root, "persona")
        with open(path, "wb") as fh:
            fh.write(b"not a safetensors file at all")
        self.assertIsNone(self._load())


@unittest.skipUnless(HAVE_MLX, "needs mlx + mlx_lm")
class PersistenceIsNeverFatal(unittest.TestCase):
    """Saving is an optimization. It has already cost this codebase the whole backend
    twice by being allowed to raise, so it must not be able to."""

    def test_saving_to_an_unwritable_root_returns_false(self):
        ok = server.save_prompt_cache_fingerprinted(
            "/nonexistent/read-only/path", "persona", _cache(), "m/x", "4bit", "p"
        )
        self.assertFalse(ok)

    def test_saving_leaves_no_temp_file_behind_on_failure(self):
        """A save that fails BEFORE the temp write: mlx_lm's save_prompt_cache raises
        on the unusable cache object ('str' object has no attribute 'state'), so no
        file is ever created and this half holds trivially. The half that bites is
        below - this one is kept as its paired control."""
        root = tempfile.mkdtemp()
        # A cache containing something unserializable makes save_prompt_cache raise
        # AFTER the temp path is chosen.
        server.save_prompt_cache_fingerprinted(
            root, "persona", ["not a cache object"], "m/x", "4bit", "p"
        )
        d = os.path.join(root, server.PROMPT_CACHE_DIRNAME)
        strays = [f for f in os.listdir(d)] if os.path.isdir(d) else []
        self.assertEqual(
            [f for f in strays if ".tmp." in f], [],
            f"a failed save left a temp file behind: {strays}",
        )

    def test_a_failure_after_the_temp_write_leaves_no_temp_file(self):
        """THE INVARIANT THIS PAIR CLAIMS, ACTUALLY EXERCISED. It was unverified twice
        over:
          (a) the scenario above dies inside mlx_lm before any file exists, so the
              directory is empty whatever the code does; and
          (b) the filter was `f.endswith(".tmp")`, while the real temp file is
              `<name>.<pid>.tmp.safetensors` - it MUST end in .safetensors because
              mx.save_safetensors validates the extension - so the filter could not
              see a stray even when one was left.
        Here the temp write SUCCEEDS and the PUBLISH fails: the destination is a
        directory, so os.replace raises. That is the shape of every real mid-write
        failure (cross-device replace, disk full, a kill between write and rename),
        and it is the only shape that can leak a partially written cache into the
        managed model-cache root."""
        root = tempfile.mkdtemp()
        d = os.path.join(root, server.PROMPT_CACHE_DIRNAME)
        os.makedirs(d, exist_ok=True)
        # A DIRECTORY where the published file belongs: os.replace onto it raises.
        os.makedirs(server._prompt_cache_path(root, "persona"))
        ok = server.save_prompt_cache_fingerprinted(
            root, "persona", _cache(), "m/x", "4bit", "p"
        )
        self.assertFalse(ok, "a failed publish must report False, never raise")
        strays = [f for f in os.listdir(d) if ".tmp." in f]
        self.assertEqual(
            strays, [], f"a failed save left a temp file behind: {strays}"
        )

    def test_a_loaded_cache_is_still_trimmable(self):
        """The classifier reuses its cache across turns via trim_prompt_cache. A
        restored cache that cannot be trimmed would break every classify after the
        first."""
        root = tempfile.mkdtemp()
        cache = _cache(tokens=20)
        server.save_prompt_cache_fingerprinted(
            root, "classifier", cache, "m/x", "4bit", "p"
        )
        got = server.load_prompt_cache_fingerprinted(
            root, "classifier", "m/x", "4bit", "p", cache
        )
        self.assertIsNotNone(got)
        self.assertTrue(can_trim_prompt_cache(got))
        trim_prompt_cache(got, 4)
        self.assertEqual([c.offset for c in got], [16, 16, 16])

    def test_the_restored_cache_is_bit_identical(self):
        """The saving is only free if the KV bytes survive the round trip."""
        root = tempfile.mkdtemp()
        cache = _cache()
        server.save_prompt_cache_fingerprinted(
            root, "persona", cache, "m/x", "4bit", "p"
        )
        got = server.load_prompt_cache_fingerprinted(
            root, "persona", "m/x", "4bit", "p", cache
        )
        for a, b in zip(cache, got):
            n = a.offset
            self.assertTrue(
                mx.array_equal(a.keys[:, :, :n, :], b.keys[:, :, :n, :]).item(),
                "restored keys differ from what was saved",
            )
            self.assertTrue(
                mx.array_equal(a.values[:, :, :n, :], b.values[:, :, :n, :]).item(),
                "restored values differ from what was saved",
            )


class FingerprintCoversEveryInput(unittest.TestCase):
    """A fingerprint that omits an input silently accepts a stale cache, so pin the
    field set. Adding a field is fine; SILENTLY DROPPING one is the bug."""

    def test_required_fields(self):
        fp = server._prompt_cache_fingerprint("m/x", "4bit", "text", [object()])
        for field in (
            "schema", "model_id", "quant", "prefix_sha256",
            "prefix_chars", "mlx_lm", "cache_classes", "layers",
        ):
            self.assertIn(field, fp, f"fingerprint lost {field!r}")

    def test_every_value_is_a_string(self):
        """safetensors metadata is str->str; a non-string would raise at save time on
        a path that is supposed to be non-fatal."""
        fp = server._prompt_cache_fingerprint("m/x", "4bit", "text", [object()])
        for k, v in fp.items():
            self.assertIsInstance(v, str, f"{k} is {type(v).__name__}, not str")

    def test_a_promoted_lora_adapter_invalidates_the_cache(self):
        """A promoted LoRA adapter changes the weights the KV state was computed
        against. Omitting it from the fingerprint meant a cache built on the base
        model could be restored while an adapter was live - decoding a persona
        against KV state from a DIFFERENT model, with nothing to signal it."""
        with tempfile.TemporaryDirectory() as tmp:
            cache = _cache()
            server.save_prompt_cache_fingerprinted(
                tmp, "persona", cache, "m/x", "4bit", "p", "adapter-v1"
            )
            same = server.load_prompt_cache_fingerprinted(
                tmp, "persona", "m/x", "4bit", "p", cache, "adapter-v1"
            )
            self.assertIsNotNone(same, "the same adapter must still load")
            for other in ("adapter-v2", None, ""):
                self.assertIsNone(
                    server.load_prompt_cache_fingerprinted(
                        tmp, "persona", "m/x", "4bit", "p", cache, other
                    ),
                    f"a cache built under adapter-v1 must be refused for {other!r}",
                )

    def test_a_base_model_cache_is_refused_once_an_adapter_is_promoted(self):
        with tempfile.TemporaryDirectory() as tmp:
            cache = _cache()
            server.save_prompt_cache_fingerprinted(
                tmp, "persona", cache, "m/x", "4bit", "p", None
            )
            self.assertIsNone(
                server.load_prompt_cache_fingerprinted(
                    tmp, "persona", "m/x", "4bit", "p", cache, "adapter-v1"
                ),
                "a base-model cache must not be used under a promoted adapter",
            )


if __name__ == "__main__":
    unittest.main(verbosity=2)
