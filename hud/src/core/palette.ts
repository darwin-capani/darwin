/**
 * Command-palette core (pure, node-testable — no DOM, React, or Tauri imports).
 *
 * The Cmd-K palette is an INPUT ACCELERATOR, not a new actuator. Every item it
 * offers resolves to a [`PaletteAction`] the component hands to `sendCommand`
 * over the SAME bounded verb channel the Command Deck uses (`ask` / `brief`), so
 * a palette selection routes through the daemon's normal router + gate exactly
 * as if the phrase were spoken. It carries NO new authority: a consequential
 * action still parks for a spoken confirm. This module only ENUMERATES the live
 * capability surface and RANKS it against a query — both pure functions, so the
 * matching and item-assembly are unit-tested without a browser.
 */

/** Which surface a palette item came from (drives the group heading + icon). */
export type PaletteGroup = "Command" | "App" | "Agent";

/**
 * What selecting an item DOES, as data (the component maps it to `sendCommand`
 * or to local input state — keeping this a pure value makes selection testable):
 *   - `ask`          fire the phrase as an utterance (== speaking it), then close;
 *   - `brief`        fire the dedicated `brief` verb, then close;
 *   - `target-agent` set the address for the next `ask` and keep the palette open.
 */
export type PaletteAction =
  | { kind: "ask"; text: string }
  | { kind: "brief" }
  | { kind: "target-agent"; agent: string };

export interface PaletteItem {
  /** Stable id (group-prefixed) for React keys + selection. */
  id: string;
  /** The primary line shown in the list. */
  label: string;
  /** The dim secondary line (role / description / status). */
  hint: string;
  group: PaletteGroup;
  /** Extra text folded into the search corpus (category, tool, description). */
  keywords: string;
  action: PaletteAction;
}

/** A minimal projection of `HudState` so item assembly is trivially testable. */
export interface PaletteSources {
  apps: ReadonlyArray<{ id: string; description: string; tool?: string }>;
  agents: ReadonlyArray<{ name: string; role: string }>;
}

/** "global-scan" / "global_scan" -> "Global Scan" for a human label. */
export function titleCase(id: string): string {
  return id
    .split(/[-_.\s]+/)
    .filter(Boolean)
    .map((w) => w.charAt(0).toUpperCase() + w.slice(1))
    .join(" ");
}

/**
 * Assemble the palette item list from the actuatable capability surface.
 * Deterministic + order-stable: a built-in Command first, then Apps, then Agents
 * — each in source order. Blank/duplicate ids are dropped so the list is clean
 * regardless of a malformed telemetry frame.
 */
export function buildPaletteItems(src: PaletteSources): PaletteItem[] {
  const items: PaletteItem[] = [];
  const seen = new Set<string>();
  const push = (item: PaletteItem) => {
    if (!item.label.trim() || seen.has(item.id)) return;
    seen.add(item.id);
    items.push(item);
  };

  // Built-in verb commands (directly fireable, no free text needed).
  push({
    id: "cmd:brief",
    label: "Daily brief",
    hint: "Ask DARWIN for the current brief",
    group: "Command",
    keywords: "summary status today intel briefing",
    action: { kind: "brief" },
  });

  // Apps — selecting sends "open <name>" as an utterance. Like any spoken phrase
  // it is CLASSIFIED daemon-side (not a deterministic route) and gated: a benign
  // app-open runs, anything the classifier reads as consequential still parks.
  for (const app of src.apps) {
    const id = app.id.trim();
    if (!id) continue;
    const name = titleCase(id);
    push({
      id: `app:${id}`,
      label: `Open ${name}`,
      hint: app.description.trim() || "micro-app",
      group: "App",
      keywords: `${id} ${app.description} ${app.tool ?? ""}`,
      action: { kind: "ask", text: `open ${name}` },
    });
  }

  // Agents — selecting addresses the next ask to that agent (keeps palette open).
  for (const agent of src.agents) {
    const id = agent.name.trim();
    if (!id) continue;
    push({
      id: `agent:${id}`,
      label: `Ask ${titleCase(id)}`,
      hint: agent.role.trim() || "agent",
      group: "Agent",
      keywords: `${id} ${agent.role} address direct`,
      action: { kind: "target-agent", agent: id },
    });
  }

  // NOTE: skills are intentionally NOT enumerated here. The HUD has no reliable
  // skill-invocation phrase, so a skill item could only be a discovery dead-end
  // (fill the input with a slug that does nothing on Enter). Skills are browsed
  // in the Skills panel; the palette stays to things it can actually actuate.

  return items;
}

