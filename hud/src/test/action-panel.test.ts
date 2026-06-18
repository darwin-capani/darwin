import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import ActionPanel from "../components/ActionPanel";
import {
  parseDraftComposed,
  parseMacroRecorded,
  parseMacroReplayStep,
  parseMissionEvent,
  coerceMissionStatus,
  draftKindLabel,
  type TelemetryEnvelope,
} from "../core/events";
import {
  type ActionSurface,
  DRAFT_CAP,
  HudState,
  initialState,
  MACRO_CAP,
  MISSION_CAP,
  reduce,
} from "../core/state";

/* helpers ------------------------------------------------------------------ */

let counter = 0;
function env(
  event: string,
  data: Record<string, unknown> = {},
  source = "system",
): TelemetryEnvelope {
  counter += 1;
  return {
    ts: `2026-06-17T12:00:${String(counter % 60).padStart(2, "0")}Z`,
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

/* ------------------------------------------------------------------------ *
 * #25 — the draft parser. A pending draft must be keyable (a usable id) and
 * its status hard-pinned to "draft" — the surface can NEVER imply a send. The
 * full body never rides the wire; the preview is bounded as defense in depth.
 * ------------------------------------------------------------------------ */
describe("parseDraftComposed (#25, defensive + never-sent)", () => {
  it("parses a well-formed draft.composed payload", () => {
    const d = parseDraftComposed(
      { id: "d1", kind: "email_reply", subject: "Re: lunch", preview: "Sounds good—" },
      "2026-06-17T12:00:00Z",
    );
    expect(d).not.toBeNull();
    expect(d!.id).toBe("d1");
    expect(d!.kind).toBe("email_reply");
    expect(d!.subject).toBe("Re: lunch");
    expect(d!.preview).toBe("Sounds good—");
    // status is ALWAYS "draft" — the draft module has no send path.
    expect(d!.status).toBe("draft");
  });

  it("returns null without a usable id (nothing to key/forget)", () => {
    expect(parseDraftComposed({ kind: "message" }, "t")).toBeNull();
    expect(parseDraftComposed({ id: "" }, "t")).toBeNull();
    expect(parseDraftComposed({ id: 42 }, "t")).toBeNull();
  });

  it("HARD-pins status to draft even if the wire claims it was sent", () => {
    const d = parseDraftComposed({ id: "d1", status: "sent" }, "t");
    expect(d!.status).toBe("draft");
  });

  it("clips an oversize preview/subject (full body never reaches the surface)", () => {
    const big = "x".repeat(5000);
    const d = parseDraftComposed({ id: "d1", subject: big, preview: big }, "t");
    expect(d!.subject.length).toBeLessThanOrEqual(140);
    expect(d!.preview.length).toBeLessThanOrEqual(200);
  });

  it("never throws on junk", () => {
    expect(() => parseDraftComposed({}, "t")).not.toThrow();
    expect(parseDraftComposed({}, "t")).toBeNull();
  });
});

/* ------------------------------------------------------------------------ *
 * #26 — the mission parser. A junk status coerces to the SAFE "paused" (never
 * auto-active); mission.cancelled forces "cancelled"; an id is required.
 * ------------------------------------------------------------------------ */
describe("parseMissionEvent (#26, loads PAUSED, no auto-active)", () => {
  it("parses mission.saved with id/goal/status/progress", () => {
    const m = parseMissionEvent(
      "mission.saved",
      { id: "m1", goal: "tidy inbox", status: "paused", done: 1, total: 4 },
      "t",
    );
    expect(m).toEqual({ id: "m1", goal: "tidy inbox", status: "paused", done: 1, total: 4, ts: "t" });
  });

  it("coerces a junk/missing status to the SAFE paused (never auto-active)", () => {
    expect(coerceMissionStatus("running")).toBe("paused");
    expect(coerceMissionStatus(undefined)).toBe("paused");
    expect(coerceMissionStatus("active")).toBe("active");
    const m = parseMissionEvent("mission.saved", { id: "m1" }, "t");
    expect(m!.status).toBe("paused");
  });

  it("forces cancelled for mission.cancelled regardless of payload status", () => {
    const m = parseMissionEvent("mission.cancelled", { id: "m1", status: "active" }, "t");
    expect(m!.status).toBe("cancelled");
  });

  it("clamps negative/NaN progress to 0", () => {
    const m = parseMissionEvent("mission.saved", { id: "m1", done: -3, total: Number.NaN }, "t");
    expect(m!.done).toBe(0);
    expect(m!.total).toBe(0);
  });

  it("returns null without a usable id; never throws on junk", () => {
    expect(parseMissionEvent("mission.saved", {}, "t")).toBeNull();
    expect(() => parseMissionEvent("mission.saved", {}, "t")).not.toThrow();
  });
});

/* ------------------------------------------------------------------------ *
 * #27 — the macro parsers. A macro stores only the recorded intents/utterances
 * (the daemon never streams a secret); a name is required.
 * ------------------------------------------------------------------------ */
describe("parseMacroRecorded / parseMacroReplayStep (#27, intents only)", () => {
  it("parses macro.recorded with name + step count, replay idle", () => {
    const m = parseMacroRecorded({ name: "morning", steps: 3 }, "t");
    expect(m).toEqual({ name: "morning", steps: 3, replayPhase: "idle", lastStep: null, ts: "t" });
  });

  it("returns null without a usable name; clamps negative steps", () => {
    expect(parseMacroRecorded({ steps: 2 }, "t")).toBeNull();
    expect(parseMacroRecorded({ name: "x", steps: -5 }, "t")!.steps).toBe(0);
  });

  it("parses a replay step (intent + utterance), null when both empty", () => {
    expect(parseMacroReplayStep({ intent: "send_message", utterance: "text mom" })).toEqual({
      intent: "send_message",
      utterance: "text mom",
    });
    expect(parseMacroReplayStep({})).toBeNull();
  });

  it("never throws on junk", () => {
    expect(() => parseMacroRecorded({}, "t")).not.toThrow();
    expect(() => parseMacroReplayStep({})).not.toThrow();
  });
});

/* ------------------------------------------------------------------------ *
 * The reducer arms. Each surface upserts by key, drops malformed events, and
 * treats a *.blocked reason=disabled (the shipped-OFF state) as a no-op.
 * ------------------------------------------------------------------------ */
describe("#25 draft.* reducer (review-only, never auto-sent)", () => {
  it("draft.composed surfaces a PENDING draft (status=draft)", () => {
    const s = tel(connected(), env("draft.composed", { id: "d1", kind: "email_reply", subject: "Re: x" }));
    expect(s.actionSurface.drafts).toHaveLength(1);
    expect(s.actionSurface.drafts[0].status).toBe("draft");
    expect(s.actionSurface.drafts[0].subject).toBe("Re: x");
  });

  it("upserts by id (a re-composed draft replaces, not duplicates)", () => {
    let s = tel(connected(), env("draft.composed", { id: "d1", subject: "v1" }));
    s = tel(s, env("draft.composed", { id: "d1", subject: "v2" }));
    expect(s.actionSurface.drafts).toHaveLength(1);
    expect(s.actionSurface.drafts[0].subject).toBe("v2");
  });

  it("draft.forgotten removes by id; an unknown id is a no-op (same ref)", () => {
    let s = tel(connected(), env("draft.composed", { id: "d1" }));
    const before = s.actionSurface;
    s = tel(s, env("draft.forgotten", { id: "nope" }));
    expect(s.actionSurface).toBe(before); // no churn
    s = tel(s, env("draft.forgotten", { id: "d1" }));
    expect(s.actionSurface.drafts).toHaveLength(0);
  });

  it("there is NO send path — only draft + forgotten ever touch a draft", () => {
    // A composed draft never becomes 'sent' through any reducer arm; the only
    // status that can exist is "draft" (the draft module has no send verb).
    const s = tel(connected(), env("draft.composed", { id: "d1", status: "sent" }));
    expect(s.actionSurface.drafts[0].status).toBe("draft");
  });

  it("never stores a secret — only id/kind/subject/preview survive", () => {
    const s = tel(
      connected(),
      env("draft.composed", {
        id: "d1",
        kind: "email_reply",
        subject: "hi",
        preview: "hello",
        body: "the FULL private body should never be here",
        to: "secret@vip.example",
        token: "sk-LEAK",
      }),
    );
    const serialized = JSON.stringify(s.actionSurface.drafts);
    expect(serialized).not.toContain("FULL private body");
    expect(serialized).not.toContain("LEAK");
    expect(serialized).not.toContain("secret@vip");
  });

  it("is bounded to DRAFT_CAP", () => {
    let s = connected();
    for (let i = 0; i < DRAFT_CAP + 8; i++) {
      s = tel(s, env("draft.composed", { id: `d${i}` }));
    }
    expect(s.actionSurface.drafts.length).toBeLessThanOrEqual(DRAFT_CAP);
  });
});

describe("#26 mission.* reducer (loads paused, no auto-run, re-gated)", () => {
  it("mission.saved surfaces a mission that loads PAUSED (never auto-active)", () => {
    // A daemon that mislabels a freshly-loaded mission as 'running' must NOT
    // read as active on the surface — the parser coerces to the safe paused.
    const s = tel(connected(), env("mission.saved", { id: "m1", goal: "g", status: "running" }));
    expect(s.actionSurface.missions).toHaveLength(1);
    expect(s.actionSurface.missions[0].status).toBe("paused");
  });

  it("mission.resumed updates the SAME mission in place to active", () => {
    let s = tel(connected(), env("mission.saved", { id: "m1", goal: "g", status: "paused", done: 0, total: 3 }));
    s = tel(s, env("mission.resumed", { id: "m1", goal: "g", status: "active", done: 1, total: 3 }));
    expect(s.actionSurface.missions).toHaveLength(1);
    expect(s.actionSurface.missions[0].status).toBe("active");
    expect(s.actionSurface.missions[0].done).toBe(1);
  });

  it("mission.cancelled marks the mission cancelled (terminal)", () => {
    let s = tel(connected(), env("mission.saved", { id: "m1", status: "paused" }));
    s = tel(s, env("mission.cancelled", { id: "m1" }));
    expect(s.actionSurface.missions[0].status).toBe("cancelled");
  });

  it("mission.blocked (reason=disabled, the shipped-OFF state) is a no-op", () => {
    const before = connected();
    const s = tel(before, env("mission.blocked", { reason: "disabled" }));
    expect(s.actionSurface.missions).toHaveLength(0);
  });

  it("drops a malformed mission event (no id)", () => {
    const s = tel(connected(), env("mission.saved", {}));
    expect(s.actionSurface.missions).toHaveLength(0);
  });

  it("never stores a secret; bounded to MISSION_CAP", () => {
    let s = tel(connected(), env("mission.saved", { id: "m1", goal: "g", token: "sk-LEAK", input: "raw secret" }));
    expect(JSON.stringify(s.actionSurface.missions)).not.toContain("LEAK");
    expect(JSON.stringify(s.actionSurface.missions)).not.toContain("raw secret");
    for (let i = 0; i < MISSION_CAP + 6; i++) s = tel(s, env("mission.saved", { id: `mm${i}` }));
    expect(s.actionSurface.missions.length).toBeLessThanOrEqual(MISSION_CAP);
  });
});

describe("#27 macro.* reducer (re-gated replay, stores no secrets)", () => {
  it("macro.recorded surfaces a named macro with its step count, replay idle", () => {
    const s = tel(connected(), env("macro.recorded", { name: "morning", steps: 2 }));
    expect(s.actionSurface.macros).toHaveLength(1);
    expect(s.actionSurface.macros[0].name).toBe("morning");
    expect(s.actionSurface.macros[0].steps).toBe(2);
    expect(s.actionSurface.macros[0].replayPhase).toBe("idle");
  });

  it("the replay lifecycle drives running -> step -> done in place", () => {
    let s = tel(connected(), env("macro.recorded", { name: "morning", steps: 2 }));
    s = tel(s, env("macro.replay_started", { name: "morning", steps: 2 }));
    expect(s.actionSurface.macros[0].replayPhase).toBe("running");
    s = tel(s, env("macro.replay_step", { intent: "send_message", utterance: "text mom" }));
    expect(s.actionSurface.macros[0].lastStep).toEqual({ intent: "send_message", utterance: "text mom" });
    s = tel(s, env("macro.replay_done", { name: "morning" }));
    expect(s.actionSurface.macros[0].replayPhase).toBe("done");
  });

  it("macro.forgotten removes by name; an unknown name is a no-op (same ref)", () => {
    let s = tel(connected(), env("macro.recorded", { name: "morning", steps: 1 }));
    const before = s.actionSurface;
    s = tel(s, env("macro.forgotten", { name: "nope" }));
    expect(s.actionSurface).toBe(before);
    s = tel(s, env("macro.forgotten", { name: "morning" }));
    expect(s.actionSurface.macros).toHaveLength(0);
  });

  it("macro.blocked (reason=disabled, the shipped-OFF state) is a no-op", () => {
    const s = tel(connected(), env("macro.blocked", { reason: "disabled" }));
    expect(s.actionSurface.macros).toHaveLength(0);
  });

  it("a recorded macro + a replayed step carry NO secret (intents only)", () => {
    // The daemon stores INTENTS/UTTERANCES only — never a resolved credential.
    // Even a hostile payload trying to smuggle a token must not be stored.
    let s = tel(connected(), env("macro.recorded", { name: "morning", steps: 1, token: "sk-LEAK", secret: "creds" }));
    s = tel(s, env("macro.replay_started", { name: "morning" }));
    s = tel(s, env("macro.replay_step", { intent: "send_message", utterance: "text mom", token: "sk-LEAK2" }));
    const serialized = JSON.stringify(s.actionSurface.macros);
    expect(serialized).not.toContain("LEAK");
    expect(serialized).not.toContain("creds");
  });

  it("is bounded to MACRO_CAP", () => {
    let s = connected();
    for (let i = 0; i < MACRO_CAP + 6; i++) s = tel(s, env("macro.recorded", { name: `mac${i}`, steps: 1 }));
    expect(s.actionSurface.macros.length).toBeLessThanOrEqual(MACRO_CAP);
  });
});

/* ------------------------------------------------------------------------ *
 * The OFF/neutral default — every flag ships OFF, so a freshly connected HUD
 * has an EMPTY action surface (the panel renders nothing).
 * ------------------------------------------------------------------------ */
describe("ActionSurface default (all three features ship OFF)", () => {
  it("starts empty", () => {
    const s = connected();
    expect(s.actionSurface).toEqual({ drafts: [], missions: [], macros: [] });
  });
});

/* ------------------------------------------------------------------------ *
 * The panel (rendered headlessly via renderToStaticMarkup). REVIEW-ONLY: no
 * send/run/replay button; the honest copy is present on all three sections.
 * ------------------------------------------------------------------------ */
describe("ActionPanel (review-only, honest copy)", () => {
  const empty: ActionSurface = { drafts: [], missions: [], macros: [] };

  function render(action: ActionSurface): string {
    return renderToStaticMarkup(createElement(ActionPanel, { action }));
  }

  it("renders nothing when all three sub-surfaces are empty (OFF resting state)", () => {
    expect(render(empty)).toBe("");
  });

  it("surfaces a pending draft (subject + preview) with the never-auto-send copy", () => {
    const html = render({
      ...empty,
      drafts: [{ id: "d1", kind: "email_reply", status: "draft", subject: "Re: lunch", preview: "Sounds good—", ts: "t" }],
    });
    expect(html).toContain("PENDING DRAFTS");
    expect(html).toContain("Re: lunch");
    expect(html).toContain("Sounds good");
    expect(html).toMatch(/never auto-sends/i);
    expect(html).toContain("DRAFT");
  });

  it("surfaces a durable mission (goal/status/progress) with the loads-paused copy", () => {
    const html = render({
      ...empty,
      missions: [{ id: "m1", goal: "tidy inbox", status: "paused", done: 2, total: 4, ts: "t" }],
    });
    expect(html).toContain("DURABLE MISSIONS");
    expect(html).toContain("tidy inbox");
    expect(html).toContain("PAUSED");
    expect(html).toContain("2/4 sub-tasks");
    expect(html).toMatch(/loads paused on restart/i);
    expect(html).toMatch(/re-gated/i);
  });

  it("surfaces a macro (name + step count + replay outcome) with the re-gated copy", () => {
    const html = render({
      ...empty,
      macros: [{ name: "morning", steps: 3, replayPhase: "done", lastStep: null, ts: "t" }],
    });
    expect(html).toContain("MACROS");
    expect(html).toContain("morning");
    expect(html).toContain("3 steps");
    expect(html).toContain("REPLAY DONE");
    expect(html).toMatch(/replays through the gate each time/i);
    expect(html).toMatch(/stores no secrets/i);
  });

  it("has NO send / run / resume / replay button — the panel is review-only", () => {
    const html = render({
      drafts: [{ id: "d1", kind: "email_reply", status: "draft", subject: "s", preview: "p", ts: "t" }],
      missions: [{ id: "m1", goal: "g", status: "paused", done: 0, total: 2, ts: "t" }],
      macros: [{ name: "morning", steps: 1, replayPhase: "idle", lastStep: null, ts: "t" }],
    });
    expect(html).not.toContain("<button");
    expect(html).not.toMatch(/<button[^>]*>[^<]*(SEND|RUN|RESUME|REPLAY|CONFIRM)/i);
  });

  it("draftKindLabel maps known kinds + renders an unknown kind verbatim", () => {
    expect(draftKindLabel("email_reply")).toBe("EMAIL REPLY");
    expect(draftKindLabel("message")).toBe("MESSAGE");
    expect(draftKindLabel("doc")).toBe("DOC");
    expect(draftKindLabel("note_to_self")).toBe("NOTE TO SELF");
  });
});
