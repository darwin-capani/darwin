import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  AUTO_UPDATE_KEY,
  AUTO_UPDATE_ON_VALUE,
  decideLaunchUpdateAction,
  isAutoUpdateOn,
  runDialogCancel,
  runDialogInstall,
  runLaunchUpdate,
  setAutoUpdateOn,
  silentUpdateNotice,
  updateDialogInitial,
  updateDialogReduce,
  type AutoUpdateStorage,
  type LaunchUpdateDeps,
  type UpdateInstallDeps,
} from "../core/autoUpdate";
import type { UpdateCheck } from "../tauri/bridge";

/* The auto-update launch feature. THE CARDINAL HONESTY RULE the tests pin:
 *   - the dialog (and the silent path) are reachable ONLY for status
 *     "available" — never not_configured / up_to_date / error / unavailable /
 *     installed / unknown, and never a fabricated update;
 *   - the persisted pref mirrors onboarding.ts (a localStorage key) and is
 *     fully REVERSIBLE (OFF clears the key);
 *   - the three dialog buttons drive the right paths (Update = install;
 *     Update&don't-ask = persist pref THEN install; Cancel = close, pref
 *     unchanged);
 *   - when the pref is ON the launch path installs SILENTLY (no dialog) with an
 *     honest notice;
 *   - the install state machine never claims success on a non-"installed"
 *     result. */

/* ----------------------------------------------------- in-memory storage stub */

/** A localStorage stub (with removeItem) so the pref helpers run in node. */
function memStorage(): AutoUpdateStorage & { map: Map<string, string> } {
  const map = new Map<string, string>();
  return {
    map,
    getItem: (k) => (map.has(k) ? map.get(k)! : null),
    setItem: (k, v) => {
      map.set(k, v);
    },
    removeItem: (k) => {
      map.delete(k);
    },
  };
}

/* small builders for the UpdateCheck contract */
const available = (version: string | null = "1.4.0"): UpdateCheck => ({
  status: "available",
  detail: `Version ${version} is available.`,
  version,
});
const notConfigured = (): UpdateCheck => ({
  status: "not_configured",
  detail: "Auto-update is not armed yet — see docs/RELEASE.md.",
  version: null,
});
const upToDate = (): UpdateCheck => ({
  status: "up_to_date",
  detail: "DARWIN is on the latest version.",
  version: null,
});
const errored = (): UpdateCheck => ({ status: "error", detail: "offline" });
const unavailable = (): UpdateCheck => ({
  status: "unavailable",
  detail: "Updates are checked from the DARWIN desktop app.",
});
const installed = (version = "1.4.0"): UpdateCheck => ({
  status: "installed",
  detail: `Version ${version} was downloaded, verified, and installed — relaunch DARWIN to finish.`,
  version,
});

/* ======================================================================== *
 * Persisted preference — mirrors the onboarding once-only flag pattern.      *
 * ======================================================================== */
describe("auto-update preference persistence", () => {
  it("default is OFF on a fresh install (no key set)", () => {
    const store = memStorage();
    expect(isAutoUpdateOn(store)).toBe(false);
  });

  it("setAutoUpdateOn(true) persists the fixed literal under the versioned key", () => {
    const store = memStorage();
    setAutoUpdateOn(true, store);
    expect(isAutoUpdateOn(store)).toBe(true);
    expect(store.getItem(AUTO_UPDATE_KEY)).toBe(AUTO_UPDATE_ON_VALUE);
  });

  it("is REVERSIBLE: setAutoUpdateOn(false) clears the key (don't-ask-again is undoable)", () => {
    const store = memStorage();
    setAutoUpdateOn(true, store);
    expect(isAutoUpdateOn(store)).toBe(true);
    setAutoUpdateOn(false, store);
    expect(isAutoUpdateOn(store)).toBe(false);
    // OFF removes the key rather than leaving a stale value behind.
    expect(store.getItem(AUTO_UPDATE_KEY)).toBeNull();
  });

  it("FAIL-SAFE toward OFF: with no storage it reads OFF (shows the dialog, never auto-installs)", () => {
    expect(isAutoUpdateOn(null)).toBe(false);
    expect(() => setAutoUpdateOn(true, null)).not.toThrow();
  });

  it("FAIL-SAFE: a storage that throws on read reads OFF", () => {
    const throwing: AutoUpdateStorage = {
      getItem: () => {
        throw new Error("blocked");
      },
      setItem: () => {},
      removeItem: () => {},
    };
    expect(isAutoUpdateOn(throwing)).toBe(false);
  });

  it("a different key/value does NOT count as ON (only the exact flag arms it)", () => {
    const store = memStorage();
    store.setItem(AUTO_UPDATE_KEY, "0");
    expect(isAutoUpdateOn(store)).toBe(false);
    store.setItem("some.other.key", AUTO_UPDATE_ON_VALUE);
    expect(isAutoUpdateOn(store)).toBe(false);
  });
});

