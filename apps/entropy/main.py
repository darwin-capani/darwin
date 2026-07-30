#!/usr/bin/env python3
"""Read-only secret-strength estimator: charset size, length, Shannon entropy bits, and strength class."""
import math
import os
import sys

# Shared host-link plumbing (socket loop, token stamping, frame bound, the
# agent-tool id echo) from apps/_sdk — fs_read-granted. The path is resolved
# relative to THIS file (apps/<app>/main.py -> ../_sdk), so it works both when
# darwind launches the app (cwd = project root) and when the tests run from the
# app dir. Bytecode writes are disabled since apps/_sdk is read-only in the
# sandbox. Re-importing drain_lines/MAX_FRAME_BYTES/TOKEN keeps them resolvable
# off `main` for the framing/contract tests.
sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "_sdk"))
from harness import (  # noqa: E402 — must follow the sys.path insert above
    MAX_FRAME_BYTES,
    TOKEN,
    drain_lines,
    reply_result,
    run,
    send,
)


def compute(payload):
    """PURE, offline, no I/O, never raises.

    Reads payload["text"] (a candidate secret). Determines the charset size from
    the character classes present (lowercase 26, uppercase 26, digits 10,
    other/symbols 32) and estimates Shannon entropy as length * log2(charset).
    Returns only aggregate stats -- the input text is never echoed.
    """
    try:
        text = payload.get("text", "") if isinstance(payload, dict) else ""
    except Exception:  # noqa: BLE001 -- never raise on hostile input
        text = ""
    if not isinstance(text, str):
        text = ""

    has_lower = has_upper = has_digit = has_other = False
    for ch in text:
        if "a" <= ch <= "z":
            has_lower = True
        elif "A" <= ch <= "Z":
            has_upper = True
        elif "0" <= ch <= "9":
            has_digit = True
        else:
            has_other = True

    charset_size = 0
    if has_lower:
        charset_size += 26
    if has_upper:
        charset_size += 26
    if has_digit:
        charset_size += 10
    if has_other:
        charset_size += 32

    length = len(text)
    # `length * log2(charset)` is the size of the KEYSPACE a random password of this
    # length and alphabet would be drawn from. It is NOT the entropy of THIS string,
    # and reporting it as such rated "aaaaaaaaaaaaaaaa" at 75.21 bits / "strong" -
    # confidently wrong about the one question this tool exists to answer, and wrong
    # in the unsafe direction. A string that repeats one character carries almost no
    # information no matter how long it is.
    #
    # So compute BOTH and rate on the lower:
    #   keyspace_bits  - the random-password bound (what this used to report alone)
    #   observed_bits  - Shannon entropy of the actual character distribution, times
    #                    length; this is what collapses for repetitive input.
    shannon_per_char = 0.0
    if length == 0 or charset_size == 0:
        keyspace_bits = 0.0
        observed_bits = 0.0
    else:
        keyspace_bits = round(length * math.log2(charset_size), 2)
        counts = {}
        for ch in text:
            counts[ch] = counts.get(ch, 0) + 1
        shannon_per_char = -sum(
            (n / length) * math.log2(n / length) for n in counts.values()
        )
        # A single repeated character gives shannon_per_char == 0.0, which
        # float arithmetic can render as -0.0; clamp so the report never
        # shows "-0.00 bits".
        observed_bits = round(max(0.0, length * shannon_per_char), 2)
    # Combine them as a REPETITION RATIO rather than a plain min().
    #
    # observed_bits alone is a biased estimator: for any string whose characters are
    # all distinct it collapses to length*log2(length), so a genuinely random 20-char
    # password would be scored 86 bits instead of its real 131 — penalising exactly
    # the passwords that deserve credit. What the observed distribution actually
    # measures well is REPETITION, so use it as a fraction of the most it could have
    # been for a string of this length:
    #
    #   ratio = H_observed / log2(min(length, charset_size))
    #
    # All-distinct characters give ratio 1.0 and the keyspace bound is kept intact.
    # One repeated character gives 0.0. "Ab1!Ab1!..." gives log2(4)/log2(20) = 0.46,
    # which is the honest discount for a pattern that only looks varied.
    if length <= 1 or charset_size == 0:
        bits = 0.0
    else:
        max_observable = math.log2(min(length, charset_size))
        ratio = 1.0 if max_observable <= 0 else (shannon_per_char / max_observable)
        ratio = max(0.0, min(1.0, ratio))
        bits = round(max(0.0, keyspace_bits * ratio), 2)

    if bits < 28:
        strength = "very weak"
    elif bits < 36:
        strength = "weak"
    elif bits < 60:
        strength = "fair"
    elif bits < 128:
        strength = "strong"
    else:
        strength = "very strong"

    return {
        "length": length,
        "keyspace_bits": keyspace_bits,
        "observed_bits": observed_bits,
        "distinct_chars": len(set(text)),
        "charset_size": charset_size,
        "bits": bits,
        "strength": strength,
    }


def handle(conn, msg):
    op = msg.get("type") or msg.get("op")
    if op == "start":
        send(conn, {"type": "status", "data": {"tool": "entropy.assess", "ready": True}})
    elif op == "refresh":
        send(conn, {"type": "items", "data": {"status": "ok"}})
    elif op == "entropy.assess":
        reply_result(conn, msg, compute(msg))
    elif op == "stop":
        raise SystemExit(0)


if __name__ == "__main__":
    sys.exit(run(handle))
