//! Net highlight + interactive tracing (SPEC §4).
//!
//! This module drives *selection state* and *trace mode* by writing the
//! per-entity [`HighlightFlag`] arrays on the [`Scene`] — it never touches
//! geometry (SPEC §2/§4: a net highlight is a flag-buffer write + redraw).
//!
//! Two behaviors:
//!
//! 1. **Net highlight** ([`Tracer::select_net`]): given a [`NetId`], set every
//!    entity on that net to [`HighlightFlag::Highlighted`] and every other
//!    connectable entity to [`HighlightFlag::Dimmed`] (SPEC §4). Returns the
//!    [`NetSelection`] summary `{net, name, entity_count, pin_count}` the `ipc`
//!    agent publishes on `canvas.selection`.
//!
//! 2. **Trace mode** ([`Tracer::start`] / [`Tracer::step`] / [`Tracer::stop`]):
//!    a step-walk over the connectivity graph from the seed node, BFS by
//!    electrical distance. The whole walk order is computed up front at
//!    `trace.start` so each `trace.step` is O(1) and deterministic — it simply
//!    advances the front one edge: pad → wire/track → via → track → pad. The
//!    current front node is flagged [`HighlightFlag::TraceFront`]; the rest of
//!    the net stays [`HighlightFlag::Highlighted`]. A step that crosses layers
//!    (or lands on a via) is reported with `crossed_layer = true` so the
//!    renderer can flash the via (SPEC §4). `trace.stop` resets the front back
//!    to a plain net highlight.
//!
//! ## Decoupling from `graph`
//!
//! The connectivity graph lives in [`crate::graph`] and is built once at import.
//! Rather than bind this state machine to that concrete type (which is owned by
//! a different module), `trace` walks any value implementing the small
//! [`TraceGraph`] trait — exactly the three queries a walk needs:
//! `nodes_on_net`, `node_net`, and `neighbors`. The `graph::Graph` satisfies
//! this trait (wired up in the graph module / converge), and the tests below use
//! a tiny in-memory fixture, so the BFS logic is exercised without a parser.

use std::collections::VecDeque;

use crate::error::{CanvasError, Result};
use crate::graph::layer_crossing;
use crate::ids::{EntityKind, EntityRef, NetId};
use crate::ops::NetSelection;
use crate::scene::{HighlightFlag, LayerId, Scene};

// ===========================================================================
// The graph capability the trace walk needs.
// ===========================================================================

/// The minimal connectivity-graph surface a trace walk requires (SPEC §1/§4).
///
/// Implemented by [`crate::graph::Graph`]; the trace state machine is generic
/// over it so this module compiles and is fully tested without the graph module
/// (the tests provide an in-memory fixture). The four queries mirror the
/// contract the graph stub documents:
///
///   - [`nodes_on_net`](TraceGraph::nodes_on_net): every entity on a net (the
///     highlight set).
///   - [`node_net`](TraceGraph::node_net): the net a node belongs to.
///   - [`neighbors`](TraceGraph::neighbors): the electrically adjacent nodes of
///     a node (one edge away) — the BFS frontier.
///   - [`node_layer`](TraceGraph::node_layer): the copper layer a node sits on —
///     so the trace walk can flag a cross-layer step with the *same* rule as
///     [`crate::graph::Graph::bfs_walk`] (a via, or a layer change vs. the BFS
///     predecessor — see [`crate::graph::layer_crossing`]), rather than a
///     divergent second definition.
pub trait TraceGraph {
    /// Every entity electrically on `net`, in a stable order. Empty when the net
    /// is [`NetId::NONE`] or has no entities.
    fn nodes_on_net(&self, net: NetId) -> &[EntityRef];

    /// The net `node` belongs to, or [`NetId::NONE`] if the node is unknown /
    /// unconnected.
    fn node_net(&self, node: EntityRef) -> NetId;

    /// The nodes one electrical edge away from `node`. The order is the graph's;
    /// the trace walk sorts each frontier deterministically (see [`Tracer`]) so
    /// the visit order does not depend on the iteration order here.
    fn neighbors(&self, node: EntityRef) -> Vec<EntityRef>;

    /// The copper layer `node` sits on (schematic nodes share the single logical
    /// [`LayerId::SCHEMATIC`]). Used purely to compute the cross-layer flag; a
    /// node not in the graph defaults to [`LayerId::SCHEMATIC`].
    fn node_layer(&self, node: EntityRef) -> LayerId;
}

// ===========================================================================
// Trace state machine.
// ===========================================================================

/// What one `trace.step` advanced to (SPEC §4). Returned by [`Tracer::step`] so
/// the `ipc` agent can glide the camera to `at`'s entity and the renderer can
/// flash a via on a cross-layer step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepInfo {
    /// The node now at the trace front (just flagged [`HighlightFlag::TraceFront`]).
    pub at: EntityRef,
    /// BFS depth of `at` from the seed (electrical distance, in edges). The seed
    /// is depth 0.
    pub distance: u32,
    /// True when this step lands on a [`EntityKind::Via`] or otherwise crosses to
    /// a node whose class implies a layer change — the renderer flashes the via
    /// and toggles layer emphasis (SPEC §4: "Cross-layer steps flash the via").
    pub crossed_layer: bool,
    /// 1-based ordinal of this front within the walk (`1..=len`).
    pub position: u32,
    /// Total nodes in the walk (so the HUD can show "step k of n").
    pub total: u32,
    /// True when this was the last node — a further [`Tracer::step`] returns the
    /// same node and `at_end = true` again (the walk does not wrap).
    pub at_end: bool,
}

