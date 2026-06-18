import type {
  KgEntity,
  KgEntityType,
  KgRelationship,
  KnowledgeGraphResult,
} from "../core/events";
import { KG_ENTITY_TYPES, kgEntityTypeLabel } from "../core/events";
import Frame from "./Frame";

/**
 * KNOWLEDGE GRAPH // FROM YOUR DOCUMENTS — the read-only surface for the graph
 * the gated build pass extracts from the user's OWN indexed documents
 * (daemon/src/knowledge_graph.rs) and upserts into the SHARED world model
 * (daemon/src/world_model.rs). It groups the resulting entities BY TYPE
 * (Project/Person/Deadline/Task/Topic/Thread), shows each entity's `source`
 * PROVENANCE citation (file:offset(+char span)), renders the relationships as
 * from-relation-to edges (the "mentions" co-occurrence edge carries a
 * `source file:offset` detail), and prints the HONEST build stats.
 *
 * HONESTY CONTRACT (load-bearing, do not regress):
 *   - EXTRACTED, NEVER INVENTED. Every node/edge is GROUNDED in real document
 *     text the daemon indexed; each carries a `source` citation so the user can
 *     trace it back to the exact file + offset. An entity with no source reads
 *     honestly as "no citation" — never hidden, never faked.
 *   - CONSERVATIVE HEURISTIC. The shipped extractor is a deterministic, on-device
 *     HEURISTIC — it errs toward MISSING rather than inventing and is NOT a
 *     sophisticated NER. The optional richer extractor is runtime-gated. The copy
 *     says so; the panel never implies the graph is complete.
 *   - SHARED + BOUNDED. The graph is the shared `user.world.*` tier only (never an
 *     agent's private namespace) and respects the world model's entity/relation
 *     caps — `skipped_at_cap` is surfaced as the honest "refused past the bound"
 *     proof, never a silent unbounded grow. This rides the local 127.0.0.1
 *     broadcast only.
 *   - SHIPS OFF + REVIEW-ONLY. Double-gated ([docsearch].enabled AND
 *     [docsearch].build_graph, both ship false); the event never arrives until
 *     deliberately enabled. There is NO button here that builds or writes —
 *     building is a SPOKEN intent ("map my documents"); this panel only SHOWS the
 *     last build's stats + the resulting grouped graph.
 *
 * The reducer only ever sets `knowledgeGraph` from a defensively-parsed
 * `knowledge_graph.built` (only real returned nodes/edges, honest stats), so this
 * component can trust the fields it is handed.
 */
export default function KnowledgeGraphPanel({
  graph,
}: {
  graph: KnowledgeGraphResult | null;
}) {
  // Nothing to show until the user has built a graph — render nothing rather than
  // a placeholder, mirroring the other event-fed gated panels (DocSearchPanel,
  // UnifiedSearchPanel). The feature ships OFF, so the event never arrives until
  // it is deliberately enabled + build_graph turned on + "map my documents" said.
  if (graph === null) return null;

  return (
    <div className="kg-panel">
      <Frame title="KNOWLEDGE GRAPH // FROM YOUR DOCUMENTS" tag="PRIVATE · REVIEW ONLY">
        <div className="kg-body">
          <BuildStats graph={graph} />
          <Entities graph={graph} />
          <Relationships rels={graph.relationships} />

          <div className="kg-foot dim-note">
            Extracted from YOUR indexed documents by a conservative, on-device{" "}
            <b>heuristic</b> (it errs toward missing rather than inventing — it is
            NOT a sophisticated NER; an optional richer model extractor is
            runtime-gated). Every entity and relationship is text-grounded and
            tagged with its <b>source file</b> (file:offset), so you can trace any
            node back to where it came from — nothing is fabricated. It writes only
            the shared, BOUNDED world-model tier (it never grows past its cap, and
            never an agent&rsquo;s private namespace), and rides the local
            broadcast only. Ships OFF: enable <code>[docsearch].enabled</code> and{" "}
            <code>[docsearch].build_graph</code>, index a folder, then say{" "}
            <b>&ldquo;map my documents&rdquo;</b> to (re)build the graph.
          </div>
        </div>
      </Frame>
    </div>
  );
}

/** The HONEST build stats: chunks scanned, entities + relationships written, and
 *  — when any — the count refused at the world-model bound (the proof the model
 *  is never grown past its cap). Also names the extractor METHOD that actually
 *  ran, so the panel never implies a richer extractor than the one used. */
function BuildStats({ graph }: { graph: KnowledgeGraphResult }) {
  return (
    <div className="kg-stats">
      <div className="kg-stats-head">
        <span className="kg-stats-title">LAST BUILD</span>
        <span
          className="kg-pill method"
          title="the extractor that actually ran — the conservative deterministic heuristic, or the runtime-gated model when enabled"
        >
          {graph.extractor}
        </span>
      </div>
      <div className="kg-stats-counts">
        <Count label="CHUNKS SCANNED" value={graph.chunksScanned} />
        <Count label="ENTITIES" value={graph.entitiesWritten} />
        <Count label="RELATIONSHIPS" value={graph.relationshipsWritten} />
        {graph.skippedAtCap > 0 && (
          <Count label="SKIPPED (AT CAP)" value={graph.skippedAtCap} warn />
        )}
      </div>
      {graph.skippedAtCap > 0 && (
        <div className="kg-note dim-note">
          {graph.skippedAtCap} node(s)/edge(s) were refused because the world model
          is at its bound — it is never grown past its cap (this is honest, not an
          error).
        </div>
      )}
    </div>
  );
}

