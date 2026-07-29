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
