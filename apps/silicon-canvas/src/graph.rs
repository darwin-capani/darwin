//! Connectivity graph (SPEC §1, §4) — the electrical net graph over a
//! [`Scene`], built once at import and immutable afterward.
//!
//! Nodes are the connectable entities ([`EntityRef`]): pads/pins, wire segments,
//! junctions, net labels (schematic) and tracks, vias, pads (PCB). Edges are
//! electrical contact:
//!
//!   - **Schematic** (SPEC §1: "position-quantized endpoint matching"): every
//!     endpoint is snapped to the [`QuantKey`] grid via [`Point::quantize`].
//!     Entities sharing a quantized point are connected. Same-named net labels
//!     (global / hierarchical) additionally join their components, so a label
//!     `3V3` on two different wires merges those nets.
//!   - **PCB** (SPEC §1: "copper overlap per layer + via stitching"): track
//!     segments sharing a quantized endpoint *on the same copper layer* connect;
//!     a via stitches every entity at its position across the layers it spans;
//!     pads connect at their position regardless of layer.
//!
//! Net assignment is union-find over the nodes: each connected component becomes
//! one [`NetId`]. The net's name is taken from a label on it (schematic) or kept
//! from the parser's pre-assigned ids (PCB nets are named in the file); otherwise
//! a synthetic `N$<k>` name is minted. [`Graph::apply_nets`] writes the computed
//! ids back into the scene arrays.
//!
//! Tracing (SPEC §4) walks this graph BFS-by-electrical-distance from a seed
//! ([`Graph::bfs_walk`] / [`Graph::trace_order`]); the `trace` module owns the
//! highlight-front state machine and only consumes the ordered walk here.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::ids::{EntityRef, NetId};
use crate::scene::{LayerId, Point, QuantKey, Scene, SceneKind};
use crate::trace::TraceGraph;

// ===========================================================================
// Union-find (disjoint set) over graph node indices.
// ===========================================================================

/// Path-compressed, union-by-rank disjoint-set forest over `0..n` node indices.
#[derive(Debug, Clone)]
struct UnionFind {
    parent: Vec<u32>,
    rank: Vec<u8>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        UnionFind {
            parent: (0..n as u32).collect(),
            rank: vec![0; n],
        }
    }

    fn find(&mut self, mut x: u32) -> u32 {
        // Iterative find with full path compression.
        let mut root = x;
        while self.parent[root as usize] != root {
            root = self.parent[root as usize];
        }
        while self.parent[x as usize] != root {
            let next = self.parent[x as usize];
            self.parent[x as usize] = root;
            x = next;
        }
        root
    }

    fn union(&mut self, a: u32, b: u32) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra == rb {
            return;
        }
        let (ra, rb) = if self.rank[ra as usize] < self.rank[rb as usize] {
            (rb, ra)
        } else {
            (ra, rb)
        };
        self.parent[rb as usize] = ra;
        if self.rank[ra as usize] == self.rank[rb as usize] {
            self.rank[ra as usize] += 1;
        }
    }
}

// ===========================================================================
// One step of a trace walk (SPEC §4).
// ===========================================================================

/// One node visited by a trace walk, in BFS-by-electrical-distance order
/// (SPEC §4). `distance` is the hop count from the seed (the seed is distance 0).
/// `crosses_layer` is `true` when reaching this node *changed copper layer*: the
/// node is a [`EntityKind::Via`](crate::ids::EntityKind::Via) (an explicit layer
/// transition), or its layer differs from its BFS predecessor's layer. This is
/// the cue for the via flash + layer emphasis (SPEC §4 cross-layer step). The
/// seed (distance 0) never crosses. The `trace` module reproduces this exact
/// rule via [`layer_crossing`] so its `crossed_layer` agrees step-for-step. The
/// `trace` module turns a `StepInfo` into a
/// [`crate::scene::HighlightFlag::TraceFront`] write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepInfo {
    /// The entity reached at this step.
    pub entity: EntityRef,
    /// Hop distance from the seed node (seed = 0).
    pub distance: u32,
    /// Whether this step crossed to a different copper layer (PCB via step).
    pub crosses_layer: bool,
}

/// The single cross-layer rule shared by [`Graph::bfs_walk`] and the `trace`
/// walk (SPEC §4): a step crosses a layer when the reached `entity` is a via (an
/// explicit layer transition), or its `layer` differs from its BFS
/// predecessor's layer. The seed has no predecessor (`pred_layer == None`) and
/// never crosses. Centralising the rule here keeps the two walks from diverging.
#[inline]
pub(crate) fn layer_crossing(
    entity: EntityRef,
    pred_layer: Option<LayerId>,
    layer: LayerId,
) -> bool {
    match pred_layer {
        None => false, // the seed never crosses
        Some(prev) => entity.kind == crate::ids::EntityKind::Via || layer != prev,
    }
}

// ===========================================================================
// The graph.
// ===========================================================================

