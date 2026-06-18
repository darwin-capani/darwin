import type {
  UnifiedCoverage,
  UnifiedHit,
  UnifiedSearchResult,
  UnifiedSource,
} from "../core/events";
import {
  unifiedCoverageSummary,
  unifiedSkipReasonLabel,
  unifiedSourceLabel,
  unifiedSourceOnDevice,
} from "../core/events";
import Frame from "./Frame";

/**
 * UNIFIED SEARCH // EVERYTHING — the read-only surface for ONE query fanned out
 * across every AVAILABLE source (daemon/src/unified_search.rs +
 * anthropic.rs::unified_search_tool). It groups the merged hits BY SOURCE, each
 * with its real CITATION, and prints an HONEST coverage line — which sources
 * were SEARCHED vs SKIPPED (each skip with a reason).
 *
 * Citations come in two honesty tiers, rendered verbatim from the daemon:
 *   - PER-ITEM anchors for the on-device sources (file path+offset / episode /
 *     fact key / world entity) — each points at exactly one concrete item.
 *   - SOURCE-LEVEL anchors for the cloud sources whose gated read returns a human
 *     SUMMARY, not per-item ids: the anchor names the READ the user can reproduce
 *     ("gmail recent messages (search: …)", "slack channel list (search: …)"),
 *     NOT a fabricated message/event id. The daemon never invents an id the read
 *     did not expose; the HUD just shows the honest anchor string it was handed.
 *
 * HONESTY CONTRACT (load-bearing, do not regress):
 *   - ON-DEVICE sources (Files / Past conversations / Memory / World model) are
 *     ALWAYS available and their content NEVER leaves the device — this event is
 *     the local 127.0.0.1 telemetry broadcast only.
 *   - CLOUD sources (Gmail / Calendar / Slack) are searched ONLY when CONNECTED.
 *     A disconnected cloud source is rendered as a SKIP with its honest reason
 *     ("not connected") — NEVER as a silent drop and NEVER as a fake hit.
 *   - CITES REAL ITEMS, NEVER FABRICATES. Every hit is a real returned item the
 *     daemon cited (the parser drops any hit with no source or no citation anchor
 *     to point at). An all-empty fan-out shows the honest "nothing found" state
 *     and still prints the coverage line, so the user always knows the answer's
 *     reach — never a fabricated result.
 *   - COVERAGE NEVER CONFLATES searched with skipped. The coverage line names the
 *     searched sources and, separately, each skipped source with its reason. An
 *     empty searched set says "Searched no sources." rather than implying reach.
 *   - REVIEW-ONLY. There is NO button here that searches, connects, or acts.
 *     Searching is a SPOKEN intent ("search everything for…"); this panel only
 *     SHOWS the last cited result + the honest coverage, mirroring the other
 *     read-only surfaces (DocSearch / MCP / Memory).
 *
 * The reducer only ever sets `unifiedSearch` from a defensively-parsed
 * `unified.searched` (only real returned hits, honest coverage), so this
 * component can trust the fields it is handed.
 */
export default function UnifiedSearchPanel({
  result,
}: {
  result: UnifiedSearchResult | null;
}) {
  // Nothing to show until the user has run a unified search — render nothing
  // rather than a placeholder, mirroring the other event-fed panels
  // (DocSearchPanel, McpPanel). The event simply never arrives until then.
  if (result === null) return null;

  return (
    <div className="unified-panel">
      <Frame title="UNIFIED SEARCH // EVERYTHING" tag="PRIVATE · REVIEW ONLY">
        <div className="unified-body">
          {result.query.length > 0 && (
            <div className="unified-query">
              <span className="unified-query-label">QUERY</span>
              <span className="unified-query-text">{result.query}</span>
            </div>
          )}

          <CoverageLine coverage={result.coverage} />
          <Results result={result} />

          <div className="unified-foot dim-note">
            One query, fanned out across every available source. On-device sources
            (Files, Past conversations, Memory, World model) are always searched
            and their content NEVER leaves this machine — these results ride the
            local broadcast only. Cloud sources (Gmail, Calendar, Slack) are
            searched ONLY when connected (the existing read-only reads); a
            disconnected source is shown as skipped, never silently dropped. Every
            hit cites a real item — nothing is fabricated; an empty result is the
            honest &ldquo;nothing found&rdquo;. Say{" "}
            <b>&ldquo;search everything for&hellip;&rdquo;</b> to run a unified
            search.
          </div>
        </div>
      </Frame>
    </div>
  );
}

/** The HONEST coverage line: the searched sources (as on-device / cloud pills)
 *  and, separately, each skipped source with its reason. Never conflates the two;
 *  an empty searched set reads "Searched no sources." The machine-token chips let
 *  the user verify reach at a glance; the full sentence is the title/aria text. */
