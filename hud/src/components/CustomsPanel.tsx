import type { EgressItem, EgressManifest } from "../core/events";
import { egressByteLabel, egressIsClean } from "../core/events";
import Frame from "./Frame";

/**
 * CUSTOMS // EGRESS — the read-only pre-flight egress surface, fed by the daemon's
 * `boundary.manifest` (daemon/src/boundary.rs EgressManifest::telemetry()),
 * emitted by the CLOUD path (anthropic.rs complete_with_tools) BEFORE each cloud
 * request leaves the box.
 *
 * It answers one question honestly: of the personal context DARWIN is about to
 * send to the cloud this turn, WHAT is leaving and HOW MUCH? The manifest is an
 * itemized INVENTORY — facts / history / world rows / persona / system prompt —
 * each with a coarse sensitivity band (personal / contextual / public), a unit
 * count, and a byte size. When a REDUCE-ONLY trim is active it also shows what was
 * WITHHELD ('no facts' drops facts; 'no memory' drops facts + history).
 *
 * HONESTY CONTRACT (do not regress):
 *   - INVENTORY, NEVER CONTENT. Every row is a category label + sensitivity band +
 *     count + size. The manifest carries NO fact value, NO history text, NO
 *     utterance — there is nothing here to leak. SECRET-FREE by construction.
 *   - REDUCE-ONLY. A trim can only WITHHOLD whole categories, never add one. A
 *     trimmed turn is labeled honestly (the active trim + the withheld list); the
 *     readout never claims to have sent something it dropped.
 *   - CLOUD ONLY — NEVER CLAIMS TO GATE THE LOCAL PATH. The local inference path
 *     egresses nothing off the box, so it never reaches CUSTOMS. The panel states
 *     this plainly (read-only; the local path never leaves the box), pinned
 *     HUD-side so a hostile payload can't flip it.
 *   - READ-ONLY. There is NO button here. This panel only SHOWS the manifest the
 *     daemon already produced before the request went out.
 *
 * The reducer holds `egressManifest` at null until the first cloud turn emits one
 * (a local-only session leaves it null), so this component renders nothing until
 * there is a real cloud egress to show.
 */
export default function CustomsPanel({ manifest }: { manifest: EgressManifest | null }) {
  // Nothing to show until a cloud turn produced a manifest. A local-only session
  // never egresses, so the reducer holds this at null and we render nothing —
  // mirroring the other event-fed panels (BriefFocusPanel, AnswerSourcesPanel).
  if (manifest === null) return null;

  const clean = egressIsClean(manifest);

  return (
    <div className="customs-panel">
      <Frame title="CUSTOMS // EGRESS" tag="HONEST · READ ONLY">
        <div className="customs-body">
          <div className="customs-head">
            <span className="customs-title">CLOUD EGRESS MANIFEST</span>
            {clean ? (
              <span
                className="customs-pill full"
                title="no trim active — the full assembled context is being sent this cloud turn"
              >
                FULL CONTEXT
              </span>
            ) : (
              <span
                className="customs-pill trimmed"
                title="a reduce-only trim withheld one or more categories from this cloud turn"
              >
                TRIMMED · {manifest.trim.toUpperCase()}
              </span>
            )}
            <span
              className="customs-pill total"
              title="total bytes of personal context leaving on this cloud turn"
            >
              {egressByteLabel(manifest.totalBytes)}
            </span>
          </div>

          <EgressItems items={manifest.items} />
          <Withheld manifest={manifest} />

          <div className="customs-foot dim-note">
            CUSTOMS inspects the CLOUD egress before it leaves and can only ever
            WITHHOLD context (reduce-only) — it never mutates or adds. Your LOCAL
            model never leaves the box, so nothing here gates it.
          </div>
        </div>
      </Frame>
    </div>
  );
}

/** The inventoried egress slices: one row per category with its sensitivity band,
 *  unit count, and byte size. Honest-empty when a cloud turn somehow sent nothing
 *  (never padded). */
function EgressItems({ items }: { items: EgressItem[] }) {
  if (items.length === 0) {
    return (
      <div className="customs-empty dim-note">
        Nothing inventoried — this cloud turn carried no assembled context.
      </div>
    );
  }
  return (
    <div className="customs-item-list">
      {items.map((it) => (
        <EgressRow key={it.category} item={it} />
      ))}
    </div>
  );
}

/** One egress row: the category, its sensitivity band, the unit count, and the
 *  byte size — the honest shape of what is leaving, never the content. */
function EgressRow({ item }: { item: EgressItem }) {
  return (
    <div className="customs-item">
      <span
        className={`customs-sens sens-${item.sensitivity}`}
        title="the coarse sensitivity band of this slice of egress"
      >
        {item.sensitivity}
      </span>
      <span className="customs-cat">{item.category}</span>
      <span className="customs-count" title="discrete units in this slice (facts, conversation turns, or one block)">
        {item.count === 1 ? "1 unit" : `${item.count} units`}
      </span>
      <span className="customs-bytes" title="byte size of this slice's content">
        {egressByteLabel(item.bytes)}
      </span>
    </div>
  );
}

/** What a reduce-only trim WITHHELD from this cloud turn — the honest "held back"
 *  list. Renders the clean pass-through note when nothing was withheld. */
function Withheld({ manifest }: { manifest: EgressManifest }) {
  if (egressIsClean(manifest)) {
    return (
      <div className="customs-withheld clean dim-note">
        No trim active — the full assembled context is being sent. Set{" "}
        <code>[boundary].default_trim</code> (or say &ldquo;no memory&rdquo; /
        &ldquo;don&rsquo;t send my facts&rdquo;) to withhold a category.
      </div>
    );
  }
  return (
    <div className="customs-withheld held">
      <span className="customs-withheld-label" title="categories a reduce-only trim held back from this cloud turn">
        WITHHELD
      </span>
      {manifest.withheld.length === 0 ? (
        <span className="customs-withheld-value muted">nothing this turn</span>
      ) : (
        <span className="customs-withheld-value">
          {manifest.withheld.map((c) => (
            <span key={c} className="customs-held-cat">
              {c}
            </span>
          ))}
        </span>
      )}
    </div>
  );
}