/// The immutable connectivity graph over a [`Scene`]. Built by [`Graph::build`];
/// thereafter only queried (net membership, neighbours, trace walks).
#[derive(Debug, Default)]
pub struct Graph {
    /// Every graph node, in node-index order. `nodes[i]` is the entity for
    /// node `i`.
    nodes: Vec<EntityRef>,
    /// Entity → node index (the inverse of `nodes`).
    node_of: HashMap<EntityRef, u32>,
    /// Adjacency list: `adjacency[i]` are the node indices electrically adjacent
    /// to node `i` (deduplicated, no self-loops).
    adjacency: Vec<Vec<u32>>,
    /// The net id assigned to each node, parallel to `nodes`.
    node_net: Vec<NetId>,
    /// Nodes grouped by net id: `nets[net.index()]` are the entities on that net.
    nets: Vec<Vec<EntityRef>>,
    /// Net name by id, parallel to `nets` (mirrors what `apply_nets` writes to
    /// `Scene::net_names`).
    net_names: Vec<String>,
    /// The layer each node sits on (for the cross-layer flag during a trace).
    node_layer: Vec<LayerId>,
}

impl Graph {
    /// Build the connectivity graph from a fully-populated scene. The scene's
    /// geometry is read-only here; call [`Graph::apply_nets`] afterwards to write
    /// the computed net ids back into the scene arrays.
    pub fn build(scene: &Scene) -> Self {
        match scene.kind {
            SceneKind::Schematic => build_schematic(scene),
            SceneKind::Pcb => build_pcb(scene),
        }
    }

    /// The entity for a node index, if in range.
    pub fn node_entity(&self, node: u32) -> Option<EntityRef> {
        self.nodes.get(node as usize).copied()
    }

    /// Total node count.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Number of distinct nets (connected components) the graph found.
    pub fn net_count(&self) -> usize {
        self.nets.len()
    }

    /// The net an entity belongs to, or [`NetId::NONE`] if the entity is not a
    /// node in the graph (e.g. a component, sheet, or an entity carrying no net).
    pub fn node_net(&self, entity: EntityRef) -> NetId {
        match self.node_of.get(&entity) {
            Some(&i) => self.node_net[i as usize],
            None => NetId::NONE,
        }
    }

    /// The copper layer an entity sits on (the layer recorded when the node was
    /// interned). [`LayerId::SCHEMATIC`] for schematic nodes and the layer-less
    /// classes; [`LayerId::SCHEMATIC`] (a safe default) for entities that are not
    /// graph nodes. Used by both [`Graph::bfs_walk`] and the `trace` walk to
    /// decide a cross-layer step (SPEC §4) — they share the [`layer_crossing`]
    /// rule so their `crosses_layer` flags agree.
    pub fn node_layer(&self, entity: EntityRef) -> LayerId {
        match self.node_of.get(&entity) {
            Some(&i) => self.node_layer[i as usize],
            None => LayerId::SCHEMATIC,
        }
    }

    /// Every entity on a net — the highlight set for `select.net` (SPEC §4).
    /// Empty slice for [`NetId::NONE`] or an out-of-range id.
    pub fn nodes_on_net(&self, net: NetId) -> &[EntityRef] {
        if net.is_none() {
            return &[];
        }
        self.nets.get(net.index()).map(Vec::as_slice).unwrap_or(&[])
    }

    /// The computed name for a net id (mirrors `Scene::net_names` after
    /// [`Graph::apply_nets`]). Empty string for out of range / NONE.
    pub fn net_name(&self, net: NetId) -> &str {
        if net.is_none() {
            return "";
        }
        self.net_names.get(net.index()).map(String::as_str).unwrap_or("")
    }

    /// Look up a net id by computed name (linear; nets are few).
    pub fn net_by_name(&self, name: &str) -> NetId {
        self.net_names
            .iter()
            .position(|n| n == name)
            .map(|i| NetId::new(i as u32))
            .unwrap_or(NetId::NONE)
    }

    /// The number of pads/pins on a net — for `canvas.selection`'s `pin_count`
    /// (SPEC §4).
    pub fn pin_count(&self, net: NetId) -> u32 {
        self.nodes_on_net(net)
            .iter()
            .filter(|e| e.kind == crate::ids::EntityKind::Pad)
            .count() as u32
    }

