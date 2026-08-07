#!/usr/bin/env python3.11
"""Stdlib-only unit tests for the Nexus control plane (apps/nexus/main.py).

Scope (the python-control module's verifiable surface — SPEC §5/§6):
  - op dispatch: route.set / gain.set / mute / monitor.set / state.get onto the
    real native engine, with the ack/telemetry side-effects captured,
  - gain/index clamping (out-of-range rejected, never crashes the dispatcher),
  - preset TOML save+load IDENTITY through the module's own serializer/parser,
  - telemetry payload SHAPES match the SPEC §6 topics (levels/spectrum/routes/
    gain), and ride the daemon-relayable `type:"items"` wire shape,
  - the capability TOKEN is stamped on EVERY emitted line,
  - preset-name PATH CONFINEMENT (no traversal out of presets/ or scratch/).

NO socket, NO device, NO audio: a FakeLink SUBCLASSES the real HostLink with a
fake socket, so the wire-shape/token assertions run against HostLink's OWN
send/telemetry framing (it used to reimplement that framing, which made every
one of those assertions vacuous — see the FakeLink docstring),
and the engine is driven purely through the FFI on synthesized state. Tests that
need the native core are skipped (not failed) if the cdylib is not yet built, so
this file is runnable headlessly even before a `cargo build`; the pure-Python
tests (TOML round-trip, path confinement, wire shape, token discipline) always
run.

Run: python3.11 -m unittest apps/nexus/test_main.py  (or `-m unittest` from here)
"""

from __future__ import annotations

import json
import math
import tempfile
import threading
import unittest
from pathlib import Path

import main as nexus


# --------------------------------------------------------------------------- #
# Test doubles.
# --------------------------------------------------------------------------- #
class FakeSocket:
    """A socket stand-in: the ONLY thing HostLink actually needs to write to.
    Every byte HostLink `sendall`s is appended here verbatim, so the framing
    under test is the real one."""

    def __init__(self) -> None:
        self.written: list[bytes] = []

    def sendall(self, payload: bytes) -> None:
        self.written.append(payload)


class FakeLink(nexus.HostLink):
    """A HostLink whose SOCKET is faked — not its logic.

    WHAT WENT WRONG: this used to be a stand-alone class that REIMPLEMENTED
    HostLink's token stamping and wire framing, and every assertion in
    TestTelemetryWireShape ran against that copy while `HostLink` was never
    instantiated anywhere in this file. Mutating main.py's HostLink to emit
    `type:"telemetry"` (a type the daemon DROPS), to nest the payload under a
    `payload` key (which leaves every HUD field one level too deep), and to strip
    the capability token entirely still passed all 45 tests. The class advertised
    that it tested "the real serialization path"; it tested itself.

    Now it SUBCLASSES HostLink and substitutes only the socket, so
    HostLink.send / HostLink.telemetry / HostLink.log ARE the code under test.
    `raw`/`lines` are derived from the bytes that reached the fake socket."""

    def __init__(self, token: str = "tok-CAFEBABE") -> None:
        # Deliberately NOT calling HostLink.__init__ — it connects a real AF_UNIX
        # socket. We bind exactly the attributes HostLink's write path touches.
        self._token = token
        self._sock = FakeSocket()
        self._wlock = threading.Lock()
        self._rfile = None  # no inbound command stream in these tests

    # -- what actually went over the wire --
    @property
    def raw(self) -> list[str]:
        """The exact JSON strings written, one per line, newline stripped."""
        out: list[str] = []
        for chunk in self._sock.written:
            text = chunk.decode("utf-8")
            # HostLink frames one newline-terminated line per send; assert that
            # here so a framing regression (missing/extra newline, batched
            # writes) surfaces instead of being smoothed over by the split.
            assert text.endswith("\n"), f"HostLink wrote an unterminated line: {text!r}"
            out.append(text[:-1])
        return out

    @property
    def lines(self) -> list[dict]:
        """The decoded {token,type,data} dicts, parsed back off the wire."""
        return [json.loads(r) for r in self.raw]

    def clear(self) -> None:
        """Drop everything captured so far, so a test can assert on only what a
        LATER call emits. `lines`/`raw` are DERIVED from the fake socket's byte
        log, so `link.lines.clear()` would empty a temporary copy and silently do
        nothing — the source has to be cleared."""
        self._sock.written.clear()

    # -- assertion helpers --
    def telemetry_for(self, topic: str) -> list[dict]:
        out = []
        for ln in self.lines:
            if ln["type"] == "items" and ln["data"].get("topic") == topic:
                # Flattened wire: payload fields sit in data alongside topic. Return
                # just the fields (topic stripped) so callers assert on the payload.
                out.append({k: v for k, v in ln["data"].items() if k != "topic"})
        return out

    def logs(self) -> list[str]:
        return [ln["data"]["line"] for ln in self.lines if ln["type"] == "log"]


