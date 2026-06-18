//! Spatial index (SPEC §1, §3) — one `rstar` R-tree per layer/class for
//! hit-testing (point query, layer-priority order; SPEC §3) and viewport culling
//! (rectangle query; SPEC §2). Built once at import, immutable afterward.
//!
//! Layout: one [`rstar::RTree`] per [`LayerId`] (the `u16` raw value is the map
//! key). Every entity is reduced to a small `Copy` payload ([`Spatial`]) holding
//! its [`EntityRef`], its layer, and its scene-space [`Aabb`]. The R-tree
//! envelope is an `rstar::AABB<[f64; 2]>` built straight from
//! [`Aabb::corners`], so deep-zoom hit-tests stay f64-precise (SPEC §3).
//!
//! Hit-test priority (SPEC §3, "layer-priority order"): the caller passes the
//! layers in priority order; within one layer the *tightest* containing box wins
//! (a pad sitting on a track is selected over the track), with a stable per-kind
//! tiebreak. Viewport culling ([`Index::query_rect`]) is an envelope-intersection
//! query and returns every entity whose box touches the rectangle.

use std::collections::HashMap;

use rstar::{RTree, RTreeObject, PointDistance, AABB};

use crate::ids::{EntityKind, EntityRef};
use crate::scene::{Aabb, LayerId, Point, Scene};

/// A tiny inflation (mm) applied to zero-area entity boxes (junctions, labels,
/// schematic pin points) so a point query can land on them. Far below the
/// schematic placement grid (0.0254 mm) yet large enough to be hittable.
const HIT_PAD_MM: f64 = 0.05;

/// One R-tree payload: a scene entity reduced to its spatial footprint. `Copy`
/// and cheap so `bulk_load` moves a `Vec<Spatial>` per layer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spatial {
    /// Which scene entity this box stands for.
    pub entity: EntityRef,
    /// The layer this entity was indexed on (the map key it lives under).
    pub layer: LayerId,
    /// Scene-space bounding box (already inflated for point-like entities).
    pub bbox: Aabb,
}

impl Spatial {
    #[inline]
    fn min(&self) -> [f64; 2] {
        [self.bbox.min.x, self.bbox.min.y]
    }
    #[inline]
    fn max(&self) -> [f64; 2] {
        [self.bbox.max.x, self.bbox.max.y]
    }
    /// Box area — the tiebreak metric for "topmost" (tightest) at a point.
    #[inline]
    fn area(&self) -> f64 {
        self.bbox.width() * self.bbox.height()
    }
}

impl RTreeObject for Spatial {
    type Envelope = AABB<[f64; 2]>;

    #[inline]
    fn envelope(&self) -> Self::Envelope {
        AABB::from_corners(self.min(), self.max())
    }
}

impl PointDistance for Spatial {
    /// Squared distance from `point` to this box (0 inside). With this, rstar's
    /// default `contains_point` (distance_2 <= 0) correctly reports containment,
    /// which is what [`Index::hit_test`] relies on via `locate_all_at_point`.
    #[inline]
    fn distance_2(&self, point: &[f64; 2]) -> f64 {
        let dx = clamp_axis(point[0], self.bbox.min.x, self.bbox.max.x);
        let dy = clamp_axis(point[1], self.bbox.min.y, self.bbox.max.y);
        dx * dx + dy * dy
    }
}

/// Distance from `v` to the `[lo, hi]` interval (0 when inside).
#[inline]
fn clamp_axis(v: f64, lo: f64, hi: f64) -> f64 {
    if v < lo {
        lo - v
    } else if v > hi {
        v - hi
    } else {
        0.0
    }
}

/// Per-kind tiebreak when two boxes of equal area contain the same point. Lower
/// rank wins. Small terminal/connection entities beat large area entities so a
/// click on a pad lands on the pad, not the courtyard or copper under it.
#[inline]
fn kind_rank(kind: EntityKind) -> u8 {
    match kind {
        EntityKind::Pad => 0,
        EntityKind::Junction => 1,
        EntityKind::Via => 2,
        EntityKind::Label => 3,
        EntityKind::Wire => 4,
        EntityKind::Track => 5,
        EntityKind::Sheet => 6,
        EntityKind::Component => 7,
        EntityKind::Zone => 8,
    }
}