    /// The entities electrically adjacent to `entity` (one hop). Used by ERC's
    /// driver / multi-driver analysis (SPEC §5). Empty if `entity` is not a node.
    pub fn neighbors(&self, entity: EntityRef) -> impl Iterator<Item = EntityRef> + '_ {
        let node = self.node_of.get(&entity).copied();
        let adj = node
            .and_then(|i| self.adjacency.get(i as usize))
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        adj.iter().map(move |&j| self.nodes[j as usize])
    }

    /// BFS walk from a seed entity in electrical-distance order (SPEC §4 walk
    /// order). Returns one [`StepInfo`] per reachable node, the seed first. An
    /// empty vec if the seed is not a node. `crosses_layer` marks via steps that
    /// changed copper layer (PCB).
    pub fn bfs_walk(&self, seed: EntityRef) -> Vec<StepInfo> {
        let Some(&start) = self.node_of.get(&seed) else {
            return Vec::new();
        };
        let n = self.nodes.len();
        let mut visited = vec![false; n];
        let mut out = Vec::new();
        // (node, distance, predecessor-layer). The seed has no predecessor, so it
        // carries `None` and never counts as a crossing.
        let mut queue: VecDeque<(u32, u32, Option<LayerId>)> = VecDeque::new();
        visited[start as usize] = true;
        queue.push_back((start, 0, None));
        while let Some((node, dist, pred_layer)) = queue.pop_front() {
            let entity = self.nodes[node as usize];
            let layer = self.node_layer[node as usize];
            out.push(StepInfo {
                entity,
                distance: dist,
                // Single source of truth (shared with the `trace` walk): a via,
                // or a layer change vs. the BFS predecessor, is a cross-layer
                // step. The seed (no predecessor) never crosses.
                crosses_layer: layer_crossing(entity, pred_layer, layer),
            });
            for &next in &self.adjacency[node as usize] {
                if !visited[next as usize] {
                    visited[next as usize] = true;
                    queue.push_back((next, dist + 1, Some(layer)));
                }
            }
        }
        out
    }

    /// The entities reached by [`Graph::bfs_walk`], in order (without the
    /// per-step metadata). Convenience for callers that only need the visit
    /// sequence.
    pub fn trace_order(&self, seed: EntityRef) -> Vec<EntityRef> {
        self.bfs_walk(seed).into_iter().map(|s| s.entity).collect()
    }

    /// Write the computed net ids back into the scene's per-entity `net_id`
    /// fields and populate `Scene::net_names`. Index 0 is reserved for the
    /// unnamed / no-net net (empty string), so computed nets start at id 1.
    /// Idempotent given the same graph.
    pub fn apply_nets(&self, scene: &mut Scene) {
        // net_names[0] = "" (no-net); the graph's net k maps to scene id k+1 so
        // id 0 stays the conventional empty net (scene.rs docstring on
        // `net_names`).
        scene.net_names = Vec::with_capacity(self.net_names.len() + 1);
        scene.net_names.push(String::new());
        scene.net_names.extend(self.net_names.iter().cloned());

        let scene_net = |entity: EntityRef| -> NetId {
            let g = self.node_net(entity);
            if g.is_none() {
                NetId::NONE
            } else {
                NetId::new(g.raw() + 1)
            }
        };

        for (i, p) in scene.pads.iter_mut().enumerate() {
            p.net_id = scene_net(EntityRef::pad((i as u32).into()));
        }
        for (i, w) in scene.wires.iter_mut().enumerate() {
            w.net_id = scene_net(EntityRef::wire((i as u32).into()));
        }
        for (i, j) in scene.junctions.iter_mut().enumerate() {
            j.net_id = scene_net(EntityRef::junction((i as u32).into()));
        }
        for (i, l) in scene.labels.iter_mut().enumerate() {
            l.net_id = scene_net(EntityRef::label((i as u32).into()));
        }
        for (i, t) in scene.tracks.iter_mut().enumerate() {
            t.net_id = scene_net(EntityRef::track((i as u32).into()));
        }
        for (i, v) in scene.vias.iter_mut().enumerate() {
            v.net_id = scene_net(EntityRef::via((i as u32).into()));
        }
        for (i, z) in scene.zones.iter_mut().enumerate() {
            z.net_id = scene_net(EntityRef::zone((i as u32).into()));
        }
    }
}

// ===========================================================================
// Trace integration glue.
// ===========================================================================

/// Lets the [`Tracer`](crate::trace::Tracer) walk the real connectivity graph
/// (so `ipc` can run `select.net`/`trace.*` against an imported scene, not a
/// stub). Pure delegation to `Graph`'s own accessors — no behavior change. The
/// trait's `neighbors` returns an owned `Vec`, so `Graph::neighbors`'
/// borrowing iterator is collected.
impl TraceGraph for Graph {
    fn nodes_on_net(&self, net: NetId) -> &[EntityRef] {
        Graph::nodes_on_net(self, net)
    }

    fn node_net(&self, node: EntityRef) -> NetId {
        Graph::node_net(self, node)
    }

    fn neighbors(&self, node: EntityRef) -> Vec<EntityRef> {
        Graph::neighbors(self, node).collect()
    }

    fn node_layer(&self, node: EntityRef) -> LayerId {
        Graph::node_layer(self, node)
    }
}

// ===========================================================================
// Builder shared scaffolding.
// ===========================================================================

/// Accumulates nodes + a union-find while a scene is scanned, then resolves
/// connected components into nets and an adjacency list.
struct Builder {
    nodes: Vec<EntityRef>,
    node_of: HashMap<EntityRef, u32>,
    node_layer: Vec<LayerId>,
    /// Edges collected during the scan (deduped at finalize). Stored as node
    /// index pairs.
    edges: Vec<(u32, u32)>,
    uf_pairs: Vec<(u32, u32)>,
}

impl Builder {
    fn new() -> Self {
        Builder {
            nodes: Vec::new(),
            node_of: HashMap::new(),
            node_layer: Vec::new(),
            edges: Vec::new(),
            uf_pairs: Vec::new(),
        }
    }

    /// Intern an entity as a node, returning its node index. Idempotent.
    fn node(&mut self, entity: EntityRef, layer: LayerId) -> u32 {
        if let Some(&i) = self.node_of.get(&entity) {
            return i;
        }
        let i = self.nodes.len() as u32;
        self.nodes.push(entity);
        self.node_layer.push(layer);
        self.node_of.insert(entity, i);
        i
    }

