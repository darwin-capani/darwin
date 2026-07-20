import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";

// Mock the Tauri runtime so importing the component (which imports sendCommand ->
// invoke) does not require a shell. Render tests do not fire it (SSR skips
// effects + handlers), but the import graph must resolve.
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

import CommandPalette from "../components/CommandPalette";
import type { PaletteSources } from "../core/palette";

const SOURCES: PaletteSources = {
  apps: [{ id: "global-scan", description: "network radar", tool: "scan.run" }],
  agents: [{ name: "darwin", role: "Prime Orchestrator" }],
};

describe("CommandPalette render (SSR, node env — no DOM interaction)", () => {
  const render = (props: Parameters<typeof CommandPalette>[0]) =>
    renderToStaticMarkup(createElement(CommandPalette, props));

  it("renders nothing when closed (additive — never occludes the HUD)", () => {
    expect(render({ open: false, onClose: () => {}, sources: SOURCES })).toBe("");
  });

  it("renders the palette dialog + the enumerated capability surface when open", () => {
    const html = render({ open: true, onClose: () => {}, sources: SOURCES });
    expect(html).toContain("COMMAND PALETTE");
    expect(html).toContain('role="dialog"');
    expect(html).toContain('aria-modal="true"');
    // Built-in + each enumerated group (Command / App / Agent).
    expect(html).toContain("Daily brief");
    expect(html).toContain("Open Global Scan");
    expect(html).toContain("Ask Darwin");
    // Combobox a11y linkage is present.
    expect(html).toContain('role="combobox"');
    expect(html).toContain('id="palette-list"');
  });

  it("shows the keyboard-hint footer", () => {
    const html = render({ open: true, onClose: () => {}, sources: SOURCES });
    expect(html).toContain("navigate");
    expect(html).toContain("close");
  });
});
