/**
 * KIOSK-TAKEOVER bridge — the React-facing wrapper around the Tauri backend's
 * `enter_takeover` / `exit_takeover` commands. Both backend commands are
 * parameter-less from JS: Tauri auto-injects the calling window + the shared
 * Takeover state, so React invokes by name with no args.
 *
 * Like every wrapper in tauri/bridge.ts, each call degrades GRACEFULLY when the
 * frontend runs in a plain browser (vite dev / vitest) where no Tauri runtime
 * exists — it never throws and never blanks the HUD. Outside the shell there is
 * no real window to take over, so the bridge is a no-op that resolves false;
 * the React layer still flips its own `takeoverActive` bit so the windowed dev
 * session can preview the takeover LAYOUT without a live desktop.
 *
 * SAFETY: nothing here auto-enters takeover. `enterTakeover` is only ever called
 * from an explicit operator action. `exitTakeover` is the always-available exit;
 * even if the backend setter errors, the backend command is total + idempotent
 * (it reverses every recorded mutation and restores the Dock/menu bar), and this
 * wrapper swallows any throw so the EXIT affordance can never itself fail closed.
 */
import { invoke } from "@tauri-apps/api/core";
import { inTauri } from "./bridge";

/**
 * Enter kiosk takeover. In the shell this invokes the backend `enter_takeover`
 * (fullscreen + no decorations + always-on-top + macOS Dock/menu-bar hide),
 * which returns true once active (idempotent; rolls back and stays OUT on any
 * step failure). In a plain browser there is no window to mutate — resolve false
 * (the React layer still shows the takeover layout for a windowed preview).
 */
export async function enterTakeover(): Promise<boolean> {
  if (!inTauri()) return false;
  try {
    return await invoke<boolean>("enter_takeover");
  } catch {
    // A backend rejection must never leave the operator wedged. The backend
    // rolls back on failure; the HUD simply treats a thrown enter as "not
    // entered" and stays in (or returns to) the windowed layout.
    return false;
  }
}

/**
 * Exit kiosk takeover — the always-available escape hatch. In the shell this
 * invokes the backend `exit_takeover`, which is total + idempotent: it restores
 * the Dock/menu bar and reverses every recorded window mutation even if a setter
 * errors, and is a clean no-op when not active. In a plain browser it is a no-op
 * that resolves true. Never throws — the exit path must not be able to fail.
 */
export async function exitTakeover(): Promise<boolean> {
  if (!inTauri()) return true;
  try {
    return await invoke<boolean>("exit_takeover");
  } catch {
    // The exit affordance can NEVER fail closed. macOS also auto-restores the
    // presentation options when the app process dies, so a hard failure here is
    // still recoverable by quitting — but we report success-of-intent so the HUD
    // returns to the windowed layout rather than trapping the user in takeover.
    return true;
  }
}
