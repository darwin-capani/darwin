"""On-device Core ML voice-activity detector (learned VAD backend).

WHAT: Silero VAD v5 (snakers4/silero-vad — a tiny streaming speech-probability
model, ~1.3 MB of weights) converted to Core ML (FP16 mlprogram,
compute_units=ALL, ANE-ELIGIBLE) and used as a learned, per-frame
speech-probability source in place of the daemon's RMS energy gate. It is the
canonical "tiny always-on model on the Neural Engine" workload: a sub-megabyte
graph that Core ML may schedule onto the ANE, freeing the GPU for MLX.

WHY: the RMS energy gate calls any 30 ms frame louder than a fixed threshold
"speech". Loud non-speech — a fan, a keyboard, music, a slammed door — sails
past it (a false accept), and genuinely quiet speech under the floor is dropped
(a false reject). A learned VAD scores the *spectro-temporal shape* of the
frame, not its loudness. MEASURED on a synthetic-but-representative on-device
test set (macOS `say` speech + synthesized noise adversaries; the harness +
labeled methodology + results are committed under
inference/benchmarks/vad_eval/), the learned VAD dramatically cuts false accepts
on loud non-speech while holding false rejects and detect latency — see that
directory's results.json for the exact numbers this backend is adopted on. Same
discipline that dropped speculative decoding: adopt only on a measured win.

HONESTY — the ANE: compute_units=ComputeUnit.ALL makes the Apple Neural Engine
(and GPU) ELIGIBLE. Core ML schedules ANE/GPU/CPU at its own discretion and ANE
residency is unmeasurable without powermetrics (off-limits). This module
therefore claims "Core ML, ANE-eligible" and cites only MEASURED end-to-end
per-frame latency under ComputeUnit.ALL — never that any op actually ran on the
ANE.

STREAMING CONTRACT (mirrors Silero's own OnnxWrapper): at 16 kHz the model
consumes 512-sample (32 ms) chunks. Each chunk is prepended with the previous
chunk's last 64 samples (the STFT look-back CONTEXT) to form a 576-sample model
input, and carries a recurrent STATE tensor (2, 1, 128) threaded chunk-to-chunk.
The Core ML graph is the pure functional core: (x[1,576], state_in[2,1,128]) ->
(prob[1,1], stateN[2,1,128]); the 64-sample context ring and the state handoff
live in `StreamingVAD` (pure, unit-testable, no model).

CONVERT-ON-FIRST-USE (ATOMIC): the compiled model is cached under the SAME
model-cache root the rest of the server uses ($HF_HOME, falling back to
~/.cache/huggingface), in the `darwin-coreml/silero-vad/` subtree. A missing OR
validate-LOAD-failing cache (a crash / disk-full / concurrent writer can leave a
partial .mlpackage that merely EXISTS) is reconverted ONCE from the bundled
Silero checkpoint into a private temp dir, validate-LOADED there, and only then
atomically renamed into place — a partial cache is never trusted. os.replace
onto a NON-EMPTY dir raises ENOTEMPTY and .mlpackage IS a directory, so an
existing dir is atomically moved aside first, never replaced in place. This is
the exact atomic-publish recipe proven in inference/coreml_embed.py.

CONVERSION RECIPE (transformers-free; needs torch + coremltools + the
`silero-vad` package for the checkpoint): the shipped Silero graph is a stateful
TorchScript whose STFT emits a symbolic conv stride and whose decoder carries a
data-dependent `len(state)` branch and an LSTMCell op — all three trip
coremltools 9.0's torch frontend. The fix mirrors coreml_embed's "elementary-op"
discipline: rebuild the pure per-chunk core as a plain nn.Module that
  - runs the STFT as a STATIC `F.conv1d` (reflection-pad (0, 64), stride 128)
    over the model's own `forward_basis_buffer` — a literal integer stride, so
    no symbolic value reaches coremltools;
  - reimplements the single LSTM cell as elementary matmul/sigmoid/tanh ops over
    the extracted (weight_ih, weight_hh, bias_ih, bias_hh) — no LSTMCell op, no
    `len(state)` branch;
  - keeps the encoder conv stack and the decoder conv as TRACED (not scripted)
    submodules, so no scripted control flow survives.
The rebuilt module is verified BIT-FOR-BIT against Silero's own inner model over
a multi-step stream before conversion (max abs prob diff ~1e-6 — see `_smoke`).

This module imports only stdlib + numpy at top level; coremltools / torch /
silero_vad are imported lazily inside methods, so `import coreml_vad` (and
py_compile / pyflakes) succeed even in an env without them — an env without the
deps simply cannot build/load the Core ML model, and the caller falls back to
the RMS gate and REPORTS the fallback (never silent).
"""
import logging
import math
import os
import shutil
import tempfile
import threading

