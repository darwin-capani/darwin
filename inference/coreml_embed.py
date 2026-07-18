"""On-device Core ML sentence embedder (op=embed backend).

WHAT: a purpose-built contrastive sentence embedder — BAAI/bge-small-en-v1.5
(BERT, 33M params, 384-dim) — converted to Core ML (FP16 mlprogram,
compute_units=ALL, ANE-ELIGIBLE) and used as the on-device retrieval-embedding
backend, in place of mean-pooling the resident 4B LLM's hidden states.

WHY: measured on this M1 Pro (see scratch-bench/ane-probe/eval — a synthetic-
but-representative MNEMOSYNE-style retrieval set of short user facts), the bge
384-dim embedder is BOTH dramatically higher retrieval quality AND ~30x faster:

    embedder                     recall@1  recall@3  recall@5   MRR    latency
    bge-small (this module,384d)  0.8241    0.9213    0.9861   0.9606  ~2.5ms single / ~1.7ms/text batched
    Qwen3-4B mean-pool (2560d)    0.2454    0.4630    0.5556   0.4235  ~124ms single / ~56.8ms/text batched

    (recall@k / MRR are SYNTHETIC-BUT-REPRESENTATIVE — measured on the probe's
    generated fact/query set, not a production corpus. Latency = MEASURED
    end-to-end median on M1 Pro. The bge model does PLAIN mean-pool of the last
    hidden state with NO query-instruction prefix, so these are the plain-variant
    numbers; a query-instruction variant reaches recall@5 = 1.0 but is not what
    this module computes.)

HONESTY — the ANE: compute_units=ComputeUnit.ALL makes the Apple Neural Engine
(and GPU) ELIGIBLE. Core ML schedules ANE/GPU/CPU at its own discretion and ANE
residency is unmeasurable without powermetrics. This module therefore claims
"Core ML, ANE-eligible" and cites only MEASURED end-to-end latency — never that
any op actually ran on the ANE.

TRUNCATION: inputs are tokenized to a FIXED sequence length of 128 tokens
(SEQ). Facts longer than 128 tokens are TRUNCATED to the first 128 — an
embedding of the lead, surfaced honestly (the caller caps the batch; this caps
the per-text length). Short user facts (the MNEMOSYNE shape) are well under 128.

CONVERT-ON-FIRST-USE: the compiled model + tokenizer are cached under the SAME
model-cache root the rest of the server uses ($HF_HOME, falling back to
~/.cache/huggingface — see `hf_cache_root`), in a `darwin-coreml/` subtree. On
first use, if the cache is absent, the model is converted ONCE from the HF bge
checkpoint using the proven recipe (see `_convert`); subsequent starts load the
cached compiled model. Conversion needs torch + transformers (heavy, one-time);
runtime prediction after conversion needs only coremltools + the fast tokenizer.

The conversion recipe is the one proven in scratch-bench/ane-probe/convert.py
for the transformers 5.11 / coremltools 9.0 / torch 2.12 combo:
  - position_ids / token_type_ids are baked as CONSTANT buffers at the full
    (batch, seq) shape, so the traced graph emits no tensor-derived Python
    scalar (which trips a coremltools _int-cast bug).
  - BertModel._create_attention_masks is overridden to build the 4D additive
    mask with elementary ops (bypasses transformers 5.11 masking_utils, which
    coremltools 9.0's torch frontend does not support).
  - the additive masked value is a FINITE -1e4 (NOT finfo.min): in FP16
    finfo(fp32).min casts to -inf and 0 * -inf = NaN; exp(-1e4) underflows to 0
    in both fp16 and fp32, so masking stays exact.
  - masked mean-pool + L2-normalize are baked INTO the graph, so the model
    outputs a single normalized sentence vector per row.

This module imports only stdlib + numpy at top level; coremltools / torch /
transformers are imported lazily inside methods, so `import coreml_embed`
(and py_compile / pyflakes) succeed even in an env without them — an env
without the deps simply cannot build/load the Core ML model, and the caller
falls back to the 4B mean-pool path and reports the fallback (never silently).
"""
import math
import os
import threading

import numpy as np

