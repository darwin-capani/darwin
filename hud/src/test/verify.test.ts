import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import VerifyPanel from "../components/VerifyPanel";
import {
  parseVerifyStatus,
  verifyBadgeFor,
  verifyStatusIsEmpty,
  type TelemetryEnvelope,
  type VerifyStatus,
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

function render(status: VerifyStatus | null): string {
  return renderToStaticMarkup(createElement(VerifyPanel, { status }));
}

/* The daemon's verify_telemetry shape (anthropic.rs verify_telemetry / the
 * verify_telemetry_is_secret_free_and_honest fixture). Gate ON, with each
 * possible outcome. The honest note is taken verbatim from the daemon. */
const NOTE =
  "A second self-check against the sources this turn used. It REDUCES " +
  "hallucination on important turns; it is NOT a correctness guarantee. " +
  "Runs only on important turns, at most one critique + one revise, and " +
  "ships OFF by default.";

const verifiedClean: Record<string, unknown> = {
  verify_on: true,
  outcome: "verified-clean",
  badge: "VERIFIED",
  note: NOTE,
};

const revised: Record<string, unknown> = {
  verify_on: true,
  outcome: "revised",
  badge: "REVISED",
  note: NOTE,
};

const flagged: Record<string, unknown> = {
  verify_on: true,
  outcome: "flagged",
  badge: "FLAGGED",
  note: NOTE,
};

/* The SHIPPED DEFAULT: [answers].verify OFF — outcome "off" + null badge. The
 * HUD must render NOTHING. */
const verifyOff: Record<string, unknown> = {
  verify_on: false,
  outcome: "off",
  badge: null,
  note: NOTE,
};

/* ------------------------------------------------------------------------- */

describe("verifyBadgeFor", () => {
  it("derives the HUD badge from the outcome token (single source of truth)", () => {
    expect(verifyBadgeFor("off")).toBeNull();
    expect(verifyBadgeFor("verified-clean")).toBe("VERIFIED");
    expect(verifyBadgeFor("revised")).toBe("REVISED");
    expect(verifyBadgeFor("flagged")).toBe("FLAGGED");
  });
});

describe("parseVerifyStatus", () => {
  it("parses a verified-clean outcome", () => {
    const v = parseVerifyStatus(verifiedClean);
    expect(v.verifyOn).toBe(true);
    expect(v.outcome).toBe("verified-clean");
    expect(v.badge).toBe("VERIFIED");
    expect(v.note).toBe(NOTE);
  });

  it("parses a revised outcome", () => {
    const v = parseVerifyStatus(revised);
    expect(v.outcome).toBe("revised");
    expect(v.badge).toBe("REVISED");
  });

  it("parses a flagged outcome", () => {
    const v = parseVerifyStatus(flagged);
    expect(v.outcome).toBe("flagged");
    expect(v.badge).toBe("FLAGGED");
  });

  it("the OFF default yields the empty (renders-nothing) status", () => {
    const v = parseVerifyStatus(verifyOff);
    expect(v.verifyOn).toBe(false);
    expect(v.outcome).toBe("off");
    expect(v.badge).toBeNull();
    expect(verifyStatusIsEmpty(v)).toBe(true);
  });

  it("DERIVES the badge from the outcome — a spoofed wire badge never wins", () => {
    // A daemon (or attacker) that put a mismatched badge on the wire: the parser
    // ignores it and derives the badge from the validated outcome.
    const v = parseVerifyStatus({
      verify_on: true,
      outcome: "verified-clean",
      badge: "FLAGGED", // lie on the wire
      note: NOTE,
    });
    expect(v.outcome).toBe("verified-clean");
    expect(v.badge).toBe("VERIFIED"); // derived, not "FLAGGED"
  });

  it("collapses an unknown outcome token to OFF (no unrecognized badge)", () => {
    const v = parseVerifyStatus({
      verify_on: true,
      outcome: "totally-verified-trust-me",
      badge: "VERIFIED",
      note: NOTE,
    });
    expect(v.outcome).toBe("off");
    expect(v.badge).toBeNull();
    expect(verifyStatusIsEmpty(v)).toBe(true);
  });

  it("never throws on junk and yields an honest OFF status", () => {
    const v = parseVerifyStatus({ outcome: 7, verify_on: "yes", note: null });
    expect(v.outcome).toBe("off");
    expect(v.badge).toBeNull();
    expect(v.verifyOn).toBe(false);
    expect(v.note).toBe("");
    expect(verifyStatusIsEmpty(v)).toBe(true);
  });
});

describe("answer.verified reducer", () => {
  it("folds a verified-clean outcome onto state", () => {
    const s = tel(connected(), env("answer.verified", verifiedClean));
    expect(s.verifyStatus).not.toBeNull();
    expect(s.verifyStatus?.outcome).toBe("verified-clean");
    expect(s.verifyStatus?.badge).toBe("VERIFIED");
  });

  it("folds a revised and a flagged outcome", () => {
    let s = tel(connected(), env("answer.verified", revised));
    expect(s.verifyStatus?.badge).toBe("REVISED");
    s = tel(connected(), env("answer.verified", flagged));
    expect(s.verifyStatus?.badge).toBe("FLAGGED");
  });

  it("the OFF default leaves nothing to render (stays null, same reference)", () => {
    const base = connected();
    const s = tel(base, env("answer.verified", verifyOff));
    expect(s.verifyStatus).toBeNull();
    // A stream of off-gate turns must not churn the tree.
    expect(s.verifyStatus).toBe(base.verifyStatus);
  });

  it("a fresh turn REPLACES the prior outcome (per-turn, no cross-turn leak)", () => {
    // Turn N: revised.
    let s = tel(connected(), env("answer.verified", revised));
    expect(s.verifyStatus?.badge).toBe("REVISED");
    // Turn N+1: verified-clean — must NOT keep N's REVISED badge.
    s = tel(s, env("answer.verified", verifiedClean));
    expect(s.verifyStatus?.badge).toBe("VERIFIED");
  });

  it("an off-gate turn after a verified turn CLEARS the prior badge", () => {
    // Turn N: verified. Turn N+1: gate off (or pass did not run) -> clears to
    // null so turn N's badge never lingers onto a later off turn.
    let s = tel(connected(), env("answer.verified", verifiedClean));
    expect(s.verifyStatus).not.toBeNull();
    s = tel(s, env("answer.verified", verifyOff));
    expect(s.verifyStatus).toBeNull();
  });
});

describe("VerifyPanel", () => {
  it("renders nothing when there is no status (shipped OFF default)", () => {
    expect(render(null)).toBe("");
  });

  it("renders nothing for an off-gate turn (reducer holds it null)", () => {
    const s = tel(connected(), env("answer.verified", verifyOff));
    expect(render(s.verifyStatus)).toBe("");
  });

  it("renders nothing for a status whose badge is null", () => {
    expect(render(parseVerifyStatus(verifyOff))).toBe("");
  });

  it("surfaces the VERIFIED badge with HONEST copy (not a guarantee)", () => {
    const html = render(parseVerifyStatus(verifiedClean));
    expect(html).toContain("ANSWER // SELF-CHECK");
    expect(html).toContain("VERIFIED");
    // Honest framing — reduces, not eliminates; verified != correct.
    const lower = html.toLowerCase();
    expect(lower).toContain("reduce");
    expect(lower).toContain("does not eliminate");
    expect(lower).toContain("does not mean guaranteed-correct");
    expect(lower).toContain("important turns");
    expect(lower).toContain("ships off");
  });

  it("surfaces the REVISED badge (the self-check corrected the answer)", () => {
    const html = render(parseVerifyStatus(revised));
    expect(html).toContain("REVISED");
    expect(html.toLowerCase()).toContain("corrected");
  });

  it("surfaces the FLAGGED badge (an unresolved caveat)", () => {
    const html = render(parseVerifyStatus(flagged));
    expect(html).toContain("FLAGGED");
    expect(html.toLowerCase()).toContain("caveat");
  });

  it("is SECRET-FREE: never renders flagged-claim text or any content", () => {
    // A daemon that (incorrectly) tried to attach the flagged claim / content /
    // an embedding alongside the honest fields: the panel reads ONLY outcome +
    // badge + note, never the secret.
    const v = parseVerifyStatus({
      verify_on: true,
      outcome: "flagged",
      badge: "FLAGGED",
      note: NOTE,
      issues: ["the launch date is wrong"],
      draft: "SECRET_DRAFT_TEXT",
      embedding: [0.123456, 0.654321],
      audio: "AUDIO_BLOB",
    });
    // The parsed status carries ONLY the four honest fields.
    expect(Object.keys(v).sort()).toEqual(["badge", "note", "outcome", "verifyOn"]);
    const html = render(v);
    expect(html).toContain("FLAGGED");
    expect(html).not.toContain("the launch date is wrong");
    expect(html).not.toContain("SECRET_DRAFT_TEXT");
    expect(html).not.toContain("0.123456");
    expect(html).not.toContain("AUDIO_BLOB");
    expect(html).not.toContain("embedding");
  });
});
