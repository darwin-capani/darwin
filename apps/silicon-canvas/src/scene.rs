//! The struct-of-arrays scene (SPEC §1, "Scene").
//!
//! Geometry is stored as flat, typed, parallel arrays — one array per entity
//! class — never per-entity heap objects. An id is an index into its class's
//! array ([`crate::ids`]). Each connectable class carries a `net_id` parallel
//! array so net highlighting is a flag write keyed by index, never a graph walk
//! (SPEC §2: "selection changes write flags, no geometry touched").
//!
//! Layout rationale (why SoA, not Vec<Entity>):
//!   - The renderer uploads each array once into a GPU instance buffer; pan/zoom
//!     is a uniform, not a rebuild (SPEC §2). A `&[PadInstanceFields]` maps
//!     straight to an instance buffer; an array of fat enums would not.
//!   - The per-entity 1-byte highlight flag lives in its own parallel array
//!     ([`Scene::component_flags`] etc.) so a net highlight is a byte-buffer
//!     write of the same length, uploaded as a GPU flag buffer.
//!   - Cache-friendly import/graph passes iterate one field across all entities.
//!
//! `coord` units: KiCad internal coordinates are nanometres for `.kicad_pcb` and
//! 1/10000-inch ("mils*10") historically, but KiCad 7/8 S-expr emits millimetres
//! as floating point. The scene stores positions as `f64` millimetres (SPEC §3
//! requires double-precision view math at deep zoom); the renderer downcasts to
//! f32 per-frame after applying the f64 view transform.
//!
//! This module is the CONTRACT. Downstream module agents fill these arrays
//! (parser), index them (rtree), walk them (graph/trace), inspect them (erc),
//! and render them (render) — but must NOT change the types here.

use serde::{Deserialize, Serialize};

use crate::ids::{
    ComponentId, EntityRef, JunctionId, LabelId, NetId, PadId, SheetId, TrackId, ViaId, WireId,
    ZoneId,
};

/// Quantization grid for endpoint matching, in the same f64-millimetre units as
/// [`Point`]. The connectivity graph (SPEC §1) matches wire/pin endpoints by
/// snapping each coordinate to this grid before comparing, so two endpoints that
/// are "the same point" up to float noise hash equal. 1e-4 mm = 100 nm, far
/// finer than any real KiCad placement grid (the smallest schematic grid is
/// 0.0254 mm) yet coarse enough to absorb f64 round-trip error from the parser.
pub const QUANTUM_MM: f64 = 1.0e-4;

/// A 2-D point in scene space (f64 millimetres). f64 is mandatory: SPEC §3 needs
/// double-precision view math across the 0.01x–500x zoom range on large boards,
/// where f32 loses sub-pixel precision.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    #[inline]
    pub const fn new(x: f64, y: f64) -> Self {
        Point { x, y }
    }

    pub const ORIGIN: Point = Point { x: 0.0, y: 0.0 };

    /// Quantize each coordinate to the [`QUANTUM_MM`] grid, returning the
    /// integer grid cell as `(i64, i64)`. Two points that should be considered
    /// the same electrical contact produce the same key (SPEC §1: "position-
    /// quantized endpoint matching for schematics"). This is the canonical key
    /// the connectivity graph and the parser MUST use for endpoint matching, so
    /// every agent quantizes identically.
    #[inline]
    pub fn quantize(self) -> QuantKey {
        QuantKey {
            x: (self.x / QUANTUM_MM).round() as i64,
            y: (self.y / QUANTUM_MM).round() as i64,
        }
    }

    /// Euclidean distance to another point (millimetres).
    #[inline]
    pub fn distance(self, other: Point) -> f64 {
        let dx = self.x - other.x;
        let dy = self.y - other.y;
        (dx * dx + dy * dy).sqrt()
    }
}

/// The integer grid cell a [`Point`] quantizes to — the hashable, `Eq` endpoint-
/// matching key. Returned by [`Point::quantize`]. Use this (never raw `Point`
/// equality) as a `HashMap` key when grouping coincident endpoints into nets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct QuantKey {
    pub x: i64,
    pub y: i64,
}

/// An axis-aligned bounding box in scene space (f64 mm). Used for viewport
/// fitting (`view.set {fit}`), per-entity extents handed to the R-tree, and
/// "fit all" framing. `min`/`max` are inclusive corners.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Aabb {
    pub min: Point,
    pub max: Point,
}

