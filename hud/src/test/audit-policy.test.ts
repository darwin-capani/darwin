import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import AuditPanel from "../components/AuditPanel";
import SettingsModal, { POLICY_PHRASES } from "../components/SettingsModal";
import {
  coercePolicyDecision,
  liveGateEventFrom,
  parseAuditSnapshot,
  parsePolicySnapshot,
  voiceIdInitial,
  modelTierInitial,
  sttTierInitial,
  type AuditSnapshot,
  type LiveGateEvent,
  type PolicySnapshot,
  type TelemetryEnvelope,
} from "../core/events";
import { HudState, initialState, reduce, LIVE_GATE_CAP } from "../core/state";

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

const noop = () => {};

/** A realistic audit.snapshot payload: an OK chain, a mix of decisions/outcomes,
 *  a redacted target — and a hostile token-shaped field the daemon would never
 *  send, to pin the secret-free contract. */
/*  NOTE: there is deliberately NO `truncated` key here. audit.rs::snapshot_json
 *  emits ONLY {enabled,total,chain,entries}; the fixture used to carry
 *  `truncated: false`, a key the daemon never sends, which made the wire shape
 *  look authoritative for a field it does not carry. */
const mockAudit: Record<string, unknown> = {
  enabled: true,
  total: 3,
  chain: { ok: true, count: 3 },
  entries: [
    {
      seq: 3,
      ts: "2026-06-16T12:00:03Z",
      agent: "agent.pepper",
      tool: "gmail_send",
      target_redacted: "to a@example.com (subj redacted)",
      decision: "always",
      outcome: "executed",
      prev_hash: "abc",
      entry_hash: "def",
    },
    {
      seq: 2,
      ts: "2026-06-16T12:00:02Z",
      agent: "agent.friday",
      tool: "x_post",
      target_redacted: "post (140 chars)",
      decision: "never",
      outcome: "blocked_by_policy",
      prev_hash: "xyz",
      entry_hash: "abc",
    },
    {
      seq: 1,
      ts: "2026-06-16T12:00:01Z",
      agent: "darwin",
      tool: "slack_post_message",
      target_redacted: "#ops",
      decision: "ask",
      outcome: "parked",
      prev_hash: "GENESIS",
      entry_hash: "xyz",
    },
  ],
};

const mockPolicy: Record<string, unknown> = {
  enabled: true,
  rules: [
    { scope: { tool: "gmail_send" }, decision: "always" },
    { scope: { tool: "x_post" }, decision: "never" },
    { scope: { tool: "slack_post_message", recipient: "#ops" }, decision: "always" },
  ],
};

/* ======================================================================== *
 * parseAuditSnapshot (defensive, SECRET-FREE)                                *
 * ======================================================================== */
