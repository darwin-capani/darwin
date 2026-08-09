// The component source as TEXT (Vite `?raw`, typed by vite/client — no
// @types/node, which this tsconfig does not carry). Used only for the
// source-anchored guards below.
import SETTINGS_MODAL_SRC from "../components/SettingsModal.tsx?raw";
import APP_SRC from "../App.tsx?raw";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import SettingsModal, {
  MODEL_SWAP_BUTTON_PHRASES,
  VOICE_CLONE_PHRASES,
} from "../components/SettingsModal";
import {
  modelTierInitial,
  sttTierInitial,
  voiceIdInitial,
} from "../core/events";

/* ==========================================================================
 * REACHABILITY — a HUD control must not claim an operation the command
 * channel cannot perform.
 *
 * THE RULE (from the voice-id Enroll/Forget and mirror "contest" regressions):
 * a control is DEAD iff its intent lives in the daemon's ROUTER and there is no
 * corresponding TOOL. `{cmd:"ask"}` reaches `LivePipeline::ask` ->
 * `anthropic::complete_with_tools`; `router::route` is never entered, so every
 * router-only fast path (`classify_model_swap`, `voiceclone::classify_intent`,
 * `voiceid::classify_intent`, `user_model::contest_belief`) is unreachable from
 * a click. These tests pin the two controls that were still firing into it.
 * ========================================================================== */

/** The body of one top-level `function <name>(` in SettingsModal.tsx, from its
 *  signature up to the next top-level `function `/`export ` declaration. Scoping
 *  matters: an unscoped `src.includes("sendCommand(")` would pass on the file's
 *  OTHER, legitimate senders (LockdownSection's panic/unlock, PolicySection's
 *  `policy` verb) and prove nothing about the region under test. */
function sectionBody(name: string): string {
  const start = SETTINGS_MODAL_SRC.indexOf(`\nfunction ${name}(`);
  expect(start, `${name} must exist in SettingsModal.tsx`).toBeGreaterThan(-1);
  const after = start + 1;
  const nextFn = SETTINGS_MODAL_SRC.indexOf("\nfunction ", after);
  const nextExport = SETTINGS_MODAL_SRC.indexOf("\nexport ", after);
  const candidates = [nextFn, nextExport].filter((i) => i > -1);
  const end = candidates.length > 0 ? Math.min(...candidates) : SETTINGS_MODAL_SRC.length;
  return SETTINGS_MODAL_SRC.slice(after, end);
}

const noop = () => {};

function renderSettings(): string {
  return renderToStaticMarkup(
    createElement(SettingsModal, {
      mcp: null,
      voiceId: voiceIdInitial(),
      modelTier: modelTierInitial(),
      sttTier: sttTierInitial(),
      onClose: noop,
    }),
  );
}

/* --------------------------------------------------------- MODEL TIER */

