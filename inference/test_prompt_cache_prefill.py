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
import time
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

    def test_the_cache_is_actually_materialized_not_left_lazy(self):
        """THE PROPERTY THAT MATTERS, and the one the first version of this file
        missed. Every assertion here used to pass when the prefill evaluated NOTHING
        at all: the identity test called .tolist(), which forces the graph in both
        branches, and the memory test was a one-sided assertLess that evaluating LESS
        always satisfies. A prefill that leaves the cache lazy is a real regression -
        the ~33 s of GPU work migrates into the first user request while the engine
        lock is held, and the "prefilled: N tokens in Xms" log reports a time that is
        a lie. So: after the prefill, forcing the raw K/V must be nearly FREE."""
        m = self._model()
        cache = [KVCache() for _ in range(LAYERS)]
        x = mx.random.normal((1, TOKENS, HIDDEN))
        mx.eval(x)
        m(x, cache)
        mx.eval([c.state for c in cache])          # the shipped strategy
        t0 = time.perf_counter()
        mx.eval([(c.keys, c.values) for c in cache])
        forced_after = time.perf_counter() - t0

        m2 = self._model()
        cache2 = [KVCache() for _ in range(LAYERS)]
        m2(x, cache2)                               # evaluate NOTHING (the mutant)
        t0 = time.perf_counter()
        mx.eval([(c.keys, c.values) for c in cache2])
        forced_lazy = time.perf_counter() - t0

        self.assertLess(
            forced_after, forced_lazy * 0.5,
            f"the cache was left LAZY: forcing K/V after the prefill took "
            f"{forced_after*1000:.1f} ms vs {forced_lazy*1000:.1f} ms with no eval at "
            "all - the prefill's GPU work would land on the first real request",
        )

    def test_the_cache_contents_are_unchanged_by_the_strategy(self):
        """The saving must be free: same KV values, only the transient differs."""
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
        self.assertEqual(results[0], results[1])

    def test_every_prefill_site_evaluates_the_cache(self):
        """Tie the property to the REAL call sites. The two previous versions of this
        guard both failed the mutant that matters: replacing the eval with `pass`
        leaves a prefill that evaluates NOTHING, and neither a "does not bind logits"
        check nor a self-contained behavioural test (which builds its own model, so
        server.py cannot affect it) notices. So: find every model call made for its
        KV side effect, and require a cache-state eval right after it."""
        import pathlib
        import re
        src = pathlib.Path(__file__).resolve().parent / "server.py"
        lines = src.read_text(encoding="utf-8").splitlines()
        # A prefill-for-side-effect call: any model invocation passing cache=cache.
        call = re.compile(r"^\s*(?:\w+\s*=\s*)?[\w.]*model\(.*cache=cache\)", re.IGNORECASE)
        bad = []
        for i, line in enumerate(lines):
            if not call.match(line):
                continue
            window = "\n".join(lines[i + 1:i + 4])
            if "mx.eval([c.state for c in cache])" not in window:
                bad.append(f"line {i + 1}: {line.strip()}")
        self.assertEqual(
            bad, [],
            "a prefill runs the model for its KV side effect but does not evaluate "
            "the cache right after. Either it evaluates the logits (materializing the "
            "tied vocab projection it throws away) or it evaluates NOTHING (leaving "
            "the cache lazy, so the prefill's GPU work lands on the first real request "
            f"while the engine lock is held). Sites: {bad}",
        )

    def test_every_state_eval_is_paired_with_clear_cache(self):
        """mlx_lm pairs its prefill state-eval with mx.clear_cache() at every
        occurrence; we shipped only half the pattern once already, leaving tens of MB
        per prefill in MLX's buffer pool past boot."""
        import pathlib
        src = pathlib.Path(__file__).resolve().parent / "server.py"
        text = src.read_text(encoding="utf-8")
        evals = text.count("mx.eval([c.state for c in cache])")
        clears = text.count("mx.clear_cache()")
        self.assertGreaterEqual(
            clears, evals,
            f"{evals} cache-state evals but only {clears} mx.clear_cache() calls",
        )

if __name__ == "__main__":
    unittest.main(verbosity=2)