describe("parseAuditSnapshot (defensive, secret-free)", () => {
  it("parses a well-formed audit.snapshot", () => {
    const s = parseAuditSnapshot(mockAudit);
    expect(s.enabled).toBe(true);
    expect(s.total).toBe(3);
    expect(s.truncated).toBe(false);
    expect(s.chain.ok).toBe(true);
    expect(s.chain.count).toBe(3);
    expect(s.entries.length).toBe(3);
    const top = s.entries[0];
    expect(top.seq).toBe(3);
    expect(top.tool).toBe("gmail_send");
    expect(top.target).toBe("to a@example.com (subj redacted)");
    expect(top.decision).toBe("always");
    expect(top.outcome).toBe("executed");
  });

  it("NEVER surfaces the chain bytes (prev_hash/entry_hash) — only the verdict", () => {
    const s = parseAuditSnapshot(mockAudit);
    const blob = JSON.stringify(s);
    expect(blob).not.toContain("prev_hash");
    expect(blob).not.toContain("entry_hash");
    // the actual hash values are gone too
    expect(blob).not.toContain("GENESIS");
    // but the SECRET-FREE decision/outcome/target survive
    expect(blob).toContain("gmail_send");
    expect(blob).toContain("executed");
  });

  it("NEVER surfaces a hostile token/secret field", () => {
    const s = parseAuditSnapshot({
      enabled: true,
      chain: { ok: true, count: 1 },
      entries: [
        {
          seq: 1,
          tool: "gmail_send",
          target_redacted: "to a@example.com",
          decision: "always",
          outcome: "executed",
          // hostile extras a malformed/compromised payload might carry
          token: "sk-SECRET",
          bearer: "leak",
          input: { password: "hunter2", body: "the real secret email body" },
          raw: "https://user:pw@host",
        },
      ],
    });
    const blob = JSON.stringify(s);
    expect(blob).not.toContain("SECRET");
    expect(blob).not.toContain("leak");
    expect(blob).not.toContain("hunter2");
    expect(blob).not.toContain("the real secret email body");
    expect(blob).not.toContain("user:pw");
    expect(blob).not.toContain("token");
  });

  it("surfaces a BROKEN chain verdict with where + why", () => {
    const s = parseAuditSnapshot({
      enabled: true,
      chain: { ok: false, count: 5, broken_seq: 4, reason: "entry_hash mismatch (a field was altered)" },
      entries: [],
    });
    expect(s.chain.ok).toBe(false);
    expect(s.chain.brokenSeq).toBe(4);
    expect(s.chain.reason).toContain("entry_hash mismatch");
  });

  it("fails toward NOT-OK when the chain status is absent/garbled (never a false green)", () => {
    expect(parseAuditSnapshot({}).chain.ok).toBe(false);
    expect(parseAuditSnapshot({ chain: "nope" }).chain.ok).toBe(false);
    expect(parseAuditSnapshot({ chain: { count: 2 } }).chain.ok).toBe(false);
  });

  it("defaults to the shipped posture + drops malformed entries, never throws", () => {
    const s = parseAuditSnapshot({
      enabled: "yes", // non-bool -> false
      entries: [
        { tool: "x" }, // no seq -> dropped
        { seq: 2 }, // no tool -> dropped
        42, // non-object -> dropped
        { seq: 1, tool: "ok", decision: "garbage", outcome: "weird_future_token" },
      ],
    });
    expect(s.enabled).toBe(false);
    expect(s.entries.length).toBe(1);
    // a junk decision reads as the SAFE "ask", never a loosening
    expect(s.entries[0].decision).toBe("ask");
    // an unknown outcome is carried verbatim (forward-tolerant)
    expect(s.entries[0].outcome).toBe("weird_future_token");
  });

  it("never throws on junk", () => {
    expect(() => parseAuditSnapshot({ entries: "nope" })).not.toThrow();
    expect(parseAuditSnapshot({ entries: "nope" }).entries).toEqual([]);
  });
});

/* ======================================================================== *
 * parsePolicySnapshot (defensive)                                            *
 * ======================================================================== */
describe("parsePolicySnapshot (defensive)", () => {
  it("parses a well-formed policy.snapshot (scope-nested)", () => {
    const s = parsePolicySnapshot(mockPolicy);
    expect(s.enabled).toBe(true);
    expect(s.rules.length).toBe(3);
    expect(s.rules[0]).toEqual({
      tool: "gmail_send",
      agent: null,
      recipient: null,
      decision: "always",
    });
    const scoped = s.rules.find((r) => r.recipient === "#ops")!;
    expect(scoped.tool).toBe("slack_post_message");
    expect(scoped.decision).toBe("always");
  });

  it("SHIPPED-EMPTY default: enabled=false, rules=[] (ASK everywhere)", () => {
    const s = parsePolicySnapshot({});
    expect(s.enabled).toBe(false);
    expect(s.rules).toEqual([]);
  });

  it("a junk decision reads as the SAFE ask, never an always loosening", () => {
    const s = parsePolicySnapshot({
      enabled: true,
      rules: [{ scope: { tool: "gmail_send" }, decision: "definitely allow it" }],
    });
    expect(s.rules[0].decision).toBe("ask");
  });

  it("drops a rule with no tool anchor, never throws", () => {
    const s = parsePolicySnapshot({
      rules: [{ scope: {}, decision: "always" }, "junk", { decision: "never" }],
    });
    expect(s.rules).toEqual([]);
  });
});

describe("coercePolicyDecision", () => {
  it("passes through known tokens", () => {
    expect(coercePolicyDecision("always")).toBe("always");
    expect(coercePolicyDecision("never")).toBe("never");
    expect(coercePolicyDecision("ask")).toBe("ask");
  });
  it("defaults anything else to the SAFE ask (never always)", () => {
    expect(coercePolicyDecision("Always")).toBe("ask"); // case-sensitive
    expect(coercePolicyDecision("allow")).toBe("ask");
    expect(coercePolicyDecision(1)).toBe("ask");
    expect(coercePolicyDecision(null)).toBe("ask");
    expect(coercePolicyDecision(undefined)).toBe("ask");
  });
});

