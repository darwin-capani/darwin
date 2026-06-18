import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

/* ------------------------------------------------------------------------ *
 * Mock the Tauri runtime BEFORE importing the bridge/component, so the deck *
 * dispatches through a controllable invoke and `inTauri()` reads true.      *
 * No dev server, no real socket — the invoke is a spy.                      *
 * ------------------------------------------------------------------------ */
const invokeMock = vi.fn();

vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args: unknown) => invokeMock(cmd, args),
}));

// `inTauri()` gates whether sendCommand invokes; force it true for the wiring
// tests. The component/render tests don't depend on it.
vi.mock("../tauri/bridge", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../tauri/bridge")>();
  return { ...actual, inTauri: () => true };
});

import CommandDeck from "../components/CommandDeck";
import { ROSTER } from "../core/agents";
import { sendCommand } from "../tauri/command";
import type { PendingSnapshot } from "../tauri/command";

beforeEach(() => {
  invokeMock.mockReset();
});
afterEach(() => {
  vi.restoreAllMocks();
});

/* ------------------------------------------------------------------------ *
 * The deck renders the agents + the pending tray (renderToStaticMarkup —    *
 * node env, no jsdom, the same pattern as forge.test.ts). Effects (the      *
 * pending poll) do NOT run under SSR, so no socket call fires on render.    *
 * The tray is seeded via the test-only `initialPending` prop.               *
 * ------------------------------------------------------------------------ */
describe("CommandDeck render (device-gated R3F aside; the DOM deck is real)", () => {
  const render = (props: Parameters<typeof CommandDeck>[0]) =>
    renderToStaticMarkup(createElement(CommandDeck, props));

  it("renders nothing when closed (additive — never occludes the HUD)", () => {
    expect(render({ open: false, onClose: () => {} })).toBe("");
  });

  it("renders the full constellation as an addressable deck when open", () => {
    const html = render({ open: true, onClose: () => {} });
    expect(html).toContain("COMMAND DECK");
    expect(html).toContain(`CONSTELLATION — ${ROSTER.length} AGENTS`);
    // Every roster agent is addressable in the deck.
    for (const a of ROSTER) {
      expect(html).toContain(a.name);
    }
    // The brief / mission launchers + the auto-route option exist.
    expect(html).toContain("BRIEF");
    expect(html).toContain("LAUNCH MISSION");
    expect(html).toContain("Auto-route");
  });

  it("shows a pending confirmation with APPROVE + DENY", () => {
    const pending: PendingSnapshot = {
      confirmation: { id: "deadbeef", agent: "agent.pepper", tool: "gmail_send", preview: "Would email Alice" },
      forge_pending_ts: null,
    };
    const html = render({ open: true, onClose: () => {}, initialPending: pending });
    expect(html).toContain("PENDING ACTIONS");
    expect(html).toContain("gmail_send");
    expect(html).toContain("Would email Alice");
    expect(html).toContain("APPROVE");
    expect(html).toContain("DENY");
  });

  it("the forge tray shows the manual apply command + a DISMISS, never an apply button", () => {
    const pending: PendingSnapshot = { confirmation: null, forge_pending_ts: "1770000000" };
    const html = render({ open: true, onClose: () => {}, initialPending: pending });
    // The EXACT manual deploy command is surfaced.
    expect(html).toContain("scripts/apply_forge.sh 1770000000");
    expect(html).toContain("DISMISS");
    // REVIEW-ONLY: no button that auto-applies/deploys/installs/runs the forge.
    expect(html).not.toMatch(/<button[^>]*>[^<]*(APPLY|DEPLOY|INSTALL)/i);
    // It states there is no auto-deploy.
    expect(html).toMatch(/no auto-deploy/i);
  });

  it("never renders a token or secret-shaped string", () => {
    const pending: PendingSnapshot = {
      confirmation: { id: "abc", agent: "agent.pepper", tool: "gmail_send", preview: "Would email" },
      forge_pending_ts: "1770000000",
    };
    const html = render({ open: true, onClose: () => {}, initialPending: pending });
    expect(html).not.toMatch(/token/i);
    expect(html).not.toMatch(/sk-/);
    expect(html).not.toMatch(/secret/i);
  });
});

/* ------------------------------------------------------------------------ *
 * The wiring: the bridge dispatches the RIGHT bounded command to the Tauri  *
 * backend (mocked invoke). This proves Approve -> confirm{id}, Deny ->      *
 * deny{id}, Dismiss -> dismiss_forge{ts}, ask -> ask{text,agent}, with NO   *
 * token field crossing JS (the backend injects it). The deck's buttons call *
 * exactly these.                                                            *
 * ------------------------------------------------------------------------ */
describe("command bridge dispatch (mocked Tauri invoke)", () => {
  it("ask sends cmd:ask with text + agent and NO token", async () => {
    invokeMock.mockResolvedValue({ ok: true, reply: "ok" });
    await sendCommand({ cmd: "ask", text: "status", agent: "edith" });
    expect(invokeMock).toHaveBeenCalledTimes(1);
    const [name, args] = invokeMock.mock.calls[0];
    expect(name).toBe("send_command");
    expect(args).toEqual({ request: { cmd: "ask", text: "status", agent: "edith" } });
    // The token is never part of the JS-side request.
    expect(JSON.stringify(args)).not.toContain("token");
  });

  it("Approve dispatches confirm{id}", async () => {
    invokeMock.mockResolvedValue({ ok: true, reply: "Sent." });
    await sendCommand({ cmd: "confirm", id: "deadbeef" });
    expect(invokeMock.mock.calls[0][1]).toEqual({ request: { cmd: "confirm", id: "deadbeef" } });
  });

  it("Deny dispatches deny{id}", async () => {
    invokeMock.mockResolvedValue({ ok: true, reply: "Cancelled." });
    await sendCommand({ cmd: "deny", id: "deadbeef" });
    expect(invokeMock.mock.calls[0][1]).toEqual({ request: { cmd: "deny", id: "deadbeef" } });
  });

  it("Dismiss dispatches dismiss_forge{ts} (never an apply)", async () => {
    invokeMock.mockResolvedValue({ ok: true, reply: "Dismissed." });
    await sendCommand({ cmd: "dismiss_forge", ts: 1770000000 });
    const [name, args] = invokeMock.mock.calls[0];
    expect(name).toBe("send_command");
    expect(args).toEqual({ request: { cmd: "dismiss_forge", ts: 1770000000 } });
    // There is no apply/deploy verb anywhere in the bridge surface.
    expect(JSON.stringify(args)).not.toMatch(/apply|deploy/i);
  });

  it("the brief / mission / roster / pending verbs all route through the one command", async () => {
    invokeMock.mockResolvedValue({ ok: true, reply: "ok" });
    await sendCommand({ cmd: "brief" });
    await sendCommand({ cmd: "mission", goal: "scan the markets" });
    await sendCommand({ cmd: "roster" });
    await sendCommand({ cmd: "pending" });
    expect(invokeMock.mock.calls.map((c) => (c[1] as { request: { cmd: string } }).request.cmd)).toEqual([
      "brief",
      "mission",
      "roster",
      "pending",
    ]);
  });

  it("a thrown invoke becomes a clean error reply, never an unhandled rejection", async () => {
    invokeMock.mockRejectedValue("backend exploded");
    const reply = await sendCommand({ cmd: "roster" });
    expect(reply.ok).toBe(false);
    expect(reply.error).toBe("backend exploded");
  });
});