impl Default for Aabb {
    /// The empty (inverted-extremes) box — the right default for a fresh
    /// [`Scene::bounds`] before any geometry is added.
    fn default() -> Self {
        Aabb::EMPTY
    }
}

impl Aabb {
    /// An empty box (inverted extremes) that absorbs the first point exactly.
    pub const EMPTY: Aabb = Aabb {
        min: Point { x: f64::INFINITY, y: f64::INFINITY },
        max: Point { x: f64::NEG_INFINITY, y: f64::NEG_INFINITY },
    };

    #[inline]
    pub fn new(min: Point, max: Point) -> Self {
        Aabb { min, max }
    }

    /// Grow to include a point.
    #[inline]
    pub fn expand_point(&mut self, p: Point) {
        self.min.x = self.min.x.min(p.x);
        self.min.y = self.min.y.min(p.y);
        self.max.x = self.max.x.max(p.x);
        self.max.y = self.max.y.max(p.y);
    }

    /// Grow to include another box.
    #[inline]
    pub fn expand_aabb(&mut self, other: Aabb) {
        if other.is_empty() {
            return;
        }
        self.expand_point(other.min);
        self.expand_point(other.max);
    }

    /// True when no point has been added (the inverted-extremes sentinel).
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.min.x > self.max.x || self.min.y > self.max.y
    }

    #[inline]
    pub fn width(&self) -> f64 {
        (self.max.x - self.min.x).max(0.0)
    }

    #[inline]
    pub fn height(&self) -> f64 {
        (self.max.y - self.min.y).max(0.0)
    }

    #[inline]
    pub fn center(&self) -> Point {
        Point::new(
            (self.min.x + self.max.x) * 0.5,
            (self.min.y + self.max.y) * 0.5,
        )
    }

    /// As an `rstar`-friendly `[[f64; 2]; 2]` (min corner, max corner) for
    /// building `rstar::AABB` payloads without depending on `rstar` here.
    #[inline]
    pub fn corners(&self) -> ([f64; 2], [f64; 2]) {
        ([self.min.x, self.min.y], [self.max.x, self.max.y])
    }
}

/// The document class a scene was imported from: a schematic (`.kicad_sch`) or a
/// board (`.kicad_pcb`). Selects which entity arrays are populated and which
/// connectivity rule the graph uses (endpoint-quantize for schematics, copper
/// overlap + via stitching for PCBs — SPEC §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentKind {
    Schematic,
    Pcb,
}

/// A copper / fabrication layer (PCB) or the single logical schematic layer.
/// Layer is a small id so per-layer R-trees, per-layer instanced pipelines, and
/// `layer.set {layer, visible}` (SPEC §6) all key off the same `u16`. The string
/// name (e.g. "F.Cu", "B.Cu", "In1.Cu") is kept in [`Scene::layer_names`] so the
/// op surface can address layers by KiCad name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
#[repr(transparent)]
pub struct LayerId(pub u16);

impl LayerId {
    /// The single logical layer a schematic lives on.
    pub const SCHEMATIC: LayerId = LayerId(0);

    #[inline]
    pub const fn new(raw: u16) -> Self {
        LayerId(raw)
    }
    #[inline]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
    #[inline]
    pub const fn raw(self) -> u16 {
        self.0
    }
}

/// Pad/pin geometric shape — drawn as an SDF quad instance (SPEC §2). The
/// renderer picks the fragment branch from this discriminant; the graph treats
/// every shape identically (the connection point is the pad position).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PadShape {
    Circle,
    Rect,
    RoundRect,
    Oval,
}

/// KiCad pin electrical type — the metadata ERC reasons over (SPEC §5:
/// output↔output conflicts, power pins without a driver, NC handling). Parsed
/// from the symbol library pin definitions. Conservative: ERC only flags what is
/// provable from these types + the netlist, nothing heuristic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PinType {
    Input,
    Output,
    Bidirectional,
    TriState,
    Passive,
    Free,
    Unspecified,
    PowerIn,
    PowerOut,
    OpenCollector,
    OpenEmitter,
    /// Not-connected: ERC must NOT flag an unconnected NC pin.
    NoConnect,
}