/* ======================================================================== *
 * liveGateEventFrom (chokepoint events, secret-free)                         *
 * ======================================================================== */
describe("liveGateEventFrom (chokepoint events)", () => {
  it("maps policy.blocked / policy.auto_approved / confirm.parked to kinds", () => {
    expect(liveGateEventFrom("policy.blocked", { tool: "x", agent: "a" }, "t", 1)!.kind).toBe(
      "blocked",
    );
    expect(
      liveGateEventFrom("policy.auto_approved", { tool: "x", agent: "a" }, "t", 1)!.kind,
    ).toBe("auto_approved");
    expect(liveGateEventFrom("confirm.parked", { tool: "x", agent: "a" }, "t", 1)!.kind).toBe(
      "parked",
    );
  });

  it("carries the mcp / via routing marker (secret-free)", () => {
    expect(
      liveGateEventFrom("policy.blocked", { tool: "t", agent: "a", mcp: true }, "t", 1)!.via,
    ).toBe("mcp");
    expect(
      liveGateEventFrom("policy.auto_approved", { tool: "t", via: "selector" }, "t", 1)!.via,
    ).toBe("selector");
    expect(liveGateEventFrom("policy.blocked", { tool: "t" }, "t", 1)!.via).toBe(null);
  });

  it("returns null for an unrelated event", () => {
    expect(liveGateEventFrom("audio.level", {}, "t", 1)).toBeNull();
    expect(liveGateEventFrom("answer.verified", {}, "t", 1)).toBeNull();
  });

  it("never carries a target/input (chokepoint events are tool/agent only)", () => {
    const ev = liveGateEventFrom(
      "policy.auto_approved",
      { tool: "gmail_send", agent: "a", body: "secret", token: "sk-X" },
      "t",
      1,
    );
    const blob = JSON.stringify(ev);
    expect(blob).not.toContain("secret");
    expect(blob).not.toContain("sk-X");
  });
});

/* ======================================================================== *
 * Reducer arms                                                               *
 * ======================================================================== */
describe("audit.snapshot / policy.snapshot reducer", () => {
  it("sets the audit snapshot from a well-formed event (secret-free)", () => {
    const s = tel(connected(), env("audit.snapshot", mockAudit));
    expect(s.audit).not.toBeNull();
    expect(s.audit!.chain.ok).toBe(true);
    expect(s.audit!.entries.length).toBe(3);
    expect(JSON.stringify(s.audit)).not.toContain("entry_hash");
  });

  it("sets the policy snapshot; an empty store is the honest ASK-everywhere state", () => {
    const s = tel(connected(), env("policy.snapshot", { enabled: true, rules: [] }));
    expect(s.policy).not.toBeNull();
    expect(s.policy!.enabled).toBe(true);
    expect(s.policy!.rules).toEqual([]);
  });

  it("folds the live chokepoint events newest-first into a bounded ring", () => {
    let s = connected();
    s = tel(s, env("policy.blocked", { tool: "x_post", agent: "agent.friday" }));
    s = tel(s, env("confirm.parked", { tool: "gmail_send", agent: "agent.pepper" }));
    s = tel(s, env("policy.auto_approved", { tool: "slack_post_message", agent: "darwin" }));
    expect(s.liveGate.length).toBe(3);
    // newest-first
    expect(s.liveGate[0].kind).toBe("auto_approved");
    expect(s.liveGate[0].tool).toBe("slack_post_message");
    expect(s.liveGate[2].kind).toBe("blocked");
  });

  it("bounds the live ring at LIVE_GATE_CAP", () => {
    let s = connected();
    for (let i = 0; i < LIVE_GATE_CAP + 10; i++) {
      s = tel(s, env("policy.blocked", { tool: `tool_${i}`, agent: "a" }));
    }
    expect(s.liveGate.length).toBe(LIVE_GATE_CAP);
  });

  it("a live chokepoint event never stores a secret", () => {
    const s = tel(
      connected(),
      env("policy.auto_approved", { tool: "gmail_send", agent: "a", body: "secret-body", token: "sk-X" }),
    );
    const blob = JSON.stringify(s.liveGate);
    expect(blob).not.toContain("secret-body");
    expect(blob).not.toContain("sk-X");
  });

  it("audit.truncated flips the truncated flag on the loaded snapshot", () => {
    let s = tel(connected(), env("audit.snapshot", mockAudit));
    expect(s.audit!.truncated).toBe(false);
    s = tel(s, env("audit.truncated", { removed: 100, kept: 9900 }));
    expect(s.audit!.truncated).toBe(true);
  });

  /* REGRESSION: the periodic audit_snapshot_task re-emits audit.snapshot every
     15s and the daemon's snapshot payload has NO `truncated` key, so a blind
     replace silently erased the re-root disclosure one tick after the prune.
     The rendered "chain was RE-ROOTED by a prune" notice must survive. */
  it("a later audit.snapshot does NOT erase a prune's re-root disclosure", () => {
    let s = tel(connected(), env("audit.snapshot", mockAudit));
    s = tel(s, env("audit.truncated", { removed: 100, kept: 9900 }));
    expect(s.audit!.truncated).toBe(true);
    // The daemon's REAL next snapshot: no `truncated` key on the wire.
    s = tel(s, env("audit.snapshot", { ...mockAudit, total: 9900 }));
    expect(s.audit!.truncated).toBe(true);
    expect(s.audit!.total).toBe(9900); // the rest of the snapshot IS authoritative
    const html = renderToStaticMarkup(
      createElement(AuditPanel, { audit: s.audit, liveGate: [] }),
    );
    expect(html).toContain("RE-ROOTED");
  });

  it("audit.truncated is a no-op (same ref) when no snapshot is loaded", () => {
    const before = connected();
    const after = tel(before, env("audit.truncated", { removed: 1, kept: 1 }));
    expect(after.audit).toBeNull();
  });
});