def _load_core_or_skip() -> "nexus.NexusCore":
    """Load the real native core, or skip the test if the cdylib isn't built."""
    try:
        return nexus.load_core()
    except nexus.NexusCoreError as exc:  # cdylib not built yet
        raise unittest.SkipTest(f"nexus_core cdylib unavailable: {exc}")


# --------------------------------------------------------------------------- #
# Pure-Python: TOML round-trip + preset-name confinement (no native core).
# --------------------------------------------------------------------------- #
class TestPresetToml(unittest.TestCase):
    def test_route_toml_roundtrip_identity(self):
        # The module's _to_toml emitter and tomllib parser must round-trip a
        # [[route]] preset with no drift in in/out/gain_db.
        doc = {
            "route": [
                {"in": 0, "out": 0, "gain_db": 0.0},
                {"in": 1, "out": 2, "gain_db": -3.5},
                {"in": 3, "out": 1, "gain_db": 12.0},
            ]
        }
        text = nexus._to_toml(doc)
        with tempfile.TemporaryDirectory() as td:
            p = Path(td) / "vocal.toml"
            p.write_text(text, encoding="utf-8")
            back = nexus._read_toml(p)
        self.assertEqual(back["route"], doc["route"])

    def test_toml_roundtrip_preserves_float_and_int_types(self):
        doc = {"route": [{"in": 2, "out": 3, "gain_db": -6.0}]}
        back = nexus._read_toml_from_str(nexus._to_toml(doc))
        r = back["route"][0]
        self.assertIsInstance(r["in"], int)
        self.assertIsInstance(r["out"], int)
        self.assertIsInstance(r["gain_db"], float)
        self.assertEqual(r["gain_db"], -6.0)

    def test_safe_preset_name_accepts_simple_slug(self):
        # The strict allowlist is [A-Za-z0-9._-]; an interior dot is fine.
        for ok in ("vocal", "podcast_a", "Take-3", "mix.v2", "A1_b-2.x"):
            self.assertEqual(nexus._safe_preset_name(ok), ok)

    def test_safe_preset_name_rejects_traversal(self):
        for bad in ("../etc/passwd", "a/b", "..", ".hidden", "x\\y", "", "  "):
            with self.assertRaises(ValueError):
                nexus._safe_preset_name(bad)

    def test_safe_preset_name_rejects_special_chars(self):
        # The tightened allowlist rejects anything outside [A-Za-z0-9._-]:
        # spaces, '=', quotes, newline, NUL, '*', and other glob/shell/TOML
        # metacharacters that the old blocklist let through.
        for bad in (
            "my preset",   # space
            "a=b",          # equals
            'q"x',          # double quote
            "q'x",          # single quote
            "line\nbreak",  # newline
            "nul\x00byte",  # NUL
            "glob*",        # asterisk
            "semi;colon",   # semicolon
            "back`tick",    # backtick
            "dollar$ign",   # dollar
            "perc%ent",     # percent
            "café",         # non-ASCII
        ):
            with self.assertRaises(ValueError):
                nexus._safe_preset_name(bad)

    def test_safe_preset_name_strips_whitespace(self):
        self.assertEqual(nexus._safe_preset_name("  vocal  "), "vocal")