/* ======================================================================== *
 * THE honesty gate — decideLaunchUpdateAction only ever acts on "available". *
 * ======================================================================== */
describe("launch branch — only status 'available' surfaces anything", () => {
  it("status 'available' + pref OFF -> open the dialog naming the real version", () => {
    const a = decideLaunchUpdateAction(available("2.0.1"), false);
    expect(a).toEqual({ kind: "dialog", version: "2.0.1" });
  });

  it("status 'available' + pref ON -> SILENT install (no dialog)", () => {
    const a = decideLaunchUpdateAction(available("2.0.1"), true);
    expect(a).toEqual({ kind: "silent", version: "2.0.1" });
  });

  it("NEVER surfaces for not_configured (the shipped, un-armed state) — pref ON or OFF", () => {
    expect(decideLaunchUpdateAction(notConfigured(), false)).toEqual({ kind: "none" });
    expect(decideLaunchUpdateAction(notConfigured(), true)).toEqual({ kind: "none" });
  });

  it("NEVER surfaces for up_to_date / error / unavailable / installed (no dialog, no nag)", () => {
    for (const r of [upToDate(), errored(), unavailable(), installed()]) {
      expect(decideLaunchUpdateAction(r, false)).toEqual({ kind: "none" });
      expect(decideLaunchUpdateAction(r, true)).toEqual({ kind: "none" });
    }
  });

  it("NEVER surfaces for an unrecognised status (defensive default-deny)", () => {
    const weird = { status: "totally_made_up", detail: "x" } as unknown as UpdateCheck;
    expect(decideLaunchUpdateAction(weird, false)).toEqual({ kind: "none" });
    expect(decideLaunchUpdateAction(weird, true)).toEqual({ kind: "none" });
  });

  it("cannot fabricate an update: 'available' with no usable version -> none", () => {
    // An available status that can't honestly name a version never opens a dialog.
    expect(decideLaunchUpdateAction(available(null), false)).toEqual({ kind: "none" });
    expect(decideLaunchUpdateAction(available(""), true)).toEqual({ kind: "none" });
    expect(decideLaunchUpdateAction(available("   "), false)).toEqual({ kind: "none" });
  });

  it("the silent notice is honest + names the version (never a silent surprise)", () => {
    expect(silentUpdateNotice("3.1.0")).toBe("Updating DARWIN to 3.1.0…");
  });
});

/* ======================================================================== *
 * Install state machine — never claims success unless backend says installed *
 * ======================================================================== */
describe("update dialog install reducer", () => {
  it("idle -> installing on installStart (buttons disable, no success claim)", () => {
    const s = updateDialogReduce(updateDialogInitial(), { type: "installStart" });
    expect(s.phase).toBe("installing");
    expect(s.detail).toBe("");
  });

  it("a backend 'installed' result is the ONLY path to the installed phase", () => {
    const s = updateDialogReduce(
      { phase: "installing", detail: "" },
      { type: "installResult", result: installed("9.9.9") },
    );
    expect(s.phase).toBe("installed");
    expect(s.detail).toContain("9.9.9");
  });

  it("any non-'installed' result is surfaced as an honest error (never success)", () => {
    for (const r of [errored(), upToDate(), notConfigured(), unavailable()]) {
      const s = updateDialogReduce(
        { phase: "installing", detail: "" },
        { type: "installResult", result: r },
      );
      expect(s.phase).toBe("error");
      // it carries the backend's honest detail, never a fake "installed".
      expect(s.detail).toBe(r.detail);
    }
  });
});

/* ======================================================================== *
 * Dialog render — three labelled buttons + honest sub-copy.                  *
 * ======================================================================== */