/// The immutable spatial index over a [`Scene`]: one R-tree per layer. Built once
/// via [`Index::build`]; thereafter only queried.
#[derive(Debug, Default)]
pub struct Index {
    /// One R-tree per layer, keyed by [`LayerId::raw`].
    trees: HashMap<u16, RTree<Spatial>>,
}

impl Index {
    /// Build the per-layer R-trees from a fully-populated scene. Every entity is
    /// turned into a [`Spatial`] on its layer and the trees are `bulk_load`ed
    /// (the fast, balanced construction path). O(n log n).
    pub fn build(scene: &Scene) -> Self {
        // Collect payloads per layer, then bulk-load each tree once.
        let mut by_layer: HashMap<u16, Vec<Spatial>> = HashMap::new();
        let mut push = |layer: LayerId, entity: EntityRef, bbox: Aabb| {
            if bbox.is_empty() {
                return;
            }
            by_layer
                .entry(layer.raw())
                .or_default()
                .push(Spatial { entity, layer, bbox });
        };

        for (i, c) in scene.components.iter().enumerate() {
            push(c.layer, EntityRef::component((i as u32).into()), c.bbox);
        }
        for (i, p) in scene.pads.iter().enumerate() {
            push(p.layer, EntityRef::pad((i as u32).into()), pad_bbox(p.position, p.size));
        }
        for (i, w) in scene.wires.iter().enumerate() {
            push(
                LayerId::SCHEMATIC,
                EntityRef::wire((i as u32).into()),
                segment_bbox(w.a, w.b),
            );
        }
        for (i, j) in scene.junctions.iter().enumerate() {
            push(
                LayerId::SCHEMATIC,
                EntityRef::junction((i as u32).into()),
                point_bbox(j.position),
            );
        }
        for (i, l) in scene.labels.iter().enumerate() {
            push(
                LayerId::SCHEMATIC,
                EntityRef::label((i as u32).into()),
                point_bbox(l.position),
            );
        }
        for (i, s) in scene.sheets.iter().enumerate() {
            push(LayerId::SCHEMATIC, EntityRef::sheet((i as u32).into()), s.bbox);
        }
        for (i, t) in scene.tracks.iter().enumerate() {
            push(
                t.layer,
                EntityRef::track((i as u32).into()),
                track_bbox(t.a, t.b, t.width),
            );
        }
        for (i, v) in scene.vias.iter().enumerate() {
            // A via spans layers; index it on every layer in its span so a
            // hit-test on any of those layers finds it.
            let bb = disc_bbox(v.position, v.diameter);
            let (lo, hi) = layer_span(v.layer_from, v.layer_to);
            for raw in lo..=hi {
                push(LayerId::new(raw), EntityRef::via((i as u32).into()), bb);
            }
        }
        for (i, z) in scene.zones.iter().enumerate() {
            push(z.layer, EntityRef::zone((i as u32).into()), z.bbox);
        }

        let trees = by_layer
            .into_iter()
            .map(|(raw, payloads)| (raw, RTree::bulk_load(payloads)))
            .collect();
        Index { trees }
    }

    /// Total payload count across every layer tree (a via counted once per layer
    /// it spans). Mostly for tests / import benchmarks.
    pub fn len(&self) -> usize {
        self.trees.values().map(|t| t.size()).sum()
    }

    /// True when the index holds no entities.
    pub fn is_empty(&self) -> bool {
        self.trees.values().all(|t| t.size() == 0)
    }

    /// Number of distinct layers indexed.
    pub fn layer_count(&self) -> usize {
        self.trees.len()
    }

    /// Topmost entity at `point`, searched in the given layer-priority order
    /// (SPEC §3). The first layer with any containing box wins; within it the
    /// tightest (smallest-area) box wins, tiebroken by [`kind_rank`] so a pad is
    /// selected over the track beneath it. Returns `None` if nothing is hit on
    /// any of the requested layers.
    pub fn hit_test(&self, point: Point, layers: &[LayerId]) -> Option<EntityRef> {
        let p = [point.x, point.y];
        for layer in layers {
            let Some(tree) = self.trees.get(&layer.raw()) else {
                continue;
            };
            let best = tree
                .locate_all_at_point(&p)
                .min_by(|a, b| {
                    a.area()
                        .partial_cmp(&b.area())
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then(kind_rank(a.entity.kind).cmp(&kind_rank(b.entity.kind)))
                        .then(a.entity.index.cmp(&b.entity.index))
                });
            if let Some(s) = best {
                return Some(s.entity);
            }
        }
        None
    }