function CoverageLine({ coverage }: { coverage: UnifiedCoverage }) {
  const summary = unifiedCoverageSummary(coverage);
  return (
    <div className="unified-coverage" title={summary} aria-label={summary}>
      <div className="unified-coverage-row">
        <span className="unified-coverage-label">SEARCHED</span>
        {coverage.searched.length === 0 ? (
          <span className="unified-coverage-none">no sources</span>
        ) : (
          <span className="unified-coverage-chips">
            {coverage.searched.map((src) => (
              <SourcePill key={src} source={src} />
            ))}
          </span>
        )}
      </div>
      {coverage.skipped.length > 0 && (
        <div className="unified-coverage-row">
          <span className="unified-coverage-label">SKIPPED</span>
          <span className="unified-coverage-chips">
            {coverage.skipped.map((s) => (
              <span
                key={`${s.source}:${s.reason}`}
                className="unified-skip"
                title={`${unifiedSourceLabel(s.source)} — ${unifiedSkipReasonLabel(
                  s.reason,
                )}`}
              >
                {unifiedSourceLabel(s.source)}
                <span className="unified-skip-reason">
                  {" "}
                  ({unifiedSkipReasonLabel(s.reason)})
                </span>
              </span>
            ))}
          </span>
        </div>
      )}
    </div>
  );
}

/** One searched-source pill, badged ON-DEVICE (private) or CLOUD (connected) so
 *  the privacy posture reads at a glance — on-device content never leaves; cloud
 *  appears only when connected. */
function SourcePill({ source }: { source: UnifiedSource }) {
  const onDevice = unifiedSourceOnDevice(source);
  return (
    <span
      className={`unified-src ${onDevice ? "on-device" : "cloud"}`}
      title={
        onDevice
          ? "on-device — content never leaves this machine"
          : "cloud — searched only because it is connected (read-only)"
      }
    >
      {unifiedSourceLabel(source)}
    </span>
  );
}

/** The merged result, grouped BY SOURCE in the daemon's deterministic ranked
 *  order. An empty hits[] is the honest "nothing found" (the coverage line above
 *  still reports which sources were searched) — shown, never hidden or faked. */
function Results({ result }: { result: UnifiedSearchResult }) {
  if (result.hits.length === 0) {
    return (
      <div className="unified-empty dim-note">
        Nothing matched across the searched sources. This is the honest result —
        no item is invented. The coverage line above shows exactly which sources
        were reached.
      </div>
    );
  }

  // Group consecutive hits by source. The daemon already sorted the merged list
  // (score DESC, then a deterministic tie-break), grouping each source's hits
  // together in rank order, so a single linear pass preserves that ranking while
  // attributing every hit to its source header.
  const groups: { source: UnifiedSource; label: string; hits: UnifiedHit[] }[] = [];
  for (const h of result.hits) {
    const last = groups[groups.length - 1];
    if (last && last.source === h.source) {
      last.hits.push(h);
    } else {
      groups.push({ source: h.source, label: h.sourceLabel, hits: [h] });
    }
  }

  return (
    <div className="unified-groups">
      {groups.map((g, gi) => (
        <div className="unified-group" key={`${g.source}:${gi}`}>
          <div className="unified-group-head">
            <span className="unified-group-title">{g.label}</span>
            <SourcePill source={g.source} />
            <span className="unified-group-count">{g.hits.length}</span>
          </div>
          <div className="unified-hits">
            {g.hits.map((h, hi) => (
              <HitRow key={`${h.citation}:${hi}`} hit={h} />
            ))}
          </div>
        </div>
      ))}
    </div>
  );
}

/** One cited hit: the real citation anchor — either a verifiable per-item anchor
 *  (file+offset / episode / fact key / world entity) or, for a cloud-summary hit,
 *  the honest source-level anchor naming the gated read ("gmail recent messages
 *  (search: …)") rather than a fabricated id — plus the blended relevance score,
 *  the optional timestamp (only when the source carries one — honest, never
 *  invented), and the snippet/title the daemon already cited. */
function HitRow({ hit }: { hit: UnifiedHit }) {
  return (
    <div className="unified-hit">
      <div className="unified-hit-head">
        <span className="unified-hit-cite" title={hit.citation}>
          {hit.citation}
        </span>
        {hit.ts !== null && hit.ts.length > 0 && (
          <span className="unified-hit-ts" title="when this item is from">
            {hit.ts}
          </span>
        )}
        <span
          className="unified-hit-score"
          title="blended relevance score (higher = more relevant)"
        >
          {hit.score.toFixed(3)}
        </span>
      </div>
      {hit.title.length > 0 && hit.title !== hit.snippet && (
        <div className="unified-hit-title">{hit.title}</div>
      )}
      {hit.snippet.length > 0 && (
        <div className="unified-hit-snippet">{hit.snippet}</div>
      )}
    </div>
  );
}