/* ======================================================================== *
 * AuditPanel (review-only, honest, secret-free)                              *
 * ======================================================================== */
describe("AuditPanel (review-only, honest)", () => {
  const render = (audit: AuditSnapshot | null, liveGate: LiveGateEvent[] = []) =>
    renderToStaticMarkup(createElement(AuditPanel, { audit, liveGate }));

  it("renders nothing before any snapshot or live event", () => {
    expect(render(null, [])).toBe("");
  });

  it("shows the chain-OK indicator and the recent decisions", () => {
    const html = render(parseAuditSnapshot(mockAudit));
    expect(html).toContain("REVIEW ONLY");
    expect(html).toContain("CHAIN OK");
    expect(html).toContain("3 entries verified");
    // the decisions + outcomes
    expect(html).toContain("gmail_send");
    expect(html).toContain("EXECUTED");
    expect(html).toContain("x_post");
    expect(html).toContain("BLOCKED");
    expect(html).toContain("PARKED");
    // the redacted target is shown
    expect(html).toContain("#ops");
  });

  it("shows a TAMPER verdict with where it broke", () => {
    const html = render(
      parseAuditSnapshot({
        enabled: true,
        chain: { ok: false, count: 5, broken_seq: 4, reason: "entry_hash mismatch (a field was altered)" },
        entries: [],
      }),
    );
    expect(html).toContain("CHAIN TAMPER DETECTED");
    expect(html).toContain("#4");
    expect(html).toContain("entry_hash mismatch");
  });

  it("surfaces the HONEST copy: tamper-EVIDENT not tamper-PROOF; backstops; NEVER wins", () => {
    const html = render(parseAuditSnapshot(mockAudit));
    expect(html).toContain("tamper-EVIDENT");
    expect(html).toContain("not tamper-PROOF");
    expect(html.toLowerCase()).toContain("rewrites the whole on-disk chain");
    expect(html).toContain("master switch");
    expect(html).toContain("voice-id");
    expect(html.toLowerCase()).toContain("never always wins");
  });

  /* ====================================================================== *
   * THE EXTERNAL ANCHOR ROW                                                *
   *                                                                        *
   * The defect this covers: `verify_chain` re-derives the chain from the   *
   * very bytes an attacker rewrote, so a CONSISTENT whole-chain rewrite —  *
   * the one tamper the panel's own footer admits it cannot catch — showed  *
   * the owner a green CHAIN OK. The daemon HAS the detector (a Keychain    *
   * witness in a separate OS protection domain, checked at every start)    *
   * and its verdict reached nothing but a warn! line in daemon.log,        *
   * because applyEnvelope has no `audit.anchor` case and never will: that  *
   * frame fires once at boot, before any HUD connects. The verdict now     *
   * rides audit.snapshot.                                                  *
   * ====================================================================== */

  it("draws the DIVERGED witness even while the local chain verifies clean", () => {
    // This exact combination — chain.ok true, anchor mismatched — IS the
    // whole-chain-rewrite signature. If the panel only ever showed one of the
    // two verdicts, this is the case it would get wrong.
    const html = render(
      parseAuditSnapshot({
        enabled: true,
        chain: { ok: true, count: 412 },
        anchor: {
          ok: false,
          state: "mismatch",
          anchored_seq: 300,
          anchored_head: "wit-abc",
          live_seq: 412,
          live_head: "live-def",
          checked_ts: "2026-08-10T09:12:00Z",
        },
        entries: [],
      }),
    );
    expect(html).toContain("CHAIN OK"); // the local verdict is still reported honestly
    expect(html).toContain("EXTERNAL ANCHOR DIVERGED");
    expect(html).toContain("#300");
    expect(html).toContain("#412");
    // The owner is told what it means and what to do — a red pill they cannot
    // act on is the same silence with a colour.
    expect(html.toLowerCase()).toContain("triage bundle");
    expect(html.toLowerCase()).toContain("will not re-witness on its own");
    // …and no button re-witnesses for them: one click to bless a rewritten
    // chain is precisely what the daemon refuses to do automatically.
    expect(html).not.toContain("<button");

    // THE OTHER MISMATCH SHAPE, and the one a naive `#${liveSeq}` renders as
    // "#null": the daemon reports `live: None` when an anchor exists and the
    // chain is EMPTY — the whole log gone from under the witness, which is what
    // a wiped state dir looks like. It has to read as words, not a null.
    const gone = render(
      parseAuditSnapshot({
        enabled: true,
        chain: { ok: true, count: 0 },
        anchor: { ok: false, state: "mismatch", anchored_seq: 300, live_seq: null },
        entries: [],
      }),
    );
    expect(gone).toContain("EXTERNAL ANCHOR DIVERGED");
    expect(gone).toContain("GONE");
    expect(gone).not.toContain("#null");
  });

  it("says CORROBORATED for a benign verdict, and dates the reading not the frame", () => {
    // A row that can only ever go red is a row nobody believes when it does.
    const html = render(
      parseAuditSnapshot({
        enabled: true,
        chain: { ok: true, count: 5 },
        anchor: { ok: true, state: "extended", anchored_seq: 3, live_seq: 5, checked_ts: "2026-08-10T09:12:00Z" },
        entries: [],
      }),
    );
    expect(html).toContain("EXTERNAL ANCHOR OK");
    expect(html).toContain("corroborates");
    expect(html).toContain("checked "); // the reading's own age, not the 15s tick's
    expect(html).not.toContain("DIVERGED");
  });

  it("claims NOTHING when the daemon sent no anchor, and never a false green", () => {
    // An older daemon, an audit-off daemon, and a check that never ran all look
    // identical on the wire. None of them is evidence of anything.
    const silent = parseAuditSnapshot({ enabled: true, chain: { ok: true, count: 2 }, entries: [] });
    expect(silent.anchor).toBeNull();
    const html = render(silent);
    expect(html).not.toContain("EXTERNAL ANCHOR");

    // NOT-YET-WITNESSED must not tell the owner to wait for a restart. `no_anchor`
    // is benign, so `verify_and_reanchor_on_start` witnesses the head in the SAME
    // start — a few lines after the reading this row is showing — whenever the
    // chain is non-empty. That is every existing install the first time the anchor
    // shipped. Only an EMPTY chain really waits for a later start.
    const unwitnessed = render(
      parseAuditSnapshot({
        enabled: true,
        chain: { ok: true, count: 4 },
        anchor: { ok: true, state: "no_anchor" },
        entries: [],
      }),
    );
    expect(unwitnessed).toContain("NOT YET WITNESSED");
    expect(unwitnessed).toContain("once there is a chain to witness");
    expect(unwitnessed).not.toContain("the next start establishes one");

    // Junk is silent in the ALARM direction…
    expect(parseAuditSnapshot({ anchor: "nope" }).anchor).toBeNull();
    expect(parseAuditSnapshot({ anchor: {} }).anchor).toBeNull();
    // …and fails CLOSED in the reassurance direction: a future/garbled state
    // token can never be read as a corroborating witness.
    const weird = parseAuditSnapshot({ anchor: { ok: true, state: "banana" } }).anchor;
    expect(weird!.ok).toBe(false);
    expect(render(parseAuditSnapshot({ enabled: true, chain: { ok: true, count: 1 }, anchor: { ok: true, state: "banana" }, entries: [] })))
      .toContain("EXTERNAL ANCHOR — UNCLEAR");
  });

  it("carries the anchor through the reducer on the audit.snapshot frame", () => {
    // The daemon folds it onto audit.snapshot rather than emitting a topic of
    // its own; if the reducer dropped it, everything above would be untested UI.
    const s = reduce(initialState(), {
      type: "telemetry",
      envelope: {
        ts: "2026-08-10T09:12:05Z",
        source: "system",
        event: "audit.snapshot",
        data: {
          enabled: true,
          total: 412,
          chain: { ok: true, count: 412 },
          anchor: { ok: false, state: "mismatch", anchored_seq: 300, live_seq: 412 },
          entries: [],
        },
      } as TelemetryEnvelope,
      at: 1,
    });
    expect(s.audit!.anchor!.state).toBe("mismatch");
    expect(s.audit!.anchor!.ok).toBe(false);
  });

  it("has NO action button — it is review-only", () => {
    const html = render(parseAuditSnapshot(mockAudit));
    expect(html).not.toContain("<button");
  });

  it("never renders a secret / chain byte even from a hostile snapshot", () => {
    const html = render(
      parseAuditSnapshot({
        enabled: true,
        chain: { ok: true, count: 1 },
        entries: [
          {
            seq: 1,
            tool: "gmail_send",
            target_redacted: "to a@example.com",
            decision: "always",
            outcome: "executed",
            token: "sk-SECRET",
            input: { body: "the secret body" },
            entry_hash: "deadbeef",
          },
        ],
      }),
    );
    expect(html).not.toContain("SECRET");
    expect(html).not.toContain("the secret body");
    expect(html).not.toContain("deadbeef");
  });

  it("folds in the LIVE chokepoint events before the snapshot entries", () => {
    const live: LiveGateEvent[] = [
      { kind: "auto_approved", tool: "gmail_send", agent: "a", via: "mcp", ts: "2026-06-16T12:00:09Z", seq: 9 },
    ];
    const html = render(parseAuditSnapshot(mockAudit), live);
    expect(html).toContain("LIVE");
    expect(html).toContain("AUTO-APPROVED");
    expect(html).toContain("mcp");
  });

  it("shows the honest empty state when nothing has been recorded", () => {
    const html = render(parseAuditSnapshot({ enabled: true, chain: { ok: true, count: 0 }, entries: [] }));
    expect(html.toLowerCase()).toContain("no consequential decision recorded yet");
    expect(html.toLowerCase()).toContain("ask");
  });

  it("shows the truncation note when a prune re-rooted the chain", () => {
    const html = render(parseAuditSnapshot({ ...mockAudit, truncated: true }));
    expect(html.toLowerCase()).toContain("re-rooted");
    expect(html.toLowerCase()).toContain("still verifies");
  });

  it("shows the honest OFF state when audit is disabled", () => {
    const html = render(parseAuditSnapshot({ enabled: false, entries: [] }));
    expect(html.toLowerCase()).toContain("audit log is off");
  });
});