impl From<StepInfo> for crate::ops::TraceStep {
    /// Project a [`StepInfo`] onto the `canvas.selection` trace sub-payload
    /// ([`crate::ops::TraceStep`]). The wire field `crosses_layer` mirrors
    /// `StepInfo::crossed_layer`, and `step`/`of` mirror `position`/`total` — the
    /// 1-based "step k of n" the HUD shows.
    fn from(s: StepInfo) -> Self {
        crate::ops::TraceStep {
            at: s.at,
            distance: s.distance,
            crosses_layer: s.crossed_layer,
            step: s.position,
            of: s.total,
            at_end: s.at_end,
        }
    }
}

/// The phase the tracer is in. A plain net selection is `Selected`; `trace.start`
/// transitions to `Tracing` with a precomputed walk; `trace.stop` returns to
/// `Selected`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    /// No net selected — nothing highlighted.
    Idle,
    /// A net is highlighted (SPEC §4 net highlight), but not tracing.
    Selected { net: NetId },
    /// Trace mode: walking `order` (BFS-by-distance), `front` is the index of the
    /// current front node within `order`.
    Tracing {
        net: NetId,
        /// The BFS visit order from the seed (electrical distance). Each entry is
        /// `(node, depth, crossed_layer)` where `crossed_layer` is precomputed at
        /// `start` with the *same* rule as [`crate::graph::Graph::bfs_walk`]
        /// (see [`layer_crossing`]) so a `trace.step` is O(1) and never recomputes
        /// crossing from a divergent definition.
        order: Vec<(EntityRef, u32, bool)>,
        /// Index into `order` of the current front node.
        front: usize,
    },
}

/// Selection + trace state machine. Holds the current net selection and, in trace
/// mode, the precomputed BFS walk and front cursor. Owns no geometry; every
/// mutation is a write to the [`Scene`]'s `*_flags` arrays.
#[derive(Debug, Clone)]
pub struct Tracer {
    phase: Phase,
}

impl Default for Tracer {
    fn default() -> Self {
        Tracer { phase: Phase::Idle }
    }
}

impl Tracer {
    /// A fresh tracer with nothing selected.
    pub fn new() -> Self {
        Tracer::default()
    }

    /// The net currently selected / being traced, or [`NetId::NONE`] when idle.
    pub fn current_net(&self) -> NetId {
        match &self.phase {
            Phase::Idle => NetId::NONE,
            Phase::Selected { net } | Phase::Tracing { net, .. } => *net,
        }
    }

    /// True while in trace mode (between `trace.start` and `trace.stop`).
    pub fn is_tracing(&self) -> bool {
        matches!(self.phase, Phase::Tracing { .. })
    }

    // -- net highlight (SPEC §4) -------------------------------------------

    /// Highlight every entity on `net`: net entities → [`HighlightFlag::Highlighted`],
    /// every other connectable entity → [`HighlightFlag::Dimmed`] (SPEC §4: net
    /// in `--holo-bright`, everything else dimmed to 25%). Resets any in-progress
    /// trace (selecting a new net leaves trace mode). Returns the
    /// [`NetSelection`] summary.
    ///
    /// `net == NetId::NONE` clears the selection (back to all-`Normal`) and
    /// reports an empty summary on the no-net id.
    pub fn select_net<G: TraceGraph>(
        &mut self,
        scene: &mut Scene,
        graph: &G,
        net: NetId,
    ) -> NetSelection {
        scene.clear_highlights();

        if net.is_none() {
            self.phase = Phase::Idle;
            return NetSelection {
                net: NetId::NONE,
                name: String::new(),
                entity_count: 0,
                pin_count: 0,
            };
        }

        // Dim every connectable entity, then re-light the ones on this net.
        dim_all(scene);

        let nodes = graph.nodes_on_net(net);
        let mut entity_count: u32 = 0;
        let mut pin_count: u32 = 0;
        for &e in nodes {
            if set_flag(scene, e, HighlightFlag::Highlighted) {
                entity_count += 1;
                if e.kind == EntityKind::Pad {
                    pin_count += 1;
                }
            }
        }

        self.phase = Phase::Selected { net };

        NetSelection {
            net,
            name: scene.net_name(net).to_string(),
            entity_count,
            pin_count,
        }
    }

    // -- trace mode (SPEC §4) ----------------------------------------------

    /// Enter trace mode on the currently selected net (`trace.start`). Computes
    /// the BFS-by-electrical-distance walk from a seed node up front, marks the
    /// seed as the front, and reports the first [`StepInfo`].
    ///
    /// Errors with [`CanvasError::TraceState`] if no net is selected or the
    /// selected net has no nodes. Calling `start` again while already tracing
    /// restarts the walk from the seed of the current net.
    pub fn start<G: TraceGraph>(&mut self, scene: &mut Scene, graph: &G) -> Result<StepInfo> {
        let net = match &self.phase {
            Phase::Idle => {
                return Err(CanvasError::TraceState(
                    "trace.start with no net selected".to_string(),
                ));
            }
            Phase::Selected { net } | Phase::Tracing { net, .. } => *net,
        };

        let nodes = graph.nodes_on_net(net);
        let seed = match pick_seed(nodes) {
            Some(s) => s,
            None => {
                return Err(CanvasError::TraceState(format!(
                    "trace.start on net {} with no traceable nodes",
                    net.raw()
                )));
            }
        };

        let order = bfs_order(graph, net, seed);
        // `order` is non-empty: it always contains at least the seed.

        // Repaint the net as a static highlight, then mark the seed as the front.
        repaint_net_highlight(scene, graph, net);
        self.phase = Phase::Tracing {
            net,
            order,
            front: 0,
        };
        Ok(self.apply_front(scene))
    }

