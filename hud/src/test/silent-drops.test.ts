import { describe, expect, it } from "vitest";
import {
  configIssueLines,
  CONFIG_ISSUE_CAP,
  type TelemetryEnvelope,
} from "../core/events";
import { initialState, reduce, type HudState } from "../core/state";

/**
 * Frames the daemon emits FOR THE HUD that the HUD silently dropped.
 *
 * applyEnvelope's `default` returns state unchanged for anything it does not know, so
 * a missing `case` is indistinguishable from an event that carries no state — and the
 * daemon's own comments claim a HUD consumer that did not exist.
 *
 * The one that matters most: darwind records every unknown section, unknown key and
 * malformed section in darwin.toml / agents.toml and emits them as config.invalid /
 * agents.invalid specifically so this surface can show them. A typo'd key is SILENTLY
 * IGNORED by the parser, so a mistyped safety key — a confirm gate, a lockdown flag,
 * an egress switch — reads as "set" to the operator while not being in effect. The
 * daemon detects exactly that, says so, and the appliance's only live surface threw
 * the message away.
 */

const envelope = (event: string, data: Record<string, unknown>): TelemetryEnvelope => ({
  ts: "2026-07-31T00:00:00Z",
  source: "system",
  event,
  data,
});

const apply = (s: HudState, event: string, data: Record<string, unknown>): HudState =>
  reduce(s, { type: "telemetry", envelope: envelope(event, data), at: 1_000 });

describe("config.invalid / agents.invalid reach the HUD", () => {
  it("records the issues darwind reported", () => {
    const s = apply(initialState(), "config.invalid", {
      issues: ["unknown key [security].encrypt_memry", "unknown section [egres]"],
    });
    expect(s.configIssues).toEqual([
      "unknown key [security].encrypt_memry",
      "unknown section [egres]",
    ]);
  });

  it("records agents.invalid the same way", () => {
    const s = apply(initialState(), "agents.invalid", { issues: ["agent 'x': bad tool"] });
    expect(s.configIssues).toEqual(["agent 'x': bad tool"]);
  });

  it("dedupes across a replayed frame and does not churn state", () => {
    const first = apply(initialState(), "config.invalid", { issues: ["dup", "other"] });
    const second = apply(first, "config.invalid", { issues: ["dup", "other"] });
    expect(second.configIssues).toEqual(["dup", "other"]);
    // Identity: a replayed frame must not force a full-tree re-render.
    expect(second).toBe(first);
  });

  it("bounds what it retains", () => {
    const many = Array.from({ length: CONFIG_ISSUE_CAP + 10 }, (_, i) => `issue-${i}`);
    const s = apply(initialState(), "config.invalid", { issues: many });
    expect(s.configIssues).toHaveLength(CONFIG_ISSUE_CAP);
  });

  it("ignores a malformed payload rather than throwing", () => {
    expect(configIssueLines({})).toEqual([]);
    expect(configIssueLines({ issues: "not-an-array" })).toEqual([]);
    expect(configIssueLines({ issues: [1, null, "  ", "real"] })).toEqual(["real"]);
    const s = apply(initialState(), "config.invalid", { issues: [] });
    expect(s.configIssues).toEqual([]);
  });

  it("truncates an absurdly long issue instead of rendering it whole", () => {
    const [line] = configIssueLines({ issues: ["x".repeat(500)] });
    expect(line.length).toBeLessThanOrEqual(201);
  });
});

describe("a failed memory consolidation is visible", () => {
  it("marks the profile stale and raises a toast", () => {
    const s = apply(initialState(), "memory.consolidation_failed", { error: "llm timeout" });
    expect(s.consolidationStale).toBe(true);
    expect(s.toasts.some((t) => t.text.includes("CONSOLIDATION FAILED"))).toBe(true);
  });

  it("a later success clears the stale flag", () => {
    const failed = apply(initialState(), "memory.consolidation_failed", {});
    expect(failed.consolidationStale).toBe(true);
    const ok = apply(failed, "memory.consolidated", { upserts: 3, deletes: 1 });
    expect(ok.consolidationStale).toBe(false);
  });

  it("says something honest even with no reason in the payload", () => {
    const s = apply(initialState(), "memory.consolidation_failed", {});
    expect(s.toasts.some((t) => t.text.includes("may be stale"))).toBe(true);
  });
});

describe("a dead daemon stops claiming it is watching the screen", () => {
  const watching = (): HudState => {
    let s = apply(initialState(), "screen_context.configured", {
      enabled: true,
      cap: 40,
      interval_secs: 5,
    });
    s = apply(s, "screen_context.watching", {
      watching: true,
      held: 12,
      ingested: true,
    });
    return { ...s, connected: true };
  };

  it("clears the live WATCHING indicator on disconnect", () => {
    const live = watching();
    expect(live.screenContext.watching).toBe(true);
    const dead = reduce(live, { type: "ws.disconnected", at: 2_000 });
    expect(dead.screenContext.watching).toBe(false);
    expect(dead.screenContext.held).toBe(0);
    expect(dead.screenContext.ingested).toBe(false);
  });

  it("keeps the CONFIG half, which is a startup snapshot and not a live sample", () => {
    const dead = reduce(watching(), { type: "ws.disconnected", at: 2_000 });
    expect(dead.screenContext.enabled).toBe(true);
    expect(dead.screenContext.cap).toBe(40);
  });

  it("stays idempotent across repeated reconnect failures", () => {
    const once = reduce(watching(), { type: "ws.disconnected", at: 2_000 });
    const twice = reduce(once, { type: "ws.disconnected", at: 3_000 });
    expect(twice).toBe(once);
  });
});