// ===========================================================================
// Per-class field structs (one element of each SoA array).
//
// Each `*Fields` struct is ONE row of its parallel array. Keeping them as named
// structs (rather than N separate Vecs per field) is the pragmatic middle: the
// arrays stay struct-of-arrays at the CLASS level (pads separate from wires) for
// GPU upload and class-local iteration, while a single class's row is a value.
// The hot render path reads the geometry fields; the highlight flag lives in a
// SEPARATE parallel byte array (Scene::*_flags) so a highlight write never
// touches these structs.
// ===========================================================================

/// One placed component / symbol instance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Component {
    /// Reference designator, e.g. "R1", "U3". Empty if unannotated.
    pub reference: String,
    /// Value, e.g. "10k", "STM32F405". Empty if none.
    pub value: String,
    /// Symbol/footprint library id, e.g. "Device:R". Empty if unknown.
    pub lib_id: String,
    /// Placement anchor (origin) in scene space.
    pub position: Point,
    /// Rotation in degrees (KiCad uses 0/90/180/270 for schematics).
    pub rotation: f64,
    /// Whether the symbol is mirrored about its Y axis.
    pub mirror: bool,
    /// Tight bounding box of the symbol's drawn body, for fit + culling.
    pub bbox: Aabb,
    /// Layer this component sits on (board side for footprints; SCHEMATIC for
    /// schematic symbols).
    pub layer: LayerId,
}

/// One pad (PCB) or pin (schematic) — the connectable terminal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pad {
    /// Owning component.
    pub component: ComponentId,
    /// Pad name / pin number, e.g. "1", "A7", "GND".
    pub name: String,
    /// Electrical contact point (scene space) — the key the graph quantizes.
    pub position: Point,
    /// Drawn size (width, height) in mm; interpreted per `shape`.
    pub size: (f64, f64),
    pub shape: PadShape,
    /// Pin electrical type (drives ERC). For PCB pads with no schematic
    /// counterpart this is `Passive`.
    pub pin_type: PinType,
    /// Layer the pad is on (the via/copper layer for PCB; SCHEMATIC otherwise).
    pub layer: LayerId,
    /// The net this pad belongs to; [`NetId::NONE`] when unconnected.
    pub net_id: NetId,
}

/// One straight wire segment (schematic).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Wire {
    pub a: Point,
    pub b: Point,
    pub net_id: NetId,
}

/// One junction dot (schematic) — an explicit connection of crossing wires.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Junction {
    pub position: Point,
    pub net_id: NetId,
}

/// The role a text [`Label`] plays — selects ERC handling (single-ended named
/// nets / label typos, SPEC §5) and the glyph styling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LabelKind {
    /// Local net label.
    Local,
    /// Global net label (matches across sheets).
    Global,
    /// Hierarchical label (sheet boundary port).
    Hierarchical,
    /// Reference designator text (e.g. "R1").
    Reference,
    /// Value text.
    Value,
    /// Free text annotation.
    Text,
}

/// One text / net-label annotation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Label {
    pub text: String,
    pub position: Point,
    pub rotation: f64,
    pub kind: LabelKind,
    /// Net this label names; [`NetId::NONE`] for non-net text (Reference/Value/
    /// Text).
    pub net_id: NetId,
}

/// One hierarchical sheet symbol (schematic).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sheet {
    pub name: String,
    /// Sub-sheet file name (relative path inside the project).
    pub file: String,
    /// Sheet rectangle.
    pub bbox: Aabb,
}

/// One copper track segment (PCB).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub a: Point,
    pub b: Point,
    /// Track width (mm) — also the SDF capsule radius basis.
    pub width: f64,
    pub layer: LayerId,
    pub net_id: NetId,
}

/// One via (PCB) — a layer transition. Drawn as an SDF ring (SPEC §2); the trace
/// walker flashes it on a cross-layer step (SPEC §4).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Via {
    pub position: Point,
    /// Outer (pad) diameter and drill diameter (mm).
    pub diameter: f64,
    pub drill: f64,
    /// Layer span the via stitches (inclusive). For a through via this is the
    /// full stackup; equality means a micro/blind via on adjacent layers.
    pub layer_from: LayerId,
    pub layer_to: LayerId,
    pub net_id: NetId,
}