    /// Advance the trace front one electrical edge (`trace.step`). Clears the
    /// previous front back to [`HighlightFlag::Highlighted`], advances the
    /// cursor, and marks the new front [`HighlightFlag::TraceFront`]. At the end
    /// of the walk the front stays on the last node (`at_end = true`) — the walk
    /// does not wrap.
    ///
    /// Errors with [`CanvasError::TraceState`] if not in trace mode (i.e.
    /// `trace.step` before `trace.start`).
    pub fn step(&mut self, scene: &mut Scene) -> Result<StepInfo> {
        match &mut self.phase {
            Phase::Tracing { order, front, .. } => {
                if *front + 1 < order.len() {
                    *front += 1;
                }
                // else: already at the end; stay put and report at_end.
            }
            _ => {
                return Err(CanvasError::TraceState(
                    "trace.step before trace.start".to_string(),
                ));
            }
        }
        Ok(self.apply_front(scene))
    }

    /// Exit trace mode (`trace.stop`): clear the [`HighlightFlag::TraceFront`]
    /// marker, leaving the net as a plain highlight (every net entity back to
    /// [`HighlightFlag::Highlighted`]). The tracer returns to the `Selected`
    /// phase so a subsequent `trace.start` re-walks the same net.
    ///
    /// Idempotent and infallible: stopping when not tracing is a no-op that just
    /// keeps the current selection.
    pub fn stop<G: TraceGraph>(&mut self, scene: &mut Scene, graph: &G) {
        if let Phase::Tracing { net, .. } = self.phase {
            repaint_net_highlight(scene, graph, net);
            self.phase = Phase::Selected { net };
        }
    }

    // -- internals ----------------------------------------------------------

    /// Paint the current front node [`HighlightFlag::TraceFront`] (after demoting
    /// the previous front back to `Highlighted`) and build the [`StepInfo`]. The
    /// whole net is assumed already painted `Highlighted` by the caller
    /// (`start`/`step` keep that invariant: only the single front cell differs).
    fn apply_front(&self, scene: &mut Scene) -> StepInfo {
        let (order, front) = match &self.phase {
            Phase::Tracing { order, front, .. } => (order, *front),
            // apply_front is only ever called from within Tracing.
            _ => unreachable!("apply_front called outside trace mode"),
        };

        // Demote any cell currently flagged TraceFront back to Highlighted, so
        // exactly one node carries the front marker (the net is otherwise all
        // Highlighted). This is cheap relative to a redraw and keeps the
        // invariant without tracking the previous index across phase mutations.
        demote_trace_front(scene);

        let (node, distance, crossed_layer) = order[front];
        set_flag(scene, node, HighlightFlag::TraceFront);

        let total = order.len() as u32;
        let position = front as u32 + 1;
        let at_end = front + 1 >= order.len();

        StepInfo {
            at: node,
            distance,
            crossed_layer,
            position,
            total,
            at_end,
        }
    }
}

// ===========================================================================
// Free helpers (geometry-free; all operate on the flag arrays).
// ===========================================================================

/// Choose the BFS seed: the first pad on the net if any (a trace conceptually
/// starts at a terminal), else the first node. Returns `None` for an empty set.
fn pick_seed(nodes: &[EntityRef]) -> Option<EntityRef> {
    nodes
        .iter()
        .copied()
        .find(|e| e.kind == EntityKind::Pad)
        .or_else(|| nodes.first().copied())
}

/// BFS from `seed`, restricted to nodes on `net`, returning `(node, depth,
/// crossed_layer)` in visit order — by ascending electrical distance, ties
/// broken by a stable per-frontier ordering so the walk is deterministic
/// (SPEC §4: "Walk order: BFS by electrical distance"). The per-frontier
/// tiebreak sorts by `(kind, index)` so a fixed graph always yields the same
/// order regardless of `neighbors` iteration order.
///
/// `crossed_layer` is computed *here*, against each node's BFS predecessor layer
/// (carried in the queue), with [`layer_crossing`] — the very rule
/// [`crate::graph::Graph::bfs_walk`] uses. The seed has no predecessor and so
/// never crosses. This is the single source of truth: a `Graph::bfs_walk` and
/// this walk over the same connectivity flag the same steps.
fn bfs_order<G: TraceGraph>(
    graph: &G,
    net: NetId,
    seed: EntityRef,
) -> Vec<(EntityRef, u32, bool)> {
    // Small, allocation-light visited set: nets are bounded, and a Vec scan over
    // the already-ordered list is fine for the typical few-hundred-node net. We
    // keep a parallel sorted-key check via the `order` membership to avoid a
    // hashmap dependency in this hot-but-bounded path.
    let mut order: Vec<(EntityRef, u32, bool)> = Vec::new();
    let mut visited: Vec<EntityRef> = Vec::new();
    // (node, depth, predecessor-layer); the seed carries `None`.
    let mut queue: VecDeque<(EntityRef, u32, Option<LayerId>)> = VecDeque::new();

    queue.push_back((seed, 0, None));
    visited.push(seed);

    while let Some((node, depth, pred_layer)) = queue.pop_front() {
        let layer = graph.node_layer(node);
        order.push((node, depth, layer_crossing(node, pred_layer, layer)));

        // Gather not-yet-visited neighbors that are on the same net, sort them
        // deterministically, then enqueue at depth+1 carrying this node's layer
        // as their predecessor layer.
        let mut frontier: Vec<EntityRef> = graph
            .neighbors(node)
            .into_iter()
            .filter(|n| graph.node_net(*n) == net && !visited.contains(n))
            .collect();
        frontier.sort_by(entity_ref_order);
        frontier.dedup();

        for n in frontier {
            if !visited.contains(&n) {
                visited.push(n);
                queue.push_back((n, depth + 1, Some(layer)));
            }
        }
    }

    order
}

