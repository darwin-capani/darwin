import { describe, expect, it } from "vitest";
import {
  layoutGraph,
  kgTypeHue,
  type LayoutEntity,
  type LayoutRel,
} from "../core/kgLayout";

const ents = (...items: Array<[string, string, string, string | null]>): LayoutEntity[] =>
  items.map(([id, name, type, source]) => ({ id, name, type, source }));
const rel = (from: string, relation: string, to: string): LayoutRel => ({ from, relation, to });

describe("layoutGraph", () => {
  it("places every entity as a node, inside the viewBox", () => {
    const g = layoutGraph(
      ents(
        ["subaru", "Subaru", "topic", "docs/car.md:10"],
        ["geico", "Geico", "topic", null],
        ["darwin", "Darwin", "person", "docs/me.md:2"],
      ),
      [],
    );
    expect(g.nodes.map((n) => n.id).sort()).toEqual(["darwin", "geico", "subaru"]);
    for (const n of g.nodes) {
      expect(n.x).toBeGreaterThanOrEqual(0);
      expect(n.x).toBeLessThanOrEqual(g.size);
      expect(n.y).toBeGreaterThanOrEqual(0);
      expect(n.y).toBeLessThanOrEqual(g.size);
    }
  });

  it("marks source-cited nodes grounded, uncited ones not", () => {
    const g = layoutGraph(
      ents(["a", "A", "topic", "docs/x.md:1"], ["b", "B", "topic", null]),
      [],
    );
    expect(g.nodes.find((n) => n.id === "a")!.grounded).toBe(true);
    expect(g.nodes.find((n) => n.id === "b")!.grounded).toBe(false);
  });

  it("draws an edge between two present nodes", () => {
    const g = layoutGraph(
      ents(["subaru", "Subaru", "topic", null], ["geico", "Geico", "topic", null]),
      [rel("subaru", "insured_by", "geico")],
    );
    expect(g.edges).toHaveLength(1);
    const e = g.edges[0];
    expect(e.from).toBe("subaru");
    expect(e.to).toBe("geico");
    // The endpoints match the node positions; the midpoint is between them.
    const a = g.nodes.find((n) => n.id === "subaru")!;
    const b = g.nodes.find((n) => n.id === "geico")!;
    expect([e.x1, e.y1]).toEqual([a.x, a.y]);
    expect([e.x2, e.y2]).toEqual([b.x, b.y]);
    expect(e.mx).toBeCloseTo((a.x + b.x) / 2);
  });

  it("DROPS an edge to a missing entity (no phantom node) and records the gap", () => {
    const g = layoutGraph(ents(["subaru", "Subaru", "topic", null]), [
      rel("subaru", "insured_by", "geico"), // geico has no entity
    ]);
    expect(g.edges).toHaveLength(0);
    expect(g.nodes.map((n) => n.id)).toEqual(["subaru"]);
    expect(g.danglingEndpoints).toEqual(["geico"]);
  });

  it("drops a self-loop (nothing spatial to draw)", () => {
    const g = layoutGraph(ents(["a", "A", "topic", null]), [rel("a", "relates", "a")]);
    expect(g.edges).toHaveLength(0);
    expect(g.danglingEndpoints).toEqual([]);
  });

  it("is deterministic: same input -> same layout", () => {
    const build = () =>
      layoutGraph(
        ents(["a", "A", "topic", null], ["b", "B", "person", null]),
        [rel("a", "knows", "b")],
      );
    expect(build()).toEqual(build());
  });

  it("empty graph -> empty nodes/edges", () => {
    const g = layoutGraph([], []);
    expect(g.nodes).toEqual([]);
    expect(g.edges).toEqual([]);
    expect(g.danglingEndpoints).toEqual([]);
  });

  it("groups by type on separate rings (types at different radii)", () => {
    const g = layoutGraph(
      ents(["a", "A", "project", null], ["b", "B", "person", null]),
      [],
    );
    const size = g.size;
    const rOf = (id: string) => {
      const n = g.nodes.find((x) => x.id === id)!;
      return Math.hypot(n.x - size / 2, n.y - size / 2);
    };
    // Different types -> different ring radii.
    expect(rOf("a")).not.toBeCloseTo(rOf("b"));
  });

  it("gives ALL types a DISTINCT ring even at the full 6-type set (no collapse)", () => {
    // The panel groups by exactly these 6 canonical kinds — every one must land
    // on its own radius (the earlier fixed-band layout collapsed the 4th+ onto
    // one ring).
    const g = layoutGraph(
      ents(
        ["p", "P", "project", null],
        ["pe", "Pe", "person", null],
        ["d", "D", "deadline", null],
        ["t", "T", "task", null],
        ["to", "To", "topic", null],
        ["th", "Th", "thread", null],
      ),
      [],
    );
    const size = g.size;
    const radii = g.nodes.map((n) => Math.round(Math.hypot(n.x - size / 2, n.y - size / 2)));
    expect(new Set(radii).size).toBe(6);
    // All rings stay inside the viewBox.
    for (const r of radii) expect(r).toBeLessThan(size / 2);
  });

  it("draws each label INWARD (right-half node -> left, left-half node -> right)", () => {
    // Many nodes on one ring guarantees both halves are populated.
    const many: LayoutEntity[] = Array.from({ length: 8 }, (_, i) => ({
      id: `n${i}`,
      name: `N${i}`,
      type: "topic",
      source: null,
    }));
    const g = layoutGraph(many, []);
    const size = g.size;
    for (const n of g.nodes) {
      const expected = n.x > size / 2 ? "left" : "right";
      expect(n.labelSide).toBe(expected);
    }
    // Both sides actually occur on a full ring.
    expect(g.nodes.some((n) => n.labelSide === "left")).toBe(true);
    expect(g.nodes.some((n) => n.labelSide === "right")).toBe(true);
  });
});

describe("kgTypeHue", () => {
  it("gives the six known kinds distinct hues", () => {
    const hues = ["project", "person", "deadline", "task", "topic", "thread"].map(kgTypeHue);
    expect(new Set(hues).size).toBe(6);
  });
  it("hashes an unknown type to a stable in-range hue", () => {
    const h = kgTypeHue("organization");
    expect(h).toBe(kgTypeHue("organization")); // stable
    expect(h).toBeGreaterThanOrEqual(0);
    expect(h).toBeLessThan(360);
  });
});
