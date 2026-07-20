import { useLayoutEffect, useMemo, useRef, useState } from "react";
import Frame from "./Frame";
import { sendCommand } from "../tauri/command";
import {
  buildPaletteItems,
  filterPalette,
  resolveAction,
  resolveFreeText,
  type PaletteItem,
  type PaletteSources,
} from "../core/palette";

/**
 * The Cmd-K COMMAND PALETTE: a keyboard-invoked overlay that enumerates the
 * actuatable capability surface (built-in verbs, apps, agents) and lets the user
 * filter + invoke one — or type a free command and send it as an utterance.
 *
 * AUTHORITY: none new. Every selection routes through `sendCommand` over the SAME
 * bounded verb channel the Command Deck uses (`ask` / `brief`), so it reaches the
 * daemon's normal router + gate exactly as a spoken phrase would — a consequential
 * action still parks for a spoken confirm. The palette only makes the surface
 * discoverable and fast to reach; it can actuate nothing the voice path cannot.
 *
 * The matching/enumeration logic is the pure [`core/palette`] module; this
 * component is the thin overlay + keyboard-nav + send wiring.
 */
export default function CommandPalette({
  open,
  onClose,
  sources,
}: {
  open: boolean;
  onClose: () => void;
  sources: PaletteSources;
}) {
  const [query, setQuery] = useState("");
  const [selected, setSelected] = useState(0);
  const [targetAgent, setTargetAgent] = useState<string | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLUListElement>(null);

  const items = useMemo(() => buildPaletteItems(sources), [sources]);
  const filtered = useMemo(() => filterPalette(items, query), [items, query]);

  // Reset transient state the MOMENT the palette opens, and focus the input.
  // useLayoutEffect commits BEFORE paint so reopening never flashes the previous
  // session's query / results / @agent chip for a frame.
  useLayoutEffect(() => {
    if (open) {
      setQuery("");
      setSelected(0);
      setTargetAgent(null);
      inputRef.current?.focus();
    }
  }, [open]);

  // Keep the selection in range as the filtered list shrinks.
  useLayoutEffect(() => {
    setSelected((s) => (filtered.length === 0 ? 0 : Math.min(s, filtered.length - 1)));
  }, [filtered.length]);

  // Scroll the highlighted row into view on keyboard navigation (an off-screen
  // selection is invisible otherwise). "nearest" avoids yanking the whole list.
  useLayoutEffect(() => {
    if (!open) return;
    const el = listRef.current?.querySelector<HTMLElement>('[data-selected="true"]');
    el?.scrollIntoView({ block: "nearest" });
  }, [selected, open]);

  if (!open) return null;

  const runResolution = (item: PaletteItem) => {
    const res = resolveAction(item.action, targetAgent);
    if (res.kind === "send") {
      void sendCommand(res.command);
      onClose();
    } else {
      // target-agent: address the next ask, keep the palette open.
      setTargetAgent(res.agent);
      setQuery("");
      setSelected(0);
      inputRef.current?.focus();
    }
  };

  const submitFreeText = () => {
    const cmd = resolveFreeText(query, targetAgent);
    if (!cmd) return;
    void sendCommand(cmd);
    onClose();
  };

  const onKeyDown = (ev: React.KeyboardEvent<HTMLInputElement>) => {
    if (ev.key === "Escape") {
      ev.preventDefault();
      // Esc clears an agent address first (a reversible step), then closes.
      if (targetAgent) setTargetAgent(null);
      else onClose();
      return;
    }
    if (ev.key === "Tab") {
      // Focus trap: the input is the only focusable control, so keep focus here
      // instead of tabbing into the obscured HUD behind the modal.
      ev.preventDefault();
      return;
    }
    if (ev.key === "ArrowDown") {
      ev.preventDefault();
      setSelected((s) => (filtered.length === 0 ? 0 : (s + 1) % filtered.length));
    } else if (ev.key === "ArrowUp") {
      ev.preventDefault();
      setSelected((s) => (filtered.length === 0 ? 0 : (s - 1 + filtered.length) % filtered.length));
    } else if (ev.key === "Enter") {
      ev.preventDefault();
      const item = filtered[selected];
      // Run the highlighted capability item; when nothing matches, fall back to
      // sending the raw query as an utterance (the quick-command path).
      if (item) runResolution(item);
      else submitFreeText();
    }
  };

  const activeId = filtered[selected] ? `palette-opt-${selected}` : undefined;

  return (
    <div className="palette-backdrop" onClick={onClose}>
      <div
        className="palette"
        role="dialog"
        aria-label="Command Palette"
        aria-modal="true"
        onClick={(e) => e.stopPropagation()}
      >
        <Frame title="COMMAND PALETTE" tag="⌘K">
          <div className="palette-input-row">
            {targetAgent && (
              <span className="palette-agent-chip" title="Addressing this agent">
                @{targetAgent}
              </span>
            )}
            <input
              ref={inputRef}
              className="palette-input"
              type="text"
              value={query}
              placeholder={
                targetAgent
                  ? `Ask ${targetAgent}…`
                  : "Search capabilities or type a command…"
              }
              onChange={(e) => {
                setQuery(e.target.value);
                setSelected(0);
              }}
              onKeyDown={onKeyDown}
              role="combobox"
              aria-expanded={true}
              aria-controls="palette-list"
              aria-activedescendant={activeId}
              aria-label="Command palette query"
              autoComplete="off"
              spellCheck={false}
            />
          </div>

          <ul id="palette-list" ref={listRef} className="palette-list" role="listbox" aria-label="Capabilities">
            {filtered.length === 0 && (
              <li className="palette-empty">
                {query.trim()
                  ? `Press Enter to ask: "${query.trim()}"`
                  : "No capabilities available yet."}
              </li>
            )}
            {filtered.map((item, i) => (
              <li
                key={item.id}
                id={`palette-opt-${i}`}
                role="option"
                aria-selected={i === selected}
                data-selected={i === selected}
                className={`palette-item${i === selected ? " active" : ""}`}
                onMouseMove={() => setSelected(i)}
                onClick={() => runResolution(item)}
              >
                <span className={`palette-group palette-group-${item.group.toLowerCase()}`}>
                  {item.group}
                </span>
                <span className="palette-label">{item.label}</span>
                <span className="palette-hint">{item.hint}</span>
              </li>
            ))}
          </ul>

          <div className="palette-foot" aria-hidden="true">
            <span>↑↓ navigate</span>
            <span>⏎ run</span>
            <span>esc close</span>
          </div>
        </Frame>
      </div>
    </div>
  );
}
