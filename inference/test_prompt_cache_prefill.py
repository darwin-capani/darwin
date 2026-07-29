"""The prompt-cache prefills must evaluate the CACHE, never the logits.

Both KV prompt caches (persona, classifier) are built by running the model once over
a fixed prefix purely for the KV side effect. `mx.eval(logits)` used to force the
tied lm_head projection of EVERY prefix position out to vocab_size, which for
Qwen3-4B (vocab 151,936, fp16) is 626.9 MB for the 2063-token persona prefix and
264.1 MB for the 869-token classifier prefix -- ~891 MB of arrays built and dropped
on the very next line, during boot, on a machine the Phase 0 baseline measured at a
7694 MB peak against a 5352 MB steady footprint.

MLX is lazy, so evaluating only `[c.state for c in cache]` leaves that projection out
of the graph entirely (it is mlx_lm's own prefill pattern -- see generate.py). The
resulting cache is identical either way; only the transient goes away.

This test pins the PROPERTY rather than the source text: it builds a miniature model
with the same shape as the real problem (a body that fills a KV cache, then a tied
head projecting to a real-sized vocab) and asserts the peak-memory difference. A
source-level "does the file contain mx.eval([c.state" assertion would pass on a
comment, which is a trap this repo has already been caught by once.

  Run: .venv/bin/python inference/test_prompt_cache_prefill.py   (from the repo root)
"""
import unittest

try:
    import mlx.core as mx
    import mlx.nn as nn
    from mlx_lm.models.cache import KVCache
    HAVE_MLX = True
except Exception:  # noqa: BLE001 - dep-gated like the other device tests
    HAVE_MLX = False

# Real Qwen3 vocab; small hidden/layers keep the test fast while preserving the shape
# of the problem (the head is enormous relative to the hidden state).
HIDDEN = 256
VOCAB = 151936
TOKENS = 512
LAYERS = 4


@unittest.skipUnless(HAVE_MLX, "needs mlx + mlx_lm")
class PrefillEvaluatesTheCacheNotTheLogits(unittest.TestCase):
    def _model(self):
        class Body(nn.Module):
            def __init__(self):
                super().__init__()
                self.proj = [nn.Linear(HIDDEN, HIDDEN) for _ in range(LAYERS)]

            def __call__(self, x, cache):
                for i, p in enumerate(self.proj):
                    x = p(x)
                    kv = x[:, None, :, :]  # (B, 1, T, H) stand-in for K/V
                    cache[i].update_and_fetch(kv, kv)
                return x

        class Model(nn.Module):
            def __init__(self):
                super().__init__()
                self.body = Body()
                self.head = nn.Linear(HIDDEN, VOCAB, bias=False)

            def __call__(self, x, cache):
                return self.head(self.body(x, cache))

        m = Model()
        mx.eval(m.parameters())
        return m

    def _peak_mb(self, eval_logits):
        m = self._model()
        cache = [KVCache() for _ in range(LAYERS)]
        x = mx.random.normal((1, TOKENS, HIDDEN))
        mx.eval(x)
        mx.clear_cache()
        mx.reset_peak_memory()
        out = m(x, cache)
        if eval_logits:
            mx.eval(out)
        else:
            mx.eval([c.state for c in cache])
        return mx.get_peak_memory() / 1e6

    def test_evaluating_the_cache_elides_the_vocab_projection(self):
        logits_peak = self._peak_mb(eval_logits=True)
        cache_peak = self._peak_mb(eval_logits=False)
        logits_mb = TOKENS * VOCAB * 4 / 1e6  # fp32 in this miniature
        # The whole projection must vanish, not merely shrink.
        self.assertLess(
            cache_peak, logits_peak - 0.8 * logits_mb,
            f"projection was not elided: {logits_peak:.1f} MB with eval(logits) vs "
            f"{cache_peak:.1f} MB with eval(cache); the head alone is {logits_mb:.1f} MB",
        )

    def test_the_cache_is_identical_either_way(self):
        """The saving must be free: same KV cache, only the transient differs."""
        results = []
        for eval_logits in (True, False):
            mx.random.seed(1234)
            m = self._model()
            cache = [KVCache() for _ in range(LAYERS)]
            mx.random.seed(99)
            x = mx.random.normal((1, TOKENS, HIDDEN))
            out = m(x, cache)
            if eval_logits:
                mx.eval(out)
            else:
                mx.eval([c.state for c in cache])
            results.append([mx.array(c.keys).tolist() for c in cache])
        self.assertEqual(
            results[0], results[1],
            "the KV cache differs between the two eval strategies - the saving is "
            "only free if the cache is untouched",
        )

    def test_the_real_prefills_do_not_bind_logits(self):
        """Guard the actual call sites. Deliberately NOT a substring check for the
        fix (a comment would satisfy that); this asserts the ABSENCE of the old
        binding, which no comment introduces."""
        import pathlib
        src = pathlib.Path(__file__).resolve().parent / "server.py"
        text = src.read_text(encoding="utf-8")
        for bad in ("logits = self._model(", "logits = self._cls_model("):
            self.assertNotIn(
                bad, text,
                f"{bad!r} is back: the prefill binds and evaluates logits again, "
                "materializing the vocab projection it throws away",
            )


if __name__ == "__main__":
    unittest.main(verbosity=2)
