import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import CapabilityStatusPanel from "../components/CapabilityStatusPanel";
import {
  parseCapabilityMap,
  type CapabilityMap,
  type TelemetryEnvelope,
} from "../core/events";
import { initialState, reduce } from "../core/state";

let counter = 0;
function env(event: string, data: Record<string, unknown>, source = "system"): TelemetryEnvelope {
  counter += 1;
  return {
    ts: `2026-07-12T00:00:${String(counter % 60).padStart(2, "0")}Z`,
    source,
    event,
    data,
  };
}
function connected(at = 0) {
  return reduce(initialState(), { type: "ws.connected", at });
}
function tel(state: ReturnType<typeof connected>, e: TelemetryEnvelope, at = 1000) {
  return reduce(state, { type: "telemetry", envelope: e, at });
}

const sample: Record<string, unknown> = {
  capabilities: [
    { key: "cloud_reasoning", label: "Cloud reasoning", armed: true, status: "ready", dependency: "", verified: true },
    { key: "shell_run", label: "Shell", armed: true, status: "armed_needs_dependency", dependency: "/usr/bin/sandbox-exec + /bin/sh", verified: true },
    { key: "elevenlabs_voice", label: "ElevenLabs", armed: true, status: "armed_needs_dependency", dependency: "elevenlabs_api_key in Keychain", verified: false },
    { key: "voice_id", label: "Voice-id", armed: false, status: "off", dependency: "", verified: true },
  ],
};

describe("parseCapabilityMap (defensive, never over-claims)", () => {
  it("coerces a well-formed payload into typed rows", () => {
    const m = parseCapabilityMap(sample);
    expect(m.capabilities).toHaveLength(4);
    expect(m.capabilities[0]).toEqual({
      key: "cloud_reasoning",
      label: "Cloud reasoning",
      armed: true,
      status: "ready",
      dependency: "",
      verified: true,
    });
  });

  it("drops rows with no key and coerces an unknown status to off (never ready)", () => {
    const m = parseCapabilityMap({
      capabilities: [
        { label: "no key", status: "ready" }, // dropped — no key
        { key: "weird", status: "totally-bogus" }, // status -> off
        "not-an-object",
      ],
    });
    expect(m.capabilities).toHaveLength(1);
    expect(m.capabilities[0].key).toBe("weird");
    expect(m.capabilities[0].status).toBe("off"); // conservative, not "ready"
  });

  it("an absent/garbled payload yields an empty map, never throws or null", () => {
    expect(parseCapabilityMap({}).capabilities).toEqual([]);
    expect(parseCapabilityMap({ capabilities: 42 }).capabilities).toEqual([]);
  });

  it("a truthy-but-not-boolean armed/verified coerces to false (no fabricated certainty)", () => {
    const m = parseCapabilityMap({
      capabilities: [{ key: "x", armed: "yes", verified: 1, status: "ready" }],
    });
    expect(m.capabilities[0].armed).toBe(false);
    expect(m.capabilities[0].verified).toBe(false);
  });
});

describe("capability.map reducer", () => {
  it("starts null and is populated by a capability.map frame", () => {
    const s0 = connected();
    expect(s0.capabilityMap).toBeNull();
    const s1 = tel(s0, env("capability.map", sample));
    expect(s1.capabilityMap?.capabilities).toHaveLength(4);
    expect(s1.capabilityMap?.capabilities[0].status).toBe("ready");
  });
});

describe("CapabilityStatusPanel (honest render)", () => {
  const render = (map: CapabilityMap | null) =>
    renderToStaticMarkup(createElement(CapabilityStatusPanel, { map }));

  it("renders nothing before the first frame", () => {
    expect(render(null)).toBe("");
    expect(render({ capabilities: [] })).toBe("");
  });

  it("shows a READY pill, a NEEDS-DEP pill with its dependency, and an OFF pill", () => {
    const html = render(parseCapabilityMap(sample));
    expect(html).toContain("READY");
    expect(html).toContain("ARMED · NEEDS DEP");
    expect(html).toContain("OFF");
    expect(html).toContain("sandbox-exec"); // the probed dependency phrase
    expect(html).toContain("capmap-pill ready");
    expect(html).toContain("capmap-pill needs-dep");
  });

  it("marks an unprobed dependency as 'unverified', a probed one not", () => {
    const html = render(parseCapabilityMap(sample));
    // elevenlabs (verified:false) carries the unverified marker...
    expect(html).toMatch(/elevenlabs_api_key in Keychain[\s\S]*unverified/);
    // ...shell_run (verified:true) does not present its dep as unverified.
    expect(html).not.toMatch(/sandbox-exec[^<]*unverified/);
  });
});
