import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import PostureDashboardPanel from "../components/PostureDashboardPanel";
import {
  parsePostureSnapshot,
  POSTURE_SCANNER_CAP,
  type TelemetryEnvelope,
} from "../core/events";
import { initialState, reduce } from "../core/state";

let counter = 0;
function env(event: string, data: Record<string, unknown>, source = "system"): TelemetryEnvelope {
  counter += 1;
  return { ts: `2026-07-13T00:00:${String(counter % 60).padStart(2, "0")}Z`, source, event, data };
}
function connected() {
  return reduce(initialState(), { type: "ws.connected", at: 0 });
}

const protectedWire = {
  filevault: "on",
  firewall: "on",
  sip: "on",
  updates: "up_to_date",
  updates_pending: 0,
  checked_ts: "2026-07-13T10:30:00Z",
};

describe("parsePostureSnapshot (never fabricates protection)", () => {
  it("parses the daemon's exact wire tokens", () => {
    expect(parsePostureSnapshot(protectedWire)).toEqual({
      filevault: "on",
      firewall: "on",
      sip: "on",
      updates: "up_to_date",
      updatesPending: 0,
      checkedTs: "2026-07-13T10:30:00Z",
      scanners: [],
    });
    expect(
      parsePostureSnapshot({
        filevault: "off",
        firewall: "unreadable",
        sip: "unclear",
        updates: "pending",
        updates_pending: 3,
      }),
    ).toEqual({
      filevault: "off",
      firewall: "unreadable",
      sip: "unclear",
      updates: "pending",
      updatesPending: 3,
      checkedTs: "",
      scanners: [],
    });
  });

  it("coerces anything unknown to the honest can't-confirm — never 'on'", () => {
    const p = parsePostureSnapshot({
      filevault: "ON", // wrong case is not a match
      firewall: 1,
      sip: "enabled", // not a wire token
      updates: "fine",
      updates_pending: -4,
    });
    expect(p).toEqual({
      filevault: "unclear",
      firewall: "unclear",
      sip: "unclear",
      updates: "unclear",
      updatesPending: 0,
      checkedTs: "",
      scanners: [],
    });
    // Empty frame: everything unclear, nothing green.
    expect(parsePostureSnapshot({})).toEqual({
      filevault: "unclear",
      firewall: "unclear",
      sip: "unclear",
      updates: "unclear",
      updatesPending: 0,
      checkedTs: "",
      scanners: [],
    });
  });
});

describe("posture.snapshot reducer", () => {
  it("is null until the first frame, then populated", () => {
    const s0 = connected();
    expect(s0.posture).toBeNull();
    const s1 = reduce(s0, {
      type: "telemetry",
      envelope: env("posture.snapshot", protectedWire),
      at: 1000,
    });
    expect(s1.posture).not.toBeNull();
    expect(s1.posture?.filevault).toBe("on");
    expect(s1.posture?.updates).toBe("up_to_date");
  });
});

describe("PostureDashboardPanel", () => {
  const render = (posture: Parameters<typeof PostureDashboardPanel>[0]["posture"]) =>
    renderToStaticMarkup(createElement(PostureDashboardPanel, { posture }));

  it("renders nothing before the first frame", () => {
    expect(render(null)).toBe("");
  });

  it("shows a fully protected board with green pills", () => {
    const html = render({
      filevault: "on",
      firewall: "on",
      sip: "on",
      updates: "up_to_date",
      updatesPending: 0,
      checkedTs: "2026-07-13T10:30:00Z",
      scanners: [],
    });
    expect(html).toContain("FileVault");
    expect(html).toContain("Application firewall");
    expect(html).toContain("System Integrity Protection");
    expect(html).toContain("UP TO DATE");
    expect(html).toContain("protected");
    // The read-only honesty note is always present, with the honest data-age
    // stamp (the daemon re-broadcasts a cached snapshot between scans).
    expect(html).toContain("yours to do in System Settings");
    expect(html).toContain("Checked ");
  });

  it("shows exposure and honest can't-confirm distinctly", () => {
    const html = render({
      filevault: "off",
      firewall: "unreadable",
      sip: "unclear",
      updates: "pending",
      updatesPending: 3,
      checkedTs: "",
      scanners: [],
    });
    expect(html).toContain("OFF");
    expect(html).toContain("exposed");
    expect(html).toContain("UNREADABLE");
    expect(html).toContain("UNCLEAR");
    expect(html).toContain("3 PENDING");
    // Nothing on this board is green.
    expect(html).not.toContain("protected");
  });
});