# --------------------------------------------------------------------------- #
# Pure-Python: the telemetry wire shape the daemon will actually relay.
# --------------------------------------------------------------------------- #
class TestTelemetryWireShape(unittest.TestCase):
    def test_telemetry_uses_items_type_not_telemetry(self):
        # daemon/src/apps.rs classify_inbound_line drops any type other than
        # items/status/log; a telemetry drop MUST be "items".
        link = FakeLink()
        link.telemetry("audio.levels", {"ch": []})
        self.assertEqual(link.lines[0]["type"], "items")

    def test_telemetry_flattens_payload_fields_into_data(self):
        # FLAT wire: payload fields sit DIRECTLY in data alongside topic (like Vision
        # + the HUD parsers), NOT under a nested "payload" object — else every HUD
        # field is one level too deep and the panel renders blank.
        link = FakeLink()
        link.telemetry("audio.spectrum", {"bands": [0.0] * 96})
        data = link.lines[0]["data"]
        self.assertEqual(data["topic"], "audio.spectrum")
        self.assertNotIn("payload", data, "telemetry must be flat (no nested payload wrapper)")
        self.assertEqual(data["bands"], [0.0] * 96)

    def test_clipping_payload_is_flat_channel_and_true_peak(self):
        # audio.clipping rides the same FLAT wire as the other nexus topics:
        # {channel, true_peak_dbfs} sit DIRECTLY in data alongside topic (what the
        # HUD's parseNexusClipping reads), NOT under a nested payload wrapper.
        link = FakeLink()
        link.telemetry("audio.clipping", {"channel": 2, "true_peak_dbfs": -0.3})
        data = link.lines[0]["data"]
        self.assertEqual(data["topic"], "audio.clipping")
        self.assertNotIn("payload", data)
        self.assertEqual(data["channel"], 2)
        self.assertEqual(data["true_peak_dbfs"], -0.3)

    def test_every_emitted_line_carries_the_token(self):
        link = FakeLink(token="cap-123")
        link.telemetry("audio.routes", {"revision": 1})
        link.log("hello")
        link.send("status", {"ok": True})
        self.assertTrue(link.lines)
        for ln in link.lines:
            self.assertEqual(ln["token"], "cap-123")

    def test_emitted_lines_are_single_line_json(self):
        link = FakeLink()
        link.telemetry("audio.levels", {"ch": [{"peak_dbfs": -6.0, "rms_dbfs": -9.0}]})
        for raw in link.raw:
            self.assertNotIn("\n", raw)
            json.loads(raw)  # must parse


class TestFakeLinkIsTheRealHostLink(unittest.TestCase):
    """REGRESSION (vacuous test): FakeLink used to be a stand-alone class that
    REIMPLEMENTED HostLink's token stamping and wire framing, so every assertion
    in TestTelemetryWireShape ran against that copy and HostLink.send /
    HostLink.telemetry had ZERO coverage — a mutation making main.py emit
    `type:"telemetry"` with a nested payload and no token at all shipped green.
    Pin that the double delegates to the real methods."""

    def test_fake_link_subclasses_hostlink_without_overriding_the_write_path(self):
        link = FakeLink()
        self.assertIsInstance(link, nexus.HostLink)
        # The methods under test must BE HostLink's, not a copy in this file.
        self.assertIs(type(link).send, nexus.HostLink.send)
        self.assertIs(type(link).telemetry, nexus.HostLink.telemetry)
        self.assertIs(type(link).log, nexus.HostLink.log)

    def test_captured_bytes_come_from_hostlinks_own_sendall(self):
        # Nothing is captured until HostLink.send writes to the socket, and what
        # is captured is exactly the bytes it wrote.
        link = FakeLink(token="cap-xyz")
        self.assertEqual(link.raw, [])
        link.telemetry("audio.routes", {"revision": 7})
        self.assertEqual(len(link._sock.written), 1)
        self.assertEqual(link._sock.written[0].decode("utf-8"), link.raw[0] + "\n")
        self.assertEqual(link.lines[0]["token"], "cap-xyz")


# --------------------------------------------------------------------------- #
# Pure-Python: the audio.levels channel fold (must NEVER emit a null level).
# --------------------------------------------------------------------------- #
class _LevelsStubCore:
    """The two NexusCore methods `_emit_levels` calls. Pure Python, so this test
    runs whether or not the cdylib has been built."""

    def __init__(self, meters: list[tuple[float, float]]) -> None:
        self._meters = list(meters)

    def inputs(self) -> int:
        return len(self._meters)

    def channel_meter(self, c: int) -> tuple[float, float]:
        return self._meters[c]

    def loudness(self) -> tuple[float, float, float]:
        return (float("-inf"), float("-inf"), float("-inf"))


