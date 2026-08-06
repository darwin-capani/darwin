import type { AppFeed, FeedItem } from "../core/state";
import Frame from "./Frame";

/** Manifest name of the global-scan micro-app (apps/global-scan/manifest.toml).
 *  The panel renders exactly this app's feed slice; other app names are
 *  ignored here (each surface owns its own component). */
export const GLOBAL_SCAN_APP = "global-scan";

/** Values marking an item that needs the red alert accent. RED is reserved for
 *  alerts per the FUI restyle contract — a routine "world"/"tech" item must
 *  never light red. */
const ALERT_TOKENS: ReadonlySet<string> = new Set(["breaking", "alert", "urgent"]);

/** Tolerant token test. coerceFeedItem always supplies these fields, but the
 *  panel must not throw on a hand-built/partial item — the same tolerance
 *  relTime already gives an unparseable `published`. */
function alertToken(value: string | undefined): boolean {
  return value !== undefined && ALERT_TOKENS.has(value.trim().toLowerCase());
}

/**
 * Does this item carry an alert verdict?
 *
 * WHAT WENT WRONG: this used to read ONLY `category`, but the app puts the
 * feeds.toml SECTION NAME there (world / tech / science / markets — the shipped
 * registry and the DEFAULT_FEEDS fallback hold nothing else) and puts the
 * verdict in a SEPARATE `flag` field, computed by `_ALERT_RE` over the
 * headline. None of the section names is in ALERT_TOKENS, so the red accent was
 * unreachable: "BREAKING: explosion downtown" rendered exactly like a routine
 * tech item, and the `.gs-item.alert` / `.gs-chip.alert` styling was dead CSS.
 *
 * `flag` is now the load-bearing signal. `category` is still consulted so a
 * user-authored `[breaking]` section in their own feeds.toml keeps working.
 */
function isAlert(item: FeedItem): boolean {
  return alertToken(item.flag) || alertToken(item.category);
}

/** Compact relative time from an iso8601 (or feed-native) timestamp. Returns
 *  "" for anything unparseable so the row simply omits the time chip. */
function relTime(published: string, now: number): string {
  if (!published) return "";
  const t = Date.parse(published);
  if (Number.isNaN(t)) return "";
  const secs = Math.max(0, Math.round((now - t) / 1000));
  if (secs < 60) return "NOW";
  const mins = Math.round(secs / 60);
  if (mins < 60) return `${mins}M`;
  const hrs = Math.round(mins / 60);
  if (hrs < 24) return `${hrs}H`;
  const days = Math.round(hrs / 24);
  return `${days}D`;
}

export default function GlobalScanPanel({
  feed,
  running,
}: {
  /** The global-scan app's feed slice, or undefined if it never reported. */
  feed: AppFeed | undefined;
  /** Tracked-running flag from state.runningApps (authoritative over feed). */
  running: boolean;
}) {
  // Live (not running) and never-reported => offline placeholder. A stopped
  // app that previously reported keeps showing its last items, dimmed.
  const online = running || feed?.running === true;
  const items = feed?.items ?? [];
  const now = Date.now();

  const tag =
    feed && (feed.feedsOk !== null || feed.feedsFailed !== null)
      ? `${feed.feedsOk ?? 0} OK · ${feed.feedsFailed ?? 0} FAIL`
      : online
        ? `${items.length} ITEM${items.length === 1 ? "" : "S"}`
        : "OFFLINE";

  return (
    <Frame className={`global-scan ${online ? "" : "offline"}`} title="GLOBAL-SCAN // INTEL FEED" tag={tag}>
      {!online && items.length === 0 ? (
        <div className="gs-placeholder">
          <div className="gs-ph-big">GLOBAL-SCAN OFFLINE</div>
          <div className="gs-ph-small">say "open global scan"</div>
        </div>
      ) : (
        <>
          {feed?.brief ? (
            <div className="gs-brief">
              <span className="gs-brief-label">BRIEF</span>
              <span className="gs-brief-text">{feed.brief}</span>
            </div>
          ) : null}
          {/* a11y: DELIBERATELY NOT a live region. The reducer REPLACES the
              item list wholesale each poll (index-keyed rows all remount when
              one headline shifts), so role="log" would re-announce the entire
              20-item feed every cycle — announcement spam, not a chat log.
              The feed stays readable static content inside its named panel. */}
          <div className="gs-scroll">
            {items.length === 0 && (
              <div className="gs-empty dim-note">acquiring feeds…</div>
            )}
            {items.map((it, i) => {
              const alert = isAlert(it);
              const when = relTime(it.published, now);
              return (
                <div
                  key={`${it.url || it.title}-${i}`}
                  className={`gs-item ${alert ? "alert" : ""}`}
                >
                  <div className="gs-meta">
                    {it.category ? (
                      <span className={`gs-chip ${alert ? "alert" : ""}`}>
                        {it.category.toUpperCase()}
                      </span>
                    ) : null}
                    {it.source ? <span className="gs-source">{it.source}</span> : null}
                    {when ? <span className="gs-time">{when}</span> : null}
                  </div>
                  <div className="gs-headline">{it.title}</div>
                  {it.summary ? <div className="gs-summary">{it.summary}</div> : null}
                </div>
              );
            })}
          </div>
        </>
      )}
    </Frame>
  );
}