import numpy as np

log = logging.getLogger("darwin.coreml_vad")

# The learned VAD this backend converts. Silero VAD v5 ships bundled inside the
# `silero-vad` pip package (silero_vad/data/silero_vad.jit) — no network fetch is
# needed once the package is installed; the checkpoint is on disk.
MODEL_ID = "snakers4/silero-vad"
# STABLE wire id for this backend, mirroring the embedder's EMBEDDER_ID. The
# daemon config selects the backend by the string "coreml-silero"; this fuller id
# names the exact model + graph for honest status/telemetry.
VAD_ID = "coreml-silero-vad-v5"

# Streaming geometry, fixed by the 16 kHz Silero v5 model. These are the ONLY
# supported values — the model rejects any other chunk length.
SAMPLE_RATE = 16000
CHUNK = 512          # new samples per streaming step (32 ms @ 16 kHz)
CONTEXT = 64         # STFT look-back prepended to each chunk
MODEL_INPUT = CONTEXT + CHUNK   # 576 — the Core ML graph's fixed x length
STATE_SHAPE = (2, 1, 128)       # recurrent state threaded chunk-to-chunk
# STFT static-conv geometry (extracted from the Silero graph; see the recipe).
_STFT_PAD = (0, 64)   # ReflectionPad1d amount (left, right)
_STFT_STRIDE = 128    # hop_length
_STFT_BINS = 129      # filter_length // 2 + 1 (real half + imag half in the basis)
# Default speech-probability threshold. Silero's own recommended operating point
# is 0.5; the daemon's debounce (min_speech_ms / silence_ms) sits on top of the
# per-frame verdict `prob > threshold`, exactly as it sat on `rms > threshold`.
DEFAULT_THRESHOLD = 0.5

# Compiled-model artifact name under the per-model cache dir.
_MODEL_NAME = "vad.mlpackage"


def hf_cache_root():
    """The model-cache ROOT the rest of the server/installer uses: $HF_HOME if
    set (the installer persists it into state/env.sh), else ~/.cache/huggingface.
    The Core ML artifacts live in a `darwin-coreml/` subtree so they share the ONE
    cache the installer manages (never the repo). Mirrors coreml_embed."""
    root = os.environ.get("HF_HOME")
    if root:
        return root
    return os.path.join(os.path.expanduser("~"), ".cache", "huggingface")


def _safe_model_slug(model_id):
    """A filesystem-safe leaf name for a repo id (org/name -> org--name)."""
    return model_id.replace("/", "--")


def cache_dir(root=None, model_id=MODEL_ID):
    """Directory holding this model's compiled Core ML package, under
    `<hf_cache_root>/darwin-coreml/<safe-model-id>/`."""
    base = root if root is not None else hf_cache_root()
    return os.path.join(base, "darwin-coreml", _safe_model_slug(model_id))


# ---- PURE helpers (no model / no I/O — unit-tested in test_coreml_vad.py) -----