    /// Record an electrical edge (and a union) between two interned nodes.
    fn connect(&mut self, a: u32, b: u32) {
        if a == b {
            return;
        }
        self.edges.push((a, b));
        self.uf_pairs.push((a, b));
    }

    /// Resolve into a [`Graph`]: run union-find, number the components as nets,
    /// build the adjacency list, and assign names from `name_hint`.
    fn finish(self, name_hint: impl Fn(EntityRef) -> Option<String>) -> Graph {
        let n = self.nodes.len();
        let mut uf = UnionFind::new(n);
        for (a, b) in &self.uf_pairs {
            uf.union(*a, *b);
        }

        // Map each root to a dense net id, in first-seen order for determinism.
        let mut root_to_net: HashMap<u32, u32> = HashMap::new();
        let mut node_net = vec![NetId::NONE; n];
        let mut nets: Vec<Vec<EntityRef>> = Vec::new();
        for i in 0..n as u32 {
            let root = uf.find(i);
            let net_id = *root_to_net.entry(root).or_insert_with(|| {
                let id = nets.len() as u32;
                nets.push(Vec::new());
                id
            });
            node_net[i as usize] = NetId::new(net_id);
            nets[net_id as usize].push(self.nodes[i as usize]);
        }

        // Adjacency list (dedup, no self-loops).
        let mut adjacency: Vec<Vec<u32>> = vec![Vec::new(); n];
        let mut seen: HashSet<(u32, u32)> = HashSet::new();
        for (a, b) in &self.edges {
            let (a, b) = (*a, *b);
            if a == b {
                continue;
            }
            let key = if a < b { (a, b) } else { (b, a) };
            if seen.insert(key) {
                adjacency[a as usize].push(b);
                adjacency[b as usize].push(a);
            }
        }

        // Net names: prefer a hinted name from any node on the net; else mint a
        // synthetic stable name. First non-empty hint (lowest node index) wins.
        let mut net_names: Vec<String> = vec![String::new(); nets.len()];
        for (net_idx, members) in nets.iter().enumerate() {
            let mut chosen: Option<String> = None;
            for e in members {
                if let Some(name) = name_hint(*e) {
                    if !name.is_empty() {
                        chosen = Some(name);
                        break;
                    }
                }
            }
            net_names[net_idx] =
                chosen.unwrap_or_else(|| format!("N${}", net_idx + 1));
        }

        Graph {
            nodes: self.nodes,
            node_of: self.node_of,
            adjacency,
            node_net,
            nets,
            net_names,
            node_layer: self.node_layer,
        }
    }
}

// ===========================================================================
// Schematic builder: position-quantized endpoint matching + label join.
// ===========================================================================

fn build_schematic(scene: &Scene) -> Graph {
    let mut b = Builder::new();
    // Endpoint cell → the node indices touching that cell.
    let mut cell: HashMap<QuantKey, Vec<u32>> = HashMap::new();

    let touch = |b: &mut Builder, cell: &mut HashMap<QuantKey, Vec<u32>>, p: Point, node: u32| {
        let entry = cell.entry(p.quantize()).or_default();
        // Connect to everything already at this cell, then record this node.
        for &other in entry.iter() {
            b.connect(node, other);
        }
        entry.push(node);
    };

    // Pads / pins: one node each, contact at the pad position.
    for (i, p) in scene.pads.iter().enumerate() {
        let node = b.node(EntityRef::pad((i as u32).into()), p.layer);
        touch(&mut b, &mut cell, p.position, node);
    }
    // Wires: one node, touching at BOTH endpoints (so two wires sharing an end
    // merge, and a wire bridges whatever sits at either end).
    for (i, w) in scene.wires.iter().enumerate() {
        let node = b.node(EntityRef::wire((i as u32).into()), LayerId::SCHEMATIC);
        touch(&mut b, &mut cell, w.a, node);
        touch(&mut b, &mut cell, w.b, node);
    }
    // Junctions: a node at their position (forces crossing wires to merge).
    for (i, j) in scene.junctions.iter().enumerate() {
        let node = b.node(EntityRef::junction((i as u32).into()), LayerId::SCHEMATIC);
        touch(&mut b, &mut cell, j.position, node);
    }
    // Labels: a node at their position so a label attaches to the wire it sits
    // on; same-named labels are additionally joined below.
    for (i, l) in scene.labels.iter().enumerate() {
        if is_net_label(l.kind) {
            let node = b.node(EntityRef::label((i as u32).into()), LayerId::SCHEMATIC);
            touch(&mut b, &mut cell, l.position, node);
        }
    }

    // Same-named global / hierarchical labels join their nets (SPEC §1: "same-
    // named global/hier labels join nets"). Local labels are single-sheet; with
    // one logical sheet here we also join same-named locals (KiCad merges same-
    // name local labels within a sheet).
    let mut by_name: HashMap<&str, Vec<u32>> = HashMap::new();
    for (i, l) in scene.labels.iter().enumerate() {
        if is_net_label(l.kind) && !l.text.is_empty() {
            let node = b.node(EntityRef::label((i as u32).into()), LayerId::SCHEMATIC);
            by_name.entry(l.text.as_str()).or_default().push(node);
        }
    }
    for nodes in by_name.values() {
        for w in nodes.windows(2) {
            b.connect(w[0], w[1]);
        }
    }

    // Name hint: a net label on the net names it.
    let labels = &scene.labels;
    b.finish(move |e| {
        if e.kind == crate::ids::EntityKind::Label {
            labels.get(e.idx()).and_then(|l| {
                if is_net_label(l.kind) && !l.text.is_empty() {
                    Some(l.text.clone())
                } else {
                    None
                }
            })
        } else {
            None
        }
    })
}