describe("UpdateDialog render (three buttons + honest copy)", () => {
  let UpdateDialog: typeof import("../components/UpdateDialog").default;
  beforeEach(async () => {
    vi.resetModules();
    UpdateDialog = (await import("../components/UpdateDialog")).default;
  });

  function html(version = "1.4.0"): string {
    return renderToStaticMarkup(
      createElement(UpdateDialog, { version, onClose: () => {} }),
    );
  }

  it("names the real version and the signature-verification sub-copy", () => {
    const h = html("1.4.0");
    expect(h).toContain("UPDATE AVAILABLE");
    expect(h).toContain("DARWIN 1.4.0 is available.");
    expect(h.toLowerCase()).toContain("signature-verified before installing");
  });

  it("renders exactly the three labelled buttons (Update, don't-ask, Cancel)", () => {
    const h = html();
    expect(h).toContain("Cancel");
    expect(h).toContain("Update &amp; don&#x27;t ask again");
    // The primary Update button (not the don't-ask variant) is present.
    expect(h).toMatch(/>Update<\/button>/);
  });

  it("does not claim success on first paint (no 'installed' copy before any click)", () => {
    const h = html();
    expect(h.toLowerCase()).not.toContain("restart to finish");
    expect(h.toLowerCase()).not.toContain("installing…");
  });
});

/* ======================================================================== *
 * Button wiring — the exact action each of the three buttons performs.       *
 *                                                                            *
 * WHAT WENT WRONG BEFORE: this block never imported UpdateDialog. It defined  *
 * its OWN `installAndRelaunch()` (three lines calling two local spies) and    *
 * asserted on THAT, while the Cancel case only asserted that a vi.fn() the    *
 * test itself had just called had been called. Proven by mutation: making     *
 * UpdateDialog's onCancel call setAutoUpdateOn(true), making both install     *
 * buttons call checkForUpdates(FALSE), and deleting the relaunch left the     *
 * whole suite green.                                                          *
 *                                                                            *
 * The three bodies now live in core/autoUpdate.ts (runDialogInstall /         *
 * runDialogCancel) and UpdateDialog's onClicks are thin wrappers over them,   *
 * so these assertions run the SHIPPED code with injected spies. There is      *
 * still no jsdom in this env — what is pinned is the action bodies, not the   *
 * button-to-body binding, which the component keeps to one call each.         *
 * ======================================================================== */
describe("three-button wiring (the exact action each performs)", () => {
  const checkSpy = vi.fn<(install: boolean) => Promise<UpdateCheck>>();
  const relaunchSpy = vi.fn<() => Promise<boolean>>();
  const persistSpy = vi.fn<(on: boolean) => void>();

  /** The seams UpdateDialog hands to the same functions under test. */
  const deps = (): UpdateInstallDeps => ({
    check: checkSpy,
    relaunch: relaunchSpy,
    persistPref: persistSpy,
  });

  beforeEach(() => {
    checkSpy.mockReset();
    relaunchSpy.mockReset();
    persistSpy.mockReset();
  });
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("'Update' installs via the SIGNED backend command (install=true) then relaunches", async () => {
    checkSpy.mockResolvedValue(installed("1.4.0"));
    relaunchSpy.mockResolvedValue(true);
    const out = await runDialogInstall(deps(), false);
    expect(checkSpy).toHaveBeenCalledWith(true); // the EXISTING install path
    expect(checkSpy).not.toHaveBeenCalledWith(false); // never a mere check
    expect(relaunchSpy).toHaveBeenCalledTimes(1);
    expect(out.result.status).toBe("installed");
    expect(out.relaunched).toBe(true);
    expect(out.needsManualRestart).toBe(false);
    expect(persistSpy).not.toHaveBeenCalled(); // pref UNCHANGED
  });

  it("'Update & don't ask again' persists pref=ON BEFORE installing, then installs+relaunches", async () => {
    checkSpy.mockResolvedValue(installed());
    relaunchSpy.mockResolvedValue(true);
    await runDialogInstall(deps(), true);
    expect(persistSpy).toHaveBeenCalledWith(true);
    expect(checkSpy).toHaveBeenCalledWith(true);
    expect(relaunchSpy).toHaveBeenCalledTimes(1);
    // Ordering: the pref is written BEFORE the install command is issued, so a
    // failed install cannot lose the user's choice.
    expect(persistSpy.mock.invocationCallOrder[0]).toBeLessThan(
      checkSpy.mock.invocationCallOrder[0],
    );
  });

  it("an 'installed' result with NO shell to relaunch is honest, never a claimed finish", async () => {
    checkSpy.mockResolvedValue(installed());
    relaunchSpy.mockResolvedValue(false);
    const out = await runDialogInstall(deps(), false);
    expect(out.relaunched).toBe(false);
    expect(out.needsManualRestart).toBe(true);
  });

  it("'Cancel' closes WITHOUT changing the preference and WITHOUT installing", () => {
    const onClose = vi.fn();
    runDialogCancel(deps(), onClose);
    expect(onClose).toHaveBeenCalledTimes(1);
    // The two things Cancel must NEVER do — a regression that silently armed
    // permanent auto-install from the button labelled Cancel used to ship green.
    expect(persistSpy).not.toHaveBeenCalled();
    expect(checkSpy).not.toHaveBeenCalled();
    expect(relaunchSpy).not.toHaveBeenCalled();
  });

  it("an install error does NOT relaunch and is never reported as success", async () => {
    checkSpy.mockResolvedValue(errored());
    const out = await runDialogInstall(deps(), false);
    expect(out.result.status).toBe("error");
    expect(out.relaunched).toBe(false);
    expect(out.needsManualRestart).toBe(false);
    expect(relaunchSpy).not.toHaveBeenCalled();
    // The reducer keeps it honest.
    const s = updateDialogReduce(
      { phase: "installing", detail: "" },
      { type: "installResult", result: out.result },
    );
    expect(s.phase).toBe("error");
  });
});