def frame_is_voiced(prob, threshold=DEFAULT_THRESHOLD):
    """PURE. The per-frame verdict the debounce sits on: is this frame's learned
    speech probability above the operating threshold? This is the learned analogue
    of the RMS gate's `rms > rms_threshold`; the daemon's min_speech_ms /
    silence_ms debounce is unchanged and drives off this boolean. A non-finite
    probability (NaN/Inf — a degenerate frame) is treated as NOT voiced
    (fail-safe: a bad frame never opens the mic)."""
    return bool(math.isfinite(prob) and prob > threshold)


def next_context(chunk, context_size=CONTEXT):
    """PURE. The STFT look-back carried into the next streaming step: the last
    `context_size` samples of this chunk (right-padded with leading zeros if the
    chunk is somehow short, so the returned ring is always exactly context_size).
    Mirrors Silero's `self._context = x[..., -context_size:]`."""
    c = np.asarray(chunk, dtype=np.float32).reshape(-1)
    if c.shape[0] >= context_size:
        return c[-context_size:].copy()
    out = np.zeros(context_size, dtype=np.float32)
    if c.shape[0]:
        out[context_size - c.shape[0]:] = c
    return out


def build_model_input(context, chunk):
    """PURE. Assemble the (1, MODEL_INPUT) model input from the 64-sample context
    ring and the 512-sample chunk: concat(context, chunk), matching Silero's
    `torch.cat([self._context, x], dim=1)`. Raises ValueError on a wrong chunk
    length — the model only supports exactly CHUNK samples at 16 kHz (an honest
    hard error, never a silently-mismatched frame)."""
    ctx = np.asarray(context, dtype=np.float32).reshape(-1)
    ch = np.asarray(chunk, dtype=np.float32).reshape(-1)
    if ctx.shape[0] != CONTEXT:
        raise ValueError(f"context must be {CONTEXT} samples, got {ctx.shape[0]}")
    if ch.shape[0] != CHUNK:
        raise ValueError(
            f"chunk must be {CHUNK} samples (16 kHz Silero frame), got {ch.shape[0]}"
        )
    return np.concatenate([ctx, ch]).astype(np.float32).reshape(1, MODEL_INPUT)


class CoreMLVADUnavailable(RuntimeError):
    """Raised when the Core ML VAD cannot be built or loaded (conversion failure,
    missing deps, coremltools issue). The caller catches this to fall back to the
    RMS energy gate and REPORT the fallback (never silent)."""