describe("model-tier controls now reach the daemon — and still cannot leak a LOCAL turn", () => {
  // SUPERSEDED DELIBERATELY. These two tests previously asserted the buttons send
  // NOTHING. That was correct when `classify_model_swap` was reachable only from
  // `route()` and the command channel had no model-tier tool: a click set no
  // override, and the panel printed the cloud model's chatter as the result.
  //
  // The daemon now handles the tier swap on the command channel too — in
  // LivePipeline::ask, BEFORE any cloud call, calling the same
  // `model_tier::set_override` the router calls and keeping the same guest rail.
  // So the honest state changed, and the assertions change with it rather than
  // being deleted.
  it("the buttons send the canonical phrase the daemon classifies", () => {
    for (const intent of ["heavy", "fast", "local", "auto"] as const) {
      const phrase = MODEL_SWAP_BUTTON_PHRASES[intent];
      expect(phrase.length).toBeGreaterThan(0);
      // The phrase is what the daemon's classifier keys on; a rewrite here that
      // no longer classifies would silently make the button dead again.
      expect(phrase).toMatch(/model|mode|offline|private|auto|fast|best/i);
    }
  });

  // ROUND 2 — THE SAME CLASS WITH THE SIGN FLIPPED. Wiring the buttons made the
  // surrounding copy false: four tooltips still said "spoken only; shows the
  // phrase to say" and the panel body still said "The controls above do NOT set
  // the override". A working control the UI disclaims is as unreachable as a dead
  // one, and it is the harder defect to notice because everything WORKS.
  it("the tier controls no longer disclaim the capability they now have", () => {
    const body = sectionBody("ModelTierSection");
    expect(body.length).toBeGreaterThan(400);
    // The section really does send — that is what makes the old copy false, and
    // without this the guard below would pass on a section that sends nothing.
    expect(body).toContain('sendCommand({ cmd: "ask", text: MODEL_SWAP_BUTTON_PHRASES[intent] })');
    expect(body).not.toContain("spoken only; shows the phrase to say");
    expect(body).not.toContain("do NOT set the override");
    // ...and it is gone from what actually RENDERS, not merely from the source.
    const html = renderSettings();
    expect(html).toContain("Set tier");
    expect(html).not.toContain("do NOT set the override");
  });

  // The retracted rule must not survive above the code that breaks it: 20c617b
  // wired the contest button and appended its explanation, leaving "CONTEST IS
  // SPOKEN-ONLY" and "Not routed here deliberately" standing directly above the
  // handler that now routes it. Bounded at BOTH ends — the 2000 chars of comment
  // immediately preceding the handler — because an unscoped search over App.tsx
  // would run through unrelated code and prove nothing about this block.
  it("the contest handler carries no superseded spoken-only rule above it", () => {
    const at = APP_SRC.indexOf("const contestBelief = useCallback");
    expect(at).toBeGreaterThan(-1);
    const preamble = APP_SRC.slice(Math.max(0, at - 2000), at);
    // The window really is the comment block (else the two absences are vacuous).
    expect(preamble).toContain("user_model::contest_belief");
    expect(preamble).not.toContain("CONTEST IS SPOKEN-ONLY");
    expect(preamble).not.toContain("Not routed here deliberately");
  });

  it("LOCAL cannot leak the turn that asked to stay on-device", () => {
    // This is the one that used to be actively harmful: firing it did nothing AND
    // sent the very turn asking to stay local to the cloud. The daemon's swap arm
    // RETURNS before complete_with_tools is reached, which is asserted on the
    // daemon side by
    // command::tests::the_command_channel_handles_a_tier_swap_before_reaching_the_cloud.
    // Here we pin the HUD half: LOCAL's phrase is a model-control phrase, not a
    // question that would be answered by a cloud turn.
    expect(MODEL_SWAP_BUTTON_PHRASES.local).toMatch(/offline|private|local|device/i);
  });
});

/* -------------------------------------------------------- VOICE CLONE */

describe("voice-clone controls send nothing (consent machine is spoken-path)", () => {
  it("VoiceCloneSection sends NOTHING over the command channel", () => {
    const body = sectionBody("VoiceCloneSection");
    expect(body.length).toBeGreaterThan(400);
    expect(body).toContain("VOICE_CLONE_PHRASES.propose");
    expect(body).toContain("VOICE_CLONE_PHRASES.forget");
    // Previously: `await send(VOICE_CLONE_PHRASES.confirm)` etc., where `send`
    // wrapped sendCommand({cmd:"ask"}). The bare confirmation and the forget
    // phrase were pushed into complete_with_tools, which holds forget-shaped
    // tools of its own.
    expect(body).not.toContain("sendCommand(");
  });

  it("the unreachable CONFIRM/CANCEL step is gone, not merely hidden", () => {
    const body = sectionBody("VoiceCloneSection");
    // `proposed` could never become true once the fabricated "AWAITING CONFIRM"
    // latch was removed, so this branch was UI no render could reach.
    expect(body).not.toContain("CONFIRM CLONE (UPLOADS SAMPLE)");
    // The latch's own setter, not the word: the section doc still explains why
    // the branch was removed, and prose must not satisfy this guard.
    expect(body).not.toContain("setProposed(");
    const html = renderSettings();
    expect(html).toContain("CLONE MY VOICE");
    expect(html).toContain("FORGET CLONE");
    expect(html).not.toContain("CONFIRM CLONE (UPLOADS SAMPLE)");
    // The two-step gate still exists — daemon-side, on the spoken path.
    expect(html).toContain(VOICE_CLONE_PHRASES.propose);
  });
});

/* ------------------------------------------------------ MIC SOURCE */

// REMOVED: a suite asserting that voice.mic_source="app" "leaves DARWIN with no
// microphone". That premise is FALSE — it came from a grep scoped to hud/src (the
// webview), which is not where capture lives. The daemon still opens the mic under
// that setting; the option works. Shipping the warning would have told users not
// to pick a working feature, and disabled it in practice.