/**
 * THE AMBIENT SCANNERS' summaries (persistence / inbound exposure / traffic
 * interception).
 *
 * Each of those three scanners emits a full finding frame — `security.persistence`,
 * `security.exposure`, `security.interception` — that `applyEnvelope` has NO case
 * for, so every finding fell through its exact-match default and reached no pixel.
 * Their summaries reached the owner only through the SPOKEN posture report, i.e.
 * only if the owner thought to ask. The daemon now folds those one-liners onto the
 * frame this board already renders (posture.rs::scanner_notes).
 */
describe("posture.snapshot carries the ambient scanners' summaries", () => {
  const exposureLine =
    "Inbound exposure: 12 listening socket(s) — 9 loopback-only, 3 exposed to the network " +
    "(Screen Sharing:5900) — read-only";
  const interceptionLine =
    "Interception check: 1 NON-APPLE trusted root CA — this silently breaks ALL TLS";

  it("parses the daemon's lines and hands them to the board", () => {
    const snap = parsePostureSnapshot({ ...protectedWire, scanners: [exposureLine] });
    expect(snap.scanners).toEqual([exposureLine]);
    const html = renderToStaticMarkup(
      createElement(PostureDashboardPanel, { posture: snap }),
    );
    expect(html).toContain("AMBIENT SCANNERS");
    expect(html).toContain("3 exposed to the network");
  });

  it("shows an interception finding the owner never had to ask for", () => {
    const snap = parsePostureSnapshot({ ...protectedWire, scanners: [interceptionLine] });
    const html = renderToStaticMarkup(
      createElement(PostureDashboardPanel, { posture: snap }),
    );
    expect(html).toContain("NON-APPLE trusted root CA");
  });

  it("shows NOTHING rather than a fabricated all-quiet when nothing has scanned", () => {
    // The daemon OMITS the key entirely before any scanner ticks. A malformed or
    // non-string payload degrades the same way — never to a reassuring block.
    for (const wire of [
      protectedWire,
      { ...protectedWire, scanners: [] },
      { ...protectedWire, scanners: "not-an-array" },
      { ...protectedWire, scanners: [1, null, "   "] },
    ]) {
      const snap = parsePostureSnapshot(wire);
      expect(snap.scanners).toEqual([]);
      const html = renderToStaticMarkup(
        createElement(PostureDashboardPanel, { posture: snap }),
      );
      expect(html).not.toContain("AMBIENT SCANNERS");
      // PRECONDITION: this really is the rendered board, so "not present" is
      // proving the block is absent and not that nothing rendered at all.
      expect(html).toContain("FileVault");
    }
  });

  it("bounds what a producer can put on the board", () => {
    const many = Array.from({ length: 40 }, (_, i) => `scanner-line-${i}`);
    expect(parsePostureSnapshot({ ...protectedWire, scanners: many }).scanners).toHaveLength(
      POSTURE_SCANNER_CAP,
    );
    const long = "x".repeat(4000);
    const [line] = parsePostureSnapshot({ ...protectedWire, scanners: [long] }).scanners;
    expect(line.length).toBeLessThanOrEqual(401);
  });

  it("does not date the scanner lines with the machine checks' timestamp", () => {
    // `checkedTs` belongs to FileVault/firewall/SIP/updates (30-min cadence). The
    // scanner lines ride their own ~5-min cadence and are attached at emit time,
    // so the board must not print "Checked HH:MM" inside the scanner block.
    const snap = parsePostureSnapshot({ ...protectedWire, scanners: [exposureLine] });
    const html = renderToStaticMarkup(
      createElement(PostureDashboardPanel, { posture: snap }),
    );
    const block = html.slice(html.indexOf("AMBIENT SCANNERS"), html.indexOf("posture-foot"));
    expect(block).toContain("its own cadence");
    expect(block).not.toContain("Checked ");
    // PRECONDITION: the stamp IS rendered on the board, just not in this block.
    expect(html).toContain("Checked ");
  });
});
