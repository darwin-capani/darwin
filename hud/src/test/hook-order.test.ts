import { describe, it, expect } from "vitest";
import React from "react";
import CommandPalette from "../components/CommandPalette";
import CommandDeck from "../components/CommandDeck";

/**
 * A COMPONENT MUST CALL THE SAME HOOKS WHETHER IT IS OPEN OR CLOSED.
 *
 * CommandPalette had `if (!open) return null` ABOVE its `useRef` +
 * `useModalFocus` calls. The closed render made 10 hook calls, the open render
 * 13. React throws "Rendered more hooks than during the previous render" on that
 * transition — and because the throw escaped the per-column error boundaries it
 * reached the ROOT one, so the first Cmd-K on a fresh HUD replaced the entire
 * interface with the "HUD ERROR / Reload" screen. The palette never opened once.
 *
 * The rest of the suite structurally cannot catch this: every other case is a
 * fresh `renderToStaticMarkup` mount, and one mount is always internally
 * consistent. The bug only exists across TWO renders of the SAME component.
 *
 * This test needs no DOM (the suite runs in a node environment): it installs a
 * counting dispatcher into React's real internals and calls the component
 * function directly, recording the hook sequence for each value of `open`.
 */

const internals = (React as unknown as {
  __CLIENT_INTERNALS_DO_NOT_USE_OR_WARN_USERS_THEY_CANNOT_UPGRADE: { H: unknown };
}).__CLIENT_INTERNALS_DO_NOT_USE_OR_WARN_USERS_THEY_CANNOT_UPGRADE;

type Props = Record<string, unknown>;

/** The exact ordered hook names the component calls for these props. */
function hookSequence(Comp: (p: Props) => unknown, props: Props): string[] {
  const seq: string[] = [];
  const prev = internals.H;
  const t = <T,>(n: string, r: T): T => {
    seq.push(n);
    return r;
  };
  internals.H = {
    useState: (i: unknown) => t("useState", [typeof i === "function" ? (i as () => unknown)() : i, () => {}]),
    useReducer: (_r: unknown, i: unknown, init?: (a: unknown) => unknown) =>
      t("useReducer", [init ? init(i) : i, () => {}]),
    useRef: (i: unknown) => t("useRef", { current: i }),
    useMemo: (f: () => unknown) => t("useMemo", f()),
    useLayoutEffect: () => t("useLayoutEffect", undefined),
    useEffect: () => t("useEffect", undefined),
    useCallback: (f: unknown) => t("useCallback", f),
    useContext: () => t("useContext", undefined),
    useDebugValue: () => t("useDebugValue", undefined),
    useId: () => t("useId", ":r0:"),
  };
  try {
    Comp(props);
  } finally {
    internals.H = prev;
  }
  return seq;
}

const PALETTE_PROPS = { onClose: () => {}, sources: { apps: [], agents: [] } };

describe("hook order across an open/closed transition", () => {
  it("CommandPalette calls identical hooks closed and open", () => {
    const closed = hookSequence(CommandPalette as never, { ...PALETTE_PROPS, open: false });
    const opened = hookSequence(CommandPalette as never, { ...PALETTE_PROPS, open: true });
    // Guard the test's own precondition: a dispatcher that recorded nothing
    // would make the comparison below trivially true.
    expect(closed.length).toBeGreaterThan(5);
    // A differing sequence means the first Cmd-K throws and the root error
    // boundary blanks the whole HUD.
    expect(opened).toEqual(closed);
  });

  it("CommandDeck too — it has always ordered this correctly, and must keep doing so", () => {
    const props = { items: [], onRun: () => {}, onClose: () => {} };
    const closed = hookSequence(CommandDeck as never, { ...props, open: false });
    const opened = hookSequence(CommandDeck as never, { ...props, open: true });
    expect(opened).toEqual(closed);
  });
});
