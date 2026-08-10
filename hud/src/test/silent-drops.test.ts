import { readFileSync } from "node:fs";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import DiagnosticsPanel from "../components/DiagnosticsPanel";
import MemoryPanel from "../components/MemoryPanel";
import MicOfflineBanner from "../components/MicOfflineBanner";
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

  /* REGRESSION — THE RENDER, not just the reducer. Every assertion above passed
     while `configIssues` had ZERO consumers: no component, no App.tsx prop, no
     toast. From the operator's seat that was byte-for-byte identical to the
     pre-fix drop the file header describes — the daemon detected the typo, said
     so, the reducer bounded and deduped it, and it died in state. Assert the
     PIXELS. */
  it("RENDERS the issues on DiagnosticsPanel (a typo'd safety key is visible)", () => {
    const s = apply(initialState(), "config.invalid", {
      issues: ["unknown key [security].encrypt_memry", "unknown section [egres]"],
    });
    const html = renderToStaticMarkup(
      createElement(DiagnosticsPanel, {
        gauges: s.gauges,
        facts: s.facts,
        actions: s.actions,
        configIssues: s.configIssues,
      }),
    );
    expect(html).toContain("CONFIG ERRORS");
    expect(html).toContain("unknown key [security].encrypt_memry");
    expect(html).toContain("unknown section [egres]");
    // ...and it must say the setting is NOT in effect, which is the whole point.
    expect(html).toContain("NOT in effect");
  });

  it("renders NO config-errors block when the config is clean", () => {
    const s = initialState();
    const html = renderToStaticMarkup(
      createElement(DiagnosticsPanel, {
        gauges: s.gauges,
        facts: s.facts,
        actions: s.actions,
        configIssues: s.configIssues,
      }),
    );
    expect(html).not.toContain("CONFIG ERRORS");
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

  /* REGRESSION — a PERSISTENT surface, not a 4.5s toast. reflect.rs writes NO
     stamp on failure, so the reflection clock can stay stuck for days while the
     6h check keeps retrying; its own comment says the event exists to surface
     that on the HUD "instead of silent 6h retries". `consolidationStale` was set
     by the reducer and read by NOTHING, so the only artifact was a toast that
     expires after TOAST_TTL_MS = 4500ms. A user not watching in that window never
     learned the long-term memory profile had stopped being updated. */
  it("RENDERS a persistent stale marker on MemoryPanel (not just a toast)", () => {
    const s = apply(initialState(), "memory.consolidation_failed", { error: "llm timeout" });
    const html = renderToStaticMarkup(
      createElement(MemoryPanel, {
        memory: s.memory,
        beliefCount: 0,
        consolidationStale: s.consolidationStale,
      }),
    );
    expect(html).toContain("CONSOLIDATION FAILING");
    expect(html).toContain("may be out of date");
    // ...and it must not over-claim: nothing was lost, only the pass is stuck.
    expect(html).toContain("Nothing was lost");
  });

  it("renders NO stale marker once a consolidation succeeds", () => {
    const failed = apply(initialState(), "memory.consolidation_failed", {});
    const ok = apply(failed, "memory.consolidated", { upserts: 3, deletes: 1 });
    const html = renderToStaticMarkup(
      createElement(MemoryPanel, {
        memory: ok.memory,
        beliefCount: 0,
        consolidationStale: ok.consolidationStale,
      }),
    );
    expect(html).not.toContain("CONSOLIDATION FAILING");
  });
});

/**
 * THE MICROPHONE THAT WAS NEVER ACQUIRED.
 *
 * `audio.rs`'s `acquire_input_device()` was measured blocking 173-450s on this
 * machine and then failing. Returning `Err` there used to drop `tx` and end main,
 * so launchd relaunched into the same hang every ~450s; the fix parks the capture
 * thread forever instead and lets the router, scheduler, HUD feed and tool host
 * keep running. Correct — and it means NO AUDIO FRAME IS EVER PRODUCED for the
 * life of the process, with nothing in-process that retries.
 *
 * The daemon says so on `capture.unavailable`. `applyEnvelope` had no case, so the
 * HUD rendered its idle ring: pixel-for-pixel what it renders while listening to a
 * quiet room. A user could sit in front of a fully live appliance talking to a
 * microphone the OS never handed over, and the only difference on screen was the
 * absence of something that is also absent in silence.
 */
