import type { ReportReadout, ReportCitation } from "../core/events";
import Frame from "./Frame";

/**
 * REPORT GENERATION (#40) — the read-only readout for a report the daemon's
 * report.rs assembled from already-cited notebook/research sources (emitted as
 * `report.built` from router.rs).
 *
 * It surfaces the MOST RECENT built report:
 *   - the report TITLE (the topic, as asked);
 *   - the SECTION count + the section headings (a bounded preview);
 *   - the CITATION count + the REAL citations (run-local id + title + url) — the
 *     actual source refs the input claims carried; AND
 *   - the honest-empty state when no source was citable.
 *
 * HONESTY CONTRACT (do not regress):
 *   - CITES ONLY REAL SOURCES, NEVER FABRICATES. Every citation row is a real
 *     source ref an input claim carried (the daemon drops any uncited claim and
 *     never synthesizes a citation; the parser drops any citation with no usable
 *     locator). There is nothing here to invent a citation from.
 *   - HONEST EMPTY. An empty report (no citable source) says "no sources to
 *     report on" plainly rather than pretending a body or a citation.
 *   - BOUNDED PREVIEW. This is a PREVIEW (title + counts + headings + the cited
 *     locators), not the full document — the rendered markdown is spoken/shown
 *     elsewhere. The counts are the daemon's real totals even when the preview
 *     list is bounded shorter.
 *   - REVIEW-ONLY. There is NO button — the daemon already built the report; this
 *     panel only SHOWS it.
 *   - SHIPPED OFF. The [report].enabled gate ships false; until it is enabled the
 *     off verb carries no report and this panel renders NOTHING.
 *   - SECRET-FREE. The wire carries only the title, counts, headings, and the
 *     real citation locators — never raw body content, never an embedding/secret.
 *
 * The reducer only ever sets `report` from a defensively-parsed `report.built`
 * that carried a report object (the off/error verbs carry report:null, kept as a
 * no-op) — so this component can trust the readout it is handed.
 */
export default function ReportPanel({ report }: { report: ReportReadout | null }) {
  // Nothing to show until a report.built carries a report. The [report].enabled
  // gate ships OFF, so the reducer holds `report` at null until it is enabled AND
  // a "generate a report on X" command runs — render nothing rather than a
  // placeholder.
  if (report === null) return null;

  const title = report.title.length > 0 ? report.title : "Report";

  return (
    <div className="report-panel">
      <Frame title="REPORT // GENERATED" tag="CITED · REVIEW ONLY">
        <div className="report-body">
          <div className="report-head">
            <span className="report-title" title="the report topic, as asked">
              {title}
            </span>
            {report.empty ? (
              <span
                className="report-pill empty"
                title="no source was citable — an honest empty report, never a fabricated body"
              >
                NO SOURCES
              </span>
            ) : (
              <span className="report-pill built" title="the report's section and citation totals">
                {report.sectionCount} {report.sectionCount === 1 ? "SECTION" : "SECTIONS"} ·{" "}
                {report.citationCount} CITED
              </span>
            )}
          </div>

          {report.empty ? (
            <ReportEmpty />
          ) : (
            <>
              <SectionsPreview headings={report.headings} total={report.sectionCount} />
              <CitationsPreview citations={report.citations} total={report.citationCount} />
            </>
          )}

          <div className="report-foot dim-note">
            Every citation is a REAL source the research actually consulted and the
            notebook persisted — assembled from already-cited material, never
            invented. An empty report says so plainly. This is a review-only
            preview; the full report was rendered for you. Report generation ships
            OFF behind <code>[report].enabled</code>.
          </div>
        </div>
      </Frame>
    </div>
  );
}

/** The honest-empty state — the daemon found no citable source on the topic, so
 *  the report is honestly empty. Say so; never show a fabricated body/citation. */
function ReportEmpty() {
  return (
    <div className="report-empty dim-note">
      No sources to report on — nothing cited was available on this topic, so there
      was nothing to assemble. No body or citation is invented.
    </div>
  );
}

/** The section headings preview — the ordered headings the report assembled. The
 *  count is the daemon's real total; when the preview is bounded shorter we say
 *  how many more there are. */
function SectionsPreview({ headings, total }: { headings: string[]; total: number }) {
  if (headings.length === 0) return null;
  const more = total - headings.length;
  return (
    <div className="report-sections">
      <div className="report-sections-head">
        <span className="report-sections-title">SECTIONS</span>
      </div>
      <ol className="report-section-list">
        {headings.map((h, i) => (
          <li className="report-section-item" key={`${h}:${i}`}>
            {h}
          </li>
        ))}
      </ol>
      {more > 0 && (
        <div className="report-more dim-note">
          + {more} more {more === 1 ? "section" : "sections"} in the full report
        </div>
      )}
    </div>
  );
}

/** The citations preview — the REAL source refs the report rests on. Each row is
 *  a source an input claim carried (id + title + url); the parser dropped any
 *  with no usable locator. The count is the daemon's real total. */
function CitationsPreview({
  citations,
  total,
}: {
  citations: ReportCitation[];
  total: number;
}) {
  if (citations.length === 0) {
    // Sections exist but no citation survived the preview bound — surface the
    // honest count rather than implying there are none.
    return (
      <div className="report-citations">
        <div className="report-citations-head">
          <span className="report-citations-title">SOURCES</span>
          <span className="report-pill cited">{total} CITED</span>
        </div>
      </div>
    );
  }
  const more = total - citations.length;
  return (
    <div className="report-citations">
      <div className="report-citations-head">
        <span className="report-citations-title">SOURCES</span>
        <span
          className="report-pill cited"
          title="the real source refs the report rests on — never fabricated"
        >
          {total} CITED
        </span>
      </div>
      <ul className="report-citation-list">
        {citations.map((c, i) => (
          <CitationRow key={`${c.id}:${c.url}:${i}`} citation={c} />
        ))}
      </ul>
      {more > 0 && (
        <div className="report-more dim-note">
          + {more} more {more === 1 ? "source" : "sources"} in the full report
        </div>
      )}
    </div>
  );
}

/** One cited source: the run-local id, the title, and the real url — exactly the
 *  locator the input claim carried. Real source refs only — never fabricated. */
function CitationRow({ citation }: { citation: ReportCitation }) {
  return (
    <li className="report-citation">
      <span className="report-citation-id" title="the report's source id">
        [{citation.id}]
      </span>
      <span className="report-citation-text">
        {citation.title.length > 0 && (
          <span className="report-citation-title">{citation.title}</span>
        )}
        {citation.url.length > 0 && (
          <span className="report-citation-url" title="the real source locator">
            {citation.url}
          </span>
        )}
      </span>
    </li>
  );
}
