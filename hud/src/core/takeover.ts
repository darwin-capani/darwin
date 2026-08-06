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

/* --- App's ENTER / EXIT / Esc-guard BODIES (injectable seams) -------------- *
 *
 * WHAT WENT WRONG: the enter/exit handlers and the Esc guard lived inline in
 * App.tsx, and takeover.test.ts's "App takeover handlers" block claimed to be
 * "modeled exactly as App.tsx wires them ... so the assertions track the real
 * handlers" while importing nothing from App at all. Proven by mutation: making
 * App's exit handler an empty body AND killing its Esc guard
 * (`if (false && takeoverActive && isExitKey(ev.key))`) left the whole HUD
 * suite green — i.e. the "EXIT IS ALWAYS REACHABLE" invariant, on a chrome-less
 * full-desktop mode with no OS escape, had no coverage at the wiring level.
 *
 * The bodies live here now and App calls them, so the test drives the real
 * code. The OS-level window/Dock restore still belongs to the Tauri backend's
 * exit_takeover (proven by the src-tauri cargo tests, not here). */

/** The seams App hands to the takeover handlers: the reducer dispatch plus the
 *  two device-gated backend commands. */
export interface TakeoverHandlerDeps {
  dispatch(action: TakeoverAction): void;
  enter(): Promise<boolean>;
  exit(): Promise<boolean>;
}

/** App's ENTER body: flip the HUD bit, then ask the backend to mutate the real
 *  window (a graceful no-op outside the Tauri shell). */
export function runEnterTakeover(deps: TakeoverHandlerDeps): void {
  deps.dispatch({ type: "enter" });
  void deps.enter();
}

/** App's EXIT body — the always-available escape hatch. Clears the HUD bit AND
 *  asks the backend to reverse every window mutation. BOTH the visible EXIT
 *  control and the Esc key route here; it must never become conditional. */
export function runExitTakeover(deps: TakeoverHandlerDeps): void {
  deps.dispatch({ type: "exit" });
  void deps.exit();
}

/**
 * App's keydown guard for takeover: Esc exits FIRST, and ONLY while takeover is
 * active (so an idle Esc still reaches the deck/palette handlers). Returns true
 * when it HANDLED the key — App then preventDefaults and stops. Pure of the
 * DOM, so the guard itself is assertable.
 */
export function handleTakeoverKey(
  active: boolean,
  key: string,
  deps: TakeoverHandlerDeps,
): boolean {
  if (!active || !isExitKey(key)) return false;
  runExitTakeover(deps);
  return true;
}
