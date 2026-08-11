import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import EgressBeaconPanel from "../components/EgressBeaconPanel";
import { parseEgressBeacon, EGRESS_BEACON_CAP } from "../core/events";
import type { EgressBeaconAlert, TelemetryEnvelope } from "../core/events";
import { initialState, reduce } from "../core/state";
import type { HudState } from "../core/state";

/* helpers ------------------------------------------------------------------ */
let counter = 0;
function env(
  event: string,
  data: Record<string, unknown> = {},
  source = "egress",
): TelemetryEnvelope {
  counter += 1;
  return {
    ts: `2026-08-11T12:00:${String(counter % 60).padStart(2, "0")}Z`,
    source,
    event,
    data,
  };
}
function tel(state: HudState, e: TelemetryEnvelope, at = 1000): HudState {
  return reduce(state, { type: "telemetry", envelope: e, at });
}
function connected(at = 0): HudState {
  return reduce(initialState(), { type: "ws.connected", at });
}

/** The exact shape daemon/src/egress_beacon.rs::beacon_frame puts on the wire. */
function beaconPayload(overrides: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    process: "implant",
    host: "203.0.113.7",
    port: 443,
    period_secs: 60.0,
    jitter_ratio: 0.012,
    samples: 6,
    proposal: "# DARWIN egress proposal — PROPOSE-ONLY...\nblock drop out quick proto tcp",
    caveat: "unprivileged lsof...",
    ...overrides,
  };
}

/* the defensive parser ------------------------------------------------------ */
describe("parseEgressBeacon (defensive)", () => {
  it("reads a beacon_frame into a keyed row", () => {
    const a = parseEgressBeacon(beaconPayload());
    expect(a).not.toBeNull();
    expect(a!.key).toBe("implant → 203.0.113.7");
    expect(a!.line).toBe("BEACON: implant → 203.0.113.7:443 every ~60s (jitter 0.012, 6 edges)");
    expect(a!.proposal).toContain("block drop out quick proto tcp");
  });

  it("drops a frame without an attributable process + host", () => {
    expect(parseEgressBeacon({})).toBeNull();
    expect(parseEgressBeacon(beaconPayload({ process: "" }))).toBeNull();
    expect(parseEgressBeacon(beaconPayload({ host: 7 }))).toBeNull();
  });

  it("renders missing numbers as honest zeroes and never throws", () => {
    const a = parseEgressBeacon({ process: "p", host: "h" });
    expect(a).not.toBeNull();
    expect(a!.line).toBe("BEACON: p → h:0 every ~0s (jitter 0, 0 edges)");
    expect(a!.proposal).toBe("");
  });
});

/* the reducer arm ------------------------------------------------------------ */
describe("egress.beacon reducer", () => {
  it("accumulates beacons newest-first", () => {
    let s = tel(connected(), env("egress.beacon", beaconPayload()));
    s = tel(s, env("egress.beacon", beaconPayload({ process: "poller", host: "198.51.100.9" })));
    expect(s.egressBeacons.map((b) => b.key)).toEqual([
      "poller → 198.51.100.9",
      "implant → 203.0.113.7",
    ]);
  });

  it("REFRESHES the row for a re-alerted talker instead of stacking near-duplicates", () => {
    // The daemon re-alerts the same key after its 6h cooldown with a freshly
    // measured period; two rows for one talker would read as two beacons.
    let s = tel(connected(), env("egress.beacon", beaconPayload()));
    s = tel(s, env("egress.beacon", beaconPayload({ period_secs: 61.2, samples: 8 })));
    expect(s.egressBeacons).toHaveLength(1);
    expect(s.egressBeacons[0].line).toContain("~61s");
    expect(s.egressBeacons[0].line).toContain("8 edges");
  });

  it("caps the accumulated rows", () => {
    let s = connected();
    for (let i = 0; i < EGRESS_BEACON_CAP + 5; i++) {
      s = tel(s, env("egress.beacon", beaconPayload({ host: `10.0.0.${i}` })));
    }
    expect(s.egressBeacons.length).toBe(EGRESS_BEACON_CAP);
    // Newest survive the cap.
    expect(s.egressBeacons[0].key).toBe(`implant → 10.0.0.${EGRESS_BEACON_CAP + 4}`);
  });

  it("ignores a malformed frame without state churn", () => {
    const before = tel(connected(), env("egress.beacon", beaconPayload()));
    const after = tel(before, env("egress.beacon", { period_secs: 60 }));
    expect(after).toBe(before);
  });
});

/* the panel (headless) -------------------------------------------------------- */
describe("EgressBeaconPanel (propose-only)", () => {
  const render = (beacons: EgressBeaconAlert[]) =>
    renderToStaticMarkup(createElement(EgressBeaconPanel, { beacons }));

  it("renders nothing while no beacon has been flagged", () => {
    expect(render([])).toBe("");
  });

  it("shows the beacon line, carries the pf proposal, and states the posture", () => {
    const html = render([
      {
        key: "implant → 203.0.113.7",
        line: "BEACON: implant → 203.0.113.7:443 every ~60s (jitter 0.012, 6 edges)",
        proposal: "# PROPOSE-ONLY\nblock drop out quick proto tcp from any to 203.0.113.7 port 443",
      },
    ]);
    expect(html).toContain("implant");
    expect(html).toContain("203.0.113.7");
    expect(html).toContain("PROPOSE ONLY");
    // The pf rule is present (the row's tooltip), and the note says the owner
    // applies it themselves — the panel never claims DARWIN will.
    expect(html).toContain("block drop out quick proto tcp");
    expect(html).toContain("yourself with sudo");
  });
});
