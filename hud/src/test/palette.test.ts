import { describe, expect, it } from "vitest";
import {
  buildPaletteItems,
  filterPalette,
  fuzzyScore,
  resolveAction,
  resolveFreeText,
  titleCase,
  type PaletteItem,
  type PaletteSources,
} from "../core/palette";

const SOURCES: PaletteSources = {
  apps: [
    { id: "global-scan", description: "network + process radar", tool: "scan.run" },
    { id: "jwtpeek", description: "decode a JWT on-device", tool: "jwt.decode" },
    { id: "", description: "malformed frame" }, // dropped (blank id)
  ],
  agents: [
    { name: "darwin", role: "Prime Orchestrator" },
    { name: "friday", role: "Daily Intel" },
  ],
};

/* ------------------------------------------------------------------------ *
 * titleCase                                                                 *
 * ------------------------------------------------------------------------ */
describe("titleCase", () => {
  it("splits hyphens/underscores/dots into Title Words", () => {
    expect(titleCase("global-scan")).toBe("Global Scan");
    expect(titleCase("jwt_peek.decode")).toBe("Jwt Peek Decode");
    expect(titleCase("plain")).toBe("Plain");
  });
});

/* ------------------------------------------------------------------------ *
 * buildPaletteItems — enumeration from the live surface                     *
 * ------------------------------------------------------------------------ */
describe("buildPaletteItems", () => {
  const items = buildPaletteItems(SOURCES);

  it("leads with the built-in brief command", () => {
    expect(items[0].group).toBe("Command");
    expect(items[0].action).toEqual({ kind: "brief" });
  });

  it("makes an app fire an 'open <name>' ask and drops a blank id", () => {
    const app = items.find((i) => i.id === "app:global-scan")!;
    expect(app.group).toBe("App");
    expect(app.label).toBe("Open Global Scan");
    expect(app.action).toEqual({ kind: "ask", text: "open Global Scan" });
    expect(items.some((i) => i.id === "app:")).toBe(false); // malformed dropped
  });

  it("makes an agent address the next ask (target-agent, no auto-fire)", () => {
    const agent = items.find((i) => i.id === "agent:friday")!;
    expect(agent.action).toEqual({ kind: "target-agent", agent: "friday" });
  });

  it("does NOT enumerate skills (they have no HUD-side invocation — no dead-ends)", () => {
    expect(items.some((i) => i.id.startsWith("skill:"))).toBe(false);
    expect(items.every((i) => i.group === "Command" || i.group === "App" || i.group === "Agent")).toBe(true);
  });

  it("dedups by id and produces stable order (Command, App, Agent)", () => {
    const groups = items.map((i) => i.group);
    expect(groups.indexOf("App")).toBeLessThan(groups.indexOf("Agent"));
    // No duplicate ids.
    expect(new Set(items.map((i) => i.id)).size).toBe(items.length);
  });
});

/* ------------------------------------------------------------------------ *
 * fuzzyScore — deterministic subsequence match                             *
 * ------------------------------------------------------------------------ */
describe("fuzzyScore", () => {
  it("returns null when the query is not a subsequence", () => {
    expect(fuzzyScore("global scan", "xyz")).toBeNull();
    expect(fuzzyScore("abc", "abcd")).toBeNull(); // longer than text
  });
  it("scores an empty query as 0 (matches everything)", () => {
    expect(fuzzyScore("anything", "")).toBe(0);
  });
  it("rewards word-initial matches over a scattered subsequence", () => {
    const initials = fuzzyScore("global scan", "gs")!; // g(start) + s(after space)
    const scattered = fuzzyScore("gems", "gs")!; // g(start) + s(scattered)
    expect(initials).toBeGreaterThan(scattered);
  });
  it("rewards a contiguous run over a broken one", () => {
    const contiguous = fuzzyScore("scan", "sca")!;
    const broken = fuzzyScore("s-c-a", "sca")!;
    expect(contiguous).toBeGreaterThan(broken);
  });
});

/* ------------------------------------------------------------------------ *
 * filterPalette — filter + rank                                            *
 * ------------------------------------------------------------------------ */
describe("filterPalette", () => {
  const items = buildPaletteItems(SOURCES);

  it("returns everything (assembly order) for an empty query", () => {
    expect(filterPalette(items, "")).toEqual(items);
    expect(filterPalette(items, "   ")).toEqual(items);
  });

  it("ranks the label match for 'global' first", () => {
    const out = filterPalette(items, "global");
    expect(out[0].id).toBe("app:global-scan");
  });

  it("matches on keywords (description), ranked below a label hit", () => {
    // 'decode' appears only in jwtpeek's description/keywords, not any label.
    const out = filterPalette(items, "decode");
    expect(out.some((i) => i.id === "app:jwtpeek")).toBe(true);
  });

  it("drops non-matches and honors the limit", () => {
    expect(filterPalette(items, "zzzznope")).toEqual([]);
    expect(filterPalette(items, "", 2).length).toBe(2);
  });

  it("is deterministic: same input, same order", () => {
    expect(filterPalette(items, "a")).toEqual(filterPalette(items, "a"));
  });
});

/* ------------------------------------------------------------------------ *
 * resolveAction / resolveFreeText — the send contract (no Tauri)           *
 * ------------------------------------------------------------------------ */
describe("resolveAction", () => {
  const ask: PaletteItem["action"] = { kind: "ask", text: "open Global Scan" };

  it("sends an ask, folding in the addressed agent", () => {
    expect(resolveAction(ask, null)).toEqual({
      kind: "send",
      command: { cmd: "ask", text: "open Global Scan" },
    });
    expect(resolveAction(ask, "friday")).toEqual({
      kind: "send",
      command: { cmd: "ask", text: "open Global Scan", agent: "friday" },
    });
  });

  it("sends the bare brief verb", () => {
    expect(resolveAction({ kind: "brief" }, null)).toEqual({
      kind: "send",
      command: { cmd: "brief" },
    });
  });

  it("keeps target-agent as local (never a send)", () => {
    expect(resolveAction({ kind: "target-agent", agent: "friday" }, null)).toEqual({
      kind: "target-agent",
      agent: "friday",
    });
  });
});

describe("resolveFreeText", () => {
  it("turns non-blank input into an ask (the quick-command fallback)", () => {
    expect(resolveFreeText("what is my battery", null)).toEqual({
      cmd: "ask",
      text: "what is my battery",
    });
    expect(resolveFreeText("  status  ", "friday")).toEqual({
      cmd: "ask",
      text: "status",
      agent: "friday",
    });
  });
  it("returns null for blank input (nothing to send)", () => {
    expect(resolveFreeText("", null)).toBeNull();
    expect(resolveFreeText("   ", "friday")).toBeNull();
  });
});