class TestLevelsWireShape(unittest.TestCase):
    """REGRESSION: a SILENT input was dropped from `ch[]`, RENUMBERING every meter
    after it.

    Silence reads -inf from the engine, and every input index at or above the
    device's real input count keeps `ChannelMeter::default()` (= -inf) forever.
    `_emit_levels` folded those through `_finite` -> None; the HUD's
    `coerceChannelLevel` returns null for a non-number `peak_dbfs` and
    `parseNexusLevels` filters that entry OUT of the array — but `ch[]` is
    positional and NexusPanel labels each meter by its ARRAY INDEX. On a 4-input
    matrix with input 0 idle, input 1's meter was drawn under the label "0" and
    the header tag read "3 CH"."""

    @staticmethod
    def _hud_coerce(ch: list) -> list:
        """The HUD's own coercion, transcribed from hud/src/core/events.ts
        (`num` + `coerceChannelLevel` + `parseNexusLevels`): an entry whose
        `peak_dbfs` is not a finite number is DROPPED from the array."""
        out = []
        for entry in ch:
            peak = entry.get("peak_dbfs")
            if not isinstance(peak, (int, float)) or isinstance(peak, bool):
                continue
            if math.isinf(peak) or math.isnan(peak):
                continue
            out.append(peak)
        return out

    def test_silent_channels_keep_their_slot_so_meters_stay_index_aligned(self):
        # 4 inputs; input 0 is an idle/unconnected lane and input 3 is past the
        # device's real input count — both read -inf forever.
        core = _LevelsStubCore([
            (float("-inf"), float("-inf")),   # 0: silent
            (-6.0, -9.0),                     # 1: the mic
            (-12.0, -15.0),                   # 2
            (float("-inf"), float("-inf")),   # 3: unbacked index
        ])
        link = FakeLink()
        nexus._emit_levels(core, link)
        p = link.telemetry_for("audio.levels")[-1]

        self.assertEqual(len(p["ch"]), core.inputs(), "a channel was dropped from the wire")
        json.dumps(p)  # still JSON-safe: no -inf reached the wire

        # THE ACTUAL DEFECT: after the HUD's coercion the array must still be
        # index-aligned, so ch[1] is the mic and nothing shifted up into slot 0.
        coerced = self._hud_coerce(p["ch"])
        self.assertEqual(len(coerced), core.inputs(),
                         "the HUD dropped a channel — every later meter is mislabelled")
        self.assertEqual(coerced[1], -6.0, "input 1's level is no longer at index 1")
        self.assertEqual(coerced[2], -12.0, "input 2's level is no longer at index 2")
        self.assertEqual(coerced[0], nexus.LEVEL_FLOOR_DBFS)
        self.assertEqual(coerced[3], nexus.LEVEL_FLOOR_DBFS)

    def test_level_fold_never_returns_none(self):
        for bad in (float("-inf"), float("inf"), float("nan"), None):
            self.assertEqual(nexus._level_dbfs(bad), nexus.LEVEL_FLOOR_DBFS)
        self.assertEqual(nexus._level_dbfs(-6.0), -6.0)
        self.assertEqual(nexus._level_dbfs(0.0), 0.0)

    def test_the_floor_is_below_the_panel_display_floor(self):
        # If the floor were ever raised into the visible range, a silent channel
        # would draw as a real level instead of an empty meter.
        self.assertLessEqual(nexus.LEVEL_FLOOR_DBFS, -120.0)


# --------------------------------------------------------------------------- #
# Pure-Python: the audio.spectrum band fold (must NEVER emit a null band).
# --------------------------------------------------------------------------- #
class _SpectrumStubCore:
    """The one NexusCore method `_emit_spectrum` calls."""

    def __init__(self, bands: list[float]) -> None:
        self._bands = list(bands)

    def spectrum(self) -> list[float]:
        return list(self._bands)


class TestSpectrumWireShape(unittest.TestCase):
    """REGRESSION: the 96-band strip never rendered a single frame, on any
    hardware, ever. `_emit_spectrum` folded each band through `_finite`, which
    maps -inf to None — and ~24 of the 96 bands are STRUCTURALLY -inf (the log
    fold is finer down low than the FFT's bin spacing; see the nexus_core test
    `log_fold_leaves_bands_with_no_bin_so_minus_inf_is_structural`). The HUD's
    parseNexusSpectrum rejects the WHOLE frame on the first non-number, so one
    null killed every frame."""

    @staticmethod
    def _realistic_bands() -> list[float]:
        # What the engine really produces: mostly real levels, with the
        # structurally-unmapped low bands at -inf.
        bands = [-30.0 - (i % 40) for i in range(96)]
        for i in (0, 1, 3, 4, 5, 6, 7, 8, 9, 10, 12, 13):
            bands[i] = float("-inf")
        return bands

    def test_every_emitted_band_is_a_finite_number(self):
        link = FakeLink()
        nexus._emit_spectrum(_SpectrumStubCore(self._realistic_bands()), link)
        wire = link.lines[0]["data"]["bands"]
        self.assertEqual(len(wire), 96)
        for i, v in enumerate(wire):
            self.assertIsInstance(v, float, f"band {i} is not a number: {v!r}")
            self.assertTrue(math.isfinite(v), f"band {i} is not finite: {v!r}")

    def test_unmeasurable_bands_floor_and_measured_bands_pass_through(self):
        bands = [float("-inf")] * 96
        bands[2] = -77.65314
        bands[95] = float("nan")
        link = FakeLink()
        nexus._emit_spectrum(_SpectrumStubCore(bands), link)
        wire = link.lines[0]["data"]["bands"]
        self.assertEqual(wire[0], nexus.SPECTRUM_FLOOR_DBFS)
        self.assertEqual(wire[95], nexus.SPECTRUM_FLOOR_DBFS)
        self.assertAlmostEqual(wire[2], -77.65314, places=5)

    def test_serialized_line_carries_no_null_infinity_or_nan(self):
        # The ACTUAL bytes on the wire. `json.dumps` emits the non-standard
        # `-Infinity`/`NaN` literals for non-finite floats (which the daemon's
        # serde_json rejects) and `null` for None (which parseNexusSpectrum
        # rejects) — neither may appear.
        link = FakeLink()
        nexus._emit_spectrum(_SpectrumStubCore(self._realistic_bands()), link)
        raw = link.raw[0]
        self.assertNotIn("null", raw)
        self.assertNotIn("Infinity", raw)
        self.assertNotIn("NaN", raw)

    def test_frame_satisfies_the_hud_parser_rule_verbatim(self):
        # hud/src/core/events.ts parseNexusSpectrum: reject unless `bands` is an
        # array of EXACTLY 96 entries, each `typeof v === "number"` AND
        # `Number.isFinite(v)`. `typeof null === "object"`, so a single None is
        # fatal to the frame. Re-parse from the wire text, as the HUD does.
        link = FakeLink()
        nexus._emit_spectrum(_SpectrumStubCore(self._realistic_bands()), link)
        raw = json.loads(link.raw[0])["data"]["bands"]
        accepted = (
            isinstance(raw, list)
            and len(raw) == 96
            and all(
                isinstance(v, (int, float))
                and not isinstance(v, bool)
                and math.isfinite(v)
                for v in raw
            )
        )
        self.assertTrue(accepted, "parseNexusSpectrum would reject this frame")

    def test_spectrum_band_fold_never_returns_none(self):
        for bad in (float("-inf"), float("inf"), float("nan"), None):
            self.assertEqual(nexus._spectrum_band(bad), nexus.SPECTRUM_FLOOR_DBFS)
        self.assertEqual(nexus._spectrum_band(-6.0), -6.0)
        self.assertEqual(nexus._spectrum_band(0.0), 0.0)


