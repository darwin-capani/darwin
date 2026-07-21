/**
 * Spatial knowledge-graph layout (pure, node-testable — no DOM/React/physics).
 *
 * The KnowledgeGraphPanel already lists the entities/relationships as text with
 * their source provenance. This adds a SPATIAL node-link view: a DETERMINISTIC
 * layout (no force simulation, no randomness, no animation loop) so it renders
 * identically every time, is SSR-safe (the HUD's vitest is node-env, no DOM),
 * and never churns React. Nodes are placed on concentric rings GROUPED BY
 * ENTITY TYPE (each type its own ring band + angular sector), which reads as a
 * clean radial graph and keeps same-type entities visually clustered.
 *
 * HONESTY: this is a VIEW of the same grounded [`KnowledgeGraphResult`] the panel
 * already shows — it invents no node or edge, and an edge whose endpoint id has
 * no entity node is DROPPED (never a phantom node), so the picture can only ever
 * under-draw the data, never over-draw it.
 */

/** A laid-out node: the entity plus its position in the viewBox. */
export interface KgNode {
  id: string;
  name: string;
  type: string;
  /** Whether the entity carried a source citation (drives a "grounded" ring). */
  grounded: boolean;
  x: number;
  y: number;
  /** Which side of the node its text label is drawn, so labels point INWARD and
   *  never overflow the viewBox edge: "left" for a right-half node, "right"
   *  otherwise. */
  labelSide: "left" | "right";
}

/** A laid-out edge: the endpoint positions + the relation label midpoint. */
export interface KgEdge {
  from: string;
  to: string;
  relation: string;
  x1: number;
  y1: number;
  x2: number;
  y2: number;
  /** Label anchor (segment midpoint). */
  mx: number;
  my: number;
}

export interface KgGraphLayout {
  nodes: KgNode[];
  edges: KgEdge[];
  /** Square viewBox side (the layout is centered in `size`×`size`). */
  size: number;
  /** Entity ids that appeared on an edge but had no node — DROPPED, surfaced so
   *  the panel can note the honest under-draw rather than hide it. */
  danglingEndpoints: string[];
}

/** Minimal projections (so layout is testable without the full event types). */
export interface LayoutEntity {
  id: string;
  name: string;
  type: string;
  source: string | null;
}
export interface LayoutRel {
  from: string;
  to: string;
  relation: string;
}

const SIZE = 520;
const CENTER = SIZE / 2;
/** Innermost + outermost ring radii. The per-type band spacing is computed from
 *  the ACTUAL number of types so every type gets its own DISTINCT ring however
 *  many there are (a fixed step would collapse the 4th+ type onto one ring once
 *  it hit the outer margin). OUTER_R leaves room for the node radius; node LABELS
 *  are drawn INWARD (see `labelSide`) so they never run off the viewBox edge. */
const INNER_R = 70;
const OUTER_R = CENTER - 46;

/**
 * Lay out a knowledge graph deterministically. Nodes are grouped by type (types
 * in first-appearance order for stability), each type on its own ring; within a
 * ring, nodes are spread at equal angles (first-appearance order). Positions are
 * a pure function of the input order, so the same graph always lays out the same.
 *
 * Edges connect laid-out node centers; an edge referencing an id with no node is
 * recorded in `danglingEndpoints` and NOT drawn (no phantom node). Deterministic
 * and total: an empty graph yields empty nodes/edges.
 */
export function layoutGraph(
  entities: ReadonlyArray<LayoutEntity>,
  rels: ReadonlyArray<LayoutRel>,
): KgGraphLayout {
  // Group node indices by type, preserving first-appearance order of both the
  // types and the nodes within a type.
  const typeOrder: string[] = [];
  const byType = new Map<string, LayoutEntity[]>();
  for (const e of entities) {
    if (!byType.has(e.type)) {
      byType.set(e.type, []);
      typeOrder.push(e.type);
    }
    byType.get(e.type)!.push(e);
  }

  const nodes: KgNode[] = [];
  const pos = new Map<string, { x: number; y: number }>();
  // Spread the type rings evenly across [INNER_R, OUTER_R] so each type gets a
  // DISTINCT radius no matter how many types there are (a single type sits on
  // the inner ring).
  const bandCount = typeOrder.length;
  const bandStep = bandCount > 1 ? (OUTER_R - INNER_R) / (bandCount - 1) : 0;
  typeOrder.forEach((type, bandIdx) => {
    const group = byType.get(type)!;
    const r = INNER_R + bandIdx * bandStep;
    const n = group.length;
    group.forEach((e, i) => {
      // Equal angular spread; a per-band phase offset so adjacent rings don't
      // line their nodes up radially (avoids overlapping spokes).
      const angle = (2 * Math.PI * i) / Math.max(n, 1) + bandIdx * 0.4;
      const x = CENTER + r * Math.cos(angle);
      const y = CENTER + r * Math.sin(angle);
      const node: KgNode = {
        id: e.id,
        name: e.name,
        type: e.type,
        grounded: e.source !== null && e.source.trim() !== "",
        x,
        y,
        // Draw the label INWARD: a right-half node labels to its LEFT, a left-half
        // node to its RIGHT — so a name never runs off the viewBox edge.
        labelSide: x > CENTER ? "left" : "right",
      };
      nodes.push(node);
      pos.set(e.id, { x, y });
    });
  });

  const edges: KgEdge[] = [];
  const dangling = new Set<string>();
  for (const r of rels) {
    const a = pos.get(r.from);
    const b = pos.get(r.to);
    if (!a || !b) {
      if (!a) dangling.add(r.from);
      if (!b) dangling.add(r.to);
      continue; // no phantom node — drop the edge, record the gap
    }
    if (r.from === r.to) continue; // self-loop: nothing spatial to draw
    edges.push({
      from: r.from,
      to: r.to,
      relation: r.relation,
      x1: a.x,
      y1: a.y,
      x2: b.x,
      y2: b.y,
      mx: (a.x + b.x) / 2,
      my: (a.y + b.y) / 2,
    });
  }

  return { nodes, edges, size: SIZE, danglingEndpoints: [...dangling] };
}

/** Deterministic hue (0..360) per entity type, so the graph colors nodes the
 *  same way every render. The six known kinds get distinct, spread hues; any
 *  other (tolerated) type slug hashes to a stable hue in the same wheel. */
export function kgTypeHue(type: string): number {
  const known: Record<string, number> = {
    project: 190, // cyan (matches the HUD's primary)
    person: 35, // amber
    deadline: 0, // red
    task: 265, // violet
    topic: 140, // green
    thread: 300, // magenta
  };
  if (type in known) return known[type];
  let h = 0;
  for (let i = 0; i < type.length; i++) h = (h * 31 + type.charCodeAt(i)) % 360;
  return h;
}
