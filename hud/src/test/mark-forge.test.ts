import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import {
  PHYSICS_TOPIC_BODIES,
  PHYSICS_TOPIC_SCENE,
  PHYSICS_TOPIC_STEP,
  parsePhysicsBodies,
  parsePhysicsScene,
  parsePhysicsStep,
  type TelemetryEnvelope,
} from "../core/events";
import { initialState, reduce, type HudState } from "../core/state";
import CapHitIndicator from "../components/CapHitIndicator";

/* ------------------------------------------------------------------------ *
 * Pure payload parsers (events.ts) — verbatim against the engine's wire       *
 * structs in apps/mark-forge/src/ipc.rs (BodiesFrame / StepReport /           *
 * SceneTopology, ShapeTag tagged on `kind`). The engine itself is verified by *
 * `cargo test`; the live R3F render is DEVICE-GATED (the headless preview     *
 * suspends the loop) and is NOT exercised here — these cover only the         *
 * telemetry data path the HUD panel renders. `pos` is [x,y,z], `quat` is      *
 * [x,y,z,w] (xyzw, w scalar).                                                  *
 * ------------------------------------------------------------------------ */

describe("parsePhysicsBodies (physics.bodies)", () => {
  it("parses a per-frame body feed with sphere + cuboid transforms", () => {
    const f = parsePhysicsBodies({
      frame: 120,
      sim_time: 2.0,
      bodies: [
        {
          id: 0,
          shape: { kind: "sphere", radius: 0.5 },
          pos: [0, 5, 0],
          quat: [0, 0, 0, 1],
          sleeping: false,
        },
        {
          id: 1,
          shape: { kind: "cuboid", half_extents: [0.5, 0.5, 0.5] },
          pos: [1, 2, 3],
          quat: [0, 0.7071, 0, 0.7071],
          sleeping: true,
        },
      ],
    });
    expect(f).not.toBeNull();
    expect(f!.frame).toBe(120);
    expect(f!.simTime).toBe(2.0);
    expect(f!.bodies).toHaveLength(2);
    expect(f!.bodies[0]).toEqual({
      id: 0,
      shape: { kind: "sphere", radius: 0.5 },
      pos: [0, 5, 0],
      quat: [0, 0, 0, 1],
      sleeping: false,
    });
    expect(f!.bodies[1].shape).toEqual({ kind: "cuboid", halfExtents: [0.5, 0.5, 0.5] });
    expect(f!.bodies[1].sleeping).toBe(true);
  });

  it("parses a plane body (normal + offset)", () => {
    const f = parsePhysicsBodies({
      frame: 0,
      sim_time: 0,
      bodies: [
        {
          id: 0,
          shape: { kind: "plane", normal: [0, 1, 0], offset: 0 },
          pos: [0, 0, 0],
          quat: [0, 0, 0, 1],
          sleeping: false,
        },
      ],
    });
    expect(f!.bodies[0].shape).toEqual({ kind: "plane", normal: [0, 1, 0], offset: 0 });
  });

  it("defaults quat to the identity when absent but drops a malformed quat", () => {
    const ok = parsePhysicsBodies({
      frame: 1,
      bodies: [{ id: 0, shape: { kind: "sphere", radius: 1 }, pos: [0, 0, 0] }],
    });
    expect(ok!.bodies[0].quat).toEqual([0, 0, 0, 1]);

    // A PRESENT but wrong-length quat drops the body (no NaN into THREE).
    const bad = parsePhysicsBodies({
      frame: 1,
      bodies: [{ id: 0, shape: { kind: "sphere", radius: 1 }, pos: [0, 0, 0], quat: [0, 0, 1] }],
    });
    expect(bad!.bodies).toEqual([]);
  });

  it("drops bodies with a missing id, unknown shape, bad size, or malformed pos", () => {
    const f = parsePhysicsBodies({
      frame: 7,
      bodies: [
        { shape: { kind: "sphere", radius: 1 }, pos: [0, 0, 0] }, // no id
        { id: 1, shape: { kind: "sphere" }, pos: [0, 0, 0] }, // sphere w/o radius
        { id: 2, shape: { kind: "cuboid", half_extents: [1, 1] }, pos: [0, 0, 0] }, // bad half_extents
        { id: 3, shape: { kind: "blob", radius: 1 }, pos: [0, 0, 0] }, // unknown kind
        { id: 4, shape: { kind: "sphere", radius: 1 }, pos: [0, 0] }, // pos len 2
        { id: 5, shape: { kind: "sphere", radius: 1 }, pos: [0, NaN, 0] }, // non-finite pos
        "not-an-object",
        { id: 6, shape: { kind: "sphere", radius: 1 }, pos: [1, 2, 3] }, // the one good body
      ],
    });
    expect(f!.bodies.map((b) => b.id)).toEqual([6]);
  });

  it("returns null when frame is missing/non-finite; never throws on junk", () => {
    expect(parsePhysicsBodies({ bodies: [] })).toBeNull();
    expect(parsePhysicsBodies({ frame: "x", bodies: [] })).toBeNull();
    expect(parsePhysicsBodies({ frame: NaN })).toBeNull();
    // A frame with no bodies array is an empty (valid) frame.
    expect(parsePhysicsBodies({ frame: 3 })).toEqual({ frame: 3, simTime: 0, bodies: [] });
  });
});