# --------------------------------------------------------------------------- #
# Pure-Python: the -inf/NaN -> None JSON-safety fold.
# --------------------------------------------------------------------------- #
class TestFiniteFold(unittest.TestCase):
    def test_finite_maps_non_finite_to_none(self):
        self.assertIsNone(nexus._finite(float("-inf")))
        self.assertIsNone(nexus._finite(float("inf")))
        self.assertIsNone(nexus._finite(float("nan")))
        self.assertIsNone(nexus._finite(None))

    def test_finite_passes_through_real_values(self):
        self.assertEqual(nexus._finite(-6.0), -6.0)
        self.assertEqual(nexus._finite(0.0), 0.0)

    def test_levels_payload_is_json_serializable_for_silence(self):
        # Silence reads -inf from the engine; the fold must make the payload
        # encodable (JSON has no -inf).
        payload = {
            "ch": [{"peak_dbfs": nexus._finite(float("-inf")),
                    "rms_dbfs": nexus._finite(float("-inf"))}],
            "lufs_m": nexus._finite(float("-inf")),
        }
        json.dumps(payload)  # must not raise


# --------------------------------------------------------------------------- #
# Native-core: op dispatch onto the real engine (skipped if cdylib not built).
# --------------------------------------------------------------------------- #
class TestOpDispatch(unittest.TestCase):
    def setUp(self):
        self.core = _load_core_or_skip()
        self.link = FakeLink()
        self.disp = nexus.OpDispatcher(self.core, self.link)
        self.addCleanup(self.core.close)

    def test_route_set_applies_crosspoint_and_emits_routes(self):
        self.disp.dispatch("route.set", {"op": "route.set", "in": 0, "out": 1, "gain_db": -6.0})
        self.assertAlmostEqual(self.core.crosspoint(0, 1), -6.0, places=4)
        routes = self.link.telemetry_for("audio.routes")
        self.assertTrue(routes, "route.set must emit an audio.routes telemetry")
        # Crosspoints ride the WIRE under "matrix" (what the HUD's
        # parseNexusRoutes reads); "route" is only the preset-TOML table name.
        self.assertIn({"in": 0, "out": 1, "gain_db": -6.0}, routes[-1]["matrix"])
        self.assertNotIn("route", routes[-1])

    def test_route_set_clears_with_minus_inf(self):
        self.disp.dispatch("route.set", {"in": 0, "out": 0, "gain_db": -3.0})
        self.assertAlmostEqual(self.core.crosspoint(0, 0), -3.0, places=4)
        self.disp.dispatch("route.set", {"in": 0, "out": 0, "gain_db": "off"})
        self.assertEqual(self.core.crosspoint(0, 0), float("-inf"))

    def test_route_set_missing_gain_clears(self):
        self.disp.dispatch("route.set", {"in": 1, "out": 1, "gain_db": 0.0})
        self.disp.dispatch("route.set", {"in": 1, "out": 1})  # no gain_db -> -inf
        self.assertEqual(self.core.crosspoint(1, 1), float("-inf"))

    def test_gain_set_input_trim_and_gain_telemetry(self):
        self.disp.dispatch("gain.set", {"channel": 0, "gain_db": -2.0, "stage": "input"})
        gains = self.link.telemetry_for("audio.gain")
        self.assertTrue(gains)
        self.assertEqual(gains[-1], {"channel": 0, "gain_db": -2.0, "stage": "input"})

    def test_gain_set_output_stage(self):
        self.disp.dispatch("gain.set", {"channel": 1, "gain_db": 1.5, "stage": "output"})
        gains = self.link.telemetry_for("audio.gain")
        self.assertEqual(gains[-1]["stage"], "output")

    def test_gain_set_mute_the_mic(self):
        # "mute the mic" -> gain.set with mute:true on an input. Must mutate the
        # matrix (revision bumps) and not raise.
        r0 = self.core.matrix_revision()
        self.disp.dispatch("gain.set", {"channel": 0, "mute": True})
        self.assertGreater(self.core.matrix_revision(), r0)
        # The mute rides a DISTINCT audio.gain payload the HUD accepts:
        # {channel, muted, stage} — never a gain_db=null frame (parseNexusGain
        # rejected those wholesale, so mutes were invisible on the panel).
        gains = self.link.telemetry_for("audio.gain")
        self.assertTrue(gains, "a mute must emit an audio.gain telemetry")
        self.assertEqual(gains[-1], {"channel": 0, "muted": True, "stage": "input"})
        self.assertNotIn("gain_db", gains[-1])

    def test_gain_set_unmute_emits_muted_false(self):
        self.disp.dispatch("gain.set", {"channel": 0, "mute": True})
        self.disp.dispatch("gain.set", {"channel": 0, "mute": False})
        gains = self.link.telemetry_for("audio.gain")
        self.assertEqual(gains[-1], {"channel": 0, "muted": False, "stage": "input"})

    def test_monitor_set_assigns_and_routes(self):
        self.disp.dispatch("monitor.set", {"in": 2, "out": 0, "on": True})
        # The direct monitor route is opened at unity.
        self.assertAlmostEqual(self.core.crosspoint(2, 0), 0.0, places=4)
        self.assertTrue(self.link.telemetry_for("audio.routes"))

    def test_monitor_set_off_clears_route(self):
        self.disp.dispatch("monitor.set", {"in": 2, "out": 0, "on": True})
        self.disp.dispatch("monitor.set", {"in": 2, "out": 0, "on": False})
        self.assertEqual(self.core.crosspoint(2, 0), float("-inf"))

    def test_monitor_measure_reports_null_rtt(self):
        # Device-gated: must report measured_rtt_ms=None, never fabricate.
        self.disp.dispatch("monitor.measure", {})
        routes = self.link.telemetry_for("audio.routes")
        self.assertTrue(routes)
        self.assertIsNone(routes[-1]["measured_rtt_ms"])

    def test_state_get_emits_full_snapshot(self):
        self.disp.dispatch("route.set", {"in": 0, "out": 0, "gain_db": -1.0})
        self.link.clear()
        self.disp.dispatch("state.get", {})
        routes = self.link.telemetry_for("audio.routes")
        self.assertTrue(routes)
        snap = routes[-1]
        self.assertEqual(snap["inputs"], self.core.inputs())
        self.assertEqual(snap["outputs"], self.core.outputs())
        self.assertIn("revision", snap)
        self.assertIsNone(snap["measured_rtt_ms"])

    def test_unknown_op_logs_and_does_not_crash(self):
        self.disp.dispatch("frobnicate", {"op": "frobnicate"})
        self.assertTrue(any("unknown op" in l for l in self.link.logs()))

    def test_emit_routes_payload_is_json_safe(self):
        self.disp.dispatch("route.set", {"in": 0, "out": 0, "gain_db": 0.0})
        for ln in self.link.lines:
            json.dumps(ln)  # the full wire line must serialize


