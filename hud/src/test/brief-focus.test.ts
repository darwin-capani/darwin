import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import BriefFocusPanel from "../components/BriefFocusPanel";
import {
  briefPriorityLabel,
  focusIsDefault,
  parseFocusActive,
  parseProactiveDigest,
  type FocusActive,
  type ProactiveDigest,
  type TelemetryEnvelope,
} from "../core/events";
import { HudState, initialState, reduce } from "../core/state";

/* helpers ------------------------------------------------------------------ */

let counter = 0;
function env(
  event: string,
  data: Record<string, unknown> = {},
  source = "agent.edith",
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

function renderPanel(
  digest: ProactiveDigest | null,
  focus: FocusActive | null,
): string {
  return renderToStaticMarkup(createElement(BriefFocusPanel, { digest, focus }));
}

/** The daemon's Brief::telemetry() shape for a NON-empty digest — three cited
 *  rows the daemon already ranked (Urgent > Important > Routine). Mirrors
 *  brief.rs's fixtures: real "source:ref_id" citations only. */
const digestNonEmpty: Record<string, unknown> = {
  empty: false,
  count: 3,
  items: [
    { priority: "urgent", text: "1:1 in 5 min", source: "calendar:evt_9" },
    { priority: "important", text: "3 unread", source: "gmail:msg_42" },
    { priority: "routine", text: "Markets steady", source: "global_scan:reuters-1" },
  ],
};

/** The daemon's TunedBehavior::telemetry() for the SLEEP profile — surfaces only
 *  the critical floor, brief verbosity, suggestions quieted. Mirrors focus.rs's
 *  telemetry_states_the_permission_neutral_posture fixture. */
const focusSleep: Record<string, unknown> = {
  profile: "sleep",
  surfacing: ["critical"],
  verbosity: "brief",
  suggestions_quieted: true,
  permission_neutral: true,
  raises_autonomy: false,
  loosens_gate: false,
};

/** The default-identity posture — every category surfaces, full verbosity,
 *  suggestions not quieted (today's behavior). */
const focusDefault: Record<string, unknown> = {
  profile: "default",
  surfacing: ["calendar", "mail", "health", "market", "news", "routine", "critical"],
  verbosity: "full",
  suggestions_quieted: false,
  permission_neutral: true,
  raises_autonomy: false,
  loosens_gate: false,
};

/* parseProactiveDigest — #23 ranking + capping + citation + honest-empty ---- */

describe("parseProactiveDigest (#23 smarter brief)", () => {
  it("keeps the daemon's ranked, cited rows in order", () => {
    const d = parseProactiveDigest(digestNonEmpty);
    expect(d.empty).toBe(false);
    expect(d.items).toHaveLength(3);
    // The daemon already ranked Urgent first; the parser preserves that order.
    expect(d.items.map((i) => i.priority)).toEqual(["urgent", "important", "routine"]);
    // Every row carries its REAL rendered source citation.
    expect(d.items.map((i) => i.source)).toEqual([
      "calendar:evt_9",
      "gmail:msg_42",
      "global_scan:reuters-1",
    ]);
    expect(d.items[0].text).toBe("1:1 in 5 min");
  });

  it("is honestly empty when the wire flags empty — never padded", () => {
    const d = parseProactiveDigest({ empty: true, count: 0, items: [] });
    expect(d.empty).toBe(true);
    expect(d.items).toHaveLength(0);
  });

  it("drops a row with no real source (refuses a fabricated citation)", () => {
    const d = parseProactiveDigest({
      empty: false,
      items: [
        { priority: "urgent", text: "real", source: "calendar:evt_1" },
        { priority: "urgent", text: "uncited", source: "" }, // dropped — no source
        { priority: "important", text: "", source: "gmail:msg_2" }, // dropped — no text
      ],
    });
    expect(d.empty).toBe(false);
    expect(d.items).toHaveLength(1);
    expect(d.items[0].source).toBe("calendar:evt_1");
  });

  it("degrades a garbled/empty item list to the honest-empty digest", () => {
    // A payload that claims non-empty but carries no usable rows is honest-empty,
    // not padded into a phantom item.
    const d = parseProactiveDigest({ empty: false, items: [{ junk: 1 }, "nope", 3] });
    expect(d.empty).toBe(true);
    expect(d.items).toHaveLength(0);
    // Missing items field entirely => honest-empty too.
    expect(parseProactiveDigest({}).empty).toBe(true);
  });

  it("floors an unknown/garbled priority to routine (never inflates urgency)", () => {
    const d = parseProactiveDigest({
      empty: false,
      items: [{ priority: "CRITICAL!!", text: "x", source: "calendar:e" }],
    });
    expect(d.items[0].priority).toBe("routine");
  });

  it("never throws on hostile junk", () => {
    expect(() => parseProactiveDigest({ items: null })).not.toThrow();
    expect(() => parseProactiveDigest({ items: [null, undefined] })).not.toThrow();
  });

  it("briefPriorityLabel maps each priority to an honest label", () => {
    expect(briefPriorityLabel("urgent")).toBe("URGENT");
    expect(briefPriorityLabel("important")).toBe("IMPORTANT");
    expect(briefPriorityLabel("routine")).toBe("ROUTINE");
  });
});

/* parseFocusActive — #24 permission-neutral posture ------------------------ */

describe("parseFocusActive (#24 focus profiles)", () => {
  it("reads the active posture from the wire", () => {
    const f = parseFocusActive(focusSleep);
    expect(f.profile).toBe("sleep");
    expect(f.surfacing).toEqual(["critical"]);
    expect(f.verbosity).toBe("brief");
    expect(f.suggestionsQuieted).toBe(true);
  });

  it("PINS the permission-neutral contract — a hostile payload cannot flip it", () => {
    // A malicious card claiming raised autonomy / loosened gate is NOT honored:
    // a focus profile can only ever quiet, never broaden — enforced HUD-side too.
    const f = parseFocusActive({
      profile: "deep_focus",
      surfacing: [],
      verbosity: "silent",
      suggestions_quieted: true,
      permission_neutral: false, // hostile claim
      raises_autonomy: true, // hostile claim
      loosens_gate: true, // hostile claim
    });
    expect(f.permissionNeutral).toBe(true);
    expect(f.raisesAutonomy).toBe(false);
    expect(f.loosensGate).toBe(false);
  });

  it("default profile is the IDENTITY (today's behavior)", () => {
    const f = parseFocusActive(focusDefault);
    expect(focusIsDefault(f)).toBe(true);
    // Sleep is NOT the identity (it quiets).
    expect(focusIsDefault(parseFocusActive(focusSleep))).toBe(false);
  });

  it("normalizes a garbled verbosity to full + defaults a missing profile", () => {
    const f = parseFocusActive({ surfacing: "nope", verbosity: "LOUD" });
    expect(f.profile).toBe("default");
    expect(f.verbosity).toBe("full"); // unknown => loosest (never understates)
    expect(f.surfacing).toEqual([]); // non-array dropped
    expect(f.suggestionsQuieted).toBe(false);
  });

  it("drops non-string surfacing entries", () => {
    const f = parseFocusActive({ profile: "work", surfacing: ["mail", 7, null, "calendar"] });
    expect(f.surfacing).toEqual(["mail", "calendar"]);
  });

  it("never throws on hostile junk", () => {
    expect(() => parseFocusActive({})).not.toThrow();
    expect(() => parseFocusActive({ surfacing: 5, verbosity: 9 })).not.toThrow();
  });
});

/* reducer folding ----------------------------------------------------------- */

describe("reducer: proactive.digest + focus.active", () => {
  it("folds a non-empty proactive.digest into proactiveDigest", () => {
    const s = tel(connected(), env("proactive.digest", digestNonEmpty));
    expect(s.proactiveDigest).not.toBeNull();
    expect(s.proactiveDigest?.items).toHaveLength(3);
    expect(s.proactiveDigest?.items[0].source).toBe("calendar:evt_9");
  });

  it("clears proactiveDigest to null on an empty/garbled digest (no phantom)", () => {
    let s = tel(connected(), env("proactive.digest", digestNonEmpty));
    expect(s.proactiveDigest).not.toBeNull();
    // An empty digest clears the surface — never a padded shell.
    s = tel(s, env("proactive.digest", { empty: true, items: [] }));
    expect(s.proactiveDigest).toBeNull();
  });

  it("replaces a prior digest in place with the latest glance", () => {
    let s = tel(connected(), env("proactive.digest", digestNonEmpty));
    s = tel(
      s,
      env("proactive.digest", {
        empty: false,
        items: [{ priority: "urgent", text: "newer", source: "calendar:evt_x" }],
      }),
    );
    expect(s.proactiveDigest?.items).toHaveLength(1);
    expect(s.proactiveDigest?.items[0].text).toBe("newer");
  });

  it("folds focus.active into focusProfile (permission-neutral pinned)", () => {
    const s = tel(connected(), env("focus.active", focusSleep));
    expect(s.focusProfile?.profile).toBe("sleep");
    expect(s.focusProfile?.permissionNeutral).toBe(true);
    expect(s.focusProfile?.raisesAutonomy).toBe(false);
    expect(s.focusProfile?.loosensGate).toBe(false);
  });

  it("both surfaces are null on a fresh (default/off) state", () => {
    const s = connected();
    expect(s.proactiveDigest).toBeNull();
    expect(s.focusProfile).toBeNull();
  });
});

/* panel rendering ----------------------------------------------------------- */

describe("BriefFocusPanel render", () => {
  it("renders nothing when there is no digest AND no focus", () => {
    expect(renderPanel(null, null)).toBe("");
  });

  it("renders the prioritized brief items with their real source citations", () => {
    const digest = parseProactiveDigest(digestNonEmpty);
    const html = renderPanel(digest, null);
    expect(html).toContain("BRIEF");
    expect(html).toContain("1:1 in 5 min");
    expect(html).toContain("calendar:evt_9");
    expect(html).toContain("gmail:msg_42");
    expect(html).toContain("URGENT");
    expect(html).toContain("3 CITED");
  });

  it("renders the honest-empty brief when there are no signals", () => {
    // A focus posture present (so the panel renders) but an empty digest.
    const focus = parseFocusActive(focusDefault);
    const html = renderPanel(parseProactiveDigest({ empty: true, items: [] }), focus);
    expect(html).toContain("NOTHING TO BRIEF");
    // The honest-empty copy is stated plainly (the apostrophe renders as the
    // literal U+2019, not an HTML entity, under renderToStaticMarkup).
    expect(html).toContain("pad it with an invented item");
  });

  it("renders the default focus as today's behavior (nothing quieted)", () => {
    const focus = parseFocusActive(focusDefault);
    const html = renderPanel(null, focus);
    expect(html).toContain("FOCUS PROFILE");
    expect(html).toContain("DEFAULT");
    expect(html).toContain("Nothing is quieted");
    // The permission-neutral contract is stated in the copy.
    expect(html).toContain("never loosens a gate");
  });

  it("renders what an active profile is quieting + the honest contract", () => {
    const focus = parseFocusActive(focusSleep);
    const html = renderPanel(null, focus);
    expect(html).toContain("SLEEP");
    // Sleep surfaces only the critical category (non-empty list => a chip).
    expect(html).toContain(">critical<");
    expect(html).toContain("SURFACING");
    expect(html).toContain("quieted");
    expect(html).toContain("never loosens a gate");
  });

  it("renders the 'critical only' copy for a profile that surfaces nothing", () => {
    // DeepFocus surfaces an EMPTY category set (critical is the never-silenced
    // floor) — the panel states "critical only — everything else quieted".
    const focus = parseFocusActive({
      profile: "deep_focus",
      surfacing: [],
      verbosity: "silent",
      suggestions_quieted: true,
      permission_neutral: true,
      raises_autonomy: false,
      loosens_gate: false,
    });
    const html = renderPanel(null, focus);
    expect(html).toContain("DEEP_FOCUS");
    expect(html).toContain("critical only");
    expect(html).toContain("never loosens a gate");
  });
});
