/**
 * Pure state + logic for KIOSK TAKEOVER — the full-desktop holographic mode the
 * HUD renders into when the operator promotes the windowed HUD to a no-OS-chrome
 * takeover. No DOM/React/three/Tauri imports here, so the takeover state machine,
 * the exit-reachability invariant, and the keyboard-exit predicate are verifiable
 * headlessly under vitest (node env), exactly like deck.ts / state.ts.
 *
 * SAFETY POSTURE (the headline property — the user must NEVER be locked out):
 *   - Takeover ships OFF and is NEVER auto-entered. It is an explicit operator
 *     action; the default render is the windowed HUD (`takeoverActive=false`).
 *   - Exit is ALWAYS reachable. `exitAlwaysReachable(state)` is a pure invariant
 *     the layout test asserts: whenever takeover is active, the visible in-HUD
 *     EXIT control is rendered AND the Esc key exits. There is no active state
 *     in which the operator cannot leave.
 *   - The window-mutation reversal + the macOS Dock/menu-bar restore live in the
 *     Tauri backend's `exit_takeover` (device-gated). This module models only the
 *     HUD-side active/idle bit + the exit triggers; the actual presentation-option
 *     restore is proven by the src-tauri cargo tests, not here.
 *   - Idempotent: entering when already active, or exiting when already idle, is a
 *     no-op on the HUD state — mirroring the backend command's idempotency.
 */

/** The HUD-side takeover state. A single explicit bit; the OS-level mutations it
 *  implies are owned (and reversed) by the Tauri backend, not modeled here. */
export interface TakeoverState {
  /** True only while the full-desktop takeover layout is mounted. Ships false. */
  active: boolean;
}

/** The initial (shipped) takeover state: OFF. Nothing auto-enters takeover. */
export function initialTakeoverState(): TakeoverState {
  return { active: false };
}

/** The takeover transitions. `enter`/`exit` are explicit operator intents; the
 *  reducer is idempotent so a double-enter or double-exit cannot wedge the bit. */
export type TakeoverAction = { type: "enter" } | { type: "exit" };

/** Fold a takeover action into the state. PURE + idempotent: enter sets active,
 *  exit clears it, and repeating either is a no-op (returns the same reference so
 *  React skips a needless re-render). */
export function takeoverReduce(state: TakeoverState, action: TakeoverAction): TakeoverState {
  switch (action.type) {
    case "enter":
      return state.active ? state : { active: true };
    case "exit":
      return state.active ? { active: false } : state;
    default:
      return state;
  }
}

/**
 * THE EXIT-REACHABILITY INVARIANT. Whenever takeover is active, the operator MUST
 * have a visible, always-present way out (the in-HUD EXIT control) — there is no
 * active state that may hide it. The layout renders the EXIT control iff this is
 * true, and the test asserts it holds for every active state. When inactive there
 * is nothing to exit, so the control is not rendered (the windowed HUD has OS
 * chrome of its own).
 */
export function exitAlwaysReachable(state: TakeoverState): boolean {
  // The control is present exactly when (and because) takeover is active.
  return state.active === true;
}

/**
 * Does this keyboard key trigger a takeover exit? Esc is the always-available
 * keyboard escape hatch (independent of the visible control and of any backend
 * global shortcut). PURE so the App's key handler is a thin wrapper over this and
 * the predicate itself is unit-testable without a DOM.
 */
export function isExitKey(key: string): boolean {
  return key === "Escape";
}
