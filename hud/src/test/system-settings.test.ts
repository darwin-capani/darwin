import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import SystemSettingsPanel from "../components/SystemSettingsPanel";
import {
  AUTONOMY_IDS,
  AUTONOMY_OPTIONS,
  CATALOG,
  GROUP_ORDER,
  type CatalogEntry,
  dangerousPending,
  entriesForGroup,
  entryById,
  isDangerousChange,
  pendingChanges,
  valueMapFromStates,
} from "../core/systemSettings";
import type { Change, SettingState, SettingValue } from "../tauri/configSettings";

/* The SYSTEM SETTINGS catalog is the UI-facing mirror of the backend whitelist
 * (src-tauri/src/config_settings.rs SETTINGS + AUTONOMY_SECTIONS). These pin the
 * groups, the batched-diff model, the dangerous-change confirm gating, and the
 * KEY invariant: the catalog can only offer ids the backend already allows
 * (a drift on either side fails CI). */

/* ------------------------------------------- backend whitelist (mirror) */

/** The EXACT id set the backend whitelist allows — a static mirror of
 *  src-tauri/src/config_settings.rs SETTINGS (each "section.key") plus the
 *  AUTONOMY_SECTIONS (the bare 3-way ids). This is the same "explicit expected
 *  list" discipline credentials.test.ts uses: a drift on either side fails CI,
 *  and the backend ALSO has its own Rust drift test against the real config
 *  file (every_whitelisted_key_exists_in_the_real_config_file). */
const BACKEND_WHITELIST_IDS: string[] = [
  // SAFETY & GATES
  "integrations.allow_consequential",
  "voice_id.enabled",
  "voice_id.gate_scope",
  "voice_id.threshold",
  "security.encrypt_memory",
  "policy.enabled",
  // AUTONOMY (flat toggles)
  "standing.enabled",
  "drafts.enabled",
  "missions.durable",
  "macros.enabled",
  // PROACTIVITY
  "proactive.enabled",
  "proactive.speak",
  "proactive.suggest",
  "proactive.quiet_start",
  "proactive.quiet_end",
  "focus.profile",
  // PERCEPTION
  "screen_context.enabled",
  "screen_context.interval_secs",
  "vision.enabled",
  "image.enabled",
  "audio.sound_monitor",
  "interpret.live",
  "interpret.speak",
  "episodic.enabled",
  // VOICE & SPEECH
  "voice.cloud_tier",
  "voice.cloud_stt",
  "voice.adaptive_prosody",
  "voice.whisper",
  "voice.whisper_auto",
  "voice.diarize",
  "speech.engine",
  "speech.instant_opener",
  // CAPABILITIES
  "shell.enabled",
  "ui_automation.enabled",
  "mcp.enabled",
  "webhooks.enabled",
  "plugin_sdk.enabled",
  "docsearch.enabled",
  "docsearch.build_graph",
  "code.enabled",
  "local_tools.enabled",
  "report.enabled",
  "chart.enabled",
  "answers.cite",
  "answers.confidence",
  "answers.verify",
  "answers.cross_check",
  "answers.debate",
  // PERFORMANCE & MODELS
  "power.adaptive",
  "inference.speculative",
  "inference.quant",
  "router.conversation_route",
  // AUTONOMY 3-way (bare section ids — AUTONOMY_SECTIONS)
  "self_heal",
  "forge",
  "optimize",
];