/// Total order over [`EntityRef`] for deterministic per-frontier sorting:
/// `(kind, index)`. `EntityKind` derives `Ord`, so this is a stable tiebreak.
fn entity_ref_order(a: &EntityRef, b: &EntityRef) -> std::cmp::Ordering {
    (a.kind, a.index).cmp(&(b.kind, b.index))
}

/// Set every connectable entity to [`HighlightFlag::Dimmed`] (SPEC §4: off-net
/// entities dim to 25%). Components/sheets carry no net but are dimmed too so the
/// selected net visually pops; the renderer maps `Dimmed` uniformly.
fn dim_all(scene: &mut Scene) {
    for f in &mut scene.component_flags {
        *f = HighlightFlag::Dimmed;
    }
    for f in &mut scene.pad_flags {
        *f = HighlightFlag::Dimmed;
    }
    for f in &mut scene.wire_flags {
        *f = HighlightFlag::Dimmed;
    }
    for f in &mut scene.junction_flags {
        *f = HighlightFlag::Dimmed;
    }
    for f in &mut scene.label_flags {
        *f = HighlightFlag::Dimmed;
    }
    for f in &mut scene.track_flags {
        *f = HighlightFlag::Dimmed;
    }
    for f in &mut scene.via_flags {
        *f = HighlightFlag::Dimmed;
    }
    for f in &mut scene.zone_flags {
        *f = HighlightFlag::Dimmed;
    }
}

/// Re-establish the static net highlight: dim everything, then set every node on
/// `net` to [`HighlightFlag::Highlighted`]. Used by `start`/`stop` to drop the
/// trace front back to a plain highlight.
fn repaint_net_highlight<G: TraceGraph>(scene: &mut Scene, graph: &G, net: NetId) {
    dim_all(scene);
    for &e in graph.nodes_on_net(net) {
        set_flag(scene, e, HighlightFlag::Highlighted);
    }
}

/// Demote any cell currently flagged [`HighlightFlag::TraceFront`] back to
/// [`HighlightFlag::Highlighted`]. Cheap linear pass over the small flag arrays;
/// keeps the "exactly one front" invariant without threading the previous index
/// through the phase enum.
fn demote_trace_front(scene: &mut Scene) {
    let demote = |arr: &mut [HighlightFlag]| {
        for f in arr {
            if *f == HighlightFlag::TraceFront {
                *f = HighlightFlag::Highlighted;
            }
        }
    };
    demote(&mut scene.component_flags);
    demote(&mut scene.pad_flags);
    demote(&mut scene.wire_flags);
    demote(&mut scene.junction_flags);
    demote(&mut scene.label_flags);
    demote(&mut scene.track_flags);
    demote(&mut scene.via_flags);
    demote(&mut scene.zone_flags);
}

/// Write `flag` into the flag array for `e`'s class at `e`'s index. Returns
/// `true` if the index was in range (the entity exists), `false` otherwise
/// (out-of-range refs are ignored, never panic — the graph and scene agree on
/// indices, but trace stays total against adversarial input). Components and
/// sheets have flag arrays too, so every [`EntityKind`] is handled.
fn set_flag(scene: &mut Scene, e: EntityRef, flag: HighlightFlag) -> bool {
    let i = e.idx();
    let arr: &mut Vec<HighlightFlag> = match e.kind {
        EntityKind::Component => &mut scene.component_flags,
        EntityKind::Pad => &mut scene.pad_flags,
        EntityKind::Wire => &mut scene.wire_flags,
        EntityKind::Junction => &mut scene.junction_flags,
        EntityKind::Label => &mut scene.label_flags,
        EntityKind::Sheet => return false, // no sheet flag array in the scene
        EntityKind::Track => &mut scene.track_flags,
        EntityKind::Via => &mut scene.via_flags,
        EntityKind::Zone => &mut scene.zone_flags,
    };
    match arr.get_mut(i) {
        Some(slot) => {
            *slot = flag;
            true
        }
        None => false,
    }
}

