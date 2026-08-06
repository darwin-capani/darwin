#!/usr/bin/env python3
"""Tests for cronwise.compute: wildcards, steps, ranges, lists, and hostile/empty input."""
import unittest

from main import compute


class TestCronwiseExplain(unittest.TestCase):
    def test_every_five_minutes(self):
        r = compute({"cron": "*/5 * * * *"})
        self.assertTrue(r["valid"])
        self.assertEqual(r["minute"], "every 5 minutes")
        self.assertEqual(r["hour"], "every hour")
        self.assertEqual(r["day_of_month"], "every day-of-month")
        self.assertEqual(r["month"], "every month")
        self.assertEqual(r["day_of_week"], "every day-of-week")
        self.assertEqual(
            r["summary"],
            "every 5 minutes, every hour, every day-of-month, every month, every day-of-week",
        )

    def test_single_values(self):
        # "0 0 1 1 *" -> midnight on the first of January, any weekday.
        r = compute({"cron": "0 0 1 1 *"})
        self.assertTrue(r["valid"])
        self.assertEqual(r["minute"], "at minute 0")
        self.assertEqual(r["hour"], "at hour 0")
        self.assertEqual(r["day_of_month"], "on day-of-month 1")
        self.assertEqual(r["month"], "on month January")
        self.assertEqual(r["day_of_week"], "every day-of-week")

    def test_ranges_lists_and_names(self):
        # Business hours: minute 30, hours 9-17, Mon-Fri.
        r = compute({"cron": "30 9-17 * * 1-5"})
        self.assertTrue(r["valid"])
        self.assertEqual(r["minute"], "at minute 30")
        self.assertEqual(r["hour"], "every hour from 9 through 17")
        self.assertEqual(r["day_of_week"], "every day-of-week from Monday through Friday")
        # Comma list of values.
        r2 = compute({"cron": "0,15,30,45 * * * *"})
        self.assertTrue(r2["valid"])
        self.assertEqual(
            r2["minute"], "at minute 0; at minute 15; at minute 30; at minute 45"
        )
        # Named month/dow abbreviations resolve.
        r3 = compute({"cron": "0 12 * jan mon"})
        self.assertTrue(r3["valid"])
        self.assertEqual(r3["month"], "on month January")
        self.assertEqual(r3["day_of_week"], "on day-of-week Monday")

    def test_day_of_week_seven_is_sunday(self):
        # Standard cron allows day-of-week 0-7 with both 0 and 7 = Sunday.
        r = compute({"cron": "0 0 * * 7"})
        self.assertTrue(r["valid"], r)
        self.assertEqual(r["day_of_week"], "on day-of-week Sunday")
        # A range through 7 (Fri-Sun) is valid too.
        r2 = compute({"cron": "0 0 * * 5-7"})
        self.assertTrue(r2["valid"], r2)
        self.assertEqual(
            r2["day_of_week"], "every day-of-week from Friday through Sunday"
        )

    def test_day_of_week_steps_do_not_model_the_week_as_eight_days(self):
        # REGRESSION: day-of-week is the ONE field whose numeric range lies about the
        # length of its cycle — cron accepts 0-7 with BOTH 0 and 7 meaning Sunday, so
        # hi - lo + 1 is 8 while a week is 7 days. Every cadence decision is arithmetic
        # over that length, so all three step branches claimed a rhythm cron does not
        # perform: "*/2" fires Sun, Tue, Thu, Sat and then Sunday again — a ONE-day
        # wrap — yet 4 * 2 == 8 "proved" it evenly spaced and it read "every 2
        # days-of-week", the exact misconception this app exists to correct. And "*/7"
        # fires on Sunday and nothing else, but its raw expansion is [0, 7], so it
        # printed the duplicated non-set "on days-of-week Sunday, Sunday".
        r = compute({"cron": "0 0 * * */2"})
        self.assertTrue(r["valid"], r)
        self.assertEqual(
            r["day_of_week"], "on days-of-week Sunday, Tuesday, Thursday, Saturday"
        )
        self.assertEqual(compute({"cron": "0 0 * * */7"})["day_of_week"],
                         "on day-of-week Sunday")
        # The start-offset form and the range form share the same cycle.
        self.assertEqual(
            compute({"cron": "0 0 * * 0/2"})["day_of_week"],
            "on days-of-week Sunday, Tuesday, Thursday, Saturday",
        )
        # 1/2 fires Mon, Wed, Fri and Sun (7 IS Sunday) — listed in cycle order.
        self.assertEqual(
            compute({"cron": "0 0 * * 1/2"})["day_of_week"],
            "on days-of-week Sunday, Monday, Wednesday, Friday",
        )
        # A named start resolves to the same number, so it gets the same answer.
        self.assertEqual(
            compute({"cron": "0 0 * * mon/2"})["day_of_week"],
            compute({"cron": "0 0 * * 1/2"})["day_of_week"],
        )
        self.assertEqual(compute({"cron": "0 0 * * 0-7/7"})["day_of_week"],
                         "on day-of-week Sunday")
        # None of these may name a cadence or list one day twice.
        for expr in ["*/2", "*/7", "0/2", "1/2", "mon/2", "0-7/2", "0-7/7"]:
            got = compute({"cron": "0 0 * * " + expr})["day_of_week"]
            self.assertNotIn("Sunday, Sunday", got, expr)
            self.assertFalse(got.startswith("every"), "%s -> %r" % (expr, got))

    def test_a_range_whose_endpoints_name_the_same_value_is_not_an_interval(self):
        # "from X through X" describes no interval. Day-of-week is the only field that
        # can collide its endpoints while still spanning days, because cron writes
        # Sunday twice: "0-7" is the WHOLE week and "7-7" is Sunday ALONE, yet both
        # read "every day-of-week from Sunday through Sunday".
        self.assertEqual(compute({"cron": "0 0 * * 0-7"})["day_of_week"],
                         "every day-of-week")
        self.assertEqual(compute({"cron": "0 0 * * 0-7/1"})["day_of_week"],
                         "every day-of-week")
        self.assertEqual(compute({"cron": "0 0 * * 7-7"})["day_of_week"],
                         "on day-of-week Sunday")
        self.assertEqual(compute({"cron": "5-5 * * * *"})["minute"], "at minute 5")
        # A real interval is untouched, and so are the other fields' cadences.
        self.assertEqual(
            compute({"cron": "0 0 * * 5-7"})["day_of_week"],
            "every day-of-week from Friday through Sunday",
        )
        self.assertEqual(compute({"cron": "*/5 * * * *"})["minute"], "every 5 minutes")
        self.assertEqual(
            compute({"cron": "0 0-23/2 * * *"})["hour"], "every 2 hours from 0 through 23"
        )

    def test_step_variants(self):
        # Stepped range and stepped single-start.
        r = compute({"cron": "0 0-23/2 * * *"})
        self.assertTrue(r["valid"])
        self.assertEqual(r["hour"], "every 2 hours from 0 through 23")
        r2 = compute({"cron": "5/10 * * * *"})
        self.assertTrue(r2["valid"])
        self.assertEqual(r2["minute"], "every 10 minutes starting at minute 5")

    def test_step_wider_than_the_field_matches_one_value_not_an_interval(self):
        # REGRESSION: cron expands the base range and keeps every step'th value, so
        # a step at/beyond the range width matches exactly ONE value. "*/90" fires
        # HOURLY (minute 0 only) — describing it as "every 90 minutes" restated an
        # interval cron never performs, the very misconception this app exists to
        # correct.
        r = compute({"cron": "*/90 * * * *"})
        self.assertTrue(r["valid"])
        self.assertEqual(r["minute"], "at minute 0")
        self.assertNotIn("90", r["summary"])
        # Same on the range form and the single-start form.
        r2 = compute({"cron": "1-10/20 * * * *"})
        self.assertEqual(r2["minute"], "at minute 1")
        r3 = compute({"cron": "5/70 * * * *"})
        self.assertEqual(r3["minute"], "at minute 5")

    def test_a_step_that_does_not_divide_the_field_enumerates_the_real_set(self):
        # REGRESSION: "*/25" matches 0, 25, 50 and then WRAPS with a 10-minute gap —
        # it is not "every 25 minutes". Same for a month step that only matches two
        # months.
        r = compute({"cron": "*/25 * * * *"})
        self.assertTrue(r["valid"])
        self.assertEqual(r["minute"], "at minutes 0, 25, 50")
        r2 = compute({"cron": "0 0 * */7 *"})
        self.assertTrue(r2["valid"])
        self.assertEqual(r2["month"], "on months January, August")
        # A step that DOES divide the field evenly keeps the compact phrasing.
        self.assertEqual(compute({"cron": "*/5 * * * *"})["minute"], "every 5 minutes")

    def test_invalid_field_count(self):
        r = compute({"cron": "* * * *"})
        self.assertFalse(r["valid"])
        self.assertIn("5", r["error"])
        r2 = compute({"cron": "* * * * * *"})
        self.assertFalse(r2["valid"])

    def test_out_of_range_and_bad_syntax(self):
        # Minute 60 is out of range (0-59).
        r = compute({"cron": "60 * * * *"})
        self.assertFalse(r["valid"])
        self.assertIn("minute", r["error"])
        # Non-numeric where a number is required.
        r2 = compute({"cron": "abc * * * *"})
        self.assertFalse(r2["valid"])
        # Reversed range.
        r3 = compute({"cron": "* 17-9 * * *"})
        self.assertFalse(r3["valid"])
        # Zero step is invalid.
        r4 = compute({"cron": "*/0 * * * *"})
        self.assertFalse(r4["valid"])

    def test_hostile_and_empty_inputs_do_not_raise(self):
        for bad in [None, {}, {"cron": 123}, {"cron": None}, {"cron": ["x"]},
                    [], "str", 42, {"cron": ""}, {"cron": "   "}]:
            r = compute(bad)
            self.assertIsInstance(r, dict)
            self.assertFalse(r["valid"])
            self.assertIn("error", r)