/* ------------------------------------------------- GREP-VISIBLE SOURCE */

/* ------------------------------------- THE PHRASE TABLES' OWN DOCS */

/* Round 2 caught the tooltips and the panel body. It did not catch the two
 * module-scope doc blocks, because its guard is scoped to sectionBody(...) and
 * these sit above every section. Both stated a rule their own code had already
 * retracted:
 *   - MODEL_TIER_SPOKEN_ONLY_NOTE's JSDoc explained why the tier controls state
 *     a phrase INSTEAD of acting — one line above the `/// SUPERSEDED. These
 *     buttons WORK now.` that retracts it;
 *   - VOICE_CLONE_PHRASES' JSDoc opened by describing the command-channel sends
 *     that a565029 deleted, which the test above pins as absent.
 * Each window is bounded at BOTH ends by the block's own delimiters and asserts
 * a PRECONDITION first, so a bound that captured nothing cannot pass it
 * vacuously. A FIXED-SPAN window was tried first and was WRONG: 1400 chars back
 * from `export const VOICE_CLONE_PHRASES` starts INSIDE a 1601-char block, past
 * the very opening line under test, and the mutation restoring that line
 * SURVIVED. The bound is the block delimiter now, not a guessed number. */
describe("the phrase tables' docs match what their controls actually do", () => {
  /** The doc comment block immediately preceding `marker`: from the `/**` that
   *  opens it (the last one before the marker) up to the marker itself. */
  function docAbove(marker: string): string {
    const at = SETTINGS_MODAL_SRC.indexOf(marker);
    expect(at, `${marker} must exist in SettingsModal.tsx`).toBeGreaterThan(-1);
    const open = SETTINGS_MODAL_SRC.lastIndexOf("\n/**", at);
    expect(open, `${marker} must carry a doc block`).toBeGreaterThan(-1);
    return SETTINGS_MODAL_SRC.slice(open, at);
  }

  it("the model-tier note is not introduced as a reason the controls do not act", () => {
    // PRECONDITION: the section really does send, which is what makes the old
    // doc false. Without it this test would pass on a dead control.
    expect(sectionBody("ModelTierSection")).toContain(
      'sendCommand({ cmd: "ask", text: MODEL_SWAP_BUTTON_PHRASES[intent] })',
    );
    const doc = docAbove("export const MODEL_TIER_SPOKEN_ONLY_NOTE");
    // The window really is the doc block (else the absences below are vacuous).
    expect(doc).toContain("SUPERSEDED");
    expect(doc).not.toContain("state a phrase instead of acting");
    expect(doc).not.toContain("not reachable from the command channel this HUD holds");
    const helper = docAbove("export function modelTierSpokenInstruction");
    expect(helper).toContain("MODEL_SWAP_BUTTON_PHRASES");
    expect(helper).not.toContain("why it is spoken-only");
  });

  it("the voice-clone table is not introduced as phrases the control sends", () => {
    // PRECONDITION: the section really sends nothing — the fact the doc denied.
    expect(sectionBody("VoiceCloneSection")).not.toContain("sendCommand(");
    const doc = docAbove("export const VOICE_CLONE_PHRASES");
    // The window must reach the block's FIRST line, which is where the retracted
    // claim lived; a short window would silently skip past it.
    expect(doc).toContain("voiceclone::classify_intent");
    expect(doc).toContain("QUOTES for the user to SAY");
    expect(doc).not.toContain("sends over the command channel");
    expect(doc).not.toContain("a single click can never upload");
  });
});

describe("HUD sources stay greppable", () => {
  it("SettingsModal.tsx carries no raw NUL byte", () => {
    // A literal NUL in `ruleKey`'s template made grep/ugrep treat the whole file
    // as BINARY and skip it silently — every grep-based audit (including the
    // source-anchored guards above, when run from a shell) missed this file and
    // its five command-channel call sites. The escape is the same string.
    const RAW_NUL = String.fromCharCode(0);
    expect(SETTINGS_MODAL_SRC.includes(RAW_NUL)).toBe(false);
    // ...and the separator is still U+0000, so no two distinct rule scopes can
    // collide on a shared React key.
    const ruleKey = SETTINGS_MODAL_SRC.slice(
      SETTINGS_MODAL_SRC.indexOf("function ruleKey("),
    ).slice(0, 700);
    expect(ruleKey.split("\\u0000").length - 1).toBe(2);
  });
});