describe("a microphone that was never acquired says so", () => {
  it("raises the sticky MIC OFFLINE state with the acquisition error", () => {
    const s = apply(initialState(), "capture.unavailable", {
      error: "no default input device",
    });
    expect(s.micOffline).toBe("no default input device");
  });

  it("drops the core state to idle — a listening ring would be the lie itself", () => {
    let s = apply(initialState(), "audio.level", { rms: 0.9, speaking: false });
    s = { ...s, coreState: "listening" };
    const dead = apply(s, "capture.unavailable", { error: "device busy" });
    expect(dead.coreState).toBe("idle");
  });

  it("survives a payload with no error field rather than throwing", () => {
    const s = apply(initialState(), "capture.unavailable", {});
    expect(s.micOffline).toBe("");
    expect(() => apply(initialState(), "capture.unavailable", {})).not.toThrow();
  });

  it("does not churn state on a replayed frame", () => {
    const first = apply(initialState(), "capture.unavailable", { error: "e" });
    const second = apply(first, "capture.unavailable", { error: "e" });
    expect(second).toBe(first);
  });

  it("clears on the first real audio frame, and only then", () => {
    const down = apply(initialState(), "capture.unavailable", { error: "e" });
    // Something unrelated must NOT clear it.
    expect(apply(down, "daemon.started", { cloud_key: true }).micOffline).toBe("e");
    // An actual audio frame proves capture is alive again.
    expect(apply(down, "audio.level", { rms: 0.02, speaking: false }).micOffline).toBeNull();
  });

  it("does not churn on audio.level once the banner is already down", () => {
    const s = apply(initialState(), "audio.level", { rms: 0.01, speaking: false });
    expect(apply(s, "audio.level", { rms: 0.01, speaking: false })).toBe(s);
  });

  /* REGRESSION — THE PIXELS. `micOffline` set by the reducer and read by nothing
     is the same silence one layer up: that is exactly how `configIssues` above and
     `consolidationStale` below each shipped. Assert the rendered banner. */
  it("RENDERS the banner, and says NOT LISTENING in those words", () => {
    const s = apply(initialState(), "capture.unavailable", { error: "device busy" });
    const html = renderToStaticMarkup(
      createElement(MicOfflineBanner, { error: s.micOffline }),
    );
    expect(html).toContain("MICROPHONE OFFLINE");
    expect(html).toContain("NOT LISTENING");
    expect(html).toContain("device busy");
    expect(html).toContain("visible");
  });

  it("renders NOTHING visible while the microphone is fine", () => {
    const html = renderToStaticMarkup(
      createElement(MicOfflineBanner, { error: initialState().micOffline }),
    );
    expect(html).not.toContain("visible");
    expect(html).not.toContain("device busy");
  });

  /* …and the banner has to be MOUNTED. Every assertion above passes for a
     component nobody renders — the identical shape of the `configIssues` drop
     this file was opened for, one layer further out. App.tsx pulls in three.js
     and the Tauri API and cannot be imported in the node test environment, so
     this reads the source: narrow, anchored at both ends, and it cannot
     self-match (the string it looks for lives in App.tsx, not here). */
  it("is actually MOUNTED in App.tsx, fed from state.micOffline", () => {
    const app = readFileSync(new URL("../App.tsx", import.meta.url), "utf8");
    expect(app).toContain('import MicOfflineBanner from "./components/MicOfflineBanner"');
    expect(app).toMatch(/<MicOfflineBanner\s+error=\{state\.micOffline\}\s*\/>/);
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

/**
 * TWO daemon comments still promised a HUD surface for a frame that reaches none.
 * They are the same defect the `command.routed` deletion pass named and fixed
 * elsewhere, in the two files that pass did not reach:
 *
 *   - daemon/src/anthropic.rs `standing_create_tool` called `standing.created` a
 *     "HUD card". daemon/src/standing.rs says the opposite in as many words at its
 *     own emit: applyEnvelope has a case for NEITHER standing.tripwire_armed NOR
 *     standing.created. One twin was corrected, the other was not.
 *   - daemon/src/apps.rs's ToolResult arm said the `app.result` breadcrumb lets
 *     "the HUD/audit see THAT a tool answered". The HUD cannot — no case. Nor does
 *     audit: that arm calls telemetry::emit only, and audit.rs emits telemetry
 *     rather than consuming it.
 *
 * This pins what those comments now say AT RUNTIME rather than by grep — a grep for
 * a missing `case` raises a candidate, it never clears one. If either topic is ever
 * genuinely wired, THIS test fails and the comment must be rewritten with it, which
 * is the point: the note cannot rot in either direction.
 */
describe("frames whose daemon comment promised a HUD surface that does not exist", () => {
  it("standing.created and app.result leave the reducer untouched", () => {
    const base = initialState();
    const cases: [string, Record<string, unknown>][] = [
      ["standing.created", { id: "m1", goal: "water the plants", schedule: "every day at 9am" }],
      ["app.result", { name: "jsonpath", id: "7", delivered: true }],
    ];
    for (const [event, data] of cases) {
      // Reference identity, not deep equality: applyEnvelope's default returns the
      // SAME object, so this also proves no field was rebuilt on the way through.
      expect(apply(base, event, data)).toBe(base);
    }
  });

  it("PRECONDITION: the same helper does move state for a topic that has a case", () => {
    // Without this, the assertion above would pass just as well if `apply` were
    // broken and reduced nothing at all.
    const base = initialState();
    const moved = apply(base, "config.invalid", { issues: ["unknown key [egres].on"] });
    expect(moved).not.toBe(base);
    expect(moved.configIssues).toEqual(["unknown key [egres].on"]);
  });
});