    /// Convenience: hit-test against every indexed layer at once, in ascending
    /// layer order (used when the caller has no explicit layer priority, e.g. a
    /// schematic with one logical layer).
    pub fn hit_test_any(&self, point: Point) -> Option<EntityRef> {
        let mut layers: Vec<LayerId> = self.trees.keys().map(|r| LayerId::new(*r)).collect();
        layers.sort_by_key(|l| l.raw());
        self.hit_test(point, &layers)
    }

    /// Every entity on `layer` whose box intersects `rect` — the visible set for
    /// viewport culling (SPEC §2). Order is unspecified (R-tree traversal order).
    /// Returns an empty `Vec` if the layer is not indexed or `rect` is empty.
    ///
    /// Returns an owned `Vec` (not a borrowed iterator) so callers can collect
    /// the visible set without juggling the R-tree's envelope-borrow lifetime;
    /// the visible set is small relative to the scene after culling.
    pub fn query_rect(&self, rect: Aabb, layer: LayerId) -> Vec<EntityRef> {
        if rect.is_empty() {
            return Vec::new();
        }
        let (min, max) = rect.corners();
        let envelope = AABB::from_corners(min, max);
        match self.trees.get(&layer.raw()) {
            Some(tree) => tree
                .locate_in_envelope_intersecting(&envelope)
                .map(|s| s.entity)
                .collect(),
            None => Vec::new(),
        }
    }