/* ======================================================================== *
 * Launch path when the pref is ON — installs silently, no dialog.            *
 * ======================================================================== */
describe("silent launch install when pref is ON", () => {
  const checkSpy = vi.fn<(install: boolean) => Promise<UpdateCheck>>();
  const relaunchSpy = vi.fn<() => Promise<boolean>>();
  const toastSpy = vi.fn<(text: string) => void>();

  beforeEach(() => {
    checkSpy.mockReset();
    relaunchSpy.mockReset();
    toastSpy.mockReset();
  });

  const openDialogSpy = vi.fn<(version: string) => void>();

  /** App's real seams for the launch branch. */
  const deps = (): LaunchUpdateDeps => ({
    check: checkSpy,
    relaunch: relaunchSpy,
    persistPref: () => {
      throw new Error("the launch path must never write the auto-update pref");
    },
    notify: toastSpy,
  });

  /* WHAT WENT WRONG BEFORE: this block also defined its own `runLaunch()` copy
   * of App's launch effect, so mutating App.tsx to
   * `decideLaunchUpdateAction(result, true)` — auto-installing regardless of the
   * user's OFF preference, the exact unattended-binary-replacement this module
   * exists to prevent — left the suite green. It now calls App's real body
   * (core/autoUpdate.ts runLaunchUpdate). */

  it("pref ON + available -> shows the honest notice, installs (install=true), relaunches; NO dialog", async () => {
    openDialogSpy.mockReset();
    checkSpy.mockResolvedValue(installed("5.0.0"));
    relaunchSpy.mockResolvedValue(true);
    const outcome = await runLaunchUpdate(deps(), available("5.0.0"), true, openDialogSpy);
    expect(outcome).toBe("silent"); // never "dialog"
    expect(openDialogSpy).not.toHaveBeenCalled();
    expect(toastSpy).toHaveBeenCalledWith("Updating DARWIN to 5.0.0…");
    expect(checkSpy).toHaveBeenCalledWith(true);
    expect(relaunchSpy).toHaveBeenCalledTimes(1);
  });

  it("pref OFF + available -> opens the dialog (no silent install, no toast)", async () => {
    openDialogSpy.mockReset();
    const outcome = await runLaunchUpdate(deps(), available("5.0.0"), false, openDialogSpy);
    expect(outcome).toBe("dialog");
    // THE preference gate: with the pref OFF nothing may install unattended.
    expect(openDialogSpy).toHaveBeenCalledWith("5.0.0");
    expect(toastSpy).not.toHaveBeenCalled();
    expect(checkSpy).not.toHaveBeenCalled();
    expect(relaunchSpy).not.toHaveBeenCalled();
  });

  it("pref ON but NOT armed (not_configured) -> nothing happens (no install, no toast)", async () => {
    openDialogSpy.mockReset();
    const outcome = await runLaunchUpdate(deps(), notConfigured(), true, openDialogSpy);
    expect(outcome).toBe("none");
    expect(openDialogSpy).not.toHaveBeenCalled();
    expect(toastSpy).not.toHaveBeenCalled();
    expect(checkSpy).not.toHaveBeenCalled();
  });

  it("an unmount mid-install stops before the relaunch (cancellation is honoured)", async () => {
    openDialogSpy.mockReset();
    checkSpy.mockResolvedValue(installed("5.0.0"));
    relaunchSpy.mockResolvedValue(true);
    const outcome = await runLaunchUpdate(
      deps(),
      available("5.0.0"),
      true,
      openDialogSpy,
      () => true,
    );
    expect(outcome).toBe("silent");
    expect(relaunchSpy).not.toHaveBeenCalled();
  });
});