# --- input-frame bounding (defense in depth) ---------------------------------
# main()'s socket read loop routes every recv() chunk through main.drain_lines,
# which DROPS a partial frame once it passes MAX_FRAME_BYTES with no newline, so a
# peer streaming bytes without a newline cannot grow the read buffer without bound
# (OOM). These assert that real helper — the daemon side is already bounded
# (apps.rs read_line_bounded / genproxy MAX_PROXY_LINE_BYTES).
import main as _frame_mod  # noqa: E402 — deliberately mid-file, after the app's own imports


def test_max_frame_bytes_is_8_mib():
    assert _frame_mod.MAX_FRAME_BYTES == 8 * 1024 * 1024


def test_oversized_frame_is_dropped_not_accumulated():
    # A newline-less frame past the cap is DISCARDED, not retained -> memory bounded.
    cap = _frame_mod.MAX_FRAME_BYTES
    lines, buf, overflowed = _frame_mod.drain_lines(b"x" * (cap + 1))
    assert overflowed is True
    assert buf == b""
    assert lines == []


def test_complete_lines_drain_and_partial_is_preserved():
    # Newline framing is intact: whole lines come out in order; a small partial stays.
    lines, buf, overflowed = _frame_mod.drain_lines(b'{"a":1}\n{"b":2}\n{"c":3')
    assert lines == [b'{"a":1}', b'{"b":2}']
    assert buf == b'{"c":3'
    assert overflowed is False