# Stable HF checkpoint this backend embeds with.
MODEL_ID = "BAAI/bge-small-en-v1.5"
# STABLE wire id for this embedder's vector space (op=embed `embedder` field).
# The daemon/docsearch store this with the index to refuse cross-space
# comparison. MUST match server.py EMBEDDER_COREML and the Rust docsearch side.
EMBEDDER_ID = "coreml-bge-small-en-v1.5"
# Output dimension (bge-small hidden size). Fixed by the checkpoint.
DIM = 384
# Fixed sequence length: inputs are padded / truncated to this many tokens.
SEQ = 128
# Fixed batch size of the primary compiled model. The MNEMOSYNE call shape is a
# query + K candidate facts (K~=8), so an (8, 128) fixed-shape graph is both the
# realistic batch and ANE-eligible. A batch is chunked into groups of this many
# rows; the final partial group is padded up to this size (padded rows are
# discarded). A separate batch-1 graph serves singleton chunks without paying
# the full 8-row forward.
COREML_BATCH = 8

# Compiled-model / tokenizer artifact names under the per-model cache dir.
_B1_NAME = "emb_b1.mlpackage"
_B8_NAME = "emb_b8.mlpackage"
_TOK_DIRNAME = "tokenizer"


def hf_cache_root():
    """The model-cache ROOT the rest of the server/installer uses: $HF_HOME if
    set (the installer persists it into state/env.sh), else ~/.cache/huggingface
    — the exact resolution scripts/doctor.sh checks. The Core ML artifacts live
    in a `darwin-coreml/` subtree of this root so they share the ONE cache the
    installer manages (never the repo)."""
    root = os.environ.get("HF_HOME")
    if root:
        return root
    return os.path.join(os.path.expanduser("~"), ".cache", "huggingface")


def _safe_model_slug(model_id):
    """A filesystem-safe leaf name for a HF repo id (org/name -> org--name)."""
    return model_id.replace("/", "--")


def cache_dir(root=None, model_id=MODEL_ID):
    """Directory holding this model's compiled Core ML packages + tokenizer,
    under `<hf_cache_root>/darwin-coreml/<safe-model-id>/`."""
    base = root if root is not None else hf_cache_root()
    return os.path.join(base, "darwin-coreml", _safe_model_slug(model_id))


# ---- PURE helpers (no model / no tokenizer / no I/O — unit-tested) ----------


def pad_batch(id_lists, seq=SEQ):
    """PURE. Pad/truncate a list of token-id lists to a dense (n, seq) int32
    array + its (n, seq) int32 attention mask. Each row is truncated to the
    first `seq` ids (the honest length cap) and right-padded with 0; the mask is
    1 on real tokens, 0 on padding. Right-padding + the model's mask means a pad
    position never affects a real token's hidden state and is excluded from the
    mean-pool. Returns (ids, mask)."""
    n = len(id_lists)
    ids = np.zeros((n, seq), dtype=np.int32)
    mask = np.zeros((n, seq), dtype=np.int32)
    for i, row in enumerate(id_lists):
        r = row[:seq]
        k = len(r)
        if k:
            ids[i, :k] = np.asarray(r, dtype=np.int32)
            mask[i, :k] = 1
    return ids, mask


def plan_coreml_chunks(n, batch=COREML_BATCH):
    """PURE, ORDER-PRESERVING. Split row index space [0, n) into contiguous
    [start, end) ranges of at most `batch` rows each (the fixed-shape forward
    processes one range per predict, padding a short final range up to `batch`).
    Returns a list of (start, end). Order is untouched so results write straight
    back to their input slots."""
    if n <= 0:
        return []
    if batch <= 0:
        raise ValueError("batch must be positive")
    return [(s, min(s + batch, n)) for s in range(0, n, batch)]


def scrub_vector(vec):
    """PURE. Map any non-finite component (NaN / +-Inf) to 0.0 so the JSON
    response stays strict-valid (a degenerate-but-finite vector keeps the whole
    batch from failing). The model L2-normalizes in-graph; this is the last-line
    guard that NaN/Inf never reach the wire. Returns a new list[float]."""
    return [float(x) if math.isfinite(x) else 0.0 for x in vec]


def normalize_text(text):
    """PURE. An input that is empty or whitespace-only still needs a vector, so
    it falls back to a single space (so the tokenizer yields at least one real
    content position). Mirrors the 4B path's _embed_encode empty-input guard."""
    if text is None:
        return " "
    return text if text.strip() else " "


class CoreMLEmbedderUnavailable(RuntimeError):
    """Raised when the Core ML embedder cannot be built or loaded (conversion
    failure, missing deps, coremltools issue). The engine catches this to fall
    back to the 4B mean-pool path and REPORT the fallback (never silent)."""


