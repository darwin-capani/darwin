#!/usr/bin/env python3
"""Read-only cron explainer: parse a 5-field cron string and describe each field in plain English."""
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


# Per-field metadata: (label, unit_singular, min, max, names).
# names maps lowercased month/day-of-week abbreviations to their numeric value.
_MONTHS = {
    "jan": 1, "feb": 2, "mar": 3, "apr": 4, "may": 5, "jun": 6,
    "jul": 7, "aug": 8, "sep": 9, "oct": 10, "nov": 11, "dec": 12,
}
_DOW = {
    "sun": 0, "mon": 1, "tue": 2, "wed": 3, "thu": 4, "fri": 5, "sat": 6,
}
# 5 fields in cron order: minute, hour, day-of-month, month, day-of-week.
_FIELDS = [
    ("minute", "minute", 0, 59, {}),
    ("hour", "hour", 0, 23, {}),
    ("day_of_month", "day-of-month", 1, 31, {}),
    ("month", "month", 1, 12, _MONTHS),
    # Standard cron accepts day-of-week 0-7 where BOTH 0 and 7 mean Sunday.
    ("day_of_week", "day-of-week", 0, 7, _DOW),
]
_UNIT_PLURAL = {
    "minute": "minutes",
    "hour": "hours",
    "day-of-month": "days-of-month",
    "month": "months",
    "day-of-week": "days-of-week",
}
_MONTH_NAMES = {
    1: "January", 2: "February", 3: "March", 4: "April", 5: "May", 6: "June",
    7: "July", 8: "August", 9: "September", 10: "October", 11: "November", 12: "December",
}
_DOW_NAMES = {
    0: "Sunday", 1: "Monday", 2: "Tuesday", 3: "Wednesday",
    4: "Thursday", 5: "Friday", 6: "Saturday",
    7: "Sunday",  # cron's alternate Sunday (0 and 7 both denote Sunday)
}


def _resolve(token, names, lo, hi):
    """Resolve a single value token to an int within [lo, hi]. Raises ValueError on bad input."""
    key = token.strip().lower()
    if key in names:
        return names[key]
    val = int(token)  # raises ValueError on non-numeric
    if val < lo or val > hi:
        raise ValueError("out of range %d-%d" % (lo, hi))
    return val


def _pretty_value(unit, val):
    """Render a resolved numeric value using friendly names for months/days-of-week."""
    if unit == "month":
        return _MONTH_NAMES.get(val, str(val))
    if unit == "day-of-week":
        return _DOW_NAMES.get(val, str(val))
    return str(val)


def _single_phrase(unit, val):
    """The phrase for a field that matches EXACTLY ONE value."""
    if unit == "minute":
        return "at minute %d" % val
    if unit == "hour":
        return "at hour %d" % val
    return "on %s %s" % (unit, _pretty_value(unit, val))


def _set_phrase(unit, vals):
    """The phrase ENUMERATING the values a field actually matches."""
    if len(vals) == 1:
        return _single_phrase(unit, vals[0])
    listed = ", ".join(_pretty_value(unit, v) for v in vals)
    if unit in ("minute", "hour"):
        return "at %s %s" % (_UNIT_PLURAL[unit], listed)
    return "on %s %s" % (_UNIT_PLURAL[unit], listed)