# -- the agent-tool request/response contract (SHARED shape; copy per app) ----
# cronwise.explain is the declared, non-consequential tool the agent loop invokes.
# A request carrying a request `id` is answered with a type:"result" line echoing
# that id; an op without one keeps the legacy uncorrelated type:"items" line.
# `{"cron": "*/5 * * * *"}` is a minimal payload compute() returns valid=True for.
import json  # noqa: E402 — used only by FakeConn below


class FakeConn:
    """Captures sendall payloads so handle() can be driven without a socket."""

    def __init__(self):
        self.lines = []

    def sendall(self, raw):
        self.lines.append(json.loads(raw.decode("utf-8").strip()))


def test_tool_op_with_id_answers_a_correlated_result():
    conn = FakeConn()
    _frame_mod.handle(conn, {"type": "cronwise.explain", "id": "req-7", "cron": "*/5 * * * *"})
    assert len(conn.lines) == 1
    reply = conn.lines[0]
    assert reply["type"] == "result", reply
    assert reply["id"] == "req-7", "the request id is echoed verbatim"
    assert reply["data"]["valid"] is True
    assert reply["token"] == _frame_mod.TOKEN


def test_tool_op_without_id_keeps_the_legacy_items_line():
    conn = FakeConn()
    _frame_mod.handle(conn, {"type": "cronwise.explain", "cron": "*/5 * * * *"})
    assert len(conn.lines) == 1
    reply = conn.lines[0]
    assert reply["type"] == "items", "no id -> uncorrelated legacy line"
    assert "id" not in reply
    assert reply["data"]["valid"] is True