class CoreMLEmbedder:
    """Loads/caches the Core ML bge model + tokenizer and embeds a batch of
    strings to 384-dim L2-normalized vectors. Convert-on-first-use; thread-safe
    (its own lock guards lazy load + predict — it does NOT touch the engine's
    MLX GPU lock, so embedding runs independently of LLM generation)."""

    def __init__(self, root=None, batch=COREML_BATCH):
        self._dir = cache_dir(root)
        self._batch = batch
        self._lock = threading.Lock()
        self._loaded = False
        self._tokenizer = None
        self._m1 = None  # batch-1 MLModel (singleton chunks)
        self._m8 = None  # batch-`_batch` MLModel (the batched forward)

    # -- artifact paths --
    @property
    def _tok_dir(self):
        return os.path.join(self._dir, _TOK_DIRNAME)

    @property
    def _b1_path(self):
        return os.path.join(self._dir, _B1_NAME)

    @property
    def _b8_path(self):
        return os.path.join(self._dir, _B8_NAME)

    def _artifacts_present(self):
        return (
            os.path.isdir(self._b1_path)
            and os.path.isdir(self._b8_path)
            and os.path.isdir(self._tok_dir)
        )

    def ensure_loaded(self):
        """Lazily convert-on-first-use (if the cache is absent) then load the
        compiled models + tokenizer. Idempotent + thread-safe. Raises
        CoreMLEmbedderUnavailable on any failure so the caller falls back."""
        if self._loaded:
            return
        with self._lock:
            if self._loaded:
                return
            try:
                import coremltools as ct
                from transformers import AutoTokenizer
            except Exception as e:  # deps absent -> honest unavailable
                raise CoreMLEmbedderUnavailable(
                    f"Core ML embedder deps unavailable: {e}"
                ) from e
            try:
                if not self._artifacts_present():
                    self._convert()  # ONE-TIME, needs torch + transformers
                self._tokenizer = AutoTokenizer.from_pretrained(self._tok_dir)
                self._m1 = ct.models.MLModel(
                    self._b1_path, compute_units=ct.ComputeUnit.ALL
                )
                self._m8 = ct.models.MLModel(
                    self._b8_path, compute_units=ct.ComputeUnit.ALL
                )
            except CoreMLEmbedderUnavailable:
                raise
            except Exception as e:
                raise CoreMLEmbedderUnavailable(
                    f"Core ML embedder build/load failed: {e}"
                ) from e
            self._loaded = True

    def _convert(self):
        """ONE-TIME conversion of the HF bge checkpoint to two fixed-shape Core
        ML packages (batch 1 and `_batch`) + the saved tokenizer, using the
        proven recipe (module docstring). Heavy: needs torch + transformers and
        downloads the checkpoint (honoring HF_HOME) on the first ever run.
        Writes atomically into the cache dir. Raises on any failure."""
        import types

        import coremltools as ct
        import torch
        from transformers import AutoModel, AutoTokenizer

        seq = SEQ

        def _simple_masks(
            self_enc, attention_mask, encoder_attention_mask,
            embedding_output, encoder_hidden_states, past_key_values,
        ):
            # Elementary-op replacement for BertModel._create_attention_masks:
            # finite -1e4 (not finfo.min) so fp16 never does 0 * -inf = NaN.
            dtype = embedding_output.dtype
            if attention_mask is None:
                return None, None
            am = attention_mask.to(dtype)
            add = (1.0 - am)[:, None, None, :] * (-1.0e4)
            return add, None

        class MeanPoolNorm(torch.nn.Module):
            """BERT encoder -> masked mean-pool -> L2-normalize -> sentence vec."""

            def __init__(self, encoder, batch, seq):
                super().__init__()
                self.encoder = encoder
                pos = (
                    torch.arange(seq, dtype=torch.long)
                    .unsqueeze(0)
                    .expand(batch, seq)
                    .contiguous()
                )
                self.register_buffer("pos_ids", pos)
                self.register_buffer(
                    "tok_type", torch.zeros(batch, seq, dtype=torch.long)
                )

            def forward(self, input_ids, attention_mask):
                out = self.encoder(
                    input_ids=input_ids,
                    attention_mask=attention_mask,
                    token_type_ids=self.tok_type,
                    position_ids=self.pos_ids,
                )
                last = out.last_hidden_state
                mask = attention_mask.unsqueeze(-1).to(last.dtype)
                summed = (last * mask).sum(dim=1)
                counts = mask.sum(dim=1).clamp(min=1e-9)
                mean = summed / counts
                norm = mean.norm(p=2, dim=-1, keepdim=True).clamp(min=1e-12)
                return mean / norm

        def trace_and_convert(encoder, batch, out_path):
            wrapper = MeanPoolNorm(encoder, batch, seq).eval()
            ex_ids = torch.randint(0, 1000, (batch, seq), dtype=torch.int64)
            ex_mask = torch.ones((batch, seq), dtype=torch.int64)
            with torch.no_grad():
                traced = torch.jit.trace(
                    wrapper, (ex_ids, ex_mask), check_trace=False
                )
            mlmodel = ct.convert(
                traced,
                inputs=[
                    ct.TensorType(
                        name="input_ids", shape=(batch, seq), dtype=np.int32
                    ),
                    ct.TensorType(
                        name="attention_mask", shape=(batch, seq), dtype=np.int32
                    ),
                ],
                outputs=[ct.TensorType(name="embedding")],
                convert_to="mlprogram",
                compute_precision=ct.precision.FLOAT16,
                minimum_deployment_target=ct.target.macOS15,
                compute_units=ct.ComputeUnit.ALL,
            )
            mlmodel.save(out_path)

        os.makedirs(self._dir, exist_ok=True)
        tok = AutoTokenizer.from_pretrained(MODEL_ID)
        enc = AutoModel.from_pretrained(
            MODEL_ID, attn_implementation="eager"
        ).eval()
        enc._create_attention_masks = types.MethodType(_simple_masks, enc)
        if enc.config.hidden_size != DIM:
            raise CoreMLEmbedderUnavailable(
                f"{MODEL_ID} hidden size {enc.config.hidden_size} != expected {DIM}"
            )
        trace_and_convert(enc, 1, self._b1_path)
        trace_and_convert(enc, self._batch, self._b8_path)
        tok.save_pretrained(self._tok_dir)

    def _encode(self, texts):
        """Tokenize a list of strings to a list of token-id lists, truncated to
        SEQ. Empty/whitespace-only inputs fall back to a single space."""
        norm = [normalize_text(t) for t in texts]
        enc = self._tokenizer(
            norm, truncation=True, max_length=SEQ, return_attention_mask=False
        )
        return enc["input_ids"]

    def _predict(self, model, ids, mask):
        """Run one fixed-shape predict; returns a numpy (rows, DIM) array."""
        out = model.predict({"input_ids": ids, "attention_mask": mask})
        return np.asarray(out["embedding"], dtype=np.float32)

    def embed(self, texts):
        """Embed a batch of strings -> list of DIM-dim L2-normalized float
        vectors, in input order. Empty batch -> []. Vectors are scrubbed so no
        NaN/Inf reaches the wire. The batch/length caps are enforced by the
        caller (batch) and tokenization (length). Thread-safe."""
        if not texts:
            return []
        self.ensure_loaded()
        id_lists = self._encode(texts)
        n = len(id_lists)
        out = [None] * n
        with self._lock:
            for start, end in plan_coreml_chunks(n, self._batch):
                rows = id_lists[start:end]
                r = len(rows)
                if r == 1:
                    ids, mask = pad_batch(rows, SEQ)  # (1, SEQ)
                    vecs = self._predict(self._m1, ids, mask)
                else:
                    # Pad the chunk up to the fixed batch size with a repeat of
                    # the first row (its output is discarded); real rows are
                    # unaffected (each row's vector depends only on its tokens).
                    padded_rows = rows + [rows[0]] * (self._batch - r)
                    ids, mask = pad_batch(padded_rows, SEQ)  # (_batch, SEQ)
                    vecs = self._predict(self._m8, ids, mask)[:r]
                for i, row in zip(range(start, end), vecs):
                    out[i] = scrub_vector(row.tolist())
        return out

    def reference_vectors(self, texts):
        """DEVICE/DEP-GATED faithfulness reference: compute the SAME recipe in
        torch fp32 (no Core ML) for `texts`, for the smoke test to compare
        against (cosine ~= 1.0 confirms the FP16 Core ML graph is faithful).
        Needs torch + transformers. Returns a list of DIM-dim vectors."""
        import types

        import torch
        from transformers import AutoModel

        def _simple_masks(
            self_enc, attention_mask, encoder_attention_mask,
            embedding_output, encoder_hidden_states, past_key_values,
        ):
            dtype = embedding_output.dtype
            if attention_mask is None:
                return None, None
            am = attention_mask.to(dtype)
            return (1.0 - am)[:, None, None, :] * (-1.0e4), None

        enc = AutoModel.from_pretrained(
            MODEL_ID, attn_implementation="eager"
        ).eval()
        enc._create_attention_masks = types.MethodType(_simple_masks, enc)
        self.ensure_loaded()
        id_lists = self._encode(texts)
        ids, mask = pad_batch(id_lists, SEQ)
        ii = torch.from_numpy(ids).long()
        mm = torch.from_numpy(mask).long()
        pos = torch.arange(SEQ, dtype=torch.long).unsqueeze(0).expand(len(id_lists), SEQ)
        tt = torch.zeros(len(id_lists), SEQ, dtype=torch.long)
        with torch.no_grad():
            out = enc(
                input_ids=ii, attention_mask=mm,
                token_type_ids=tt, position_ids=pos,
            )
            last = out.last_hidden_state
            m = mm.unsqueeze(-1).to(last.dtype)
            mean = (last * m).sum(dim=1) / m.sum(dim=1).clamp(min=1e-9)
            normed = mean / mean.norm(p=2, dim=-1, keepdim=True).clamp(min=1e-12)
        return [scrub_vector(r) for r in normed.numpy().tolist()]