# --------------------------------------------------------------------------- #
# Native-core: gain / index clamping (bad ops rejected, dispatcher survives).
# --------------------------------------------------------------------------- #
class TestClamping(unittest.TestCase):
    def setUp(self):
        self.core = _load_core_or_skip()
        self.link = FakeLink()
        self.disp = nexus.OpDispatcher(self.core, self.link)
        self.addCleanup(self.core.close)

    def test_crosspoint_above_max_gain_is_rejected(self):
        # +12 dB is the ceiling; +13 must be rejected by the core.
        with self.assertRaises(nexus.NexusCoreError):
            self.core.set_crosspoint(0, 0, 13.0)

    def test_crosspoint_at_max_gain_ok(self):
        self.core.set_crosspoint(0, 0, 12.0)
        self.assertAlmostEqual(self.core.crosspoint(0, 0), 12.0, places=4)

    def test_out_of_range_index_rejected(self):
        with self.assertRaises(nexus.NexusCoreError):
            self.core.set_crosspoint(99, 0, 0.0)

    def test_dispatch_swallows_bad_route_op(self):
        # A route.set with an out-of-range index must log, not raise/crash.
        self.disp.dispatch("route.set", {"in": 99, "out": 0, "gain_db": 0.0})
        self.assertTrue(any("route.set failed" in l for l in self.link.logs()))

    def test_dispatch_swallows_nan_gain(self):
        # NaN is neither the -inf sentinel nor finite<=+12: the core rejects it
        # and the dispatcher logs rather than dying.
        self.disp.dispatch("route.set", {"in": 0, "out": 0, "gain_db": float("nan")})
        self.assertTrue(any("route.set failed" in l for l in self.link.logs()))

    def test_input_trim_nan_rejected(self):
        with self.assertRaises(nexus.NexusCoreError):
            self.core.set_input_trim(0, float("nan"))