def test_non_string_or_empty_id_is_treated_as_absent():
    for bad_id in (7, "", None, ["x"]):
        conn = FakeConn()
        _frame_mod.handle(conn, {"type": "cronwise.explain", "id": bad_id, "cron": "*/5 * * * *"})
        assert conn.lines[0]["type"] == "items", f"id={bad_id!r} must not correlate"


def test_step_forms_never_claim_a_cadence_cron_does_not_perform():
    """_describe_field documents the rule -- "STEP FORMS describe the set cron ACTUALLY
    matches, not the step number" -- and only the `*/n` branch enforced it. 0/25 fires
    at :00, :25, :50 and then :00 again, a 10-minute gap, yet it was described as
    "every 25 minutes starting at minute 0". The same firing set was described two
    different ways depending on how it was written."""
    pairs = [
        ("*/25 * * * *", "0/25 * * * *"),
        ("0 */15 * * *", "0 0/15 * * *"),
    ]
    for star_form, base_form in pairs:
        a = compute({"cron": star_form})["summary"]
        b = compute({"cron": base_form})["summary"]
        assert a == b, f"{star_form!r} -> {a!r} but {base_form!r} -> {b!r}"
        assert "every 25 minutes" not in b or "*/25" in base_form


def test_a_range_step_that_stops_short_does_not_claim_the_endpoint():
    """0-30/25 last fires at minute 25, never 30, so "through 30" names a firing that
    does not happen."""
    got = compute({"cron": "0-30/25 * * * *"})["summary"]
    assert "through" not in got, got
    assert "25" in got and "0" in got


def test_a_start_offset_step_is_not_an_even_cadence():
    """MY PREVIOUS FIX WAS INCOMPLETE. It hoisted `(hi - lo + 1) % step == 0`, which
    only asks whether the step divides the field's cycle and says nothing about where
    the set STARTS. So every N/step with N >= step still claimed an even cadence:
    30/15 fires at :30 and :45 and then :30 again — a 45-minute gap — and was described
    as "every 15 minutes starting at minute 30"."""
    for expr, forbidden in [
        ("30/15 * * * *", "every 15 minutes"),
        ("0 12/6 * * *", "every 6 hours"),
        ("45/20 * * * *", "every 20 minutes"),
    ]:
        got = compute({"cron": expr})["summary"]
        assert forbidden not in got, f"{expr!r} -> {got!r}"


def test_honest_cadences_are_still_phrased_as_cadences():
    """The fix must not flatten every step form into an enumeration."""
    assert "every 15 minutes" in compute({"cron": "*/15 * * * *"})["summary"]
    assert "every 15 minutes" in compute({"cron": "5/15 * * * *"})["summary"]
    assert "every 15 minutes" in compute({"cron": "0-59/15 * * * *"})["summary"]


if __name__ == "__main__":
    # Script-style runs exercise the framing tests too — they are plain
    # functions the runner below would otherwise never call.
    test_max_frame_bytes_is_8_mib()
    test_oversized_frame_is_dropped_not_accumulated()
    test_complete_lines_drain_and_partial_is_preserved()
    print("framing: 3 checks ok")
    test_tool_op_with_id_answers_a_correlated_result()
    test_tool_op_without_id_keeps_the_legacy_items_line()
    test_non_string_or_empty_id_is_treated_as_absent()
    print("agent-tool contract: 3 checks ok")
    test_step_forms_never_claim_a_cadence_cron_does_not_perform()
    test_a_range_step_that_stops_short_does_not_claim_the_endpoint()
    test_a_start_offset_step_is_not_an_even_cadence()
    test_honest_cadences_are_still_phrased_as_cadences()
    print("step honesty: 3 checks ok")
    unittest.main()