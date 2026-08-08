// The component source as TEXT (Vite `?raw`, typed by vite/client — no
// @types/node, which this tsconfig does not carry). Used only for the
// source-anchored guards below.
import SETTINGS_MODAL_SRC from "../components/SettingsModal.tsx?raw";
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