/// One copper zone / filled polygon (PCB). The outline is tessellated ONCE at
/// import via `lyon` and cached in `state/tmp/silicon-canvas/` keyed by file
/// hash (SPEC §2); the scene keeps the outline points (the tessellation lives in
/// the render module / cache, not here).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Zone {
    /// Outline polygon vertices (scene space), in order.
    pub outline: Vec<Point>,
    pub layer: LayerId,
    pub net_id: NetId,
    pub bbox: Aabb,
}

/// Highlight / selection state for one entity, stored in a parallel 1-byte
/// array (SPEC §2: "per-entity 1-byte flag buffer"). A net highlight is a byte
/// write across the flag array + a redraw — no geometry touched. `repr(u8)` so
/// the array is uploadable straight to the GPU flag buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
#[derive(Default)]
pub enum HighlightFlag {
    /// Normal: drawn at full intensity, no emphasis.
    #[default]
    Normal = 0,
    /// On the selected net: drawn in `--holo-bright` (SPEC §4).
    Highlighted = 1,
    /// Off the selected net: dimmed to 25% (SPEC §4).
    Dimmed = 2,
    /// The active trace front node this step (SPEC §4 trace mode).
    TraceFront = 3,
}


// ===========================================================================
// The Scene: struct-of-arrays.
// ===========================================================================

/// The whole imported document as struct-of-arrays. Every `Vec` is one entity
/// class; an id ([`crate::ids`]) indexes its class's `Vec`. The `*_flags` arrays
/// are parallel to their geometry arrays (same length, same index space) and
/// hold the per-entity [`HighlightFlag`].
///
/// Built once by the parser; the R-tree (`rtree`) and connectivity graph
/// (`graph`) index it; thereafter geometry is immutable and only the `*_flags`
/// arrays and the selection mutate (SPEC §1: "built once at import, immutable
/// afterward").
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Scene {
    pub kind: SceneKind,

    // --- entity arrays (one per class) ---------------------------------
    pub components: Vec<Component>,
    pub pads: Vec<Pad>,
    pub wires: Vec<Wire>,
    pub junctions: Vec<Junction>,
    pub labels: Vec<Label>,
    pub sheets: Vec<Sheet>,
    pub tracks: Vec<Track>,
    pub vias: Vec<Via>,
    pub zones: Vec<Zone>,

    // --- parallel highlight-flag arrays (same length as their class) ----
    pub component_flags: Vec<HighlightFlag>,
    pub pad_flags: Vec<HighlightFlag>,
    pub wire_flags: Vec<HighlightFlag>,
    pub junction_flags: Vec<HighlightFlag>,
    pub label_flags: Vec<HighlightFlag>,
    pub track_flags: Vec<HighlightFlag>,
    pub via_flags: Vec<HighlightFlag>,
    pub zone_flags: Vec<HighlightFlag>,

    // --- net + layer metadata ------------------------------------------
    /// Net name by [`NetId`] index, e.g. "GND", "3V3", "/USB_DP". The empty
    /// string at index 0 is conventionally the no-net / unnamed net.
    pub net_names: Vec<String>,
    /// Layer name by [`LayerId`] index, e.g. "F.Cu". For a schematic this holds
    /// the single logical layer.
    pub layer_names: Vec<String>,

    /// Overall document bounds, used for `view.set {fit:"all"}`.
    pub bounds: Aabb,
}

/// Whether the scene is a schematic or a board. A `Default` scene is an empty
/// schematic so `Scene::default()` is valid before any import.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum SceneKind {
    #[default]
    Schematic,
    Pcb,
}


impl From<DocumentKind> for SceneKind {
    fn from(d: DocumentKind) -> Self {
        match d {
            DocumentKind::Schematic => SceneKind::Schematic,
            DocumentKind::Pcb => SceneKind::Pcb,
        }
    }
}

impl Scene {
    /// An empty schematic scene.
    pub fn new(kind: SceneKind) -> Self {
        Scene {
            kind,
            ..Default::default()
        }
    }

    /// Total entity count across every class — used by `canvas.selection`
    /// reporting and import benchmarks.
    pub fn entity_count(&self) -> usize {
        self.components.len()
            + self.pads.len()
            + self.wires.len()
            + self.junctions.len()
            + self.labels.len()
            + self.sheets.len()
            + self.tracks.len()
            + self.vias.len()
            + self.zones.len()
    }