describe("system settings catalog ↔ backend whitelist", () => {
  const backend = new Set(BACKEND_WHITELIST_IDS);

  it("the whitelist mirror is the expected size (flat settings + 3 autonomy)", () => {
    expect(backend.size).toBe(BACKEND_WHITELIST_IDS.length); // no dup in the mirror
    expect(backend.size).toBeGreaterThan(40);
    expect(backend.has("integrations.allow_consequential")).toBe(true);
    expect(backend.has("self_heal")).toBe(true);
  });

  it("every catalog id is a backend-whitelisted id (no UI can offer an off-list key)", () => {
    const offlist = CATALOG.map((e) => e.id).filter((id) => !backend.has(id));
    expect(offlist).toEqual([]);
  });

  it("every backend-whitelisted id is exposed in the catalog (no setting silently dropped)", () => {
    const catalogIds = new Set(CATALOG.map((e) => e.id));
    const missing = [...backend].filter((id) => !catalogIds.has(id));
    expect(missing).toEqual([]);
  });

  it("has no duplicate ids", () => {
    const ids = CATALOG.map((e) => e.id);
    expect(new Set(ids).size).toBe(ids.length);
  });

  it("the three autonomy ids render as the 3-way control", () => {
    for (const id of AUTONOMY_IDS) {
      const entry = entryById(id);
      expect(entry?.control).toBe("autonomy");
    }
    // The 3-way values mirror the backend AUTONOMY_STATES order.
    expect(AUTONOMY_OPTIONS.map((o) => o.value)).toEqual(["off", "propose", "auto"]);
  });

  it("every entry belongs to one of the seven groups", () => {
    for (const e of CATALOG) {
      expect(GROUP_ORDER).toContain(e.group);
    }
  });
});

/* ------------------------------------------------------ control kinds */

describe("control kinds match the catalog requirements", () => {
  it("voice_id exposes on/off + scope select + a float threshold", () => {
    expect(entryById("voice_id.enabled")?.control).toBe("toggle");
    expect(entryById("voice_id.gate_scope")?.control).toBe("select");
    const thr = entryById("voice_id.threshold");
    expect(thr?.control).toBe("number");
    expect(thr?.step).toBeLessThan(1); // a float step, not an integer
  });

  it("the enum selects are present with their option labels", () => {
    for (const id of [
      "speech.engine",
      "inference.quant",
      "router.conversation_route",
      "focus.profile",
    ]) {
      const e = entryById(id);
      expect(e?.control).toBe("select");
      expect(e?.optionLabels).toBeTruthy();
    }
  });

  it("the key numeric bounds are numbers", () => {
    expect(entryById("screen_context.interval_secs")?.control).toBe("number");
    expect(entryById("proactive.quiet_start")?.control).toBe("number");
    expect(entryById("proactive.quiet_end")?.control).toBe("number");
  });
});

/* ------------------------------------------------------ dangerous confirms */

describe("dangerous-change gating", () => {
  function entry(id: string): CatalogEntry {
    const e = entryById(id);
    if (!e) throw new Error(`missing ${id}`);
    return e;
  }

  it("disarming the master switch (allow_consequential -> false) is dangerous", () => {
    const e = entry("integrations.allow_consequential");
    expect(isDangerousChange(e, false)).toBe(true);
    expect(isDangerousChange(e, true)).toBe(false);
  });

  it("enabling encrypt_memory / shell / ui / mcp / cloud tiers / screen-context is dangerous (ON)", () => {
    for (const id of [
      "security.encrypt_memory",
      "shell.enabled",
      "ui_automation.enabled",
      "mcp.enabled",
      "voice.cloud_tier",
      "voice.cloud_stt",
      "screen_context.enabled",
    ]) {
      const e = entry(id);
      expect(isDangerousChange(e, true)).toBe(true);
      expect(isDangerousChange(e, false)).toBe(false);
      expect(e.danger).toBeTruthy();
    }
  });

  it("self_heal/forge/optimize Full-auto is dangerous; Off/Propose are not", () => {
    for (const id of AUTONOMY_IDS) {
      const e = entry(id);
      expect(isDangerousChange(e, "auto")).toBe(true);
      expect(isDangerousChange(e, "propose")).toBe(false);
      expect(isDangerousChange(e, "off")).toBe(false);
    }
  });

  it("a plain feature toggle (e.g. answers.cite) is never dangerous", () => {
    const e = entry("answers.cite");
    expect(isDangerousChange(e, true)).toBe(false);
    expect(isDangerousChange(e, false)).toBe(false);
  });
});

