import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import AperturePanel from "../components/AperturePanel";
import {
  parseApertureStatus,
  formatApertureDuration,
  APERTURE_PREVIEW_CAP,
  type ApertureStatus,
  type TelemetryEnvelope,
} from "../core/events";
import { initialState, reduce } from "../core/state";

let counter = 0;
function env(event: string, data: Record<string, unknown>, source = "aperture"): TelemetryEnvelope {
  counter += 1;
  return { ts: `2026-07-15T00:00:${String(counter % 60).padStart(2, "0")}Z`, source, event, data };
}
function connected() {
  return reduce(initialState(), { type: "ws.connected", at: 0 });
}

describe("formatApertureDuration (matches the daemon buckets)", () => {
  it("buckets seconds into a human span", () => {
    expect(formatApertureDuration(0)).toBe("under a minute");
    expect(formatApertureDuration(59)).toBe("under a minute");
    expect(formatApertureDuration(60)).toBe("1m");
    expect(formatApertureDuration(150)).toBe("2m");
    expect(formatApertureDuration(3600)).toBe("1h");
    expect(formatApertureDuration(3660)).toBe("1h 1m");
    expect(formatApertureDuration(2 * 3600 + 5 * 60)).toBe("2h 5m");
  });

  it("coerces a garbled duration to under a minute (never NaN)", () => {
    expect(formatApertureDuration(-5)).toBe("under a minute");
    expect(formatApertureDuration(Number.NaN)).toBe("under a minute");
  });
});

describe("parseApertureStatus (never invents a recorded state)", () => {
  it("parses a well-formed enabled payload with redacted activities", () => {
    const p = parseApertureStatus({
      enabled: true,
      count: 2,
      cap: 500,
      poll_interval_secs: 20,
      recent: [
        { app: "Xcode", title: "aperture.rs", duration_secs: 3600 },
        { app: "Mail", title: "Re: [redacted] about the lease", duration_secs: 900 },
      ],
    });
    expect(p).toEqual({
      enabled: true,
      count: 2,
      cap: 500,
      pollIntervalSecs: 20,
      recent: [
        { app: "Xcode", title: "aperture.rs", durationSecs: 3600 },
        { app: "Mail", title: "Re: [redacted] about the lease", durationSecs: 900 },
      ],
    });
  });

  it("coerces an absent/garbled payload to the honest OFF, empty snapshot", () => {
    expect(parseApertureStatus({})).toEqual({
      enabled: false,
      count: 0,
      cap: 0,
      pollIntervalSecs: 0,
      recent: [],
    });
    // A garbled recent (not an array) degrades to no activities, never throws.
    expect(parseApertureStatus({ enabled: true, recent: 5 }).recent).toEqual([]);
  });

  it("drops rows with no app name and defaults a missing title/duration", () => {
    const p = parseApertureStatus({
      enabled: true,
      count: 3,
      cap: 10,
      poll_interval_secs: 20,
      recent: [
        { app: "Safari", duration_secs: 120 }, // no title -> ""
        { title: "orphan title", duration_secs: 50 }, // no app -> dropped
        { app: "Notes", title: "todo", duration_secs: -9 }, // negative -> 0
      ],
    });
    expect(p.recent).toEqual([
      { app: "Safari", title: "", durationSecs: 120 },
      { app: "Notes", title: "todo", durationSecs: 0 },
    ]);
  });

  it("drops activities when OFF (a disabled timeline shows nothing recorded)", () => {
    const p = parseApertureStatus({
      enabled: false,
      count: 2,
      cap: 10,
      poll_interval_secs: 20,
      recent: [{ app: "Safari", title: "leaked", duration_secs: 10 }],
    });
    expect(p.enabled).toBe(false);
    expect(p.recent).toEqual([]);
  });

  it("clamps counts and caps the activity list", () => {
    const many = Array.from({ length: APERTURE_PREVIEW_CAP + 5 }, (_, i) => ({
      app: `App${i}`,
      title: `w${i}`,
      duration_secs: i,
    }));
    const p = parseApertureStatus({
      enabled: true,
      count: -4,
      cap: 12.9,
      poll_interval_secs: -1,
      recent: many,
    });
    expect(p.count).toBe(0);
    expect(p.cap).toBe(12);
    expect(p.pollIntervalSecs).toBe(0);
    expect(p.recent).toHaveLength(APERTURE_PREVIEW_CAP);
  });
});

describe("reduce (aperture.status)", () => {
  it("is null until the first status frame", () => {
    expect(connected().aperture).toBeNull();
  });

  it("stores the parsed status on an aperture.status frame", () => {
    const s = reduce(connected(), {
      type: "telemetry",
      envelope: env("aperture.status", {
        enabled: true,
        count: 1,
        cap: 500,
        poll_interval_secs: 20,
        recent: [{ app: "Xcode", title: "aperture.rs", duration_secs: 1800 }],
      }),
      at: 1,
    });
    expect(s.aperture).not.toBeNull();
    expect(s.aperture?.enabled).toBe(true);
    expect(s.aperture?.count).toBe(1);
    expect(s.aperture?.recent).toEqual([{ app: "Xcode", title: "aperture.rs", durationSecs: 1800 }]);
  });

  it("reflects a later OFF frame (disable wipes the activities)", () => {
    let s = reduce(connected(), {
      type: "telemetry",
      envelope: env("aperture.status", {
        enabled: true,
        count: 1,
        cap: 10,
        recent: [{ app: "Safari", title: "x", duration_secs: 30 }],
      }),
      at: 1,
    });
    expect(s.aperture?.recent).toHaveLength(1);
    s = reduce(s, {
      type: "telemetry",
      envelope: env("aperture.status", { enabled: false, count: 0, cap: 10, recent: [] }),
      at: 2,
    });
    expect(s.aperture?.enabled).toBe(false);
    expect(s.aperture?.recent).toEqual([]);
  });
});

describe("AperturePanel", () => {
  function render(aperture: ApertureStatus | null): string {
    return renderToStaticMarkup(createElement(AperturePanel, { aperture }));
  }

  it("renders nothing until a status arrives", () => {
    expect(render(null)).toBe("");
  });

  it("renders the OFF state with an opt-in note and no activities", () => {
    const html = render({ enabled: false, count: 0, cap: 500, pollIntervalSecs: 20, recent: [] });
    expect(html).toContain("OFF");
    expect(html.toLowerCase()).toContain("opt-in");
    expect(html).not.toContain("<li");
  });

  it("renders the redacted activities with bucketed durations when recording", () => {
    const html = render({
      enabled: true,
      count: 2,
      cap: 500,
      pollIntervalSecs: 20,
      recent: [
        { app: "Xcode", title: "aperture.rs", durationSecs: 3600 },
        { app: "Mail", title: "Re: [redacted] about the lease", durationSecs: 900 },
      ],
    });
    expect(html).toContain("RECORDING");
    expect(html).toContain("Xcode");
    expect(html).toContain("aperture.rs");
    expect(html).toContain("1h");
    expect(html).toContain("[redacted]");
    expect(html).toContain("2 / 500 activities");
    // Honest about coverage: app + title + time, never pixels.
    expect(html.toLowerCase()).toContain("never screen pixels");
  });

  it("shows an honest empty note when enabled but nothing recorded yet", () => {
    const html = render({ enabled: true, count: 0, cap: 500, pollIntervalSecs: 20, recent: [] });
    expect(html).toContain("RECORDING");
    expect(html.toLowerCase()).toContain("nothing recorded yet");
  });
});