# --------------------------------------------------------------------------- #
# Native-core: preset save (scratch) + load (presets) round-trip through ops.
# --------------------------------------------------------------------------- #
class TestPresetOps(unittest.TestCase):
    def setUp(self):
        self.core = _load_core_or_skip()
        self.link = FakeLink()
        self.disp = nexus.OpDispatcher(self.core, self.link)
        self.addCleanup(self.core.close)

    def test_preset_save_writes_scratch_and_load_replays(self):
        # Set a couple of routes, save them, clear, then load the saved file back
        # and assert the matrix is restored. Save goes to SCRATCH (fs_write);
        # load reads PRESETS (fs_read), so we redirect both at the module dirs
        # for a hermetic round-trip.
        self.core.set_crosspoint(0, 0, -3.0)
        self.core.set_crosspoint(1, 2, -6.0)

        with tempfile.TemporaryDirectory() as td:
            scratch = Path(td) / "scratch"
            presets = Path(td) / "presets"
            scratch.mkdir()
            presets.mkdir()
            orig_scratch, orig_presets = nexus.SCRATCH_DIR, nexus.PRESETS_DIR
            nexus.SCRATCH_DIR = scratch
            nexus.PRESETS_DIR = presets
            try:
                self.disp.dispatch("preset.save", {"name": "vocal"})
                saved = scratch / "vocal.toml"
                self.assertTrue(saved.exists(), "save must write scratch/<name>.toml")

                # Promote the saved file into presets/ (the manual curation step
                # the app documents: presets/ is read-only to the app) and clear
                # the matrix.
                (presets / "vocal.toml").write_text(saved.read_text(), encoding="utf-8")
                self.core.set_crosspoint(0, 0, float("-inf"))
                self.core.set_crosspoint(1, 2, float("-inf"))
                self.assertEqual(self.core.crosspoint(0, 0), float("-inf"))

                self.disp.dispatch("preset.load", {"name": "vocal"})
            finally:
                nexus.SCRATCH_DIR, nexus.PRESETS_DIR = orig_scratch, orig_presets

        # The loaded preset restored both crosspoints identically.
        self.assertAlmostEqual(self.core.crosspoint(0, 0), -3.0, places=4)
        self.assertAlmostEqual(self.core.crosspoint(1, 2), -6.0, places=4)

    def test_preset_load_missing_logs_not_crashes(self):
        with tempfile.TemporaryDirectory() as td:
            orig = nexus.PRESETS_DIR
            nexus.PRESETS_DIR = Path(td)
            try:
                self.disp.dispatch("preset.load", {"name": "nope"})
            finally:
                nexus.PRESETS_DIR = orig
        self.assertTrue(any("preset.load failed" in l for l in self.link.logs()))

    def test_preset_load_rejects_traversal_name(self):
        # A traversal preset name must be rejected before any filesystem touch.
        self.disp.dispatch("preset.load", {"name": "../../etc/hosts"})
        self.assertTrue(any("preset.load failed" in l for l in self.link.logs()))

    def test_preset_save_confines_to_scratch(self):
        with tempfile.TemporaryDirectory() as td:
            scratch = Path(td) / "scratch"
            scratch.mkdir()
            orig = nexus.SCRATCH_DIR
            nexus.SCRATCH_DIR = scratch
            try:
                self.disp.dispatch("preset.save", {"name": "../escape"})
            finally:
                nexus.SCRATCH_DIR = orig
            # Nothing was written outside the scratch dir.
            self.assertEqual(list(scratch.glob("*.toml")), [])
            self.assertFalse((Path(td) / "escape.toml").exists())
        self.assertTrue(any("preset.save failed" in l for l in self.link.logs()))