describe("parsePhysicsStep (physics.step)", () => {
  it("parses a full solver/step-stats payload", () => {
    const s = parsePhysicsStep({
      frames: 600,
      substeps: 4,
      bodies: 12,
      contacts: 8,
      solver_iterations: 10,
      last_penetration: 0.0004,
    });
    expect(s).toEqual({
      frames: 600,
      substeps: 4,
      bodies: 12,
      contacts: 8,
      solverIterations: 10,
      lastPenetration: 0.0004,
      pairsCapHit: false,
      contactCapHit: false,
    });
  });

  it("parses the per-substep budget cap-hit flags (default false when absent)", () => {
    const base = {
      frames: 600,
      substeps: 4,
      bodies: 4096,
      contacts: 16384,
      solver_iterations: 10,
      last_penetration: 0.01,
    };
    // present + true
    const hit = parsePhysicsStep({ ...base, pairs_cap_hit: true, contact_cap_hit: true })!;
    expect(hit.pairsCapHit).toBe(true);
    expect(hit.contactCapHit).toBe(true);
    // absent -> false (backward compatible with older payloads)
    const none = parsePhysicsStep(base)!;
    expect(none.pairsCapHit).toBe(false);
    expect(none.contactCapHit).toBe(false);
    // non-boolean junk -> false, never throws
    const junk = parsePhysicsStep({ ...base, pairs_cap_hit: "yes", contact_cap_hit: 1 })!;
    expect(junk.pairsCapHit).toBe(false);
    expect(junk.contactCapHit).toBe(false);
  });

  it("returns null when any counter/stat is missing or non-finite", () => {
    const full = {
      frames: 1,
      substeps: 4,
      bodies: 2,
      contacts: 0,
      solver_iterations: 10,
      last_penetration: 0,
    };
    expect(parsePhysicsStep({ ...full, contacts: undefined })).toBeNull();
    expect(parsePhysicsStep({ ...full, last_penetration: "x" })).toBeNull();
    expect(parsePhysicsStep({ ...full, solver_iterations: Infinity })).toBeNull();
    expect(parsePhysicsStep({})).toBeNull();
  });
});