def _cosine(a, b):
    dot = sum(x * y for x, y in zip(a, b))
    na = math.sqrt(sum(x * x for x in a))
    nb = math.sqrt(sum(x * x for x in b))
    return dot / (na * nb + 1e-12)


def _smoke():
    """DEVICE-GATED smoke: build/load the Core ML embedder, embed a couple of
    fixed strings, print MEASURED single/batched latency + the FP16-vs-torch
    faithfulness cosine + a similar/unrelated separation sanity. Run once by
    hand (NOT in CI):  .venv/bin/python inference/coreml_embed.py"""
    import statistics
    import time

    emb = CoreMLEmbedder()
    emb.ensure_loaded()
    print(f"loaded Core ML embedder from {emb._dir}", flush=True)

    fixed = [
        "The user prefers dark mode and lives in the Pacific timezone.",
        "The user's project is named DARWIN.",
    ]
    # Faithfulness: Core ML (FP16) vs torch (fp32) on the fixed strings.
    cml = emb.embed(fixed)
    ref = emb.reference_vectors(fixed)
    faith = [round(_cosine(a, b), 6) for a, b in zip(cml, ref)]
    print(f"dim = {len(cml[0])}  (expected {DIM})")
    print(f"faithfulness cosine (CoreML fp16 vs torch fp32): {faith}")

    # Similar / unrelated separation sanity.
    pairs = [
        ("similar", "A man is playing an acoustic guitar.", "Someone is strumming a guitar."),
        ("unrelated", "The stock market fell sharply this week.", "My cat likes to sleep on the couch."),
    ]
    for kind, a, b in pairs:
        va, vb = emb.embed([a])[0], emb.embed([b])[0]
        print(f"  [{kind:>9}] cosine {_cosine(va, vb):+.4f}")

    # Latency: single-text (batch-1 graph) and the K=8 batched forward.
    batch8 = [
        "What timezone does the user live in?",
        "The user lives in the Pacific timezone.",
        "The user prefers dark mode across all apps.",
        "The user's primary language is English.",
        "The user drinks coffee, not tea.",
        "The user's project is named DARWIN.",
        "The user owns an Apple M1 Pro laptop.",
        "The user usually works late at night.",
    ]

    def med(fn, n=5, warmup=1):
        for _ in range(warmup):
            fn()
        runs = []
        for _ in range(n):
            t0 = time.perf_counter()
            fn()
            runs.append((time.perf_counter() - t0) * 1000.0)
        return statistics.median(runs)

    single_ms = med(lambda: emb.embed([fixed[0]]))
    batch_ms = med(lambda: emb.embed(batch8))
    print(f"single-text latency: {single_ms:.2f} ms")
    print(f"batch-8 latency: {batch_ms:.2f} ms  ({batch_ms / 8:.2f} ms/text)")


if __name__ == "__main__":
    _smoke()