    /// Like [`Index::query_rect`] but across every indexed layer — the full
    /// visible set regardless of layer (used for "fit all" / schematic culling).
    pub fn query_rect_all(&self, rect: Aabb) -> Vec<EntityRef> {
        if rect.is_empty() {
            return Vec::new();
        }
        let (min, max) = rect.corners();
        let envelope = AABB::from_corners(min, max);
        let mut out = Vec::new();
        for tree in self.trees.values() {
            out.extend(tree.locate_in_envelope_intersecting(&envelope).map(|s| s.entity));
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Per-class AABB derivation. Each entity is reduced to a scene-space box. Point-
// like entities (pads with no size, junctions, labels) get a small inflation so
// they remain hittable; zero-area boxes are otherwise un-queryable by a point.
// ---------------------------------------------------------------------------

/// A small box centred on a point — for junctions, labels, and pin points.
#[inline]
fn point_bbox(p: Point) -> Aabb {
    Aabb::new(
        Point::new(p.x - HIT_PAD_MM, p.y - HIT_PAD_MM),
        Point::new(p.x + HIT_PAD_MM, p.y + HIT_PAD_MM),
    )
}

/// A pad's box from its centre + (w, h) size, half-extents about the centre.
/// A degenerate (0,0) size falls back to a hittable point box.
#[inline]
fn pad_bbox(centre: Point, size: (f64, f64)) -> Aabb {
    let hw = (size.0.abs() * 0.5).max(HIT_PAD_MM);
    let hh = (size.1.abs() * 0.5).max(HIT_PAD_MM);
    Aabb::new(
        Point::new(centre.x - hw, centre.y - hh),
        Point::new(centre.x + hw, centre.y + hh),
    )
}

/// A wire/segment's box: the bbox of its two endpoints, inflated so a thin
/// horizontal/vertical segment still has area to be hit.
#[inline]
fn segment_bbox(a: Point, b: Point) -> Aabb {
    let mut bb = Aabb::EMPTY;
    bb.expand_point(a);
    bb.expand_point(b);
    inflate(bb, HIT_PAD_MM)
}

/// A track's box: the segment bbox inflated by the half-width (the capsule
/// radius), so a click within the copper width hits the track.
#[inline]
fn track_bbox(a: Point, b: Point, width: f64) -> Aabb {
    let mut bb = Aabb::EMPTY;
    bb.expand_point(a);
    bb.expand_point(b);
    inflate(bb, (width.abs() * 0.5).max(HIT_PAD_MM))
}

/// A disc (via / circular pad) box from centre + outer diameter.
#[inline]
fn disc_bbox(centre: Point, diameter: f64) -> Aabb {
    let r = (diameter.abs() * 0.5).max(HIT_PAD_MM);
    Aabb::new(
        Point::new(centre.x - r, centre.y - r),
        Point::new(centre.x + r, centre.y + r),
    )
}

/// Grow a box outward by `m` on every side (no-op on an empty box).
#[inline]
fn inflate(bb: Aabb, m: f64) -> Aabb {
    if bb.is_empty() {
        return bb;
    }
    Aabb::new(
        Point::new(bb.min.x - m, bb.min.y - m),
        Point::new(bb.max.x + m, bb.max.y + m),
    )
}

/// Normalize a via's `(layer_from, layer_to)` into an inclusive `(lo, hi)` raw
/// range, tolerating either ordering.
#[inline]
fn layer_span(from: LayerId, to: LayerId) -> (u16, u16) {
    let a = from.raw();
    let b = to.raw();
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::NetId;
    use crate::scene::{
        Component, Junction, Pad, PadShape, PinType, SceneKind, Track, Via, Wire,
    };

    fn schem() -> Scene {
        Scene::new(SceneKind::Schematic)
    }

    #[test]
    fn pad_bbox_is_centred_half_extents() {
        let bb = pad_bbox(Point::new(10.0, 20.0), (2.0, 4.0));
        assert!((bb.min.x - 9.0).abs() < 1e-9, "{:?}", bb);
        assert!((bb.max.x - 11.0).abs() < 1e-9, "{:?}", bb);
        assert!((bb.min.y - 18.0).abs() < 1e-9, "{:?}", bb);
        assert!((bb.max.y - 22.0).abs() < 1e-9, "{:?}", bb);
    }

    #[test]
    fn point_query_returns_the_pad_at_that_point() {
        // SPEC §3: a point query returns the right entity. Place two pads; a
        // query on the first must return the first.
        let mut s = schem();
        s.pads.push(Pad {
            component: 0u32.into(),
            name: "1".into(),
            position: Point::new(0.0, 0.0),
            size: (1.0, 1.0),
            shape: PadShape::Rect,
            pin_type: PinType::Passive,
            layer: LayerId::SCHEMATIC,
            net_id: NetId::NONE,
        });
        s.pads.push(Pad {
            component: 0u32.into(),
            name: "2".into(),
            position: Point::new(50.0, 50.0),
            size: (1.0, 1.0),
            shape: PadShape::Rect,
            pin_type: PinType::Passive,
            layer: LayerId::SCHEMATIC,
            net_id: NetId::NONE,
        });
        s.init_flags();
        let idx = Index::build(&s);

        let hit = idx.hit_test(Point::new(0.1, -0.1), &[LayerId::SCHEMATIC]).unwrap();
        assert_eq!(hit.kind, EntityKind::Pad);
        assert_eq!(hit.index, 0);

        let hit2 = idx.hit_test(Point::new(50.0, 50.0), &[LayerId::SCHEMATIC]).unwrap();
        assert_eq!(hit2.index, 1);

        // A miss far from any entity.
        assert!(idx.hit_test(Point::new(999.0, 999.0), &[LayerId::SCHEMATIC]).is_none());
    }

    #[test]
    fn pad_wins_over_track_underneath() {
        // A pad sitting on a track: the tighter pad box should win the hit.
        let mut s = Scene::new(SceneKind::Pcb);
        let cu = LayerId::new(0);
        s.tracks.push(Track {
            a: Point::new(-10.0, 0.0),
            b: Point::new(10.0, 0.0),
            width: 0.5,
            layer: cu,
            net_id: NetId::new(1),
        });
        s.pads.push(Pad {
            component: 0u32.into(),
            name: "A".into(),
            position: Point::new(0.0, 0.0),
            size: (0.6, 0.6),
            shape: PadShape::Circle,
            pin_type: PinType::Passive,
            layer: cu,
            net_id: NetId::new(1),
        });
        s.init_flags();
        let idx = Index::build(&s);
        let hit = idx.hit_test(Point::new(0.0, 0.0), &[cu]).unwrap();
        assert_eq!(hit.kind, EntityKind::Pad, "pad should win over the track it sits on");
    }

    #[test]
    fn layer_priority_is_honoured() {
        // Two pads at the same coordinate on different layers; the first layer in
        // priority order should win.
        let mut s = Scene::new(SceneKind::Pcb);
        let front = LayerId::new(0);
        let back = LayerId::new(31);
        s.pads.push(Pad {
            component: 0u32.into(),
            name: "F".into(),
            position: Point::new(5.0, 5.0),
            size: (1.0, 1.0),
            shape: PadShape::Rect,
            pin_type: PinType::Passive,
            layer: front,
            net_id: NetId::NONE,
        });
        s.pads.push(Pad {
            component: 0u32.into(),
            name: "B".into(),
            position: Point::new(5.0, 5.0),
            size: (1.0, 1.0),
            shape: PadShape::Rect,
            pin_type: PinType::Passive,
            layer: back,
            net_id: NetId::NONE,
        });
        s.init_flags();
        let idx = Index::build(&s);

        // Back first → back pad (index 1).
        assert_eq!(idx.hit_test(Point::new(5.0, 5.0), &[back, front]).unwrap().index, 1);
        // Front first → front pad (index 0).
        assert_eq!(idx.hit_test(Point::new(5.0, 5.0), &[front, back]).unwrap().index, 0);
    }

    #[test]
    fn query_rect_returns_entities_in_window() {
        let mut s = schem();
        for i in 0..5 {
            s.junctions.push(Junction {
                position: Point::new(i as f64 * 10.0, 0.0),
                net_id: NetId::NONE,
            });
        }
        s.init_flags();
        let idx = Index::build(&s);

        // A window covering x in [-1, 21] catches junctions at 0, 10, 20.
        let rect = Aabb::new(Point::new(-1.0, -1.0), Point::new(21.0, 1.0));
        let mut got: Vec<u32> = idx
            .query_rect(rect, LayerId::SCHEMATIC)
            .iter()
            .map(|e| e.index)
            .collect();
        got.sort_unstable();
        assert_eq!(got, vec![0, 1, 2]);

        // An empty rect yields nothing.
        assert_eq!(idx.query_rect(Aabb::EMPTY, LayerId::SCHEMATIC).len(), 0);
    }

    #[test]
    fn via_indexed_on_every_spanned_layer() {
        let mut s = Scene::new(SceneKind::Pcb);
        s.vias.push(Via {
            position: Point::new(1.0, 1.0),
            diameter: 0.8,
            drill: 0.4,
            layer_from: LayerId::new(0),
            layer_to: LayerId::new(2),
            net_id: NetId::new(1),
        });
        s.init_flags();
        let idx = Index::build(&s);
        for raw in 0u16..=2 {
            let hit = idx.hit_test(Point::new(1.0, 1.0), &[LayerId::new(raw)]);
            assert!(hit.is_some(), "via must be hittable on layer {raw}");
            assert_eq!(hit.unwrap().kind, EntityKind::Via);
        }
        // Outside the span: nothing.
        assert!(idx.hit_test(Point::new(1.0, 1.0), &[LayerId::new(5)]).is_none());
    }

    #[test]
    fn empty_scene_builds_empty_index() {
        let s = schem();
        let idx = Index::build(&s);
        assert!(idx.is_empty());
        assert_eq!(idx.len(), 0);
        assert!(idx.hit_test(Point::new(0.0, 0.0), &[LayerId::SCHEMATIC]).is_none());
    }

    #[test]
    fn component_box_is_hittable() {
        let mut s = schem();
        s.components.push(Component {
            reference: "U1".into(),
            value: "MCU".into(),
            lib_id: "MCU:Foo".into(),
            position: Point::new(0.0, 0.0),
            rotation: 0.0,
            mirror: false,
            bbox: Aabb::new(Point::new(-5.0, -5.0), Point::new(5.0, 5.0)),
            layer: LayerId::SCHEMATIC,
        });
        s.init_flags();
        let idx = Index::build(&s);
        let hit = idx.hit_test(Point::new(0.0, 0.0), &[LayerId::SCHEMATIC]).unwrap();
        assert_eq!(hit.kind, EntityKind::Component);
    }

    #[test]
    fn wire_segment_is_hittable_along_its_length() {
        let mut s = schem();
        s.wires.push(Wire {
            a: Point::new(0.0, 0.0),
            b: Point::new(20.0, 0.0),
            net_id: NetId::NONE,
        });
        s.init_flags();
        let idx = Index::build(&s);
        let hit = idx.hit_test(Point::new(10.0, 0.0), &[LayerId::SCHEMATIC]).unwrap();
        assert_eq!(hit.kind, EntityKind::Wire);
    }
}