def _describe_field(label, unit, lo, hi, names, raw):
    """Return a plain-English phrase for one cron field. Raises ValueError on invalid syntax.

    STEP FORMS describe the set cron ACTUALLY matches, not the step number. Real
    cron expands the base range and keeps every step'th value, so a step at or
    beyond the range's width matches exactly ONE value: `*/90` on minutes fires
    ONLY at minute 0 — hourly — yet this read "every 90 minutes", which is the
    single most common cron misconception and the reason an explainer exists.
    Likewise `*/25` matches 0, 25, 50 with a 10-minute wrap gap, so "every 25
    minutes" overstates a uniform cadence the job never has.
    """
    raw = raw.strip()
    if raw == "":
        raise ValueError("empty field")
    plural = _UNIT_PLURAL[unit]

    # Comma list: describe each part and join.
    if "," in raw:
        parts = [p for p in raw.split(",")]
        phrases = [_describe_field(label, unit, lo, hi, names, p) for p in parts]
        return "; ".join(phrases)

    # Step form: base/step  (base may be "*", a value, or a range).
    if "/" in raw:
        base, _, step_s = raw.partition("/")
        base = base.strip()
        step = int(step_s)  # raises ValueError on non-numeric
        if step <= 0:
            raise ValueError("step must be positive")
        if base == "*":
            if step == 1:
                return "every %s" % unit
            vals = list(range(lo, hi + 1, step))
            # ONE match (step >= the field's width, e.g. */90 on minutes), or a
            # step that does NOT divide the field evenly (*/25 -> 0,25,50 then a
            # 10-minute gap at the wrap): enumerate the real firing set instead of
            # restating a cadence cron does not perform.
            if len(vals) == 1 or (hi - lo + 1) % step != 0:
                return _set_phrase(unit, vals)
            return "every %d %s" % (step, plural)
        if "-" in base:
            a_s, _, b_s = base.partition("-")
            a = _resolve(a_s, names, lo, hi)
            b = _resolve(b_s, names, lo, hi)
            if a > b:
                raise ValueError("range start > end")
            vals = list(range(a, b + 1, step))
            if len(vals) == 1:
                # e.g. 1-10/20 -> matches ONLY minute 1, never "every 20 minutes".
                return _single_phrase(unit, vals[0])
            return "every %d %s from %s through %s" % (
                step, plural, _pretty_value(unit, a), _pretty_value(unit, b))
        # base is a single value: e.g. 5/10 -> every 10 <unit> starting at <value>
        start = _resolve(base, names, lo, hi)
        vals = list(range(start, hi + 1, step))
        if len(vals) == 1:
            # e.g. 5/70 -> matches ONLY minute 5, never "every 70 minutes".
            return _single_phrase(unit, vals[0])
        return "every %d %s starting at %s %s" % (
            step, plural, unit, _pretty_value(unit, start))

    # Wildcard.
    if raw == "*":
        return "every %s" % unit

    # Range.
    if "-" in raw:
        a_s, _, b_s = raw.partition("-")
        a = _resolve(a_s, names, lo, hi)
        b = _resolve(b_s, names, lo, hi)
        if a > b:
            raise ValueError("range start > end")
        return "every %s from %s through %s" % (
            unit, _pretty_value(unit, a), _pretty_value(unit, b))

    # Single value.
    return _single_phrase(unit, _resolve(raw, names, lo, hi))


def compute(payload):
    """PURE, offline, no I/O, never raises.

    Reads payload["cron"] (a 5-field cron string like "*/5 * * * *"), splits on
    whitespace, and returns a per-field plain-English description plus a joined
    summary. If the string is not exactly 5 fields (or a field is invalid) it
    returns {"valid": False, "error": ...}.
    """
    try:
        cron = payload.get("cron", "") if isinstance(payload, dict) else ""
    except Exception:  # noqa: BLE001 — never raise on hostile input
        cron = ""
    if not isinstance(cron, str):
        return {"valid": False, "error": "cron must be a string"}

    fields = cron.split()
    if len(fields) != 5:
        return {
            "valid": False,
            "error": "expected 5 whitespace-separated fields, got %d" % len(fields),
        }

    out = {}
    phrases = []
    for (label, unit, lo, hi, names), raw in zip(_FIELDS, fields):
        try:
            phrase = _describe_field(label, unit, lo, hi, names, raw)
        except ValueError as e:
            return {
                "valid": False,
                "error": "invalid %s field %r: %s" % (label, raw, e),
            }
        except Exception as e:  # noqa: BLE001 — defensive; never raise
            return {
                "valid": False,
                "error": "invalid %s field %r: %s" % (label, raw, e),
            }
        out[label] = phrase
        phrases.append(phrase)

    out["valid"] = True
    # DAY-OF-MONTH and DAY-OF-WEEK are OR'd, not AND'd, whenever BOTH are
    # restricted. This is cron's most surprising rule (POSIX / Vixie cron), and
    # joining the field phrases with commas described "0 0 1 * 1" as "on
    # day-of-month 1 ... on day-of-week Monday" — which every reader takes as
    # "the 1st, if it is a Monday". It actually fires on the 1st OR on ANY Monday,
    # roughly five times more often. Explaining exactly this is what the tool is
    # for, so it is called out rather than left to the comma.
    dom_raw, dow_raw = fields[2], fields[4]
    dom_restricted = dom_raw not in ("*", "?")
    dow_restricted = dow_raw not in ("*", "?")
    out["dom_dow_or"] = dom_restricted and dow_restricted
    if out["dom_dow_or"]:
        out["summary"] = "%s, and runs when EITHER %s OR %s matches (cron ORs these two fields when both are restricted)" % (
            ", ".join(phrases[:2] + phrases[3:4]),
            out.get("day_of_month", "day-of-month " + dom_raw),
            out.get("day_of_week", "day-of-week " + dow_raw),
        )
    else:
        out["summary"] = ", ".join(phrases)
    return out


def handle(conn, msg):
    op = msg.get("type") or msg.get("op")
    if op == "start":
        send(conn, {"type": "status", "data": {"tool": "cronwise.explain", "ready": True}})
    elif op == "refresh":
        send(conn, {"type": "items", "data": {"status": "ok"}})
    elif op == "cronwise.explain":
        reply_result(conn, msg, compute(msg))
    elif op == "stop":
        raise SystemExit(0)


if __name__ == "__main__":
    sys.exit(run(handle))