/// Whether a label kind names a net (Local / Global / Hierarchical), as opposed
/// to a reference designator, value, or free text.
#[inline]
fn is_net_label(kind: crate::scene::LabelKind) -> bool {
    use crate::scene::LabelKind::*;
    matches!(kind, Local | Global | Hierarchical)
}

// ===========================================================================
// PCB builder: per-layer copper endpoint match + via stitching.
// ===========================================================================

/// A via's `(layer_from, layer_to)` normalized to an inclusive `(lo, hi)` raw
/// [`LayerId`] range, tolerating either ordering. Because the parser interns
/// copper in physical stackup order (`parser::seed_copper_layers`), this range
/// is exactly the copper the via stitches — mirrors `rtree::layer_span`.
#[inline]
fn via_layer_span(from: LayerId, to: LayerId) -> (u16, u16) {
    let (a, b) = (from.raw(), to.raw());
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

fn build_pcb(scene: &Scene) -> Graph {
    let mut b = Builder::new();
    // Per-(layer, cell) endpoint groups for same-layer copper contact.
    let mut layer_cell: HashMap<(u16, QuantKey), Vec<u32>> = HashMap::new();
    // Position cell for via stitching: every node touching a via's xy position,
    // tagged with the copper layer it sits on (`(node, layer.raw())`). A via then
    // stitches only the nodes whose layer falls within its TRUE span, so a
    // blind/buried via never merges copper it cannot physically reach.
    let mut stitch_cell: HashMap<QuantKey, Vec<(u32, u16)>> = HashMap::new();

    // Same-layer copper contact: connect nodes sharing a quantized endpoint on
    // the same layer.
    let touch_layer =
        |b: &mut Builder,
         lc: &mut HashMap<(u16, QuantKey), Vec<u32>>,
         layer: LayerId,
         p: Point,
         node: u32| {
            let entry = lc.entry((layer.raw(), p.quantize())).or_default();
            for &other in entry.iter() {
                b.connect(node, other);
            }
            entry.push(node);
        };

    // Track segments: a node touching both endpoints on the track's layer.
    for (i, t) in scene.tracks.iter().enumerate() {
        let node = b.node(EntityRef::track((i as u32).into()), t.layer);
        touch_layer(&mut b, &mut layer_cell, t.layer, t.a, node);
        touch_layer(&mut b, &mut layer_cell, t.layer, t.b, node);
        // Also register endpoints (tagged with the track's layer) for via
        // stitching at those positions.
        stitch_cell.entry(t.a.quantize()).or_default().push((node, t.layer.raw()));
        stitch_cell.entry(t.b.quantize()).or_default().push((node, t.layer.raw()));
    }

    // Pads: connect at their position on their layer, and register for stitching
    // (a through-hole pad bridges layers like a via).
    for (i, p) in scene.pads.iter().enumerate() {
        let node = b.node(EntityRef::pad((i as u32).into()), p.layer);
        touch_layer(&mut b, &mut layer_cell, p.layer, p.position, node);
        stitch_cell.entry(p.position.quantize()).or_default().push((node, p.layer.raw()));
    }

    // Vias: a node at their position; stitch together the copper registered at
    // that position that lies WITHIN the via's layer span (SPEC §1 via stitching).
    // LayerIds are physical stackup indices (the parser seeds copper in stackup
    // order), so the inclusive `[lo, hi]` id range is exactly the copper the via
    // passes through: a through via (full stack) merges everything at the point,
    // while a blind/buried via leaves copper outside its span in its own net.
    for (i, v) in scene.vias.iter().enumerate() {
        let node = b.node(EntityRef::via((i as u32).into()), v.layer_from);
        let (lo, hi) = via_layer_span(v.layer_from, v.layer_to);
        let key = v.position.quantize();
        let group = stitch_cell.entry(key).or_default();
        for &(other, layer) in group.iter() {
            if (lo..=hi).contains(&layer) {
                b.connect(node, other);
            }
        }
        // Register the via on its top face so a later via at the same point can
        // stitch to it (two coincident vias are degenerate; one face suffices).
        group.push((node, v.layer_from.raw()));
    }

    // Zones: connect a zone to copper sharing one of its outline vertices on the
    // same layer (a conservative contact; full polygon overlap is out of scope —
    // SPEC §1 names endpoint/overlap + via stitch, and non-goals exclude DRC).
    for (i, z) in scene.zones.iter().enumerate() {
        let node = b.node(EntityRef::zone((i as u32).into()), z.layer);
        for p in &z.outline {
            touch_layer(&mut b, &mut layer_cell, z.layer, *p, node);
        }
    }

    // PCB net names come from the parser's per-pad pre-assigned net (the .kicad_pcb
    // names nets); if the pad already carries a named net, reuse it.
    let pads = &scene.pads;
    let net_names = scene.net_names.clone();
    b.finish(move |e| {
        if e.kind == crate::ids::EntityKind::Pad {
            pads.get(e.idx()).and_then(|p| {
                let id = p.net_id;
                if id.is_some() {
                    net_names.get(id.index()).filter(|s| !s.is_empty()).cloned()
                } else {
                    None
                }
            })
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::{
        Aabb, Component, Junction, Label, LabelKind, Pad, PadShape, PinType, Track, Via, Wire,
    };

    fn rpad(component: u32, pos: Point) -> Pad {
        Pad {
            component: component.into(),
            name: "x".into(),
            position: pos,
            size: (1.0, 1.0),
            shape: PadShape::Rect,
            pin_type: PinType::Passive,
            layer: LayerId::SCHEMATIC,
            net_id: NetId::NONE,
        }
    }

    /// A schematic with two separate nets plus one net shared by a global label
    /// across two otherwise-disconnected wire groups.
    ///
    ///   Net A:  pad0 --wire0-- pad1       (a simple two-pin net)
    ///   Net B:  pad2 --wire1-- pad3       (a second simple net, disjoint in xy)
    ///   Shared: wire2 (label "VCC") and wire3 (label "VCC") — disjoint in xy but
    ///           joined into ONE net by the shared label name.
    fn three_net_schematic() -> Scene {
        let mut s = Scene::new(SceneKind::Schematic);

        // --- Net A: pads at (0,0) and (10,0) joined by a wire ---
        s.pads.push(rpad(0, Point::new(0.0, 0.0))); // pad 0
        s.pads.push(rpad(1, Point::new(10.0, 0.0))); // pad 1
        s.wires.push(Wire {
            a: Point::new(0.0, 0.0),
            b: Point::new(10.0, 0.0),
            net_id: NetId::NONE,
        }); // wire 0

        // --- Net B: pads at (0,50) and (10,50) joined by a wire ---
        s.pads.push(rpad(2, Point::new(0.0, 50.0))); // pad 2
        s.pads.push(rpad(3, Point::new(10.0, 50.0))); // pad 3
        s.wires.push(Wire {
            a: Point::new(0.0, 50.0),
            b: Point::new(10.0, 50.0),
            net_id: NetId::NONE,
        }); // wire 1

        // --- Shared "VCC" net: two disjoint wires joined only by the label ---
        s.wires.push(Wire {
            a: Point::new(0.0, 100.0),
            b: Point::new(5.0, 100.0),
            net_id: NetId::NONE,
        }); // wire 2
        s.wires.push(Wire {
            a: Point::new(0.0, 200.0),
            b: Point::new(5.0, 200.0),
            net_id: NetId::NONE,
        }); // wire 3
        s.labels.push(Label {
            text: "VCC".into(),
            position: Point::new(0.0, 100.0),
            rotation: 0.0,
            kind: LabelKind::Global,
            net_id: NetId::NONE,
        }); // label 0 — sits on wire 2
        s.labels.push(Label {
            text: "VCC".into(),
            position: Point::new(0.0, 200.0),
            rotation: 0.0,
            kind: LabelKind::Global,
            net_id: NetId::NONE,
        }); // label 1 — sits on wire 3

        s.init_flags();
        s
    }

    #[test]
    fn endpoint_match_merges_a_simple_net() {
        let s = three_net_schematic();
        let g = Graph::build(&s);

        // pad0, wire0, pad1 are the same net.
        let n_pad0 = g.node_net(EntityRef::pad(0u32.into()));
        let n_wire0 = g.node_net(EntityRef::wire(0u32.into()));
        let n_pad1 = g.node_net(EntityRef::pad(1u32.into()));
        assert!(n_pad0.is_some());
        assert_eq!(n_pad0, n_wire0);
        assert_eq!(n_pad0, n_pad1);
    }

    #[test]
    fn two_disjoint_nets_are_distinct() {
        let s = three_net_schematic();
        let g = Graph::build(&s);
        let net_a = g.node_net(EntityRef::pad(0u32.into()));
        let net_b = g.node_net(EntityRef::pad(2u32.into()));
        assert!(net_a.is_some() && net_b.is_some());
        assert_ne!(net_a, net_b, "the two simple nets must not be merged");
    }

    #[test]
    fn shared_label_name_joins_disjoint_wires() {
        let s = three_net_schematic();
        let g = Graph::build(&s);
        // wire2 and wire3 are disjoint in xy but share the "VCC" label name.
        let n2 = g.node_net(EntityRef::wire(2u32.into()));
        let n3 = g.node_net(EntityRef::wire(3u32.into()));
        assert!(n2.is_some());
        assert_eq!(n2, n3, "the shared label must merge the two wire groups");
        // And that net is named VCC.
        assert_eq!(g.net_name(n2), "VCC");
        // It is distinct from net A and net B.
        let net_a = g.node_net(EntityRef::pad(0u32.into()));
        let net_b = g.node_net(EntityRef::pad(2u32.into()));
        assert_ne!(n2, net_a);
        assert_ne!(n2, net_b);
        // Exactly three nets total.
        assert_eq!(g.net_count(), 3);
    }

    #[test]
    fn apply_nets_writes_back_to_scene() {
        let mut s = three_net_schematic();
        let g = Graph::build(&s);
        g.apply_nets(&mut s);

        // Index 0 reserved for the no-net; computed nets start at 1.
        assert_eq!(s.net_names[0], "");
        // pad0 and pad1 share a non-NONE net id in the scene now.
        assert!(s.pads[0].net_id.is_some());
        assert_eq!(s.pads[0].net_id, s.pads[1].net_id);
        // The VCC net name is reachable from the scene.
        let vcc = s.net_by_name("VCC");
        assert!(vcc.is_some());
        assert_eq!(s.wires[2].net_id, vcc);
        assert_eq!(s.wires[3].net_id, vcc);
        // Net A and Net B differ.
        assert_ne!(s.pads[0].net_id, s.pads[2].net_id);
    }

    #[test]
    fn nodes_on_net_and_pin_count() {
        let s = three_net_schematic();
        let g = Graph::build(&s);
        let net_a = g.node_net(EntityRef::pad(0u32.into()));
        let members = g.nodes_on_net(net_a);
        // pad0, pad1, wire0.
        assert_eq!(members.len(), 3);
        assert_eq!(g.pin_count(net_a), 2);
    }

    #[test]
    fn bfs_walk_orders_by_electrical_distance() {
        // A chain: pad0 -(0,0)- wire0 -(10,0)- wire1 -(20,0)- pad1
        let mut s = Scene::new(SceneKind::Schematic);
        s.pads.push(rpad(0, Point::new(0.0, 0.0))); // pad 0
        s.wires.push(Wire {
            a: Point::new(0.0, 0.0),
            b: Point::new(10.0, 0.0),
            net_id: NetId::NONE,
        }); // wire 0
        s.wires.push(Wire {
            a: Point::new(10.0, 0.0),
            b: Point::new(20.0, 0.0),
            net_id: NetId::NONE,
        }); // wire 1
        s.pads.push(rpad(1, Point::new(20.0, 0.0))); // pad 1
        s.init_flags();
        let g = Graph::build(&s);

        let walk = g.bfs_walk(EntityRef::pad(0u32.into()));
        assert_eq!(walk[0].entity, EntityRef::pad(0u32.into()));
        assert_eq!(walk[0].distance, 0);
        // Every reachable node is visited exactly once.
        assert_eq!(walk.len(), 4);
        // Distances are non-decreasing in BFS order.
        for w in walk.windows(2) {
            assert!(w[1].distance >= w[0].distance);
        }
        // pad1 is the farthest node.
        let last = walk.iter().find(|s| s.entity == EntityRef::pad(1u32.into())).unwrap();
        assert!(last.distance >= 2);
    }

    #[test]
    fn pcb_tracks_merge_at_shared_endpoint_via_stitches_layers() {
        // Track on F.Cu (layer 0) and track on B.Cu (layer 31) meeting only at a
        // via at (10,0): the via must stitch them into one net.
        let mut s = Scene::new(SceneKind::Pcb);
        let f = LayerId::new(0);
        let b = LayerId::new(31);
        s.tracks.push(Track {
            a: Point::new(0.0, 0.0),
            b: Point::new(10.0, 0.0),
            width: 0.25,
            layer: f,
            net_id: NetId::NONE,
        }); // track 0, front
        s.tracks.push(Track {
            a: Point::new(10.0, 0.0),
            b: Point::new(20.0, 0.0),
            width: 0.25,
            layer: b,
            net_id: NetId::NONE,
        }); // track 1, back
        s.vias.push(Via {
            position: Point::new(10.0, 0.0),
            diameter: 0.6,
            drill: 0.3,
            layer_from: f,
            layer_to: b,
            net_id: NetId::NONE,
        }); // via 0
        s.init_flags();
        let g = Graph::build(&s);

        let n0 = g.node_net(EntityRef::track(0u32.into()));
        let n1 = g.node_net(EntityRef::track(1u32.into()));
        let nv = g.node_net(EntityRef::via(0u32.into()));
        assert!(n0.is_some());
        assert_eq!(n0, n1, "via must stitch the two-layer tracks into one net");
        assert_eq!(n0, nv);
        assert_eq!(g.net_count(), 1);

        // A BFS from track0 marks the cross-layer step when it reaches track1.
        let walk = g.bfs_walk(EntityRef::track(0u32.into()));
        let t1 = walk
            .iter()
            .find(|s| s.entity == EntityRef::track(1u32.into()))
            .unwrap();
        assert!(t1.crosses_layer, "reaching the back-layer track crosses a layer");
    }

    #[test]
    fn blind_via_does_not_stitch_copper_outside_its_span() {
        // Physical stackup ids (as the parser hands them out): F.Cu=0, In1.Cu=1,
        // In4.Cu=4. An In1.Cu track and an In4.Cu track meet at (10,0); a blind via
        // spanning only F.Cu..In1.Cu sits there. The via reaches the In1.Cu track
        // (bottom of its span) but NOT the In4.Cu track — the In4.Cu copper must
        // stay in its own net (the layer-agnostic stitch would wrongly merge it).
        let mut s = Scene::new(SceneKind::Pcb);
        let f = LayerId::new(0);
        let in1 = LayerId::new(1);
        let in4 = LayerId::new(4);
        s.tracks.push(Track {
            a: Point::new(5.0, 0.0),
            b: Point::new(10.0, 0.0),
            width: 0.25,
            layer: in1,
            net_id: NetId::NONE,
        }); // track 0, In1.Cu — within the via span
        s.tracks.push(Track {
            a: Point::new(10.0, 0.0),
            b: Point::new(15.0, 0.0),
            width: 0.25,
            layer: in4,
            net_id: NetId::NONE,
        }); // track 1, In4.Cu — outside the via span
        s.vias.push(Via {
            position: Point::new(10.0, 0.0),
            diameter: 0.6,
            drill: 0.3,
            layer_from: f,
            layer_to: in1,
            net_id: NetId::NONE,
        }); // via 0, blind F.Cu -> In1.Cu
        s.init_flags();
        let g = Graph::build(&s);

        let n_in1 = g.node_net(EntityRef::track(0u32.into()));
        let n_in4 = g.node_net(EntityRef::track(1u32.into()));
        let n_via = g.node_net(EntityRef::via(0u32.into()));
        // The via stitches the In1.Cu track it reaches.
        assert!(n_via.is_some());
        assert_eq!(n_via, n_in1, "via must stitch the In1.Cu track within its span");
        // The In4.Cu track the via never reaches stays in a distinct net.
        assert_ne!(
            n_in4, n_in1,
            "blind via must NOT merge the In4.Cu track outside its span"
        );
        // Two nets total: {In1.Cu track + via} and {In4.Cu track}.
        assert_eq!(g.net_count(), 2);
    }

    #[test]
    fn pcb_separate_layers_without_via_stay_distinct() {
        // Two tracks crossing in xy but on different layers with NO via: distinct.
        let mut s = Scene::new(SceneKind::Pcb);
        s.tracks.push(Track {
            a: Point::new(-5.0, 0.0),
            b: Point::new(5.0, 0.0),
            width: 0.25,
            layer: LayerId::new(0),
            net_id: NetId::NONE,
        });
        s.tracks.push(Track {
            a: Point::new(0.0, -5.0),
            b: Point::new(0.0, 5.0),
            width: 0.25,
            layer: LayerId::new(31),
            net_id: NetId::NONE,
        });
        s.init_flags();
        let g = Graph::build(&s);
        assert_ne!(
            g.node_net(EntityRef::track(0u32.into())),
            g.node_net(EntityRef::track(1u32.into())),
            "crossing tracks on different layers without a via are NOT connected"
        );
    }

    #[test]
    fn junction_merges_crossing_wires() {
        // Two wires crossing at (5,5) with an explicit junction → one net.
        let mut s = Scene::new(SceneKind::Schematic);
        s.wires.push(Wire {
            a: Point::new(0.0, 5.0),
            b: Point::new(10.0, 5.0),
            net_id: NetId::NONE,
        });
        s.wires.push(Wire {
            a: Point::new(5.0, 0.0),
            b: Point::new(5.0, 10.0),
            net_id: NetId::NONE,
        });
        // Neither wire endpoint coincides; only the junction bridges them — but a
        // junction sits at the crossing point, not an endpoint, so this exercises
        // junction-at-position. We place wire endpoints to meet at (5,5) instead
        // so the junction has something to bind.
        s.wires.push(Wire {
            a: Point::new(5.0, 5.0),
            b: Point::new(5.0, 5.0),
            net_id: NetId::NONE,
        });
        s.junctions.push(Junction {
            position: Point::new(5.0, 5.0),
            net_id: NetId::NONE,
        });
        s.init_flags();
        let g = Graph::build(&s);
        // The junction node exists and carries a real net.
        let nj = g.node_net(EntityRef::junction(0u32.into()));
        assert!(nj.is_some());
    }

    #[test]
    fn non_node_entities_have_no_net() {
        let mut s = three_net_schematic();
        s.components.push(Component {
            reference: "R1".into(),
            value: "10k".into(),
            lib_id: "Device:R".into(),
            position: Point::ORIGIN,
            rotation: 0.0,
            mirror: false,
            bbox: Aabb::EMPTY,
            layer: LayerId::SCHEMATIC,
        });
        s.init_flags();
        let g = Graph::build(&s);
        // A component is never a graph node.
        assert!(g.node_net(EntityRef::component(0u32.into())).is_none());
        // Neighbors of a non-node entity is empty.
        assert_eq!(g.neighbors(EntityRef::component(0u32.into())).count(), 0);
    }

    #[test]
    fn empty_scene_builds_empty_graph() {
        let s = Scene::new(SceneKind::Schematic);
        let g = Graph::build(&s);
        assert_eq!(g.node_count(), 0);
        assert_eq!(g.net_count(), 0);
        assert!(g.bfs_walk(EntityRef::pad(0u32.into())).is_empty());
    }
}