/* ------------------------------------------------------ batched diff model */

describe("batched pending-change model", () => {
  it("emits a Change only for ids whose draft differs from live", () => {
    const live: Record<string, SettingValue> = {
      "shell.enabled": false,
      "answers.cite": true,
      "voice_id.threshold": 0.86,
      self_heal: "propose",
    };
    const draft: Record<string, SettingValue> = {
      "shell.enabled": true, // changed
      "answers.cite": true, // unchanged
      "voice_id.threshold": 0.9, // changed
      self_heal: "propose", // unchanged
    };
    const changes = pendingChanges(live, draft);
    const byId = Object.fromEntries(changes.map((c) => [c.id, c.value]));
    expect(byId["shell.enabled"]).toBe(true);
    expect(byId["voice_id.threshold"]).toBe(0.9);
    expect(changes.find((c) => c.id === "answers.cite")).toBeUndefined();
    expect(changes.find((c) => c.id === "self_heal")).toBeUndefined();
  });

  it("never emits a change for an id absent from both maps", () => {
    const changes = pendingChanges({}, { "shell.enabled": true });
    expect(changes).toEqual([]);
  });

  it("returns changes in catalog order (stable, legible batch)", () => {
    const live: Record<string, SettingValue> = {
      "answers.cite": true,
      "integrations.allow_consequential": true,
    };
    const draft: Record<string, SettingValue> = {
      "answers.cite": false,
      "integrations.allow_consequential": false,
    };
    const changes = pendingChanges(live, draft);
    // integrations.allow_consequential precedes answers.cite in the catalog.
    expect(changes[0].id).toBe("integrations.allow_consequential");
    expect(changes[1].id).toBe("answers.cite");
  });

  it("dangerousPending surfaces exactly the risky subset of a batch", () => {
    const changes: Change[] = [
      { id: "answers.cite", value: false }, // benign
      { id: "shell.enabled", value: true }, // dangerous (ON)
      { id: "self_heal", value: "auto" }, // dangerous (Full-auto)
      { id: "self_heal", value: "propose" }, // not (Propose)
    ];
    const dangerous = dangerousPending(changes).map((d) => d.entry.id);
    expect(dangerous).toContain("shell.enabled");
    expect(dangerous).toContain("self_heal");
    expect(dangerous).not.toContain("answers.cite");
  });
});

/* ------------------------------------------------------ value-map helper */

describe("valueMapFromStates", () => {
  it("keys the live values by id", () => {
    const states: SettingState[] = [
      { id: "shell.enabled", section: "shell", key: "enabled", kind: "bool", value: true },
      { id: "self_heal", section: "self_heal", key: "", kind: "autonomy", value: "propose" },
    ];
    const map = valueMapFromStates(states);
    expect(map["shell.enabled"]).toBe(true);
    expect(map["self_heal"]).toBe("propose");
  });
});

/* ------------------------------------------------------ group coverage */

describe("group coverage", () => {
  it("each group has at least one entry", () => {
    for (const g of GROUP_ORDER) {
      expect(entriesForGroup(g).length).toBeGreaterThan(0);
    }
  });

  it("Safety & Gates contains the master switch, voice-id, encryption, and policy", () => {
    const ids = entriesForGroup("Safety & Gates").map((e) => e.id);
    expect(ids).toContain("integrations.allow_consequential");
    expect(ids).toContain("voice_id.enabled");
    expect(ids).toContain("security.encrypt_memory");
    expect(ids).toContain("policy.enabled");
  });
});

/* ------------------------------------------------------ component render */

describe("SystemSettingsPanel render (no daemon)", () => {
  it("renders the honest loading state without throwing (config_get is async)", () => {
    // In the node test env there is no Tauri runtime, so configGet's promise is
    // still pending on first paint — the panel shows the loading note. This
    // proves the component mounts cleanly with no daemon present.
    const html = renderToStaticMarkup(createElement(SystemSettingsPanel));
    expect(html).toContain("Reading config/jarvis.toml");
  });
});