/**
 * Deterministic subsequence fuzzy score of `query` against `text`. Returns null
 * when `query` is not a subsequence of `text` (no match), else a non-negative
 * score where higher is better. Bonuses reward matches at the start, after a
 * word boundary, and in a contiguous run — so "gs" ranks "Global Scan" (two
 * word-initials) above an incidental scatter. Case-insensitive. Pure.
 */
export function fuzzyScore(text: string, query: string): number | null {
  const q = query.toLowerCase();
  const t = text.toLowerCase();
  if (q.length === 0) return 0;
  if (q.length > t.length) return null;

  let score = 0;
  let ti = 0;
  let prevMatch = -2; // index of the previous matched char in t
  for (let qi = 0; qi < q.length; qi++) {
    const ch = q[qi];
    let found = -1;
    for (let j = ti; j < t.length; j++) {
      if (t[j] === ch) {
        found = j;
        break;
      }
    }
    if (found === -1) return null;
    // Base point for the match.
    score += 1;
    // Contiguity bonus: this match immediately follows the previous one.
    if (found === prevMatch + 1) score += 3;
    // Boundary bonus: start of string, or right after a separator.
    if (found === 0) {
      score += 4;
    } else {
      const before = t[found - 1];
      if (before === " " || before === "-" || before === "_" || before === ".") {
        score += 2;
      }
    }
    prevMatch = found;
    ti = found + 1;
  }
  // Gentle penalty for a long text (prefer the tighter match on ties).
  return score - t.length * 0.01;
}

/** A scored item (internal to ranking; exported for tests). */
export interface RankedItem {
  item: PaletteItem;
  score: number;
}

/**
 * Filter + rank palette `items` against `query`. An empty query returns every
 * item in assembly order (capped). Otherwise each item is scored against its
 * label AND keywords (best of the two, with the label weighted higher so a
 * label hit outranks an incidental keyword hit), non-matches dropped, and the
 * survivors sorted by score desc then original index asc (a STABLE, deterministic
 * tie-break). Pure — no allocation beyond the result.
 */
export function filterPalette(
  items: ReadonlyArray<PaletteItem>,
  query: string,
  limit = 50,
): PaletteItem[] {
  const q = query.trim();
  if (q === "") return items.slice(0, limit);

  const ranked: Array<{ item: PaletteItem; score: number; idx: number }> = [];
  for (let idx = 0; idx < items.length; idx++) {
    const item = items[idx];
    const labelScore = fuzzyScore(item.label, q);
    const kwScore = fuzzyScore(item.keywords, q);
    // Label match weighted 1.0, keyword match 0.5 (a hint-only match still shows
    // but ranks below any label match).
    let best: number | null = null;
    if (labelScore !== null) best = labelScore;
    if (kwScore !== null) {
      const weighted = kwScore * 0.5;
      best = best === null ? weighted : Math.max(best, weighted);
    }
    if (best !== null) ranked.push({ item, score: best, idx });
  }
  ranked.sort((a, b) => b.score - a.score || a.idx - b.idx);
  return ranked.slice(0, limit).map((r) => r.item);
}

/** The bounded command shape a resolution asks the component to send. A
 *  structural subset of `CommandRequest` (no Tauri import — keeps this pure). */
export interface PaletteCommand {
  cmd: "ask" | "brief";
  text?: string;
  agent?: string;
}

/** What the component should DO for a selection — send a command (and close),
 *  or perform a local UI change (set the agent address, staying open). */
export type PaletteResolution =
  | { kind: "send"; command: PaletteCommand }
  | { kind: "target-agent"; agent: string };

/**
 * Resolve a selected item's action into what the component does. Pure, so the
 * "what gets sent" contract is unit-tested without a browser. `targetAgent` (the
 * currently-addressed agent, if any) is folded into an `ask` so a directed
 * question reaches the right agent.
 */
export function resolveAction(
  action: PaletteAction,
  targetAgent: string | null,
): PaletteResolution {
  switch (action.kind) {
    case "ask":
      return {
        kind: "send",
        command: { cmd: "ask", text: action.text, ...(targetAgent ? { agent: targetAgent } : {}) },
      };
    case "brief":
      return { kind: "send", command: { cmd: "brief" } };
    case "target-agent":
      return { kind: "target-agent", agent: action.agent };
  }
}

/**
 * Resolve free-text (the query itself, when the user hits Enter without picking a
 * capability item) into an `ask` — the palette's quick-command fallback. Returns
 * null for blank input (nothing to send). Trims; folds in `targetAgent`.
 */
export function resolveFreeText(query: string, targetAgent: string | null): PaletteCommand | null {
  const text = query.trim();
  if (!text) return null;
  return { cmd: "ask", text, ...(targetAgent ? { agent: targetAgent } : {}) };
}