# --------------------------------------------------------------------------- #
# Native-core: telemetry fold payload shapes match the SPEC §6 topics.
# --------------------------------------------------------------------------- #
class TestTelemetryFold(unittest.TestCase):
    def setUp(self):
        self.core = _load_core_or_skip()
        self.link = FakeLink()
        self.addCleanup(self.core.close)

    def test_levels_payload_shape(self):
        nexus._emit_levels(self.core, self.link)
        payloads = self.link.telemetry_for("audio.levels")
        self.assertTrue(payloads)
        p = payloads[-1]
        # {ch: [{peak_dbfs, rms_dbfs}], lufs_m, lufs_s, lufs_i}
        self.assertEqual(len(p["ch"]), self.core.inputs())
        for entry in p["ch"]:
            self.assertIn("peak_dbfs", entry)
            self.assertIn("rms_dbfs", entry)
        for k in ("lufs_m", "lufs_s", "lufs_i"):
            self.assertIn(k, p)
        json.dumps(p)  # JSON-safe (silence folded to None)

    def test_spectrum_payload_is_96_bands(self):
        nexus._emit_spectrum(self.core, self.link)
        payloads = self.link.telemetry_for("audio.spectrum")
        self.assertTrue(payloads)
        bands = payloads[-1]["bands"]
        self.assertEqual(len(bands), 96)
        json.dumps(payloads[-1])

    def test_routes_payload_shape(self):
        disp = nexus.OpDispatcher(self.core, self.link)
        disp.emit_routes()
        payloads = self.link.telemetry_for("audio.routes")
        self.assertTrue(payloads)
        p = payloads[-1]
        for k in ("matrix", "inputs", "outputs", "revision", "measured_rtt_ms"):
            self.assertIn(k, p)
        self.assertIsInstance(p["matrix"], list)
        self.assertNotIn("route", p, "crosspoints must ride the HUD's 'matrix' wire key")
        self.assertIsNone(p["measured_rtt_ms"])


# --------------------------------------------------------------------------- #
# Native-core: the ctypes <-> cdylib contract (mirror of the --selftest).
# --------------------------------------------------------------------------- #
class TestCoreContract(unittest.TestCase):
    def setUp(self):
        self.core = _load_core_or_skip()
        self.addCleanup(self.core.close)

    def test_geometry_matches_defaults(self):
        self.assertEqual(self.core.inputs(), nexus.DEFAULT_INPUTS)
        self.assertEqual(self.core.outputs(), nexus.DEFAULT_OUTPUTS)

    def test_crosspoint_set_get_clear(self):
        self.core.set_crosspoint(0, 0, -3.0)
        self.assertAlmostEqual(self.core.crosspoint(0, 0), -3.0, places=4)
        self.core.set_crosspoint(0, 0, float("-inf"))
        self.assertEqual(self.core.crosspoint(0, 0), float("-inf"))

    def test_spectrum_band_count(self):
        self.assertEqual(len(self.core.spectrum()), 96)

    def test_loudness_returns_triplet(self):
        m, s, i = self.core.loudness()
        self.assertTrue(all(isinstance(x, float) for x in (m, s, i)))

    def test_clip_drain_reports_once_then_clears(self):
        # Fresh core: nothing has clipped, so the edge-triggered accumulator
        # drains EMPTY (the ctypes out-array binding round-trips a zero count).
        self.assertEqual(self.core.drain_clips(), [])
        # Drive ONE full-scale block through the realtime path: 0 dBFS sits above
        # the -1 dBFS true-peak ceiling, so the detector fires on channel 0.
        self.core.set_crosspoint(0, 0, 0.0)
        self.core.set_monitor_output(0)
        self.core.process_block([[1.0] * nexus.DEFAULT_BLOCK_FRAMES], out_channels=1)
        clips = self.core.drain_clips()
        self.assertTrue(clips, "a full-scale block must enqueue a clip event")
        self.assertEqual(clips[0][0], 0, "event must carry the input channel index")
        self.assertGreaterEqual(clips[0][1], -1.0, "true-peak must meet the -1 dBFS ceiling")
        # Edge-triggered: each event is delivered exactly once.
        self.assertEqual(self.core.drain_clips(), [])


if __name__ == "__main__":
    unittest.main()
