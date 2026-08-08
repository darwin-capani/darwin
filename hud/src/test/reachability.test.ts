// The component source as TEXT (Vite `?raw`, typed by vite/client — no
// @types/node, which this tsconfig does not carry). Used only for the
// source-anchored guards below.
import SETTINGS_MODAL_SRC from "../components/SettingsModal.tsx?raw";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import SettingsModal, {
  MODEL_SWAP_BUTTON_PHRASES,
  MODEL_TIER_SPOKEN_ONLY_NOTE,
  VOICE_CLONE_PHRASES,
  modelTierSpokenInstruction,
} from "../components/SettingsModal";
import {
  modelTierInitial,
  sttTierInitial,
  voiceIdInitial,
  type ModelSwapIntent,
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

describe("model-tier controls are spoken-only (router-only intent, no tool)", () => {
  it("the spoken instruction quotes the EXACT classifier-anchored phrase", () => {
    const intents: ModelSwapIntent[] = ["heavy", "fast", "local", "auto"];
    for (const intent of intents) {
      const line = modelTierSpokenInstruction(intent);
      // The phrase must appear verbatim and quoted — a paraphrase would not
      // classify (`classify_model_swap` is conservatively anchored).
      expect(line).toContain(`"${MODEL_SWAP_BUTTON_PHRASES[intent]}"`);
      expect(line.toLowerCase()).toContain("say");
      expect(line).toContain(MODEL_TIER_SPOKEN_ONLY_NOTE);
    }
    // Distinct intents must not collapse onto one phrase.
    const lines = new Set(intents.map(modelTierSpokenInstruction));
    expect(lines.size).toBe(4);
  });

  it("ModelTierSection sends NOTHING over the command channel", () => {
    const body = sectionBody("ModelTierSection");
    // Guard against a vacuous pass: the region must really be the model-tier
    // section (a bad slice would be an empty string that trivially "has no send").
    expect(body.length).toBeGreaterThan(400);
    expect(body).toContain("Set tier");
    expect(body).toContain("modelTierSpokenInstruction(");
    // The CALL, not the identifier: an import/type mention must not satisfy this.
    expect(body).not.toContain("sendCommand(");
  });

  it("the panel no longer claims the buttons send a model-control command", () => {
    const html = renderSettings();
    // Still four controls (the capability is real — it is just spoken).
    expect(html).toContain("HEAVY");
    expect(html).toContain("FAST");
    expect(html).toContain("LOCAL");
    expect(html).toContain("AUTO");
    // The honest resting hint replaced "Sends the same spoken model-control
    // command the voice path uses" — the sentence that made a dead click read
    // as an applied override.
    expect(html).not.toContain("Sends the same spoken model-control command");
    expect(html.toLowerCase()).toContain("spoken-only");
  });

  // WHAT THE FIRST PASS MISSED. The resting hint and the button titles were
  // corrected, but the durable-config note UNDER the same four controls still
  // read: "The buttons above set a RUNTIME override (...); say "use the most
  // powerful model", "fast mode", "work offline", or "auto" for the same effect
  // by voice." Two separate false claims survived in the fixed section:
  //   1. it told the user a CLICK sets the override — the exact sentence the
  //      pass existed to delete, still rendered, one paragraph lower; and
  //   2. it told the user to say "auto", which `classify_model_swap` does NOT
  //      recognize (its AUTO table opens at "auto mode"), so that utterance
  //      classifies as None, leaks to the ordinary answer path, and leaves the
  //      override exactly where it was — a documented voice phrase that reaches
  //      nothing, which is this class of defect in its other form.
  it("the durable-config note neither claims a click sets the override nor quotes an un-anchored phrase", () => {
    const html = renderSettings();
    expect(html).not.toContain("The buttons above set a RUNTIME override");

    // Scope to the model-tier note: the voice-clone note quotes ITS OWN phrases
    // in the same markup, and an unscoped sweep would mix the two. BOTH bounds
    // are asserted, so a bad slice cannot pass this vacuously.
    const start = html.indexOf("The DURABLE default lives in");
    const end = html.indexOf("MEMORY // EPISODES");
    expect(start, "the model-tier durable-config note must render").toBeGreaterThan(-1);
    expect(end).toBeGreaterThan(start);
    const note = html.slice(start, end);
    expect(note).toContain("conversation_route");

    // Every phrase the note tells the user to SAY must be a literal
    // classify_model_swap anchor. Using MODEL_SWAP_BUTTON_PHRASES as the allowed
    // set closes the round-trip: the daemon's
    // `settings_button_phrases_round_trip_to_their_intent` already pins those
    // four literals to the classifier, so a phrase printed here cannot drift
    // away from something the daemon recognizes.
    const quoted = Array.from(note.matchAll(/<b>&quot;(.*?)&quot;<\/b>/g)).map((m) => m[1]);
    expect(quoted.length, "the note must still name the spoken phrases").toBe(4);
    const anchored = Object.values(MODEL_SWAP_BUTTON_PHRASES);
    for (const phrase of quoted) expect(anchored).toContain(phrase);
    // All four intents, not the same phrase four times.
    expect(new Set(quoted).size).toBe(4);
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
