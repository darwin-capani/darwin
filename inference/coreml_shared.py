"""Shared state for the Core ML backends (embed / rerank / vad).

TWO things live here, both of which exist because they must be shared ACROSS the
backend modules rather than duplicated per module.

1. CONVERT_LOCK. coremltools conversion is NOT safe to run concurrently in one
   process - the MIL builder keeps global state, and two overlapping conversions
   corrupt each other. This is not theoretical: on the first real deploy of the
   short-graph fast path, server.preload warmed the embedder and the reranker, each
   started its background rebuild, the two converted at the same time, and BOTH died
   ("internal vars in the block are not consistent with self._internal_vars." and a
   KeyError on a linear op). Reproduced deterministically by running the two
   `_convert_into` calls on threads. Every conversion therefore takes this lock.

2. The fast-graph attempt counter. The short graph is optional, so a cache without
   one is rebuilt once to pick the speedup up - but "once" has to mean "a few times,
   then give up", not "exactly once, ever". A single binary sentinel recorded the
   concurrency failure above as PERMANENT, pinning a machine to the slow path for
   good over a transient fault that a retry would have cleared. The counter lets a
   transient failure heal on the next start while still capping a deterministic one.
"""
import logging
import os
import shutil
import threading

log = logging.getLogger("darwin.coreml_shared")

# Serializes ALL coremltools conversions in this process. See (1) above.
CONVERT_LOCK = threading.Lock()

# Cache-dir filename holding the attempt count. Kept as the same name the first
# version used so an existing single-attempt marker is read as one attempt.
FAST_ABSENT_MARK = "no_fast_graph"

# How many times a cache may be rebuilt trying to obtain a usable short graph before
# we accept it will not happen here. Each attempt is a background rebuild (~37-44 s,
# off the request path), so a few is affordable; unbounded is not.
MAX_FAST_ATTEMPTS = 3


def fast_attempts(d):
    """How many times we have already tried and failed to get a short graph in cache
    dir `d`. 0 when the marker is absent. NEVER raises - an unreadable or malformed
    marker counts as 0, i.e. we try again, which is the safe direction (the cost is
    one background rebuild, the alternative is silently never getting the speedup)."""
    try:
        with open(os.path.join(d, FAST_ABSENT_MARK), encoding="utf-8") as fh:
            first = fh.readline().strip()
        return int(first.split()[0])
    except Exception:  # noqa: BLE001 - absent/unreadable/malformed all mean "0"
        return 0


def note_fast_attempt(d, why):
    """BEST-EFFORT: record one more failed attempt at building the short graph in `d`,
    returning the new count (0 if it could not be written). NEVER raises: every caller
    is on a path where the short graph is already a write-off, and the conditions that
    make this write fail (full disk, read-only cache root) are the same ones that made
    the graph fail - turning that into an exception would trade an optional
    optimization for the whole backend, which is a bug this file exists to remember."""
    n = fast_attempts(d) + 1
    try:
        with open(os.path.join(d, FAST_ABSENT_MARK), "w", encoding="utf-8") as fh:
            fh.write(f"{n} attempts\n{why}\n")
        return n
    except Exception as e:  # noqa: BLE001 - advisory only
        log.warning("could not record the short-graph attempt in %s (%s)", d, e)
        return 0


def fast_upgrade_exhausted(d):
    """True once `d` has burned its rebuild budget, so the upgrade must stop trying."""
    return fast_attempts(d) >= MAX_FAST_ATTEMPTS

# ---------------------------------------------------------------------------
# COMPILED-MODEL CACHE
# ---------------------------------------------------------------------------
# `ct.models.MLModel(<dir>.mlpackage)` costs a FULL Core ML program specialization on
# EVERY load - MEASURED at 4.4 s for the 512 embedder graph, 4.6/2.9/2.3/1.5 s across
# the four graphs this server loads, i.e. 11.2 s of an 18.9 s startup. It is not the
# mlpackage -> mlmodelc compilation that costs that (compile_model is ~100 ms); it is
# the specialization, which Core ML caches per COMPILED model. MLModel compiles the
# package into a throwaway temp .mlmodelc each time, so it never hits that cache.
#
# Keeping the compiled artifact and loading it with CompiledMLModel does hit it:
# MEASURED 4.3 s on the very first load, then 65 ms on every later load, INCLUDING in
# a fresh process (verified across three separate processes). 4.4 s -> 65 ms, ~68x.
#
# The .mlpackage is kept alongside (it doubles the per-graph disk, +214 MB across the
# four graphs) so a corrupt or unusable .mlmodelc still has something to fall back to:
# a slow load beats a 40 s reconversion, and beats no model at all.


def _compiled_sibling(pkg_path):
    """The .mlmodelc that belongs to a given .mlpackage, beside it in the cache dir."""
    base = pkg_path[: -len(".mlpackage")] if pkg_path.endswith(".mlpackage") else pkg_path
    return base + ".mlmodelc"


def load_model_fast(pkg_path, compute_units=None):
    """Load `pkg_path` via its persisted .mlmodelc when possible, compiling it once if
    absent. Falls back to loading the .mlpackage directly on ANY problem, so this can
    only ever be faster or the same - never a new way to fail.

    Returns the loaded model. Raises only what loading the .mlpackage itself would."""
    import coremltools as ct

    units = ct.ComputeUnit.ALL if compute_units is None else compute_units
    mlmodelc = _compiled_sibling(pkg_path)
    try:
        if not os.path.isdir(mlmodelc):
            # Compile to a UNIQUE temp sibling, then claim the final name only if it is
            # still free. Two processes racing both end up with a valid compiled model;
            # the loser discards its own copy rather than trying to replace a populated
            # directory (os.replace onto a non-empty dir raises ENOTEMPTY).
            # The temp name must ALSO end in .mlmodelc - compile_model validates the
            # extension. (Second time this session an extension-validating API has
            # caught me: mx.save_safetensors does the same for prompt caches.)
            tmp = f"{mlmodelc[: -len('.mlmodelc')]}.{os.getpid()}.tmp.mlmodelc"
            shutil.rmtree(tmp, ignore_errors=True)
            ct.models.utils.compile_model(pkg_path, destination_path=tmp)
            try:
                if not os.path.isdir(mlmodelc):
                    os.rename(tmp, mlmodelc)
                else:
                    shutil.rmtree(tmp, ignore_errors=True)
            except OSError:
                shutil.rmtree(tmp, ignore_errors=True)
        if os.path.isdir(mlmodelc):
            return ct.models.CompiledMLModel(mlmodelc, compute_units=units)
    except Exception as e:  # noqa: BLE001 - an optimization must never be the failure
        log.warning(
            "compiled-model cache unusable for %s (%s); loading the package directly",
            os.path.basename(pkg_path), e,
        )
        shutil.rmtree(_compiled_sibling(pkg_path), ignore_errors=True)
    return ct.models.MLModel(pkg_path, compute_units=units)