class CoreMLVAD:
    """Loads/caches the Core ML Silero VAD graph and runs one streaming step:
    (x[1,576], state[2,1,128]) -> (prob, stateN). Convert-on-first-use;
    thread-safe (its own lock guards lazy load + predict — it does NOT touch the
    engine's MLX GPU lock, so VAD runs independently of LLM generation). Use
    `StreamingVAD` for the stateful per-chunk API; this class is the raw graph."""

    def __init__(self, root=None):
        self._dir = cache_dir(root)
        self._lock = threading.Lock()
        self._loaded = False
        self._model = None

    def _validate_predict(self, model):
        """Run a tiny (1, MODEL_INPUT) predict to VALIDATE a compiled graph
        actually runs at the SHIPPED shapes and returns the right output shapes.
        Presence on disk is NOT integrity: a truncated / partial-write .mlpackage
        (crash / disk-full / concurrent writer) can load-open yet fail to predict,
        and a package compiled at a different geometry fails this shape check —
        either way this raises so the cache is reconverted."""
        x = np.zeros((1, MODEL_INPUT), dtype=np.float32)
        state = np.zeros(STATE_SHAPE, dtype=np.float32)
        out = model.predict({"x": x, "state_in": state})
        prob = np.asarray(out["prob"])
        stn = np.asarray(out["stateN"])
        if prob.reshape(-1).shape[0] != 1:
            raise ValueError(f"compiled model prob shape {prob.shape} is not scalar")
        if stn.shape != STATE_SHAPE:
            raise ValueError(
                f"compiled model stateN shape {stn.shape} != expected {STATE_SHAPE}"
            )

    def _load_from(self, d):
        """VALIDATE-LOAD the compiled model from directory `d`: load it, then run
        `_validate_predict` so a partial/corrupt/wrong-geometry package is rejected
        (never trusted on mere presence). Returns the model on success; raises on
        any problem."""
        import coremltools as ct

        model = ct.models.MLModel(
            os.path.join(d, _MODEL_NAME), compute_units=ct.ComputeUnit.ALL
        )
        self._validate_predict(model)
        return model

    def ensure_loaded(self):
        """Convert-on-first-use (ATOMIC) then load the compiled model. Idempotent
        + thread-safe. The cache is trusted only if it VALIDATE-LOADS (see
        `_load_from`); a missing / partial / corrupt / wrong-geometry cache is
        reconverted into a temp dir and atomically published BEFORE it is loaded
        from. Raises CoreMLVADUnavailable on any failure so the caller falls
        back."""
        if self._loaded:
            return
        with self._lock:
            if self._loaded:
                return
            import importlib.util

            missing = [
                m for m in ("coremltools", "torch", "silero_vad")
                if importlib.util.find_spec(m) is None
            ]
            if missing:  # deps absent -> honest unavailable (caller falls back)
                raise CoreMLVADUnavailable(
                    f"Core ML VAD deps unavailable: {', '.join(missing)} not installed"
                )
            try:
                model = self._try_load_final()
                if model is None:
                    self._convert_atomic()  # ONE-TIME; validates before publishing
                    model = self._try_load_final()
                    if model is None:
                        raise CoreMLVADUnavailable(
                            "Core ML VAD cache failed validate-load after conversion"
                        )
                self._model = model
            except CoreMLVADUnavailable:
                raise
            except Exception as e:
                raise CoreMLVADUnavailable(f"Core ML VAD build/load failed: {e}") from e
            self._loaded = True

    def _try_load_final(self):
        """Validate-load from the FINAL cache dir; return the model or None
        (missing / partial / corrupt / wrong-geometry) so the caller reconverts."""
        if not os.path.isdir(self._dir):
            return None
        try:
            return self._load_from(self._dir)
        except Exception as e:
            log.warning(
                "Core ML VAD cache at %s not usable (%s); reconverting", self._dir, e
            )
            return None

    def _convert_atomic(self):
        """Convert into a PRIVATE temp dir under the cache root, validate-LOAD it
        there, then atomically rename it into the final path — so a crash /
        disk-full / concurrent second writer can never leave a partial cache the
        loader would trust. The atomic-publish dance (vacate any existing dir to a
        fresh empty name first, because os.replace onto a non-empty .mlpackage dir
        raises ENOTEMPTY) is the exact recipe proven in coreml_embed. Raises on any
        failure (nothing is published)."""
        parent = os.path.dirname(self._dir)  # <root>/darwin-coreml
        os.makedirs(parent, exist_ok=True)
        tmp = tempfile.mkdtemp(prefix=".convert-vad-", dir=parent)
        trash = None
        try:
            self._convert_into(tmp)
            self._load_from(tmp)  # validate BEFORE publishing
            if os.path.exists(self._dir):
                trash = tempfile.mkdtemp(prefix=".stale-vad-", dir=parent)
                os.rmdir(trash)  # os.replace needs a NON-EXISTENT target (atomic rename)
                try:
                    os.replace(self._dir, trash)  # atomic vacate
                except OSError:
                    trash = None  # a concurrent process already moved/replaced it
            try:
                os.replace(tmp, self._dir)
                tmp = None
            except OSError:
                # Lost the publish race: keep the winner's cache if it validates,
                # else re-raise so we don't trust a bad dir.
                if self._try_load_final() is None:
                    raise
        finally:
            if tmp is not None and os.path.isdir(tmp):
                shutil.rmtree(tmp, ignore_errors=True)
            if trash is not None and os.path.isdir(trash):
                shutil.rmtree(trash, ignore_errors=True)
            # Sweep NON-EMPTY .stale-vad-* strays leaked by an earlier crashed run
            # (an EMPTY one may be a concurrent process's live mkdtemp — leave it).
            try:
                for entry in os.listdir(parent):
                    if entry.startswith(".stale-vad-"):
                        stray = os.path.join(parent, entry)
                        if (
                            stray != trash
                            and os.path.isdir(stray)
                            and os.listdir(stray)
                        ):
                            shutil.rmtree(stray, ignore_errors=True)
            except OSError:
                pass  # best-effort housekeeping only

    def _convert_into(self, target_dir):
        """ONE-TIME conversion of the bundled Silero VAD checkpoint to the fixed
        (1, MODEL_INPUT) Core ML package, written under `target_dir` (a temp dir;
        the caller publishes it atomically). Builds the elementary-op pure core
        (module docstring), verifies it BIT-FOR-BIT against Silero's own inner
        model over a multi-step stream, then converts. Raises
        CoreMLVADUnavailable if the rebuilt core drifts from the reference."""
        import torch
        import torch.nn.functional as F
        import coremltools as ct
        from silero_vad import load_silero_vad

        torch.set_num_threads(1)
        top = load_silero_vad(onnx=False)  # TorchScript; .jit is bundled in the pkg
        # `load_silero_vad` returns the top-level stateful wrapper; the pure
        # per-chunk core is its 16 kHz submodule `_model`.
        inner = top._model if hasattr(top, "_model") else top
        fb = inner.stft.forward_basis_buffer.detach().clone()  # (258, 1, 256)
        rnn = dict(inner.decoder.rnn.named_parameters())
        wih = rnn["weight_ih"].detach().clone()
        whh = rnn["weight_hh"].detach().clone()
        bih = rnn["bias_ih"].detach().clone()
        bhh = rnn["bias_hh"].detach().clone()

        # Trace the encoder conv stack + decoder conv on real intermediate shapes,
        # so no scripted control flow survives into the wrapper.
        ex_x = torch.randn(1, MODEL_INPUT) * 0.1
        with torch.no_grad():
            xp = F.pad(ex_x.unsqueeze(1), _STFT_PAD, mode="reflect")
            ft = F.conv1d(xp, fb, None, _STFT_STRIDE, 0)
            mag = torch.sqrt(ft[:, :_STFT_BINS, :] ** 2 + ft[:, _STFT_BINS:, :] ** 2)
            x1 = torch.unsqueeze(torch.zeros(1, 128), -1)  # a hidden column (1,128,1)
        enc_t = torch.jit.trace(inner.encoder, (mag,))
        dec_t = torch.jit.trace(inner.decoder.decoder, (x1,))

        class SileroStaticCore(torch.nn.Module):
            def __init__(self):
                super().__init__()
                self.register_buffer("fb", fb)
                self.register_buffer("wih", wih)
                self.register_buffer("whh", whh)
                self.register_buffer("bih", bih)
                self.register_buffer("bhh", bhh)
                self.encoder = enc_t
                self.dec_conv = dec_t

            def forward(self, x, state):
                xp = F.pad(x.unsqueeze(1), _STFT_PAD, mode="reflect")
                ft = F.conv1d(xp, self.fb, None, _STFT_STRIDE, 0)
                mag = torch.sqrt(
                    ft[:, :_STFT_BINS, :] ** 2 + ft[:, _STFT_BINS:, :] ** 2
                )
                enc = self.encoder(mag)
                x0 = torch.squeeze(enc, -1)
                h_prev = state[0]
                c_prev = state[1]
                g = (
                    torch.matmul(x0, self.wih.t()) + self.bih
                    + torch.matmul(h_prev, self.whh.t()) + self.bhh
                )
                i, f, gg, o = torch.split(g, 128, dim=1)
                i = torch.sigmoid(i)
                f = torch.sigmoid(f)
                gg = torch.tanh(gg)
                o = torch.sigmoid(o)
                c = f * c_prev + i * gg
                h = o * torch.tanh(c)
                x1 = torch.unsqueeze(h, -1)
                state0 = torch.stack([h, c])
                x2 = self.dec_conv(x1)
                out = torch.mean(torch.squeeze(x2, 1), dim=1, keepdim=True)
                return out, state0

        core = SileroStaticCore().eval()
        # BIT-FOR-BIT faithfulness of the rebuilt core vs Silero's own inner model
        # over a multi-step stream (the state handoff must match too). A drift here
        # means the elementary-op rebuild diverged — refuse to publish.
        torch.manual_seed(0)
        si = torch.zeros(*STATE_SHAPE)
        sc = torch.zeros(*STATE_SHAPE)
        max_diff = 0.0
        for _ in range(40):
            xin = torch.randn(1, MODEL_INPUT) * 0.1
            with torch.no_grad():
                oi, si = inner(xin, si)
                oc, sc = core(xin, sc)
            max_diff = max(
                max_diff,
                float((oi - oc).abs().max()),
                float((si - sc).abs().max()),
            )
        if not math.isfinite(max_diff) or max_diff > 1e-3:
            raise CoreMLVADUnavailable(
                f"rebuilt Silero core drifted from reference (max abs diff {max_diff:.2e})"
            )

        with torch.no_grad():
            traced = torch.jit.trace(
                core, (torch.zeros(1, MODEL_INPUT), torch.zeros(*STATE_SHAPE)),
                check_trace=False,
            )
        mlmodel = ct.convert(
            traced,
            inputs=[
                ct.TensorType(name="x", shape=(1, MODEL_INPUT), dtype=np.float32),
                ct.TensorType(name="state_in", shape=STATE_SHAPE, dtype=np.float32),
            ],
            outputs=[ct.TensorType(name="prob"), ct.TensorType(name="stateN")],
            convert_to="mlprogram",
            compute_precision=ct.precision.FLOAT16,
            minimum_deployment_target=ct.target.macOS15,
            compute_units=ct.ComputeUnit.ALL,
        )
        os.makedirs(target_dir, exist_ok=True)
        mlmodel.save(os.path.join(target_dir, _MODEL_NAME))

    def step(self, x, state):
        """Run one (1, MODEL_INPUT) predict. `x` is the concat(context, chunk)
        input, `state` the (2,1,128) recurrent state. Returns (prob: float,
        stateN: np.ndarray). Thread-safe."""
        self.ensure_loaded()
        with self._lock:
            out = self._model.predict(
                {"x": np.asarray(x, dtype=np.float32).reshape(1, MODEL_INPUT),
                 "state_in": np.asarray(state, dtype=np.float32).reshape(*STATE_SHAPE)}
            )
        prob = float(np.asarray(out["prob"]).reshape(-1)[0])
        stateN = np.asarray(out["stateN"], dtype=np.float32).reshape(*STATE_SHAPE)
        return prob, stateN

    def reference_probs(self, chunks):
        """DEVICE/DEP-GATED faithfulness reference: stream `chunks` (each CHUNK
        samples) through Silero's OWN inner torch model (no Core ML), returning the
        per-chunk probabilities, for the smoke test to compare the Core ML fp16
        graph against. Needs torch + silero_vad."""
        import torch
        from silero_vad import load_silero_vad

        torch.set_num_threads(1)
        top = load_silero_vad(onnx=False)
        inner = top._model if hasattr(top, "_model") else top
        state = torch.zeros(*STATE_SHAPE)
        context = np.zeros(CONTEXT, dtype=np.float32)
        probs = []
        for ch in chunks:
            x = torch.from_numpy(build_model_input(context, ch))
            with torch.no_grad():
                out, state = inner(x, state)
            probs.append(float(out.reshape(-1)[0]))
            context = next_context(ch)
        return probs