describe("parsePhysicsScene (physics.scene)", () => {
  it("parses sim params + static body topology (static key from r#static)", () => {
    const s = parsePhysicsScene({
      gravity: [0, -9.81, 0],
      dt: 0.0166667,
      substeps: 4,
      bodies: [
        { id: 0, shape: { kind: "plane", normal: [0, 1, 0], offset: 0 }, static: true },
        { id: 1, shape: { kind: "sphere", radius: 0.5 }, static: false },
      ],
    });
    expect(s).not.toBeNull();
    expect(s!.gravity).toEqual([0, -9.81, 0]);
    expect(s!.dt).toBeCloseTo(0.0166667);
    expect(s!.substeps).toBe(4);
    expect(s!.bodies).toEqual([
      { id: 0, shape: { kind: "plane", normal: [0, 1, 0], offset: 0 }, isStatic: true },
      { id: 1, shape: { kind: "sphere", radius: 0.5 }, isStatic: false },
    ]);
  });

  it("defaults `static` to false when absent and drops malformed topology entries", () => {
    const s = parsePhysicsScene({
      gravity: [0, -9.81, 0],
      dt: 0.01,
      bodies: [
        { id: 0, shape: { kind: "sphere", radius: 1 } }, // no static -> false
        { id: 1, shape: { kind: "mystery" } }, // unknown kind -> dropped
        { shape: { kind: "sphere", radius: 1 } }, // no id -> dropped
        "junk",
      ],
    });
    expect(s!.bodies).toEqual([
      { id: 0, shape: { kind: "sphere", radius: 1 }, isStatic: false },
    ]);
  });

  it("treats an empty/absent bodies list as the initial empty scene (on connect)", () => {
    const s = parsePhysicsScene({ gravity: [0, -9.81, 0], dt: 0.0166667, substeps: 4 });
    expect(s).not.toBeNull();
    expect(s!.bodies).toEqual([]);
    expect(s!.substeps).toBe(4);
  });

  it("returns null when gravity or dt is missing/malformed; never throws", () => {
    expect(parsePhysicsScene({ dt: 0.01 })).toBeNull(); // no gravity
    expect(parsePhysicsScene({ gravity: [0, -9.81], dt: 0.01 })).toBeNull(); // gravity len 2
    expect(parsePhysicsScene({ gravity: [0, "x", 0], dt: 0.01 })).toBeNull(); // non-finite
    expect(parsePhysicsScene({ gravity: [0, -9.81, 0] })).toBeNull(); // no dt
    expect(parsePhysicsScene({})).toBeNull();
  });
});

/* ------------------------------------------------------------------------ *
 * Reducer: app.data stashes each physics topic under feed.topics, keyed by    *
 * the relay topic — additive, and must not disturb other app feeds. Mirrors   *
 * the silicon-canvas storage contract; the panel narrows its own slice.       *
 * ------------------------------------------------------------------------ */

const MF = "mark-forge";

function env(event: string, data: Record<string, unknown>): TelemetryEnvelope {
  return { ts: "2026-06-13T12:00:00.000Z", source: "system", event, data };
}

function tel(state: HudState, e: TelemetryEnvelope, at = 1000): HudState {
  return reduce(state, { type: "telemetry", envelope: e, at });
}

function connected(): HudState {
  return reduce(initialState(), { type: "ws.connected", at: 0 });
}