    /// The net name for a [`NetId`], or `""` for [`NetId::NONE`] / out of range.
    pub fn net_name(&self, net: NetId) -> &str {
        if net.is_none() {
            return "";
        }
        self.net_names.get(net.index()).map(String::as_str).unwrap_or("")
    }

    /// Look up a [`NetId`] by name (linear scan; nets are few). Returns
    /// [`NetId::NONE`] when not found. Used by `select.net {name}` (SPEC §6).
    pub fn net_by_name(&self, name: &str) -> NetId {
        self.net_names
            .iter()
            .position(|n| n == name)
            .map(|i| NetId::new(i as u32))
            .unwrap_or(NetId::NONE)
    }

    /// Look up a [`ComponentId`] by reference designator. Returns `None` when no
    /// component has that reference. Used by `select.component {name}`.
    pub fn component_by_reference(&self, reference: &str) -> Option<ComponentId> {
        self.components
            .iter()
            .position(|c| c.reference == reference)
            .map(|i| ComponentId::new(i as u32))
    }

    /// Resize every `*_flags` array to match its geometry array, filling with
    /// [`HighlightFlag::Normal`]. The parser calls this once after populating the
    /// geometry so the flag arrays are parallel and index-aligned.
    pub fn init_flags(&mut self) {
        self.component_flags = vec![HighlightFlag::Normal; self.components.len()];
        self.pad_flags = vec![HighlightFlag::Normal; self.pads.len()];
        self.wire_flags = vec![HighlightFlag::Normal; self.wires.len()];
        self.junction_flags = vec![HighlightFlag::Normal; self.junctions.len()];
        self.label_flags = vec![HighlightFlag::Normal; self.labels.len()];
        self.track_flags = vec![HighlightFlag::Normal; self.tracks.len()];
        self.via_flags = vec![HighlightFlag::Normal; self.vias.len()];
        self.zone_flags = vec![HighlightFlag::Normal; self.zones.len()];
    }

    /// Reset every highlight flag to [`HighlightFlag::Normal`] (clears a
    /// selection without touching geometry).
    pub fn clear_highlights(&mut self) {
        for f in &mut self.component_flags {
            *f = HighlightFlag::Normal;
        }
        for f in &mut self.pad_flags {
            *f = HighlightFlag::Normal;
        }
        for f in &mut self.wire_flags {
            *f = HighlightFlag::Normal;
        }
        for f in &mut self.junction_flags {
            *f = HighlightFlag::Normal;
        }
        for f in &mut self.label_flags {
            *f = HighlightFlag::Normal;
        }
        for f in &mut self.track_flags {
            *f = HighlightFlag::Normal;
        }
        for f in &mut self.via_flags {
            *f = HighlightFlag::Normal;
        }
        for f in &mut self.zone_flags {
            *f = HighlightFlag::Normal;
        }
    }

    /// The `net_id` an entity carries, or [`NetId::NONE`] for classes that carry
    /// no net (components, sheets). The single place that maps an [`EntityRef`]
    /// to its net — used by hit-test → highlight (click a pad, highlight its
    /// net; SPEC §4) so every agent resolves nets the same way.
    pub fn entity_net(&self, e: EntityRef) -> NetId {
        use crate::ids::EntityKind::*;
        match e.kind {
            Pad => self.pads.get(e.idx()).map(|p| p.net_id).unwrap_or(NetId::NONE),
            Wire => self.wires.get(e.idx()).map(|w| w.net_id).unwrap_or(NetId::NONE),
            Junction => self
                .junctions
                .get(e.idx())
                .map(|j| j.net_id)
                .unwrap_or(NetId::NONE),
            Label => self.labels.get(e.idx()).map(|l| l.net_id).unwrap_or(NetId::NONE),
            Track => self.tracks.get(e.idx()).map(|t| t.net_id).unwrap_or(NetId::NONE),
            Via => self.vias.get(e.idx()).map(|v| v.net_id).unwrap_or(NetId::NONE),
            Zone => self.zones.get(e.idx()).map(|z| z.net_id).unwrap_or(NetId::NONE),
            Component | Sheet => NetId::NONE,
        }
    }
}

