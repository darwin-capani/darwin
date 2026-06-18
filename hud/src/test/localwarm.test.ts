import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import StatusBar from "../components/StatusBar";
import {
  applyLocalSub,
  applyLocalWarm,
  asLocalSubTier,
  localSubLabel,
  localWarmExtraCount,
  localWarmHonest,
  localWarmInitial,
  localWarmLabel,
  localWarmTone,
  modelTierInitial,
  type LocalWarmStatus,
  type TelemetryEnvelope,
  sttTierInitial,
  voiceIdInitial,
  voiceTierInitial,
  voiceModeInitial,
} from "../core/events";
import { HudState, initialState, reduce } from "../core/state";

/* helpers ------------------------------------------------------------------ */

let counter = 0;
function env(
  event: string,
  data: Record<string, unknown> = {},
  source = "system",
): TelemetryEnvelope {
  counter += 1;
  return {
    ts: `2026-06-16T12:00:${String(counter % 60).padStart(2, "0")}Z`,
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

/* ----------------------------------------------------------------- folding */

describe("resident-models folding helpers (events.ts)", () => {
  it("seeds the honest single-resident / awaiting resting state", () => {
    const lw = localWarmInitial();
    expect(lw).toEqual({
      base: null,
      planned: [],
      multiResident: false,
      budgetGib: 0,
      activeSub: null,
    });
  });

  it("applyLocalWarm folds a single-resident snapshot (the safe default)", () => {
    const lw = applyLocalWarm(localWarmInitial(), {
      base: "qwen3-4b-4bit",
      planned: ["qwen3-4b-4bit"],
      multi_resident: false,
      budget_gib: 0,
    });
    expect(lw.base).toBe("qwen3-4b-4bit");
    expect(lw.planned).toEqual(["qwen3-4b-4bit"]);
    expect(lw.multiResident).toBe(false);
    expect(lw.budgetGib).toBe(0);
  });

  it("applyLocalWarm folds a multi-resident snapshot (base first)", () => {
    const lw = applyLocalWarm(localWarmInitial(), {
      base: "qwen3-4b-4bit",
      planned: ["qwen3-4b-4bit", "qwen3-0.6b-4bit"],
      multi_resident: true,
      budget_gib: 6,
    });
    expect(lw.planned[0]).toBe("qwen3-4b-4bit");
    expect(lw.multiResident).toBe(true);
    expect(lw.budgetGib).toBe(6);
    expect(localWarmExtraCount(lw)).toBe(1);
  });

  it("derives multiResident from planned.length>1 when the bool is absent", () => {
    const multi = applyLocalWarm(localWarmInitial(), {
      base: "b",
      planned: ["b", "f"],
    });
    expect(multi.multiResident).toBe(true);
    const single = applyLocalWarm(localWarmInitial(), { base: "b", planned: ["b"] });
    expect(single.multiResident).toBe(false);
  });

  it("a garbled snapshot never blanks the indicator (keeps prior values)", () => {
    const seeded = applyLocalWarm(localWarmInitial(), {
      base: "b",
      planned: ["b", "f"],
      multi_resident: true,
      budget_gib: 6,
    });
    const lw = applyLocalWarm(seeded, { base: 99, planned: "nope", budget_gib: [] } as unknown as Record<string, unknown>);
    expect(lw.base).toBe("b");
    expect(lw.planned).toEqual(["b", "f"]);
    expect(lw.budgetGib).toBe(6);
  });

  it("applyLocalSub folds the per-turn active sub-choice; ignores unknown", () => {
    const seeded = applyLocalWarm(localWarmInitial(), { base: "b", planned: ["b", "f"] });
    const fast = applyLocalSub(seeded, { local_sub: "fast" });
    expect(fast.activeSub).toBe("fast");
    const kept = applyLocalSub(fast, { local_sub: "supercluster" });
    expect(kept.activeSub).toBe("fast"); // unknown ignored, prior kept
    const none = applyLocalSub(seeded, {});
    expect(none.activeSub).toBe(null);
  });

  it("asLocalSubTier narrows only the three known labels", () => {
    expect(asLocalSubTier("fast")).toBe("fast");
    expect(asLocalSubTier("capable")).toBe("capable");
    expect(asLocalSubTier("auto")).toBe("auto");
    expect(asLocalSubTier("heavy")).toBe(null);
    expect(asLocalSubTier(null)).toBe(null);
  });

  it("never throws on a malformed snapshot and never surfaces a stray field", () => {
    const lw = applyLocalWarm(localWarmInitial(), {
      base: { nested: true },
      planned: 42,
      secret: "leak",
    } as unknown as Record<string, unknown>);
    expect(Object.keys(lw).sort()).toEqual(
      ["activeSub", "base", "budgetGib", "multiResident", "planned"].sort(),
    );
    expect(Object.keys(lw)).not.toContain("secret");
  });
});

/* ---------------------------------------------------------- derivation */

describe("resident-models derivation (pure)", () => {
  it("labels SINGLE / MULTI / AWAITING honestly", () => {
    expect(localWarmLabel(localWarmInitial())).toBe("AWAITING");
    expect(
      localWarmLabel({ base: "b", planned: ["b"], multiResident: false, budgetGib: 0, activeSub: null }),
    ).toBe("SINGLE");
    expect(
      localWarmLabel({ base: "b", planned: ["b", "f"], multiResident: true, budgetGib: 6, activeSub: null }),
    ).toBe("MULTI");
  });

  it("tone: multi=warn (extra capability worth noticing), single=good, awaiting=idle", () => {
    expect(localWarmTone(localWarmInitial())).toBe("idle");
    expect(
      localWarmTone({ base: "b", planned: ["b"], multiResident: false, budgetGib: 0, activeSub: null }),
    ).toBe("good");
    expect(
      localWarmTone({ base: "b", planned: ["b", "f"], multiResident: true, budgetGib: 6, activeSub: null }),
    ).toBe("warn");
  });

  it("extra-count is the warm-set minus the base (>=0)", () => {
    expect(localWarmExtraCount(localWarmInitial())).toBe(0);
    expect(
      localWarmExtraCount({ base: "b", planned: ["b", "f", "g"], multiResident: true, budgetGib: 9, activeSub: null }),
    ).toBe(2);
  });

  it("sub labels are FAST / CAPABLE / AUTO, empty before any", () => {
    expect(localSubLabel("fast")).toBe("FAST");
    expect(localSubLabel("capable")).toBe("CAPABLE");
    expect(localSubLabel("auto")).toBe("AUTO");
    expect(localSubLabel(null)).toBe("");
  });

  it("the honest copy is RAM/device-gated and never claims a measured speed benefit", () => {
    const multi = localWarmHonest({
      base: "b",
      planned: ["b", "f"],
      multiResident: true,
      budgetGib: 6,
      activeSub: null,
    });
    // RAM-gated, ~2x RAM, instant swap only when RAM allows
    expect(multi.toLowerCase()).toContain("when ram allows");
    expect(multi.toLowerCase()).toContain("2x ram");
    expect(multi.toLowerCase()).toContain("instant");
    // honest: a PLAN, NOT a measured speed benefit
    expect(multi.toLowerCase()).toContain("not a measured");
    expect(multi.toLowerCase()).toContain("device");
    // changes no safety gate / no tier choice
    expect(multi.toLowerCase()).toContain("no safety gate");

    const single = localWarmHonest({
      base: "b",
      planned: ["b"],
      multiResident: false,
      budgetGib: 0,
      activeSub: null,
    });
    // single-resident is the safe low-RAM default
    expect(single.toLowerCase()).toContain("single-resident");
    expect(single.toLowerCase()).toContain("low-ram");
    expect(single.toLowerCase()).toContain("not measured");
  });
});

/* --------------------------------------------------------------- reducer */

describe("reducer model.local_warm / local_sub events", () => {
  it("seeds localWarm as single-resident/awaiting before any event", () => {
    const lw = initialState().localWarm;
    expect(localWarmLabel(lw)).toBe("AWAITING");
    expect(lw.multiResident).toBe(false);
  });

  it("threads a single-resident snapshot into state (the safe default)", () => {
    const s = tel(connected(), env("model.local_warm", {
      base: "qwen3-4b-4bit",
      planned: ["qwen3-4b-4bit"],
      multi_resident: false,
      budget_gib: 0,
    }));
    expect(s.localWarm.base).toBe("qwen3-4b-4bit");
    expect(s.localWarm.multiResident).toBe(false);
    expect(localWarmLabel(s.localWarm)).toBe("SINGLE");
  });

  it("threads a multi-resident snapshot into state", () => {
    const s = tel(connected(), env("model.local_warm", {
      base: "qwen3-4b-4bit",
      planned: ["qwen3-4b-4bit", "qwen3-0.6b-4bit"],
      multi_resident: true,
      budget_gib: 6,
    }));
    expect(s.localWarm.multiResident).toBe(true);
    expect(localWarmExtraCount(s.localWarm)).toBe(1);
  });

  it("a model.tier turn folds the active local_sub into the resident-models surface", () => {
    let s = tel(connected(), env("model.local_warm", {
      base: "b",
      planned: ["b", "f"],
      multi_resident: true,
      budget_gib: 6,
    }));
    s = tel(s, env("model.tier", { tier: "local", reason: "auto", local_sub: "fast" }));
    expect(s.localWarm.activeSub).toBe("fast");
    // the model-tier verdict is folded normally too
    expect(s.modelTier.tier).toBe("local");
  });

  it("a model.tier turn without local_sub leaves the active sub untouched", () => {
    let s = tel(connected(), env("model.local_warm", { base: "b", planned: ["b", "f"], multi_resident: true }));
    s = tel(s, env("model.tier", { tier: "local", reason: "auto", local_sub: "auto" }));
    s = tel(s, env("model.tier", { tier: "heavy", reason: "auto" }));
    expect(s.localWarm.activeSub).toBe("auto"); // kept
  });

  it("a malformed model.local_warm never throws and never blanks a known plan", () => {
    let s = tel(connected(), env("model.local_warm", { base: "b", planned: ["b", "f"], multi_resident: true }));
    s = tel(s, env("model.local_warm", { base: 99, planned: 7 }));
    expect(s.localWarm.base).toBe("b");
    expect(s.localWarm.multiResident).toBe(true);
  });
});

/* ------------------------------------------------------------ render: chip */

const noop = () => {};

function renderStatusBar(localWarm: LocalWarmStatus | null): string {
  return renderToStaticMarkup(
    createElement(StatusBar, {
      connected: true,
      coreState: "idle" as const,
      cloudKeyPresent: true,
      inferenceOffline: false,
      heal: null,
      cloudModel: null,
      activeAgent: null,
      voiceId: voiceIdInitial(),
      modelTier: modelTierInitial(),
      localWarm,
      voiceTier: voiceTierInitial(),
      sttTier: sttTierInitial(),
      voiceMode: voiceModeInitial(),
      onOpenSettings: noop,
      onOpenDeck: noop,
    }),
  );
}

describe("StatusBar resident-models chip", () => {
  it("renders NOTHING before the snapshot arrives (bar stays uncluttered)", () => {
    // null prop (older daemon) — no chip
    expect(renderStatusBar(null)).not.toContain("residentmodels-chip");
    // seeded-but-no-snapshot (base null) — still no chip
    expect(renderStatusBar(localWarmInitial())).not.toContain("residentmodels-chip");
  });

  it("renders SINGLE in the calm good tone for the single-resident default", () => {
    const html = renderStatusBar({
      base: "qwen3-4b-4bit",
      planned: ["qwen3-4b-4bit"],
      multiResident: false,
      budgetGib: 0,
      activeSub: null,
    });
    expect(html).toContain("LOCAL SINGLE");
    expect(html).toContain("good");
    // honest hover: single-resident is the safe low-RAM default; not measured
    expect(html.toLowerCase()).toContain("safe default");
    expect(html.toLowerCase()).toContain("low-ram");
    expect(html.toLowerCase()).toContain("not measured");
  });

  it("renders MULTI in the amber accent with the extra count + honest hover", () => {
    const html = renderStatusBar({
      base: "qwen3-4b-4bit",
      planned: ["qwen3-4b-4bit", "qwen3-0.6b-4bit"],
      multiResident: true,
      budgetGib: 6,
      activeSub: "fast",
    });
    expect(html).toContain("LOCAL MULTI");
    expect(html).toContain("+1");
    expect(html).toContain("FAST"); // the active sub-choice
    expect(html).toContain("warn");
    // honest hover: instant swap ONLY when RAM allows (~2x RAM); a PLAN not measured
    expect(html.toLowerCase()).toContain("when ram allows");
    expect(html.toLowerCase()).toContain("2x ram");
    expect(html.toLowerCase()).toContain("not a measured");
  });

  it("never overclaims an always-instant multi-model speed benefit", () => {
    const html = renderStatusBar({
      base: "b",
      planned: ["b", "f"],
      multiResident: true,
      budgetGib: 6,
      activeSub: "auto",
    });
    // must not promise an unconditional / always speed win
    expect(html.toLowerCase()).not.toMatch(/always.{0,20}instant/);
    expect(html.toLowerCase()).not.toMatch(/(faster|speed).{0,30}guarantee/);
    // the swap is conditioned on RAM
    expect(html.toLowerCase()).toContain("when ram allows");
  });

  it("never renders a model path/secret beyond the secret-free model ids", () => {
    const html = renderStatusBar({
      base: "qwen3-4b-4bit",
      planned: ["qwen3-4b-4bit", "qwen3-0.6b-4bit"],
      multiResident: true,
      budgetGib: 6,
      activeSub: null,
    });
    expect(html).not.toContain("/Users/");
    expect(html).not.toContain("sk-");
  });
});