/* ======================================================================== *
 * SettingsModal — POLICY editor (user-set only, honest)                      *
 * ======================================================================== */
function renderSettings(policy: PolicySnapshot | null): string {
  return renderToStaticMarkup(
    createElement(SettingsModal, {
      mcp: null,
      voiceId: voiceIdInitial(),
      modelTier: modelTierInitial(),
      sttTier: sttTierInitial(),
      policy,
      onClose: noop,
    }),
  );
}

describe("SettingsModal policy editor (user-set only, honest)", () => {
  it("shows the section with the ALWAYS / NEVER / ASK controls", () => {
    const html = renderSettings(parsePolicySnapshot({ enabled: true, rules: [] }));
    expect(html).toContain("CONSEQUENTIAL POLICY");
    expect(html).toContain("ALWAYS ALLOW");
    expect(html).toContain("NEVER");
    expect(html).toContain("ASK (DEFAULT)");
  });

  it("renders the honest empty / ASK-everywhere state", () => {
    const html = renderSettings(parsePolicySnapshot({ enabled: true, rules: [] }));
    expect(html).toContain("EMPTY · ASK EVERYWHERE");
  });

  it("lists the user-set rules with their decisions + scope", () => {
    const html = renderSettings(parsePolicySnapshot(mockPolicy));
    expect(html).toContain("gmail_send");
    expect(html).toContain("x_post");
    expect(html).toContain("slack_post_message");
    // the scoped recipient is shown
    expect(html).toContain("#ops");
    // both ALWAYS and NEVER decisions render
    expect(html).toContain(">ALWAYS<");
    expect(html).toContain(">NEVER<");
  });

  /* REGRESSION — a control that could not do its job. The ✕ rides
     POLICY_PHRASES.ask, which names only the TOOL, so the daemon rebuilds
     PolicyScope::tool(tool) with agent=None/recipient=None; PolicyStore::clear
     keys on the full (tool, agent, recipient) triple and never matches a
     scope-narrowed rule. Nothing was removed — and because clear() returned
     false, apply_global returned false, whose ack is "the policy layer is off, so
     nothing changed. Enable [policy]...". A user revoking a standing AUTO-APPROVE
     was told the layer was disabled while the rule stayed in force. */
  it("the ✕ is DISABLED on a scope-narrowed rule and says why (it cannot clear it)", () => {
    const html = renderSettings(
      parsePolicySnapshot({
        enabled: true,
        rules: [
          { scope: { tool: "gmail_send", agent: "agent.pepper" }, decision: "always" },
        ],
      }),
    );
    expect(html).toContain("agent.pepper");
    expect(html).toContain('aria-label="clear policy rule for gmail_send"');
    // The control is inert rather than silently doing nothing. (This suite does
    // not mock inTauri(), so `shell` is false and every button carries
    // `disabled` — the DISCRIMINATING signal is therefore the title, asserted
    // next, which flips on `scopeNarrowed` alone.)
    expect(html).toMatch(/cred-remove"[^>]*disabled=""/);
    // It must name the real remedy, not blame the policy layer.
    expect(html).toContain("SCOPE-NARROWED");
    expect(html).toContain("state/policy.json");
    expect(html).not.toContain("Clear this rule back to ASK");
    expect(html.toLowerCase()).not.toContain("policy layer is off");
  });

  it("the ✕ keeps its normal CLEAR affordance on a tool-only rule (the phrase matches it)", () => {
    const html = renderSettings(
      parsePolicySnapshot({
        enabled: true,
        rules: [{ scope: { tool: "gmail_send" }, decision: "always" }],
      }),
    );
    expect(html).toContain('aria-label="clear policy rule for gmail_send"');
    expect(html).toContain("Clear this rule back to ASK");
    expect(html).not.toContain("SCOPE-NARROWED");
  });

  it("surfaces the HONEST invariants: master ceiling, NEVER wins, user-set only, inert ALWAYS", () => {
    const html = renderSettings(parsePolicySnapshot({ enabled: true, rules: [] }));
    expect(html).toContain("allow_consequential");
    expect(html.toLowerCase()).toContain("master switch");
    expect(html.toLowerCase()).toContain("inert"); // ALWAYS inert when master off
    expect(html).toContain("USER-SET ONLY");
    expect(html.toLowerCase()).toContain("cannot take effect"); // injected set-policy cannot fire
    // "NEVER always wins" — the words bracket a <b> tag in the rendered markup,
    // so assert the surrounding hard-block clause rather than a contiguous string.
    expect(html.toLowerCase()).toContain("always wins");
    expect(html.toLowerCase()).toContain("hard-block");
    expect(html.toLowerCase()).toContain("ask everywhere");
  });

  it("the policy phrases are explicit, tool-named, user-only writes", () => {
    // The HUD half of the round-trip a daemon classifier test will lock. These
    // are USER-SET writes over the command channel — there is no other write path.
    expect(POLICY_PHRASES.always("gmail_send")).toBe("always allow the gmail_send action");
    expect(POLICY_PHRASES.never("x_post")).toBe("never allow the x_post action");
    expect(POLICY_PHRASES.ask("slack_post_message")).toBe(
      "always ask before the slack_post_message action",
    );
    // each names the verb AND the specific tool — never a blanket all-tools rule
    expect(POLICY_PHRASES.always("t")).toContain("t");
    expect(POLICY_PHRASES.never("t")).toContain("never");
  });

  it("renders the AWAITING state when no policy snapshot has arrived", () => {
    const html = renderSettings(null);
    expect(html).toContain("AWAITING");
  });
});