/** One labelled count chip. `warn` tints the at-cap count so the bound proof
 *  reads at a glance. */
function Count({
  label,
  value,
  warn = false,
}: {
  label: string;
  value: number;
  warn?: boolean;
}) {
  return (
    <div className={`kg-count ${warn ? "warn" : ""}`}>
      <span className="kg-count-val">{value}</span>
      <span className="kg-count-label">{label}</span>
    </div>
  );
}

/** The entities GROUPED BY TYPE, in the world model's canonical kind order
 *  (Project/Person/Deadline/Task/Topic/Thread), with any unknown future kind
 *  collected into a trailing "OTHER" group so it still surfaces. An empty graph
 *  is the honest "extracted nothing" — shown, never hidden or faked. */
function Entities({ graph }: { graph: KnowledgeGraphResult }) {
  if (graph.entities.length === 0) {
    return (
      <div className="kg-empty dim-note">
        No entities were extracted yet. This is the honest result — no node is
        invented. The conservative heuristic errs toward missing; if you have not
        built the graph, enable file search and <code>[docsearch].build_graph</code>,
        index a folder, then say <b>&ldquo;map my documents&rdquo;</b>.
      </div>
    );
  }

  // Bucket entities by their kind. Known kinds render in the canonical order; any
  // unknown kind is appended (deterministically, by token) under its own group so
  // a future EntityType still surfaces rather than being dropped.
  const byType = new Map<KgEntityType, KgEntity[]>();
  for (const e of graph.entities) {
    const bucket = byType.get(e.type);
    if (bucket) bucket.push(e);
    else byType.set(e.type, [e]);
  }
  const knownOrder = KG_ENTITY_TYPES.filter((t) => byType.has(t));
  const extraTypes = [...byType.keys()]
    .filter((t) => !KG_ENTITY_TYPES.includes(t))
    .sort();
  const orderedTypes = [...knownOrder, ...extraTypes];

  return (
    <div className="kg-groups">
      {orderedTypes.map((type) => {
        const list = byType.get(type) ?? [];
        return (
          <div className="kg-group" key={type}>
            <div className="kg-group-head">
              <span className={`kg-group-title kg-type-${type}`}>
                {kgEntityTypeLabel(type)}
              </span>
              <span className="kg-group-count">{list.length}</span>
            </div>
            <div className="kg-entities">
              {list.map((e) => (
                <EntityRow key={`${e.type}:${e.id}`} entity={e} />
              ))}
            </div>
          </div>
        );
      })}
    </div>
  );
}

/** One entity: its display name + its `source` PROVENANCE citation (file:offset
 *  (+char span)). An entity with no source reads honestly as "no citation" rather
 *  than a fabricated one. */
function EntityRow({ entity }: { entity: KgEntity }) {
  return (
    <div className="kg-entity">
      <span className="kg-entity-name" title={entity.id}>
        {entity.name}
      </span>
      {entity.source !== null && entity.source.length > 0 ? (
        <span
          className="kg-entity-source"
          title="provenance — the file + offset (and char span) this entity was extracted from"
        >
          {entity.source}
        </span>
      ) : (
        <span className="kg-entity-source none" title="no source recorded for this node">
          no citation
        </span>
      )}
    </div>
  );
}

/** The relationships as from-relation-to edges, each with the `source` detail
 *  recorded on the edge (for the "mentions" co-occurrence edge this is the
 *  `source file:offset` that grounds it). An empty set renders nothing (the
 *  entities above already carry the graph). */
function Relationships({ rels }: { rels: KgRelationship[] }) {
  if (rels.length === 0) return null;

  return (
    <div className="kg-rels">
      <div className="kg-rels-head">
        <span className="kg-rels-title">RELATIONSHIPS</span>
        <span className="kg-rels-count">{rels.length}</span>
      </div>
      <div className="kg-rels-list">
        {rels.map((r, i) => (
          <RelRow key={`${r.from}:${r.relation}:${r.to}:${i}`} rel={r} />
        ))}
      </div>
    </div>
  );
}

/** One edge: from — relation → to, with its grounding source detail. */
function RelRow({ rel }: { rel: KgRelationship }) {
  return (
    <div className="kg-rel">
      <div className="kg-rel-edge">
        <span className="kg-rel-from" title={rel.from}>
          {rel.from}
        </span>
        <span className="kg-rel-relation">{rel.relation}</span>
        <span className="kg-rel-arrow" aria-hidden="true">
          →
        </span>
        <span className="kg-rel-to" title={rel.to}>
          {rel.to}
        </span>
      </div>
      {rel.source.length > 0 ? (
        <span
          className="kg-rel-source"
          title="provenance — where this co-occurrence edge was grounded"
        >
          {rel.source}
        </span>
      ) : (
        <span className="kg-rel-source none">no citation</span>
      )}
    </div>
  );
}
