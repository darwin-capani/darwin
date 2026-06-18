import type { Toast } from "../core/state";

/**
 * Toast stack. Keys are the reducer's monotonic toast ids (stable across
 * re-renders), the container stays mounted (no whole-column remount), and
 * expired/evicted toasts carry `exiting` for a CSS fade/slide-out during
 * the reducer's TOAST_EXIT_MS grace window — no single-frame cuts.
 */
export default function Toasts({ toasts }: { toasts: Toast[] }) {
  return (
    <div className="toasts" aria-live="polite">
      {toasts.map((t) => (
        <div key={t.id} className={`toast ${t.kind} ${t.exiting ? "exiting" : ""}`}>
          {t.text}
        </div>
      ))}
    </div>
  );
}