describe("reducer: app.data physics topic storage", () => {
  it("stores each physics topic payload verbatim under feed.topics[topic]", () => {
    let s = connected();
    s = tel(s, env("app.data", {
      name: MF,
      topic: PHYSICS_TOPIC_SCENE,
      payload: {
        gravity: [0, -9.81, 0],
        dt: 0.0166667,
        substeps: 4,
        bodies: [{ id: 0, shape: { kind: "plane", normal: [0, 1, 0], offset: 0 }, static: true }],
      },
    }));
    s = tel(s, env("app.data", {
      name: MF,
      topic: PHYSICS_TOPIC_STEP,
      payload: {
        frames: 1,
        substeps: 4,
        bodies: 1,
        contacts: 0,
        solver_iterations: 10,
        last_penetration: 0,
      },
    }));
    s = tel(s, env("app.data", {
      name: MF,
      topic: PHYSICS_TOPIC_BODIES,
      payload: {
        frame: 1,
        sim_time: 0.0166667,
        bodies: [
          { id: 0, shape: { kind: "plane", normal: [0, 1, 0], offset: 0 }, pos: [0, 0, 0], quat: [0, 0, 0, 1], sleeping: true },
        ],
      },
    }));

    const feed = s.appFeeds[MF];
    expect(feed.running).toBe(true);
    expect(s.runningApps.has(MF)).toBe(true);

    // Each topic slice round-trips through the matching parser.
    expect(parsePhysicsScene(feed.topics[PHYSICS_TOPIC_SCENE])!.gravity).toEqual([0, -9.81, 0]);
    expect(parsePhysicsStep(feed.topics[PHYSICS_TOPIC_STEP])!.solverIterations).toBe(10);
    expect(parsePhysicsBodies(feed.topics[PHYSICS_TOPIC_BODIES])!.bodies[0].shape).toEqual({
      kind: "plane",
      normal: [0, 1, 0],
      offset: 0,
    });
  });

  it("a newer bodies frame replaces it; the scene topic is retained", () => {
    let s = connected();
    s = tel(s, env("app.data", {
      name: MF,
      topic: PHYSICS_TOPIC_SCENE,
      payload: { gravity: [0, -9.81, 0], dt: 0.0166667, substeps: 4, bodies: [] },
    }));
    s = tel(s, env("app.data", {
      name: MF,
      topic: PHYSICS_TOPIC_BODIES,
      payload: { frame: 1, sim_time: 0.01, bodies: [] },
    }));
    // A newer bodies frame on the same topic.
    s = tel(s, env("app.data", {
      name: MF,
      topic: PHYSICS_TOPIC_BODIES,
      payload: {
        frame: 2,
        sim_time: 0.02,
        bodies: [{ id: 0, shape: { kind: "sphere", radius: 1 }, pos: [0, 4, 0], quat: [0, 0, 0, 1], sleeping: false }],
      },
    }));

    const feed = s.appFeeds[MF];
    const frame = parsePhysicsBodies(feed.topics[PHYSICS_TOPIC_BODIES])!;
    expect(frame.frame).toBe(2);
    expect(frame.bodies).toHaveLength(1);
    // The scene topic stored earlier survives the bodies update.
    expect(parsePhysicsScene(feed.topics[PHYSICS_TOPIC_SCENE])!.substeps).toBe(4);
  });

  it("does not mutate the prior topics map in place (immutable update)", () => {
    let s = connected();
    s = tel(s, env("app.data", {
      name: MF,
      topic: PHYSICS_TOPIC_BODIES,
      payload: { frame: 1, sim_time: 0, bodies: [] },
    }));
    const beforeTopics = s.appFeeds[MF].topics;
    s = tel(s, env("app.data", {
      name: MF,
      topic: PHYSICS_TOPIC_SCENE,
      payload: { gravity: [0, -9.81, 0], dt: 0.01, substeps: 1, bodies: [] },
    }));
    expect(PHYSICS_TOPIC_SCENE in beforeTopics).toBe(false);
    expect(PHYSICS_TOPIC_SCENE in s.appFeeds[MF].topics).toBe(true);
  });

  it("the initial empty physics.scene on connect mounts the feed (running)", () => {
    // run() emits an empty physics.scene on socket connect so the panel mounts.
    let s = connected();
    s = tel(s, env("app.data", {
      name: MF,
      topic: PHYSICS_TOPIC_SCENE,
      payload: { gravity: [0, -9.81, 0], dt: 0.0166667, substeps: 4, bodies: [] },
    }));
    expect(s.appFeeds[MF].running).toBe(true);
    expect(parsePhysicsScene(s.appFeeds[MF].topics[PHYSICS_TOPIC_SCENE])!.bodies).toEqual([]);
  });
});

/* ------------------------------------------------------------------------ *
 * CapHitIndicator — the on-screen affordance for the engine's per-substep    *
 * bounded-work signals (pairs_cap_hit / contact_cap_hit on StepReport).      *
 * Rendered to static markup (react-dom/server, no DOM/jsdom). The R3F panel  *
 * render is device-gated and NOT exercised; this covers only the            *
 * received-signal -> indicator decision the panel makes.                     *
 * ------------------------------------------------------------------------ */
describe("CapHitIndicator (MarkForgePanel cap-hit render)", () => {
  const render = (pairsCapHit: boolean, contactCapHit: boolean) =>
    renderToStaticMarkup(createElement(CapHitIndicator, { pairsCapHit, contactCapHit }));

  it("renders both pills when both cap-hit flags are set", () => {
    const html = render(true, true);
    expect(html).toContain("CONTACT CAP HIT");
    expect(html).toContain("PAIR BUDGET HIT");
  });

  it("renders only the contact pill when only contact_cap_hit is set", () => {
    const html = render(false, true);
    expect(html).toContain("CONTACT CAP HIT");
    expect(html).not.toContain("PAIR BUDGET HIT");
  });

  it("renders only the pair pill when only pairs_cap_hit is set", () => {
    const html = render(true, false);
    expect(html).toContain("PAIR BUDGET HIT");
    expect(html).not.toContain("CONTACT CAP HIT");
  });

  it("is absent (renders nothing) when both cap-hit flags are false", () => {
    expect(render(false, false)).toBe("");
  });
});