// ===========================================================================
// Tests — embedded in-memory graph fixture (no parser / no graph module).
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{PadId, TrackId, ViaId};
    use crate::scene::{LayerId, Pad, PadShape, PinType, Point, SceneKind, Track, Via};

    /// A trivial in-memory connectivity graph for tests: an explicit adjacency
    /// list plus per-node net + the per-net node list. Mirrors what
    /// [`crate::graph::Graph`] will expose, so the trace walk is exercised end to
    /// end without a parser.
    struct MockGraph {
        /// node -> its net
        nets: Vec<(EntityRef, NetId)>,
        /// node -> neighbors
        adj: Vec<(EntityRef, Vec<EntityRef>)>,
        /// net -> nodes (kept so `nodes_on_net` returns a stable slice)
        net_nodes: std::collections::BTreeMap<u32, Vec<EntityRef>>,
        /// node -> the copper layer it sits on (mirrors the scene fixture).
        layers: std::collections::HashMap<EntityRef, LayerId>,
    }

    impl MockGraph {
        fn new() -> Self {
            MockGraph {
                nets: Vec::new(),
                adj: Vec::new(),
                net_nodes: std::collections::BTreeMap::new(),
                layers: std::collections::HashMap::new(),
            }
        }

        /// Add a node on [`LayerId::SCHEMATIC`] (the default for the
        /// single-layer / order-only tests).
        fn add_node(&mut self, e: EntityRef, net: NetId) {
            self.add_node_on(e, net, LayerId::SCHEMATIC);
        }

        /// Add a node on an explicit layer (PCB cross-layer tests).
        fn add_node_on(&mut self, e: EntityRef, net: NetId, layer: LayerId) {
            self.nets.push((e, net));
            self.adj.push((e, Vec::new()));
            self.net_nodes.entry(net.raw()).or_default().push(e);
            self.layers.insert(e, layer);
        }

        /// Add an undirected edge.
        fn add_edge(&mut self, a: EntityRef, b: EntityRef) {
            for (n, adj) in &mut self.adj {
                if *n == a {
                    adj.push(b);
                }
                if *n == b {
                    adj.push(a);
                }
            }
        }
    }

    impl TraceGraph for MockGraph {
        fn nodes_on_net(&self, net: NetId) -> &[EntityRef] {
            self.net_nodes
                .get(&net.raw())
                .map(|v| v.as_slice())
                .unwrap_or(&[])
        }

        fn node_net(&self, node: EntityRef) -> NetId {
            self.nets
                .iter()
                .find(|(n, _)| *n == node)
                .map(|(_, net)| *net)
                .unwrap_or(NetId::NONE)
        }

        fn neighbors(&self, node: EntityRef) -> Vec<EntityRef> {
            self.adj
                .iter()
                .find(|(n, _)| *n == node)
                .map(|(_, a)| a.clone())
                .unwrap_or_default()
        }

        fn node_layer(&self, node: EntityRef) -> LayerId {
            self.layers
                .get(&node)
                .copied()
                .unwrap_or(LayerId::SCHEMATIC)
        }
    }

    fn pad(i: u32) -> EntityRef {
        EntityRef::pad(PadId::new(i))
    }
    fn track(i: u32) -> EntityRef {
        EntityRef::track(TrackId::new(i))
    }
    fn via(i: u32) -> EntityRef {
        EntityRef::via(ViaId::new(i))
    }

    /// Build a scene with enough entities (and `init_flags`) that every
    /// `EntityRef` in the fixture graph addresses a real flag slot. Net layout:
    /// net 1 = a multi-segment net `pad0 - track0 - via0 - track1 - pad1`;
    /// net 2 = an unrelated `pad2 - track2 - pad3` so dimming/highlight separation
    /// is observable.
    fn fixture() -> (Scene, MockGraph) {
        let mut scene = Scene::new(SceneKind::Pcb);
        scene.net_names = vec![
            String::new(),  // net 0 = no-net
            "3V3".into(),   // net 1
            "GND".into(),   // net 2
        ];
        let l0 = LayerId::new(0);
        let l1 = LayerId::new(1);

        // 4 pads (0..=3)
        for i in 0..4 {
            scene.pads.push(Pad {
                component: crate::ids::ComponentId::new(0),
                name: format!("{i}"),
                position: Point::new(i as f64, 0.0),
                size: (1.0, 1.0),
                shape: PadShape::Circle,
                pin_type: PinType::Passive,
                layer: l0,
                net_id: if i < 2 { NetId::new(1) } else { NetId::new(2) },
            });
        }
        // 3 tracks (0..=2)
        for i in 0..3 {
            scene.tracks.push(Track {
                a: Point::new(i as f64, 0.0),
                b: Point::new(i as f64 + 1.0, 0.0),
                width: 0.25,
                layer: if i == 1 { l1 } else { l0 },
                net_id: if i < 2 { NetId::new(1) } else { NetId::new(2) },
            });
        }
        // 1 via (0)
        scene.vias.push(Via {
            position: Point::new(1.5, 0.0),
            diameter: 0.6,
            drill: 0.3,
            layer_from: l0,
            layer_to: l1,
            net_id: NetId::new(1),
        });
        scene.init_flags();

        // Graph: net 1 path pad0 - track0 - via0 - track1 - pad1. Layers mirror
        // the scene above: pads & track0 on l0, the via interned at its
        // `layer_from` (l0), track1 on l1 (so the via stitches l0 → l1).
        let mut g = MockGraph::new();
        g.add_node_on(pad(0), NetId::new(1), l0);
        g.add_node_on(track(0), NetId::new(1), l0);
        g.add_node_on(via(0), NetId::new(1), l0);
        g.add_node_on(track(1), NetId::new(1), l1);
        g.add_node_on(pad(1), NetId::new(1), l0);
        g.add_edge(pad(0), track(0));
        g.add_edge(track(0), via(0));
        g.add_edge(via(0), track(1));
        g.add_edge(track(1), pad(1));

        // net 2 path pad2 - track2 - pad3 (all on l0)
        g.add_node_on(pad(2), NetId::new(2), l0);
        g.add_node_on(track(2), NetId::new(2), l0);
        g.add_node_on(pad(3), NetId::new(2), l0);
        g.add_edge(pad(2), track(2));
        g.add_edge(track(2), pad(3));

        (scene, g)
    }

    fn flag_of(scene: &Scene, e: EntityRef) -> HighlightFlag {
        match e.kind {
            EntityKind::Pad => scene.pad_flags[e.idx()],
            EntityKind::Track => scene.track_flags[e.idx()],
            EntityKind::Via => scene.via_flags[e.idx()],
            EntityKind::Wire => scene.wire_flags[e.idx()],
            EntityKind::Junction => scene.junction_flags[e.idx()],
            EntityKind::Label => scene.label_flags[e.idx()],
            EntityKind::Zone => scene.zone_flags[e.idx()],
            EntityKind::Component => scene.component_flags[e.idx()],
            EntityKind::Sheet => HighlightFlag::Normal,
        }
    }

    #[test]
    fn select_net_highlights_net_and_dims_rest() {
        let (mut scene, g) = fixture();
        let mut t = Tracer::new();
        let sel = t.select_net(&mut scene, &g, NetId::new(1));

        // Summary: net 1 has 5 nodes (pad0, track0, via0, track1, pad1), 2 pins.
        assert_eq!(sel.net, NetId::new(1));
        assert_eq!(sel.name, "3V3");
        assert_eq!(sel.entity_count, 5);
        assert_eq!(sel.pin_count, 2);

        // Net-1 entities are Highlighted.
        for e in [pad(0), track(0), via(0), track(1), pad(1)] {
            assert_eq!(flag_of(&scene, e), HighlightFlag::Highlighted, "{e:?}");
        }
        // Net-2 entities are Dimmed (off the selected net).
        for e in [pad(2), track(2), pad(3)] {
            assert_eq!(flag_of(&scene, e), HighlightFlag::Dimmed, "{e:?}");
        }
        assert_eq!(t.current_net(), NetId::new(1));
        assert!(!t.is_tracing());
    }

    #[test]
    fn select_none_clears_all() {
        let (mut scene, g) = fixture();
        let mut t = Tracer::new();
        t.select_net(&mut scene, &g, NetId::new(1));
        let sel = t.select_net(&mut scene, &g, NetId::NONE);
        assert_eq!(sel.entity_count, 0);
        assert!(sel.net.is_none());
        // Everything back to Normal.
        assert!(scene.pad_flags.iter().all(|f| *f == HighlightFlag::Normal));
        assert!(scene.track_flags.iter().all(|f| *f == HighlightFlag::Normal));
        assert!(scene.via_flags.iter().all(|f| *f == HighlightFlag::Normal));
        assert_eq!(t.current_net(), NetId::NONE);
    }

    #[test]
    fn bfs_visit_order_is_by_electrical_distance() {
        // The known multi-segment net 1: pad0 - track0 - via0 - track1 - pad1.
        // Seeded at the first pad (pad0), BFS-by-distance must visit exactly:
        //   d0 pad0, d1 track0, d2 via0, d3 track1, d4 pad1.
        let (_, g) = fixture();
        let order = bfs_order(&g, NetId::new(1), pick_seed(g.nodes_on_net(NetId::new(1))).unwrap());
        // Each entry is (node, depth, crossed_layer). Crossing follows the shared
        // rule (a via, or a layer change vs. the BFS predecessor): seed pad0 never
        // crosses; track0 stays on l0; via0 is a via (crosses); track1 changes
        // l0→l1 (crosses); pad1 changes l1→l0 (crosses).
        let got: Vec<(EntityRef, u32, bool)> = order;
        assert_eq!(
            got,
            vec![
                (pad(0), 0, false),
                (track(0), 1, false),
                (via(0), 2, true),
                (track(1), 3, true),
                (pad(1), 4, true),
            ]
        );
    }

    #[test]
    fn bfs_frontier_tiebreak_is_deterministic() {
        // A star: pad0 (seed) connected to track2, track0, track1 (added out of
        // order). The per-frontier sort by (kind,index) must yield track0,track1,
        // track2 regardless of insertion order.
        let mut g = MockGraph::new();
        let net = NetId::new(1);
        g.add_node(pad(0), net);
        g.add_node(track(2), net);
        g.add_node(track(0), net);
        g.add_node(track(1), net);
        g.add_edge(pad(0), track(2));
        g.add_edge(pad(0), track(0));
        g.add_edge(pad(0), track(1));

        let order = bfs_order(&g, net, pad(0));
        // All nodes share the single schematic layer, so no step crosses.
        assert_eq!(
            order,
            vec![
                (pad(0), 0, false),
                (track(0), 1, false),
                (track(1), 1, false),
                (track(2), 1, false),
            ]
        );
    }

    #[test]
    fn trace_step_advances_deterministically_and_marks_front() {
        let (mut scene, g) = fixture();
        let mut t = Tracer::new();
        t.select_net(&mut scene, &g, NetId::new(1));

        // start -> seed (pad0) is the front, distance 0, position 1/5.
        let s0 = t.start(&mut scene, &g).unwrap();
        assert_eq!(s0.at, pad(0));
        assert_eq!(s0.distance, 0);
        assert_eq!(s0.position, 1);
        assert_eq!(s0.total, 5);
        assert!(!s0.at_end);
        assert_eq!(flag_of(&scene, pad(0)), HighlightFlag::TraceFront);
        // The rest of the net stays Highlighted.
        assert_eq!(flag_of(&scene, track(0)), HighlightFlag::Highlighted);

        // step -> track0 (distance 1). pad0 demoted back to Highlighted.
        let s1 = t.step(&mut scene).unwrap();
        assert_eq!(s1.at, track(0));
        assert_eq!(s1.distance, 1);
        assert_eq!(s1.position, 2);
        assert!(!s1.crossed_layer);
        assert_eq!(flag_of(&scene, pad(0)), HighlightFlag::Highlighted);
        assert_eq!(flag_of(&scene, track(0)), HighlightFlag::TraceFront);

        // step -> via0 (distance 2). A via is a cross-layer step (flash).
        let s2 = t.step(&mut scene).unwrap();
        assert_eq!(s2.at, via(0));
        assert_eq!(s2.distance, 2);
        assert!(s2.crossed_layer, "stepping onto a via crosses a layer");
        assert_eq!(flag_of(&scene, via(0)), HighlightFlag::TraceFront);

        // step -> track1, step -> pad1 (the end).
        let s3 = t.step(&mut scene).unwrap();
        assert_eq!(s3.at, track(1));
        assert!(!s3.at_end);
        let s4 = t.step(&mut scene).unwrap();
        assert_eq!(s4.at, pad(1));
        assert_eq!(s4.distance, 4);
        assert_eq!(s4.position, 5);
        assert!(s4.at_end);

        // Stepping past the end stays on the last node (no wrap).
        let s5 = t.step(&mut scene).unwrap();
        assert_eq!(s5.at, pad(1));
        assert!(s5.at_end);
        assert_eq!(s5.position, 5);

        // Exactly one TraceFront cell across the whole scene.
        let fronts = scene
            .pad_flags
            .iter()
            .chain(&scene.track_flags)
            .chain(&scene.via_flags)
            .filter(|f| **f == HighlightFlag::TraceFront)
            .count();
        assert_eq!(fronts, 1);
    }

    #[test]
    fn trace_stop_resets_front_to_plain_highlight() {
        let (mut scene, g) = fixture();
        let mut t = Tracer::new();
        t.select_net(&mut scene, &g, NetId::new(1));
        t.start(&mut scene, &g).unwrap();
        t.step(&mut scene).unwrap(); // front on track0

        t.stop(&mut scene, &g);
        assert!(!t.is_tracing());
        assert_eq!(t.current_net(), NetId::new(1));

        // No TraceFront anywhere; every net-1 node back to Highlighted; net-2 dimmed.
        for e in [pad(0), track(0), via(0), track(1), pad(1)] {
            assert_eq!(flag_of(&scene, e), HighlightFlag::Highlighted, "{e:?}");
        }
        for e in [pad(2), track(2), pad(3)] {
            assert_eq!(flag_of(&scene, e), HighlightFlag::Dimmed, "{e:?}");
        }
        let any_front = scene
            .pad_flags
            .iter()
            .chain(&scene.track_flags)
            .chain(&scene.via_flags)
            .any(|f| *f == HighlightFlag::TraceFront);
        assert!(!any_front);
    }

    #[test]
    fn step_before_start_errors() {
        let (mut scene, _g) = fixture();
        let mut t = Tracer::new();
        let err = t.step(&mut scene).unwrap_err();
        assert!(matches!(err, CanvasError::TraceState(_)));
    }

    #[test]
    fn start_without_selection_errors() {
        let (mut scene, g) = fixture();
        let mut t = Tracer::new();
        let err = t.start(&mut scene, &g).unwrap_err();
        assert!(matches!(err, CanvasError::TraceState(_)));
    }

    #[test]
    fn start_on_empty_net_errors() {
        let (mut scene, g) = fixture();
        let mut t = Tracer::new();
        // Net 0 (no-net) has no nodes in the graph.
        t.select_net(&mut scene, &g, NetId::new(0));
        // select_net of an empty net leaves us in Selected{net0}; start must fail.
        let err = t.start(&mut scene, &g).unwrap_err();
        assert!(matches!(err, CanvasError::TraceState(_)));
    }

    #[test]
    fn restart_rewalks_from_seed() {
        let (mut scene, g) = fixture();
        let mut t = Tracer::new();
        t.select_net(&mut scene, &g, NetId::new(1));
        t.start(&mut scene, &g).unwrap();
        t.step(&mut scene).unwrap();
        t.step(&mut scene).unwrap(); // front on via0
        // Restart: front goes back to the seed (pad0).
        let s = t.start(&mut scene, &g).unwrap();
        assert_eq!(s.at, pad(0));
        assert_eq!(s.position, 1);
        assert_eq!(flag_of(&scene, pad(0)), HighlightFlag::TraceFront);
        assert_eq!(flag_of(&scene, via(0)), HighlightFlag::Highlighted);
    }

    #[test]
    fn stop_when_not_tracing_is_noop() {
        let (mut scene, g) = fixture();
        let mut t = Tracer::new();
        // Stop with nothing selected: no panic, still idle.
        t.stop(&mut scene, &g);
        assert_eq!(t.current_net(), NetId::NONE);
        // Stop after a plain selection: stays selected, no front.
        t.select_net(&mut scene, &g, NetId::new(1));
        t.stop(&mut scene, &g);
        assert_eq!(t.current_net(), NetId::new(1));
        assert!(!t.is_tracing());
    }

    #[test]
    fn out_of_range_entity_ref_is_ignored_not_panicked() {
        // A graph node whose index is past the scene arrays must not panic the
        // flag write (trace stays total against scene/graph disagreement).
        let mut scene = Scene::new(SceneKind::Pcb);
        scene.net_names = vec![String::new(), "N".into()];
        scene.pads.push(Pad {
            component: crate::ids::ComponentId::new(0),
            name: "1".into(),
            position: Point::ORIGIN,
            size: (1.0, 1.0),
            shape: PadShape::Circle,
            pin_type: PinType::Passive,
            layer: LayerId::new(0),
            net_id: NetId::new(1),
        });
        scene.init_flags();

        let mut g = MockGraph::new();
        g.add_node(pad(0), NetId::new(1));
        g.add_node(track(999), NetId::new(1)); // out-of-range track
        g.add_edge(pad(0), track(999));

        let mut t = Tracer::new();
        // Should not panic despite the dangling track index.
        let sel = t.select_net(&mut scene, &g, NetId::new(1));
        // Only the in-range pad counted.
        assert_eq!(sel.entity_count, 1);
        let info = t.start(&mut scene, &g).unwrap();
        assert_eq!(info.total, 2); // walk still lists both nodes
        t.step(&mut scene).unwrap(); // stepping onto the dangling node is a no-op write
    }

    #[test]
    fn pcb_layer_change_without_a_via_flags_the_crossing() {
        // The MEDIUM finding: a layer change with NO via node on the step must
        // still flag a crossing. Net 1: track_a (l0) — track_b (l1), connected
        // directly (e.g. a buried/blind transition the parser modelled as
        // adjacency) with no via between them. Stepping track_a -> track_b is a
        // cross-layer step even though neither node is a via.
        let l0 = LayerId::new(0);
        let l1 = LayerId::new(1);
        let mut g = MockGraph::new();
        let net = NetId::new(1);
        g.add_node_on(track(0), net, l0); // seed, on l0
        g.add_node_on(track(1), net, l1); // same net, different layer
        g.add_node_on(track(2), net, l1); // stays on l1 after the crossing
        g.add_edge(track(0), track(1));
        g.add_edge(track(1), track(2));

        let order = bfs_order(&g, net, track(0));
        assert_eq!(
            order,
            vec![
                (track(0), 0, false), // seed never crosses
                (track(1), 1, true),  // l0 -> l1 with NO via: still a crossing
                (track(2), 2, false), // l1 -> l1: same layer, no crossing
            ],
            "a layer change without a via must flag; a same-layer step must not"
        );
    }

    #[test]
    fn via_flags_the_crossing_even_without_a_layer_field_change() {
        // A via node is always a crossing (it *is* the layer transition), even if
        // it was interned on the same layer its predecessor sat on. pad0 (l0) ->
        // via0 (interned on l0): the via clause flags it regardless.
        let l0 = LayerId::new(0);
        let mut g = MockGraph::new();
        let net = NetId::new(1);
        g.add_node_on(pad(0), net, l0);
        g.add_node_on(via(0), net, l0); // via interned on the SAME layer as pad0
        g.add_edge(pad(0), via(0));

        let order = bfs_order(&g, net, pad(0));
        assert_eq!(
            order,
            vec![(pad(0), 0, false), (via(0), 1, true)],
            "stepping onto a via is always a crossing"
        );
    }

    #[test]
    fn step_info_crossing_matches_shared_rule_and_is_copy() {
        // StepInfo is Copy.
        let s = StepInfo {
            at: pad(0),
            distance: 0,
            crossed_layer: false,
            position: 1,
            total: 1,
            at_end: true,
        };
        let _copy = s; // moves a Copy value; original still usable
        assert_eq!(s.at, pad(0));

        // The trace walk and graph::bfs_walk share one crossing definition
        // (crate::graph::layer_crossing). Spot-check the rule directly: no
        // predecessor (seed) never crosses; a via always crosses; a layer change
        // crosses; a same-layer non-via does not.
        let l0 = LayerId::new(0);
        let l1 = LayerId::new(1);
        assert!(!layer_crossing(pad(0), None, l0)); // seed
        assert!(layer_crossing(via(0), Some(l0), l0)); // via, same layer field
        assert!(layer_crossing(track(0), Some(l0), l1)); // layer change, no via
        assert!(!layer_crossing(track(0), Some(l0), l0)); // same layer, no via
    }

    #[test]
    fn schematic_single_layer_walk_never_crosses() {
        // Regression guard: schematic nets live on the single logical layer, so
        // no step ever flags a crossing (no vias, no layer field differences).
        let net = NetId::new(1);
        let mut g = MockGraph::new();
        g.add_node(pad(0), net); // add_node => LayerId::SCHEMATIC
        g.add_node(track(0), net);
        g.add_node(pad(1), net);
        g.add_edge(pad(0), track(0));
        g.add_edge(track(0), pad(1));

        let order = bfs_order(&g, net, pad(0));
        assert!(
            order.iter().all(|(_, _, crossed)| !*crossed),
            "single-layer schematic walk must never flag a crossing"
        );
    }
}