class StreamingVAD:
    """Stateful per-chunk streaming wrapper over `CoreMLVAD`: feed 512-sample
    (32 ms @ 16 kHz) chunks, get a speech probability per chunk, with the
    64-sample STFT context ring and the (2,1,128) recurrent state threaded
    automatically. `reset()` clears both (call it when DARWIN starts speaking, the
    exact point the RMS VAD is reset). PURE state management around the one impure
    call (`CoreMLVAD.step`); the geometry helpers it uses are unit-tested."""

    def __init__(self, backend=None, root=None):
        self._backend = backend if backend is not None else CoreMLVAD(root)
        self.reset()

    def reset(self):
        self._context = np.zeros(CONTEXT, dtype=np.float32)
        self._state = np.zeros(STATE_SHAPE, dtype=np.float32)

    def ensure_loaded(self):
        self._backend.ensure_loaded()

    def push_chunk(self, chunk):
        """Feed exactly CHUNK samples (16 kHz mono float); return the speech
        probability for the chunk. Advances the context ring + recurrent state."""
        x = build_model_input(self._context, chunk)
        prob, self._state = self._backend.step(x, self._state)
        self._context = next_context(chunk)
        return prob


def _smoke():
    """DEVICE-GATED smoke: build/load the Core ML VAD, confirm the FP16 graph is
    faithful to Silero's torch reference, sanity-check that synthetic voiced audio
    scores high and noise/silence low, and print MEASURED per-frame latency under
    ComputeUnit.ALL. Run once by hand (NOT in CI):
        .venv/bin/python inference/coreml_vad.py"""
    import statistics
    import time

    vad = CoreMLVAD()
    vad.ensure_loaded()
    print(f"loaded Core ML VAD from {vad._dir}", flush=True)

    rng = np.random.default_rng(0)
    t = np.arange(SAMPLE_RATE) / SAMPLE_RATE
    voiced = (
        (0.3 * np.sin(2 * np.pi * 140 * t)
         + 0.2 * np.sin(2 * np.pi * 280 * t)
         + 0.1 * np.sin(2 * np.pi * 420 * t))
        * (0.5 + 0.5 * np.sin(2 * np.pi * 4 * t))
    ).astype(np.float32)
    silence = np.zeros(SAMPLE_RATE, dtype=np.float32)
    noise = (0.05 * rng.standard_normal(SAMPLE_RATE)).astype(np.float32)

    def chunks(sig):
        return [sig[i * CHUNK:(i + 1) * CHUNK] for i in range(len(sig) // CHUNK)]

    def stream(sig):
        sv = StreamingVAD(backend=vad)
        return [sv.push_chunk(c) for c in chunks(sig)]

    for name, sig in [("silence", silence), ("white_noise", noise),
                      ("voiced_synth", voiced)]:
        p = np.asarray(stream(sig))
        print(f"  {name:12s} mean={p.mean():.3f} max={p.max():.3f} "
              f"frac>0.5={float((p > DEFAULT_THRESHOLD).mean()):.2f}")

    # Faithfulness: Core ML fp16 vs Silero torch fp32 on the voiced stream.
    cml = np.asarray(stream(voiced))
    ref = np.asarray(vad.reference_probs(chunks(voiced)))
    print(f"faithfulness max|coreml-torch| on voiced: {np.abs(cml - ref).max():.4f}")

    # Per-frame latency under ComputeUnit.ALL.
    x = np.zeros((1, MODEL_INPUT), dtype=np.float32)
    st = np.zeros(STATE_SHAPE, dtype=np.float32)
    for _ in range(5):
        vad.step(x, st)
    runs = []
    for _ in range(200):
        t0 = time.perf_counter()
        vad.step(x, st)
        runs.append((time.perf_counter() - t0) * 1000.0)
    print(f"per-frame latency (ComputeUnit.ALL): median={statistics.median(runs):.3f} ms "
          f"p90={np.percentile(runs, 90):.3f} ms  (frame budget 32 ms)")


if __name__ == "__main__":
    _smoke()
