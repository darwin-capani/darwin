import { describe, expect, it } from "vitest";
import {
  BACKOFF_BASE_MS,
  BACKOFF_CAP_MS,
  HEALTHY_RESET_MS,
  backoffDelayMs,
  connectionWasHealthy,
} from "../ws/client";

describe("telemetry reconnect backoff", () => {
  it("ramps 1s -> 5s linearly and caps", () => {
    expect(backoffDelayMs(0)).toBe(BACKOFF_BASE_MS);
    expect(backoffDelayMs(1)).toBe(2000);
    expect(backoffDelayMs(3)).toBe(4000);
    expect(backoffDelayMs(4)).toBe(BACKOFF_CAP_MS);
    expect(backoffDelayMs(100)).toBe(BACKOFF_CAP_MS);
  });

  it("a connection shorter than HEALTHY_RESET_MS is a flap (no backoff reset)", () => {
    // A daemon crash loop — or a heal.applied restart — completes the WS
    // handshake and dies. Resetting backoff in onopen made the LINK OFFLINE
    // overlay strobe at ~1Hz; the reset must require a healthy window.
    expect(connectionWasHealthy(10_000, 10_000 + HEALTHY_RESET_MS - 1)).toBe(false);
    expect(connectionWasHealthy(10_000, 10_500)).toBe(false);
  });

  it("a connection that survives the healthy window earns the reset", () => {
    expect(connectionWasHealthy(10_000, 10_000 + HEALTHY_RESET_MS)).toBe(true);
    expect(connectionWasHealthy(10_000, 60_000)).toBe(true);
  });
});