// Small typed-id helpers so callers can construct ids without importing the id
// module separately when they already have the scene in scope.
impl Scene {
    #[inline]
    pub fn pad_id(i: usize) -> PadId {
        PadId::new(i as u32)
    }
    #[inline]
    pub fn wire_id(i: usize) -> WireId {
        WireId::new(i as u32)
    }
    #[inline]
    pub fn junction_id(i: usize) -> JunctionId {
        JunctionId::new(i as u32)
    }
    #[inline]
    pub fn label_id(i: usize) -> LabelId {
        LabelId::new(i as u32)
    }
    #[inline]
    pub fn sheet_id(i: usize) -> SheetId {
        SheetId::new(i as u32)
    }
    #[inline]
    pub fn track_id(i: usize) -> TrackId {
        TrackId::new(i as u32)
    }
    #[inline]
    pub fn via_id(i: usize) -> ViaId {
        ViaId::new(i as u32)
    }
    #[inline]
    pub fn zone_id(i: usize) -> ZoneId {
        ZoneId::new(i as u32)
    }
    #[inline]
    pub fn component_id(i: usize) -> ComponentId {
        ComponentId::new(i as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantize_collapses_float_noise() {
        // Two endpoints that differ only by float round-trip noise quantize to
        // the same key — this is the contract the graph relies on.
        let a = Point::new(10.0, 20.0);
        let b = Point::new(10.0 + 1.0e-9, 20.0 - 1.0e-9);
        assert_eq!(a.quantize(), b.quantize());
    }

    #[test]
    fn quantize_separates_distinct_points() {
        let a = Point::new(10.0, 20.0);
        // 0.001 mm apart — an order of magnitude above the quantum — must differ.
        let c = Point::new(10.001, 20.0);
        assert_ne!(a.quantize(), c.quantize());
    }

    #[test]
    fn aabb_expand_and_emptiness() {
        let mut bb = Aabb::EMPTY;
        assert!(bb.is_empty());
        bb.expand_point(Point::new(1.0, 2.0));
        bb.expand_point(Point::new(-3.0, 5.0));
        assert!(!bb.is_empty());
        assert_eq!(bb.min, Point::new(-3.0, 2.0));
        assert_eq!(bb.max, Point::new(1.0, 5.0));
        assert_eq!(bb.width(), 4.0);
        assert_eq!(bb.height(), 3.0);
    }

    #[test]
    fn highlight_flag_is_one_byte() {
        // repr(u8): the parallel flag arrays upload straight to a GPU byte buffer.
        assert_eq!(std::mem::size_of::<HighlightFlag>(), 1);
    }

    #[test]
    fn init_flags_parallels_geometry() {
        let mut s = Scene::new(SceneKind::Schematic);
        s.wires.push(Wire { a: Point::ORIGIN, b: Point::new(1.0, 0.0), net_id: NetId::new(0) });
        s.wires.push(Wire { a: Point::new(1.0, 0.0), b: Point::new(2.0, 0.0), net_id: NetId::new(0) });
        s.init_flags();
        assert_eq!(s.wire_flags.len(), s.wires.len());
        assert!(s.wire_flags.iter().all(|f| *f == HighlightFlag::Normal));
    }

    #[test]
    fn net_lookup_roundtrips() {
        let mut s = Scene::new(SceneKind::Schematic);
        s.net_names = vec![String::new(), "3V3".to_string(), "GND".to_string()];
        assert_eq!(s.net_by_name("GND"), NetId::new(2));
        assert_eq!(s.net_name(NetId::new(1)), "3V3");
        assert!(s.net_by_name("missing").is_none());
        assert_eq!(s.net_name(NetId::NONE), "");
    }

    #[test]
    fn entity_net_resolves_per_class() {
        let mut s = Scene::new(SceneKind::Pcb);
        s.tracks.push(Track {
            a: Point::ORIGIN,
            b: Point::new(5.0, 0.0),
            width: 0.25,
            layer: LayerId::new(0),
            net_id: NetId::new(7),
        });
        let r = EntityRef::track(TrackId::new(0));
        assert_eq!(s.entity_net(r), NetId::new(7));
        // A component carries no net.
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
        assert!(s.entity_net(EntityRef::component(ComponentId::new(0))).is_none());
    }
}
