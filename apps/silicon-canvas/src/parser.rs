//! KiCad document parser (SPEC §1).
//!
//! Interprets a generic [`crate::sexpr::Value`] tree (produced by
//! [`crate::sexpr::parse`]) as a KiCad `.kicad_sch` / `.kicad_pcb` document — or a
//! `.kicad_sym` / `.kicad_mod` library — and populates a [`crate::scene::Scene`]
//! struct-of-arrays. Imports are READ-ONLY (Silicon Canvas is a viewer, non-goal
//! §7).
//!
//! Contract honored here (so graph/rtree/erc/render line up):
//!   - Builds the SoA arrays in [`Scene`] and calls [`Scene::init_flags`] before
//!     returning.
//!   - Matches coincident endpoints with [`Point::quantize`] — never raw float
//!     equality (the graph relies on identical quantization).
//!   - Stores positions as f64 millimetres ([`Point`]).
//!   - Populates `net_names` / `layer_names` so id indices resolve; uses
//!     [`NetId::NONE`] for unconnected entities.
//!   - Returns [`CanvasError::Parse`] (with the file path) on a malformed
//!     document; surfaces [`crate::error::SexprError`] via `?`.
//!   - Panic-free on arbitrary input (SPEC §1): every fallible step returns an
//!     error and numeric reads are clamped to sane finite values, so any byte
//!     sequence yields a `Result`, never a panic. This is exercised by the
//!     cargo-fuzz target (`fuzz/fuzz_targets/parse_document.rs`, run with
//!     `cargo +nightly fuzz run parse_document`) and, on stable, by the seeded
//!     deterministic randomized tests `tests::fuzz_parse_document_*` below.
//!
//! ## Net assignment model
//!
//! KiCad does not store a flat net list inside `.kicad_sch`; connectivity is
//! implied by geometry (coincident wire endpoints / pins / junctions) plus
//! same-name labels. This parser computes a *conservative geometric* net
//! assignment that mirrors the connectivity graph's quantize rule (SPEC §1):
//!
//!   1. Each schematic wire endpoint, pin, junction, and label position is
//!      snapped to the [`QUANTUM_MM`] grid ([`Point::quantize`]).
//!   2. A union-find merges quantized points that a wire segment connects (its
//!      two endpoints) and points that share a junction.
//!   3. Local/global/hierarchical labels name the merged group they sit on; a
//!      named group becomes a [`NetId`] with that name, unnamed groups get a
//!      synthetic `Net-N` name.
//!
//! This is intentionally simple (the dedicated `graph` module owns the full
//! BFS-by-electrical-distance walk); it gives the scene enough net metadata for
//! highlighting and ERC without re-implementing the graph here.

use std::collections::HashMap;
use std::path::Path;

use crate::error::{CanvasError, Result};
use crate::ids::{ComponentId, NetId};
use crate::scene::{
    Aabb, Component, Junction, Label, LabelKind, LayerId, Pad, PadShape, PinType, Point, QuantKey,
    Scene, SceneKind, Sheet, Track, Via, Wire, Zone,
};
use crate::sexpr::{self, Value};

// ===========================================================================
// Public entry points
// ===========================================================================

/// Parse a KiCad document or library from its source text, dispatching on the
/// file extension. The path is used both to pick the grammar and to carry
/// diagnostics in [`CanvasError::Parse`].
///
/// Supported extensions: `.kicad_sch`, `.kicad_pcb`, `.kicad_sym`, `.kicad_mod`.
/// Anything else returns [`CanvasError::UnsupportedFileType`].
pub fn parse_document(path: &Path, src: &str) -> Result<Scene> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    let root = sexpr::parse(src)?; // SexprError -> CanvasError via ?
    match ext.as_deref() {
        Some("kicad_sch") => parse_schematic(path, &root),
        Some("kicad_pcb") => parse_pcb(path, &root),
        Some("kicad_sym") => parse_symbol_library(path, &root),
        Some("kicad_mod") => parse_footprint_library(path, &root),
        _ => Err(CanvasError::UnsupportedFileType(path.to_path_buf())),
    }
}

// ===========================================================================
// Geometry helpers — total over any Value (never panic; see the panic-freedom
// note in the module docs).
// ===========================================================================

/// Read the first `n` numeric children (after the head) of a list node, e.g.
/// `(at 100 50 90)` -> `[100.0, 50.0, 90.0]`. Missing trailing values are filled
/// with `0.0`. Non-finite values are coerced to `0.0` so downstream geometry
/// never sees NaN/inf (keeps quantize total).
fn read_nums(node: &Value, n: usize) -> Vec<f64> {
    let mut out = vec![0.0; n];
    if let Some(items) = node.list() {
        // items[0] is the head atom; numeric args follow.
        let mut filled = 0;
        for v in items.iter().skip(1) {
            if filled >= n {
                break;
            }
            if let Some(x) = v.as_f64() {
                out[filled] = if x.is_finite() { x } else { 0.0 };
                filled += 1;
            } else {
                // A non-numeric token (e.g. a layer name in `(at ...)` — KiCad
                // never does this, but stay robust): stop reading numbers.
                break;
            }
        }
    }
    out
}

/// `(at x y [angle])` -> (Point, angle_degrees). Angle defaults to 0.
fn read_at(parent: &Value) -> (Point, f64) {
    match parent.get("at") {
        Some(node) => {
            let v = read_nums(node, 3);
            (Point::new(v[0], v[1]), v[2])
        }
        None => (Point::ORIGIN, 0.0),
    }
}

/// Read a `(layer "F.Cu")` or first layer name from a `(layers ...)` node.
fn read_layer_name(parent: &Value) -> Option<String> {
    if let Some(node) = parent.get("layer") {
        if let Some(items) = node.list() {
            if let Some(name) = items.get(1).and_then(Value::as_str) {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// Apply a 2-D rotation (degrees, KiCad's CCW-positive convention is irrelevant
/// for endpoint matching as long as it is consistent) plus optional Y mirror to a
/// pin/pad offset relative to a symbol origin, then translate to the symbol
/// position. Used to place pins at their absolute scene coordinate.
fn place_offset(origin: Point, angle_deg: f64, mirror: bool, off: Point) -> Point {
    let mut ox = off.x;
    let oy = off.y;
    if mirror {
        ox = -ox;
    }
    let rad = angle_deg.to_radians();
    let (s, c) = rad.sin_cos();
    let rx = ox * c - oy * s;
    let ry = ox * s + oy * c;
    Point::new(origin.x + rx, origin.y + ry)
}

/// Map a KiCad pad shape atom to [`PadShape`].
fn pad_shape(name: &str) -> PadShape {
    match name {
        "circle" => PadShape::Circle,
        "rect" => PadShape::Rect,
        "roundrect" => PadShape::RoundRect,
        "oval" => PadShape::Oval,
        // KiCad also has "trapezoid"/"custom"; render them as rect (closest).
        _ => PadShape::Rect,
    }
}

/// Map a KiCad pin electrical-type atom to [`PinType`]. Defaults to
/// [`PinType::Unspecified`] for unknown tokens (conservative for ERC).
fn pin_type(name: &str) -> PinType {
    match name {
        "input" => PinType::Input,
        "output" => PinType::Output,
        "bidirectional" => PinType::Bidirectional,
        "tri_state" => PinType::TriState,
        "passive" => PinType::Passive,
        "free" => PinType::Free,
        "unspecified" => PinType::Unspecified,
        "power_in" => PinType::PowerIn,
        "power_out" => PinType::PowerOut,
        "open_collector" => PinType::OpenCollector,
        "open_emitter" => PinType::OpenEmitter,
        "no_connect" => PinType::NoConnect,
        _ => PinType::Unspecified,
    }
}

// ===========================================================================
// Union-find over quantized endpoints — geometric net grouping.
// ===========================================================================

/// Disjoint-set over [`QuantKey`] grid cells. Each connectable endpoint maps to a
/// set; wires and junctions merge sets; labels name them.
#[derive(Default)]
struct NetBuilder {
    /// Grid cell -> dense node index.
    index_of: HashMap<QuantKey, usize>,
    /// Union-find parent links (by dense index).
    parent: Vec<usize>,
    /// Best name discovered for each *root* node (filled lazily during finalize).
    /// Index is dense node index, value is the chosen label name if any.
    name_at: Vec<Option<String>>,
}

impl NetBuilder {
    /// Intern a grid cell, returning its dense node index (creating it if new).
    fn node(&mut self, key: QuantKey) -> usize {
        if let Some(&i) = self.index_of.get(&key) {
            return i;
        }
        let i = self.parent.len();
        self.parent.push(i);
        self.name_at.push(None);
        self.index_of.insert(key, i);
        i
    }

    /// Find with path compression. Iterative to stay stack-safe on adversarial
    /// chains.
    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            let gp = self.parent[self.parent[x]];
            self.parent[x] = gp; // path halving
            x = gp;
        }
        x
    }

    fn union(&mut self, a: usize, b: usize) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra != rb {
            self.parent[ra] = rb;
        }
    }

    /// Record a candidate net name at a node's group. The first non-empty name
    /// wins per root (deterministic given parse order); a later label of the same
    /// group is ignored so we do not thrash names.
    fn name(&mut self, node: usize, name: &str) {
        if name.is_empty() {
            return;
        }
        let root = self.find(node);
        if self.name_at[root].is_none() {
            self.name_at[root] = Some(name.to_string());
        }
    }

    /// Resolve every interned grid cell to a final [`NetId`], building the
    /// `net_names` table. Index 0 is reserved for the empty / no-net name so
    /// [`Scene::net_name`] / [`Scene::net_by_name`] behave (the scene convention
    /// is the empty string at index 0).
    ///
    /// Returns `(net_names, key -> NetId)`.
    fn finalize(&mut self) -> (Vec<String>, HashMap<QuantKey, NetId>) {
        let mut net_names: Vec<String> = vec![String::new()]; // index 0 = no-net
        let mut root_to_net: HashMap<usize, NetId> = HashMap::new();
        let mut name_in_use: HashMap<String, NetId> = HashMap::new();
        let mut synthetic = 0u32;

        // Snapshot the interned keys so we can mutate `self` (find) while iterating.
        let keys: Vec<(QuantKey, usize)> =
            self.index_of.iter().map(|(k, &i)| (*k, i)).collect();

        let mut key_to_net: HashMap<QuantKey, NetId> = HashMap::with_capacity(keys.len());
        for (key, node) in keys {
            let root = self.find(node);
            let net = if let Some(&n) = root_to_net.get(&root) {
                n
            } else {
                // Choose a name for this group.
                let chosen = self.name_at[root].clone();
                let net_id = match chosen {
                    Some(name) if !name.is_empty() => {
                        // Merge groups that ended up with the same name (e.g. two
                        // global labels "GND" on disjoint wire islands) onto the
                        // same NetId — this is the one heuristic the SPEC blesses
                        // (global/hierarchical labels connect by name §1).
                        if let Some(&existing) = name_in_use.get(&name) {
                            existing
                        } else {
                            let id = NetId::new(net_names.len() as u32);
                            net_names.push(name.clone());
                            name_in_use.insert(name, id);
                            id
                        }
                    }
                    _ => {
                        let id = NetId::new(net_names.len() as u32);
                        synthetic += 1;
                        net_names.push(format!("Net-{synthetic}"));
                        id
                    }
                };
                root_to_net.insert(root, net_id);
                net_id
            };
            key_to_net.insert(key, net);
        }
        (net_names, key_to_net)
    }
}

// ===========================================================================
// Schematic (.kicad_sch)
// ===========================================================================

/// Parse a `.kicad_sch` root `(kicad_sch ...)` into a schematic [`Scene`].
pub fn parse_schematic(path: &Path, root: &Value) -> Result<Scene> {
    if root.head() != Some("kicad_sch") {
        return Err(CanvasError::parse(
            path,
            format!(
                "expected (kicad_sch ...) root, found {:?}",
                root.head().unwrap_or("<non-list>")
            ),
        ));
    }

    let mut scene = Scene::new(SceneKind::Schematic);
    scene.layer_names = vec!["schematic".to_string()];

    let items = root.list().unwrap_or(&[]);

    let mut nb = NetBuilder::default();

    // --- pass 1: collect raw geometry, register endpoints in the union-find ---

    // Wires (and bus segments treated as wires for geometry).
    struct RawWire {
        a: Point,
        b: Point,
    }
    let mut raw_wires: Vec<RawWire> = Vec::new();
    for w in root.get_all("wire") {
        if let Some(pts) = w.get("pts") {
            let coords = read_pts(pts);
            if coords.len() >= 2 {
                let a = coords[0];
                let b = coords[1];
                let na = nb.node(a.quantize());
                let nb_ = nb.node(b.quantize());
                nb.union(na, nb_);
                raw_wires.push(RawWire { a, b });
            }
        }
    }

    // Junctions.
    let mut raw_junctions: Vec<Point> = Vec::new();
    for j in root.get_all("junction") {
        let (p, _) = read_at(j);
        nb.node(p.quantize());
        raw_junctions.push(p);
    }

    // Symbols -> components + pins. Pins need the symbol library geometry, but a
    // `.kicad_sch` embeds the used symbols under (lib_symbols ...). Build a pin
    // table from there.
    let lib_pins = build_lib_pin_table(root);

    struct RawComp {
        reference: String,
        value: String,
        lib_id: String,
        position: Point,
        rotation: f64,
        mirror: bool,
        bbox: Aabb,
        // Absolute pin placements: (name, type, position).
        pins: Vec<(String, PinType, Point)>,
    }
    let mut raw_comps: Vec<RawComp> = Vec::new();

    for sym in root.get_all("symbol") {
        // Top-level placed symbol instances have (lib_id ...) + (at ...).
        let lib_id = sym
            .get("lib_id")
            .and_then(|n| n.list())
            .and_then(|l| l.get(1))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let (position, rotation) = read_at(sym);
        let mirror = sym
            .get("mirror")
            .and_then(|n| n.list())
            .and_then(|l| l.get(1))
            .and_then(Value::as_str)
            .map(|m| m == "y")
            .unwrap_or(false);

        // Properties: Reference / Value.
        let mut reference = String::new();
        let mut value = String::new();
        for prop in sym.get_all("property") {
            if let Some(items) = prop.list() {
                let key = items.get(1).and_then(Value::as_str).unwrap_or("");
                let val = items.get(2).and_then(Value::as_str).unwrap_or("");
                match key {
                    "Reference" => reference = val.to_string(),
                    "Value" => value = val.to_string(),
                    _ => {}
                }
            }
        }

        // Resolve pins from the lib_symbols table for this lib_id.
        let mut pins: Vec<(String, PinType, Point)> = Vec::new();
        let mut bbox = Aabb::EMPTY;
        bbox.expand_point(position);
        if let Some(template) = lib_pins.get(&lib_id) {
            for lp in template {
                let abs = place_offset(position, rotation, mirror, lp.offset);
                let key = abs.quantize();
                let node = nb.node(key);
                // Power pins / their implicit nets are named later by labels; a
                // pin alone does not name a net.
                let _ = node;
                bbox.expand_point(abs);
                pins.push((lp.number.clone(), lp.pin_type, abs));
            }
        }
        if bbox.is_empty() {
            bbox = Aabb::new(position, position);
        }

        raw_comps.push(RawComp {
            reference,
            value,
            lib_id,
            position,
            rotation,
            mirror,
            bbox,
            pins,
        });
    }

    // Labels (local / global / hierarchical). Each names the group it sits on.
    struct RawLabel {
        text: String,
        position: Point,
        rotation: f64,
        kind: LabelKind,
    }
    let mut raw_labels: Vec<RawLabel> = Vec::new();
    for (head, kind) in [
        ("label", LabelKind::Local),
        ("global_label", LabelKind::Global),
        ("hierarchical_label", LabelKind::Hierarchical),
    ] {
        for l in root.get_all(head) {
            let text = l
                .list()
                .and_then(|items| items.get(1))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let (position, rotation) = read_at(l);
            let node = nb.node(position.quantize());
            nb.name(node, &text);
            raw_labels.push(RawLabel {
                text,
                position,
                rotation,
                kind,
            });
        }
    }

    // Sheet symbols.
    for sh in root.get_all("sheet") {
        let (origin, _) = read_at(sh);
        let size = sh.get("size").map(|n| read_nums(n, 2)).unwrap_or(vec![0.0, 0.0]);
        let mut bbox = Aabb::EMPTY;
        bbox.expand_point(origin);
        bbox.expand_point(Point::new(origin.x + size[0], origin.y + size[1]));
        if bbox.is_empty() {
            bbox = Aabb::new(origin, origin);
        }
        let mut name = String::new();
        let mut file = String::new();
        for prop in sh.get_all("property") {
            if let Some(items) = prop.list() {
                let key = items.get(1).and_then(Value::as_str).unwrap_or("");
                let val = items.get(2).and_then(Value::as_str).unwrap_or("");
                match key {
                    // KiCad 7/8 use "Sheetname"/"Sheetfile"; older use "Sheet name".
                    "Sheetname" | "Sheet name" => name = val.to_string(),
                    "Sheetfile" | "Sheet file" => file = val.to_string(),
                    _ => {}
                }
            }
        }
        scene.sheets.push(Sheet {
            name,
            file,
            bbox,
        });
        scene.bounds.expand_aabb(bbox);
    }

    // --- pass 2: resolve nets, then emit the scene arrays --------------------
    let (net_names, key_to_net) = nb.finalize();
    scene.net_names = net_names;
    let net_for = |p: Point| -> NetId {
        key_to_net
            .get(&p.quantize())
            .copied()
            .unwrap_or(NetId::NONE)
    };

    for w in &raw_wires {
        let net_id = net_for(w.a);
        scene.wires.push(Wire {
            a: w.a,
            b: w.b,
            net_id,
        });
        scene.bounds.expand_point(w.a);
        scene.bounds.expand_point(w.b);
    }

    for p in &raw_junctions {
        let net_id = net_for(*p);
        scene.junctions.push(Junction {
            position: *p,
            net_id,
        });
        scene.bounds.expand_point(*p);
    }

    for (ci, rc) in raw_comps.iter().enumerate() {
        let comp_id = ComponentId::new(ci as u32);
        scene.components.push(Component {
            reference: rc.reference.clone(),
            value: rc.value.clone(),
            lib_id: rc.lib_id.clone(),
            position: rc.position,
            rotation: rc.rotation,
            mirror: rc.mirror,
            bbox: rc.bbox,
            layer: LayerId::SCHEMATIC,
        });
        scene.bounds.expand_aabb(rc.bbox);
        for (name, ptype, pos) in &rc.pins {
            let net_id = net_for(*pos);
            scene.pads.push(Pad {
                component: comp_id,
                name: name.clone(),
                position: *pos,
                size: (0.0, 0.0),
                shape: PadShape::Circle,
                pin_type: *ptype,
                layer: LayerId::SCHEMATIC,
                net_id,
            });
        }
    }

    for rl in &raw_labels {
        let net_id = net_for(rl.position);
        scene.labels.push(Label {
            text: rl.text.clone(),
            position: rl.position,
            rotation: rl.rotation,
            kind: rl.kind,
            net_id,
        });
        scene.bounds.expand_point(rl.position);
    }

    // We iterated `items` only to confirm we touched the document; if a file has
    // a root head but is otherwise empty that is still a valid (empty) scene.
    let _ = items;

    scene.init_flags();
    Ok(scene)
}

/// Read a `(pts (xy x y) (xy x y) ...)` node into a list of points.
fn read_pts(pts: &Value) -> Vec<Point> {
    let mut out = Vec::new();
    for xy in pts.get_all("xy") {
        let v = read_nums(xy, 2);
        out.push(Point::new(v[0], v[1]));
    }
    out
}

/// A pin template from a library symbol: its number, electrical type, and offset
/// relative to the symbol origin.
struct LibPin {
    number: String,
    pin_type: PinType,
    offset: Point,
}

/// Build a `lib_id -> Vec<LibPin>` table from the `(lib_symbols ...)` block a
/// `.kicad_sch` embeds. Pins live on per-unit child `(symbol "Name_1_1" ...)`
/// sub-symbols; we flatten all units' pins onto the parent lib_id.
fn build_lib_pin_table(root: &Value) -> HashMap<String, Vec<LibPin>> {
    let mut table: HashMap<String, Vec<LibPin>> = HashMap::new();
    let lib_symbols = match root.get("lib_symbols") {
        Some(l) => l,
        None => return table,
    };
    for sym in lib_symbols.get_all("symbol") {
        // The library symbol's id is its first string arg, e.g.
        // (symbol "Device:R" (pin ...) (symbol "R_0_1" ...) (symbol "R_1_1" ...)).
        let lib_id = match sym.list().and_then(|items| items.get(1)).and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let mut pins = Vec::new();
        collect_pins(sym, &mut pins);
        if !pins.is_empty() {
            table.entry(lib_id).or_default().extend(pins);
        } else {
            table.entry(lib_id).or_default();
        }
    }
    table
}

/// Recursively collect `(pin ...)` definitions from a library symbol and its unit
/// sub-symbols.
fn collect_pins(sym: &Value, out: &mut Vec<LibPin>) {
    for pin in sym.get_all("pin") {
        // (pin <electrical_type> <graphic> (at x y angle) ... (number "1" ...))
        let etype = pin
            .list()
            .and_then(|items| items.get(1))
            .and_then(Value::as_str)
            .map(pin_type)
            .unwrap_or(PinType::Unspecified);
        let (offset, _) = read_at(pin);
        let number = pin
            .get("number")
            .and_then(|n| n.list())
            .and_then(|l| l.get(1))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        out.push(LibPin {
            number,
            pin_type: etype,
            offset,
        });
    }
    // Recurse into unit sub-symbols (their head is also "symbol").
    for unit in sym.get_all("symbol") {
        collect_pins(unit, out);
    }
}

// ===========================================================================
// Symbol library (.kicad_sym)
// ===========================================================================

/// Parse a `.kicad_sym` library `(kicad_symbol_lib ...)`. The viewer represents
/// each library symbol as a [`Component`] placed at the origin with its pins as
/// pads, so a library can be browsed in the same scene structure. Pins are placed
/// at their raw library offsets (no instance transform).
pub fn parse_symbol_library(path: &Path, root: &Value) -> Result<Scene> {
    if root.head() != Some("kicad_symbol_lib") {
        return Err(CanvasError::parse(
            path,
            format!(
                "expected (kicad_symbol_lib ...) root, found {:?}",
                root.head().unwrap_or("<non-list>")
            ),
        ));
    }
    let mut scene = Scene::new(SceneKind::Schematic);
    scene.layer_names = vec!["schematic".to_string()];

    for sym in root.get_all("symbol") {
        let lib_id = sym
            .list()
            .and_then(|items| items.get(1))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let mut value = String::new();
        let mut reference = String::new();
        for prop in sym.get_all("property") {
            if let Some(items) = prop.list() {
                let key = items.get(1).and_then(Value::as_str).unwrap_or("");
                let val = items.get(2).and_then(Value::as_str).unwrap_or("");
                match key {
                    "Value" => value = val.to_string(),
                    "Reference" => reference = val.to_string(),
                    _ => {}
                }
            }
        }
        let comp_id = ComponentId::new(scene.components.len() as u32);

        let mut pins = Vec::new();
        collect_pins(sym, &mut pins);

        let mut bbox = Aabb::EMPTY;
        bbox.expand_point(Point::ORIGIN);
        for lp in &pins {
            bbox.expand_point(lp.offset);
        }
        if bbox.is_empty() {
            bbox = Aabb::new(Point::ORIGIN, Point::ORIGIN);
        }

        scene.components.push(Component {
            reference,
            value,
            lib_id,
            position: Point::ORIGIN,
            rotation: 0.0,
            mirror: false,
            bbox,
            layer: LayerId::SCHEMATIC,
        });
        scene.bounds.expand_aabb(bbox);

        for lp in pins {
            scene.pads.push(Pad {
                component: comp_id,
                name: lp.number,
                position: lp.offset,
                size: (0.0, 0.0),
                shape: PadShape::Circle,
                pin_type: lp.pin_type,
                layer: LayerId::SCHEMATIC,
                net_id: NetId::NONE,
            });
        }
    }

    // A library symbol's pins carry no net (no placement context).
    scene.net_names = vec![String::new()];
    scene.init_flags();
    Ok(scene)
}

// ===========================================================================
// PCB (.kicad_pcb) — geometric pass: footprints/pads, tracks, vias, zones.
// ===========================================================================

/// A layer-name interner that hands out stable [`LayerId`]s and fills
/// `layer_names`.
struct LayerTable {
    names: Vec<String>,
    index: HashMap<String, u16>,
}
impl LayerTable {
    fn new() -> Self {
        LayerTable {
            names: Vec::new(),
            index: HashMap::new(),
        }
    }
    fn intern(&mut self, name: &str) -> LayerId {
        if let Some(&i) = self.index.get(name) {
            return LayerId::new(i);
        }
        let i = self.names.len() as u16;
        self.names.push(name.to_string());
        self.index.insert(name.to_string(), i);
        LayerId::new(i)
    }
}

/// True for a KiCad copper layer name — `F.Cu`, `B.Cu`, or an inner `In<k>.Cu`.
/// Only copper layers hold a stackup position and can be spanned by a via;
/// technical layers (`*.Mask`, `*.SilkS`, `*.Paste`, `Edge.Cuts`, …) never end
/// in `.Cu`.
fn is_copper_layer(name: &str) -> bool {
    name.ends_with(".Cu")
}

/// Pre-seed `layers` with the board's copper stack in physical (top→bottom)
/// order from the top-level `(layers (ORDINAL "name" type ...) ...)` block.
///
/// KiCad defines a layer's `ORDINAL` as its position in the stack ordering
/// (F.Cu = 0, inner copper ascending, B.Cu last), so interning the copper layers
/// by ascending ordinal hands them dense [`LayerId`]s `0..n` that are contiguous
/// and physically ordered. That is the invariant the two via consumers depend
/// on: an inclusive LayerId range then equals a real copper span, so the R-tree
/// indexes a via on exactly the copper it reaches ([`crate::rtree`]) and the
/// graph stitches only copper within the via's span ([`crate::graph`]). Without
/// it, first-appearance interning could place a blind via's inner layer numerically
/// adjacent to `B.Cu` and mis-hit / mis-merge copper the via never touches.
///
/// Non-copper layers are left for first-appearance interning and land after the
/// copper block. A board with no parseable `(layers ...)` block is a no-op —
/// layers then fall back to first-appearance interning as before.
fn seed_copper_layers(root: &Value, layers: &mut LayerTable) {
    let Some(block) = root.get("layers") else {
        return;
    };
    let Some(items) = block.list() else {
        return;
    };
    // Collect (ordinal, name) per copper layer; items[0] is the `layers` head.
    let mut copper: Vec<(i64, &str)> = Vec::new();
    for item in items.iter().skip(1) {
        let Some(fields) = item.list() else {
            continue;
        };
        let ordinal = fields.first().and_then(Value::as_f64);
        let name = fields.get(1).and_then(Value::as_str);
        if let (Some(ordinal), Some(name)) = (ordinal, name) {
            if is_copper_layer(name) {
                copper.push((ordinal as i64, name));
            }
        }
    }
    // Ascending ORDINAL is top→bottom stackup order (F.Cu first … B.Cu last).
    copper.sort_by_key(|&(ordinal, _)| ordinal);
    for (_, name) in copper {
        layers.intern(name);
    }
}

/// A net-name interner for `.kicad_pcb`, which DOES carry an explicit
/// `(net <n> "<name>")` table. Maps the file's net ordinal to our dense
/// [`NetId`], preserving index 0 == "" (KiCad's net 0 is the no-net).
struct PcbNetTable {
    names: Vec<String>,
    by_ordinal: HashMap<i64, NetId>,
}
impl PcbNetTable {
    fn new() -> Self {
        PcbNetTable {
            names: vec![String::new()],
            by_ordinal: HashMap::new(),
        }
    }
    fn register(&mut self, ordinal: i64, name: &str) {
        if self.by_ordinal.contains_key(&ordinal) {
            return;
        }
        let id = if ordinal == 0 || name.is_empty() {
            NetId::new(0)
        } else {
            let id = NetId::new(self.names.len() as u32);
            self.names.push(name.to_string());
            id
        };
        self.by_ordinal.insert(ordinal, id);
    }
    fn resolve(&self, ordinal: i64) -> NetId {
        self.by_ordinal.get(&ordinal).copied().unwrap_or(NetId::NONE)
    }
}

/// Parse a `.kicad_pcb` root `(kicad_pcb ...)` into a board [`Scene`].
pub fn parse_pcb(path: &Path, root: &Value) -> Result<Scene> {
    if root.head() != Some("kicad_pcb") {
        return Err(CanvasError::parse(
            path,
            format!(
                "expected (kicad_pcb ...) root, found {:?}",
                root.head().unwrap_or("<non-list>")
            ),
        ));
    }

    let mut scene = Scene::new(SceneKind::Pcb);
    let mut layers = LayerTable::new();
    let mut nets = PcbNetTable::new();

    // Pre-seed the layer table from the board's top-level `(layers ...)` stackup
    // so numeric LayerId adjacency reflects PHYSICAL copper adjacency, not the
    // order layers first appear in the geometry. This is the invariant the R-tree
    // via-span index (`rtree`) and the graph via stitcher (`graph`) both rely on
    // to reason about which copper a via actually reaches.
    seed_copper_layers(root, &mut layers);

    // Net table: (net 0 "") (net 1 "GND") ...
    for n in root.get_all("net") {
        if let Some(items) = n.list() {
            let ordinal = items.get(1).and_then(Value::as_f64).unwrap_or(0.0) as i64;
            let name = items.get(2).and_then(Value::as_str).unwrap_or("");
            nets.register(ordinal, name);
        }
    }

    // Read a `(net <ordinal> ...)` child of a track/via/pad and resolve it.
    let read_net = |node: &Value, nets: &PcbNetTable| -> NetId {
        match node.get("net") {
            Some(nn) => {
                let ord = nn.list().and_then(|l| l.get(1)).and_then(Value::as_f64);
                match ord {
                    Some(o) => nets.resolve(o as i64),
                    None => NetId::NONE,
                }
            }
            None => NetId::NONE,
        }
    };

    // Footprints -> components + pads.
    for fp in root.get_all("footprint") {
        let lib_id = fp
            .list()
            .and_then(|items| items.get(1))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let (origin, rotation) = read_at(fp);
        let layer_name = read_layer_name(fp).unwrap_or_else(|| "F.Cu".to_string());
        let comp_layer = layers.intern(&layer_name);

        let mut reference = String::new();
        let mut value = String::new();
        for prop in fp.get_all("property") {
            if let Some(items) = prop.list() {
                let key = items.get(1).and_then(Value::as_str).unwrap_or("");
                let val = items.get(2).and_then(Value::as_str).unwrap_or("");
                match key {
                    "Reference" => reference = val.to_string(),
                    "Value" => value = val.to_string(),
                    _ => {}
                }
            }
        }
        // Older KiCad uses (fp_text reference "R1" ...) / (fp_text value ...).
        if reference.is_empty() || value.is_empty() {
            for t in fp.get_all("fp_text") {
                if let Some(items) = t.list() {
                    let which = items.get(1).and_then(Value::as_str).unwrap_or("");
                    let text = items.get(2).and_then(Value::as_str).unwrap_or("");
                    match which {
                        "reference" if reference.is_empty() => reference = text.to_string(),
                        "value" if value.is_empty() => value = text.to_string(),
                        _ => {}
                    }
                }
            }
        }

        let comp_id = ComponentId::new(scene.components.len() as u32);
        let mut bbox = Aabb::EMPTY;
        bbox.expand_point(origin);

        // Pads, placed relative to the footprint origin/rotation.
        for pad in fp.get_all("pad") {
            let items = match pad.list() {
                Some(i) => i,
                None => continue,
            };
            let name = items.get(1).and_then(Value::as_str).unwrap_or("").to_string();
            let shape_name = items.get(3).and_then(Value::as_str).unwrap_or("rect");
            let shape = pad_shape(shape_name);
            let (off, _pad_angle) = read_at(pad);
            let abs = place_offset(origin, rotation, false, off);
            let size = pad.get("size").map(|n| read_nums(n, 2)).unwrap_or(vec![0.0, 0.0]);
            // A pad lists its copper layers in (layers ...); use the first.
            let pad_layer_name = pad
                .get("layers")
                .and_then(|n| n.list())
                .and_then(|l| l.get(1))
                .and_then(Value::as_str)
                .unwrap_or(&layer_name)
                .to_string();
            let pad_layer = layers.intern(&pad_layer_name);
            let net_id = read_net(pad, &nets);
            bbox.expand_point(abs);
            scene.pads.push(Pad {
                component: comp_id,
                name,
                position: abs,
                size: (size[0], size[1]),
                shape,
                pin_type: PinType::Passive,
                layer: pad_layer,
                net_id,
            });
        }

        if bbox.is_empty() {
            bbox = Aabb::new(origin, origin);
        }
        scene.components.push(Component {
            reference,
            value,
            lib_id,
            position: origin,
            rotation,
            mirror: false,
            bbox,
            layer: comp_layer,
        });
        scene.bounds.expand_aabb(bbox);
    }

    // Tracks: (segment (start x y) (end x y) (width w) (layer "F.Cu") (net n)).
    for seg in root.get_all("segment") {
        let start = seg.get("start").map(|n| read_nums(n, 2)).unwrap_or(vec![0.0, 0.0]);
        let end = seg.get("end").map(|n| read_nums(n, 2)).unwrap_or(vec![0.0, 0.0]);
        let width = seg.get("width").and_then(|n| n.list()).and_then(|l| l.get(1)).and_then(Value::as_f64).unwrap_or(0.0);
        let layer_name = read_layer_name(seg).unwrap_or_else(|| "F.Cu".to_string());
        let layer = layers.intern(&layer_name);
        let net_id = read_net(seg, &nets);
        let a = Point::new(start[0], start[1]);
        let b = Point::new(end[0], end[1]);
        scene.bounds.expand_point(a);
        scene.bounds.expand_point(b);
        scene.tracks.push(Track {
            a,
            b,
            width: if width.is_finite() { width } else { 0.0 },
            layer,
            net_id,
        });
    }

    // Vias: (via (at x y) (size d) (drill dr) (layers "F.Cu" "B.Cu") (net n)).
    for via in root.get_all("via") {
        let (position, _) = read_at(via);
        let diameter = via.get("size").and_then(|n| n.list()).and_then(|l| l.get(1)).and_then(Value::as_f64).unwrap_or(0.0);
        let drill = via.get("drill").and_then(|n| n.list()).and_then(|l| l.get(1)).and_then(Value::as_f64).unwrap_or(0.0);
        let (lfrom, lto) = match via.get("layers").and_then(|n| n.list()) {
            Some(items) => {
                let f = items.get(1).and_then(Value::as_str).unwrap_or("F.Cu");
                let t = items.get(2).and_then(Value::as_str).unwrap_or(f);
                (layers.intern(f), layers.intern(t))
            }
            None => (layers.intern("F.Cu"), layers.intern("B.Cu")),
        };
        let net_id = read_net(via, &nets);
        scene.bounds.expand_point(position);
        scene.vias.push(Via {
            position,
            diameter: if diameter.is_finite() { diameter } else { 0.0 },
            drill: if drill.is_finite() { drill } else { 0.0 },
            layer_from: lfrom,
            layer_to: lto,
            net_id,
        });
    }

    // Zones: (zone (net n) (layer "F.Cu") (polygon (pts (xy ..) ..))).
    for zone in root.get_all("zone") {
        let layer_name = read_layer_name(zone)
            .or_else(|| {
                // KiCad 8 multi-layer zones use (layers "F.Cu" ...): take first.
                zone.get("layers")
                    .and_then(|n| n.list())
                    .and_then(|l| l.get(1))
                    .and_then(Value::as_str)
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| "F.Cu".to_string());
        let layer = layers.intern(&layer_name);
        let net_id = read_net(zone, &nets);
        // Outline lives under (polygon (pts ...)).
        let outline = zone
            .get("polygon")
            .and_then(|p| p.get("pts"))
            .map(read_pts)
            .unwrap_or_default();
        let mut bbox = Aabb::EMPTY;
        for p in &outline {
            bbox.expand_point(*p);
        }
        if bbox.is_empty() {
            // A zone with no readable outline still gets a degenerate bbox so the
            // R-tree does not see an inverted box.
            bbox = Aabb::new(Point::ORIGIN, Point::ORIGIN);
        }
        scene.bounds.expand_aabb(bbox);
        scene.zones.push(Zone {
            outline,
            layer,
            net_id,
            bbox,
        });
    }

    scene.net_names = nets.names;
    // Ensure at least the no-net entry exists even on a board with no net table.
    if scene.net_names.is_empty() {
        scene.net_names.push(String::new());
    }
    scene.layer_names = layers.names;
    scene.init_flags();
    Ok(scene)
}

// ===========================================================================
// Footprint library (.kicad_mod) — a single (footprint ...) form.
// ===========================================================================

/// Parse a `.kicad_mod` footprint into a board [`Scene`] holding one component
/// and its pads, placed at the origin. Useful for previewing a footprint.
pub fn parse_footprint_library(path: &Path, root: &Value) -> Result<Scene> {
    // KiCad 6+ uses (footprint ...); legacy uses (module ...).
    let head = root.head();
    if head != Some("footprint") && head != Some("module") {
        return Err(CanvasError::parse(
            path,
            format!(
                "expected (footprint ...) root, found {:?}",
                head.unwrap_or("<non-list>")
            ),
        ));
    }

    let mut scene = Scene::new(SceneKind::Pcb);
    let mut layers = LayerTable::new();

    let lib_id = root
        .list()
        .and_then(|items| items.get(1))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let layer_name = read_layer_name(root).unwrap_or_else(|| "F.Cu".to_string());
    let comp_layer = layers.intern(&layer_name);

    let comp_id = ComponentId::new(0);
    let mut bbox = Aabb::EMPTY;
    bbox.expand_point(Point::ORIGIN);

    for pad in root.get_all("pad") {
        let items = match pad.list() {
            Some(i) => i,
            None => continue,
        };
        let name = items.get(1).and_then(Value::as_str).unwrap_or("").to_string();
        let shape_name = items.get(3).and_then(Value::as_str).unwrap_or("rect");
        let shape = pad_shape(shape_name);
        let (off, _) = read_at(pad);
        let size = pad.get("size").map(|n| read_nums(n, 2)).unwrap_or(vec![0.0, 0.0]);
        let pad_layer_name = pad
            .get("layers")
            .and_then(|n| n.list())
            .and_then(|l| l.get(1))
            .and_then(Value::as_str)
            .unwrap_or(&layer_name)
            .to_string();
        let pad_layer = layers.intern(&pad_layer_name);
        bbox.expand_point(off);
        scene.pads.push(Pad {
            component: comp_id,
            name,
            position: off,
            size: (size[0], size[1]),
            shape,
            pin_type: PinType::Passive,
            layer: pad_layer,
            net_id: NetId::NONE,
        });
    }

    if bbox.is_empty() {
        bbox = Aabb::new(Point::ORIGIN, Point::ORIGIN);
    }
    scene.components.push(Component {
        reference: String::new(),
        value: String::new(),
        lib_id,
        position: Point::ORIGIN,
        rotation: 0.0,
        mirror: false,
        bbox,
        layer: comp_layer,
    });
    scene.bounds.expand_aabb(bbox);

    scene.net_names = vec![String::new()];
    scene.layer_names = layers.names;
    scene.init_flags();
    Ok(scene)
}

// ===========================================================================
// Tests — small but REAL KiCad 7/8 fixtures.
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // A minimal but real-shaped .kicad_sch: lib_symbols defines Device:R with two
    // pins; one placed R1 symbol; two wires meeting the pins; a junction; a label.
    //
    // Pin offsets in the lib symbol are relative to the symbol origin. R1 is
    // placed at (100, 100) with no rotation, so pin 1 (offset 0, 3.81) lands at
    // (100, 103.81) and pin 2 (offset 0, -3.81) at (100, 96.19). Wires touch
    // those pins.
    const SCH: &str = r#"
(kicad_sch (version 20230121) (generator eeschema)
  (lib_symbols
    (symbol "Device:R"
      (property "Reference" "R" (at 2.032 0 90))
      (property "Value" "R" (at 0 0 90))
      (symbol "R_0_1"
        (rectangle (start -1.016 -2.54) (end 1.016 2.54)))
      (symbol "R_1_1"
        (pin passive line (at 0 3.81 270) (length 1.27)
          (name "~" (effects (font (size 1.27 1.27))))
          (number "1" (effects (font (size 1.27 1.27)))))
        (pin passive line (at 0 -3.81 90) (length 1.27)
          (name "~" (effects (font (size 1.27 1.27))))
          (number "2" (effects (font (size 1.27 1.27))))))))
  (junction (at 100 103.81) (diameter 0) (color 0 0 0 0))
  (wire (pts (xy 100 103.81) (xy 100 110)) (stroke (width 0)))
  (wire (pts (xy 100 96.19) (xy 100 90)) (stroke (width 0)))
  (label "SIG" (at 100 110 0) (effects (font (size 1.27 1.27))))
  (symbol (lib_id "Device:R") (at 100 100 0) (unit 1)
    (property "Reference" "R1" (at 102 98 0))
    (property "Value" "10k" (at 102 102 0))))
"#;

    #[test]
    fn parses_schematic_counts_and_fields() {
        let scene = parse_document(Path::new("test.kicad_sch"), SCH).unwrap();
        assert_eq!(scene.kind, SceneKind::Schematic);
        assert_eq!(scene.components.len(), 1, "one placed symbol");
        assert_eq!(scene.wires.len(), 2, "two wire segments");
        assert_eq!(scene.junctions.len(), 1, "one junction");
        assert_eq!(scene.labels.len(), 1, "one label");
        // The Device:R template has two pins -> two pads on the placed symbol.
        assert_eq!(scene.pads.len(), 2, "two pins resolved from lib_symbols");

        let c = &scene.components[0];
        assert_eq!(c.reference, "R1");
        assert_eq!(c.value, "10k");
        assert_eq!(c.lib_id, "Device:R");
        assert_eq!(c.position, Point::new(100.0, 100.0));

        // Flag arrays are parallel to geometry after init_flags().
        assert_eq!(scene.pad_flags.len(), scene.pads.len());
        assert_eq!(scene.wire_flags.len(), scene.wires.len());
    }

    #[test]
    fn schematic_net_assignment_connects_pin_wire_label() {
        let scene = parse_document(Path::new("test.kicad_sch"), SCH).unwrap();
        // Pin 1 sits at (100, 103.81), the junction and one wire endpoint sit
        // there too, and the wire's far end (100,110) carries the "SIG" label.
        // So pin1, that wire, the junction, and the label must share a net named
        // "SIG".
        let sig = scene.net_by_name("SIG");
        assert!(sig.is_some(), "SIG net should exist from the label");

        // The pin at (100, 103.81):
        let pin1 = scene
            .pads
            .iter()
            .find(|p| p.position == Point::new(100.0, 103.81))
            .expect("pin 1 placed at (100,103.81)");
        assert_eq!(pin1.net_id, sig, "pin1 joins the SIG net via the wire");

        // The junction at the same point:
        assert_eq!(scene.junctions[0].net_id, sig);

        // The label itself carries the named net.
        assert_eq!(scene.labels[0].net_id, sig);
        assert_eq!(scene.labels[0].kind, LabelKind::Local);

        // The second pin / second wire are on a DIFFERENT (unnamed) net.
        let pin2 = scene
            .pads
            .iter()
            .find(|p| p.position == Point::new(100.0, 96.19))
            .expect("pin 2 placed at (100,96.19)");
        assert!(pin2.net_id.is_some());
        assert_ne!(pin2.net_id, sig, "the two terminals are not the same net");
    }

    // A real-shaped .kicad_sym library with one symbol and two pins.
    const SYM: &str = r#"
(kicad_symbol_lib (version 20211014) (generator kicad_symbol_editor)
  (symbol "Device:C"
    (property "Reference" "C" (at 0.635 2.54 0))
    (property "Value" "C" (at 0.635 -2.54 0))
    (symbol "C_0_1"
      (polyline (pts (xy -2.032 -0.762) (xy 2.032 -0.762))))
    (symbol "C_1_1"
      (pin passive line (at 0 3.81 270) (length 2.794)
        (name "~" (effects (font (size 1.27 1.27))))
        (number "1" (effects (font (size 1.27 1.27)))))
      (pin passive line (at 0 -3.81 90) (length 2.794)
        (name "~" (effects (font (size 1.27 1.27))))
        (number "2" (effects (font (size 1.27 1.27))))))))
"#;

    #[test]
    fn parses_symbol_library() {
        let scene = parse_document(Path::new("Device.kicad_sym"), SYM).unwrap();
        assert_eq!(scene.components.len(), 1);
        assert_eq!(scene.components[0].lib_id, "Device:C");
        assert_eq!(scene.components[0].value, "C");
        assert_eq!(scene.pads.len(), 2, "two pins flattened from both units");
        // Library pins carry no net.
        assert!(scene.pads.iter().all(|p| p.net_id.is_none()));
        // Pin numbers came through.
        let mut nums: Vec<&str> = scene.pads.iter().map(|p| p.name.as_str()).collect();
        nums.sort();
        assert_eq!(nums, vec!["1", "2"]);
    }

    // A small real-shaped .kicad_pcb: a net table, one footprint with two pads,
    // one track segment, and one via.
    const PCB: &str = r#"
(kicad_pcb (version 20221018) (generator pcbnew)
  (net 0 "")
  (net 1 "GND")
  (net 2 "VCC")
  (footprint "Resistor_SMD:R_0805" (layer "F.Cu") (at 50 50 0)
    (property "Reference" "R1" (at 0 0 0))
    (property "Value" "10k" (at 0 0 0))
    (pad "1" smd roundrect (at -1 0) (size 1.0 1.25) (layers "F.Cu")
      (net 1 "GND"))
    (pad "2" smd roundrect (at 1 0) (size 1.0 1.25) (layers "F.Cu")
      (net 2 "VCC")))
  (segment (start 49 50) (end 40 50) (width 0.25) (layer "F.Cu") (net 1))
  (via (at 40 50) (size 0.6) (drill 0.3) (layers "F.Cu" "B.Cu") (net 1)))
"#;

    #[test]
    fn parses_pcb_counts_nets_and_fields() {
        let scene = parse_document(Path::new("board.kicad_pcb"), PCB).unwrap();
        assert_eq!(scene.kind, SceneKind::Pcb);
        assert_eq!(scene.components.len(), 1);
        assert_eq!(scene.pads.len(), 2);
        assert_eq!(scene.tracks.len(), 1);
        assert_eq!(scene.vias.len(), 1);

        // Net table: index 0 is "" (no-net), GND and VCC present.
        let gnd = scene.net_by_name("GND");
        let vcc = scene.net_by_name("VCC");
        assert!(gnd.is_some() && vcc.is_some());

        // Pad nets resolved from the per-pad (net ...) ordinals.
        let pad1 = scene.pads.iter().find(|p| p.name == "1").unwrap();
        let pad2 = scene.pads.iter().find(|p| p.name == "2").unwrap();
        assert_eq!(pad1.net_id, gnd);
        assert_eq!(pad2.net_id, vcc);

        // Footprint origin is (50,50); pad "1" offset (-1,0) -> (49,50).
        assert_eq!(pad1.position, Point::new(49.0, 50.0));

        // Track + via on GND.
        assert_eq!(scene.tracks[0].net_id, gnd);
        assert_eq!(scene.vias[0].net_id, gnd);
        assert_eq!(scene.tracks[0].width, 0.25);
        assert_eq!(scene.vias[0].diameter, 0.6);

        // Layers interned (F.Cu first, B.Cu added by the via span).
        assert!(scene.layer_names.iter().any(|l| l == "F.Cu"));
        assert!(scene.layer_names.iter().any(|l| l == "B.Cu"));

        // Component reference/value.
        assert_eq!(scene.components[0].reference, "R1");
        assert_eq!(scene.components[0].value, "10k");
    }

    #[test]
    fn via_layer_span_and_position() {
        let scene = parse_document(Path::new("board.kicad_pcb"), PCB).unwrap();
        let via = &scene.vias[0];
        assert_eq!(via.position, Point::new(40.0, 50.0));
        assert_eq!(via.drill, 0.3);
        // from != to (a through via spanning F.Cu -> B.Cu).
        assert_ne!(via.layer_from, via.layer_to);
        assert_eq!(scene.layer_names[via.layer_from.index()], "F.Cu");
        assert_eq!(scene.layer_names[via.layer_to.index()], "B.Cu");
    }

    // A board whose `(layers ...)` stackup declares F.Cu, In1.Cu, B.Cu, but whose
    // GEOMETRY first touches F.Cu, then B.Cu, then In1.Cu — the canonical Bug-1
    // divergence. Without the stackup pre-seed, first-appearance interning would
    // number copper F.Cu=0, B.Cu=1, In1.Cu=2, misordering the stack; the pre-seed
    // must intern copper in stack order so In1.Cu lands physically between F.Cu
    // and B.Cu. `B.SilkS` is declared but never referenced, so it must NOT be
    // interned (pre-seed is copper-only).
    const STACKUP_PCB: &str = r#"
(kicad_pcb (version 20221018) (generator pcbnew)
  (layers
    (0 "F.Cu" signal)
    (1 "In1.Cu" signal)
    (31 "B.Cu" signal)
    (36 "B.SilkS" user))
  (net 0 "")
  (net 1 "N1")
  (segment (start 0 0) (end 3 0) (width 0.25) (layer "F.Cu") (net 1))
  (segment (start 0 0) (end 3 0) (width 0.25) (layer "B.Cu") (net 1))
  (segment (start 0 0) (end 3 0) (width 0.25) (layer "In1.Cu") (net 1))
  (via (at 10 0) (size 0.6) (drill 0.3) (layers "F.Cu" "In1.Cu") (net 1)))
"#;

    #[test]
    fn pcb_seeds_copper_in_stackup_order_not_appearance_order() {
        let scene = parse_document(Path::new("board.kicad_pcb"), STACKUP_PCB).unwrap();
        // Copper interned in physical stack order despite the geometry touching
        // B.Cu first: F.Cu, In1.Cu, B.Cu take ids 0, 1, 2.
        assert_eq!(scene.layer_names.first().map(String::as_str), Some("F.Cu"));
        assert_eq!(scene.layer_names.get(1).map(String::as_str), Some("In1.Cu"));
        assert_eq!(scene.layer_names.get(2).map(String::as_str), Some("B.Cu"));
        // Unreferenced non-copper layers are not interned.
        assert!(!scene.layer_names.iter().any(|l| l == "B.SilkS"));
        // The blind via spans F.Cu -> In1.Cu (adjacent ids 0,1), never reaching
        // B.Cu (id 2) — the property both the R-tree and graph fixes rely on.
        let via = &scene.vias[0];
        assert_eq!(via.layer_from.raw(), 0);
        assert_eq!(via.layer_to.raw(), 1);
        let b = scene.layer_names.iter().position(|n| n == "B.Cu").unwrap();
        assert!(via.layer_to.index() < b, "In1.Cu must sit below B.Cu in the stack");
    }

    // ---- error / robustness cases (panic-freedom contract) ----------------

    #[test]
    fn unsupported_extension_errors() {
        let err = parse_document(Path::new("notes.txt"), "(kicad_sch)").unwrap_err();
        assert!(matches!(err, CanvasError::UnsupportedFileType(_)));
    }

    #[test]
    fn wrong_root_head_is_parse_error() {
        // Well-formed s-expr, wrong document head for the extension.
        let err = parse_document(Path::new("x.kicad_sch"), "(kicad_pcb (version 1))").unwrap_err();
        assert!(matches!(err, CanvasError::Parse { .. }));
    }

    #[test]
    fn malformed_sexpr_surfaces_as_sexpr_error() {
        // Unbalanced parens -> SexprError -> CanvasError::Sexpr (never a panic).
        let err = parse_document(Path::new("x.kicad_sch"), "(kicad_sch (symbol").unwrap_err();
        assert!(matches!(err, CanvasError::Sexpr(_)));
    }

    #[test]
    fn empty_input_errors() {
        let err = parse_document(Path::new("x.kicad_sch"), "   ").unwrap_err();
        assert!(matches!(err, CanvasError::Sexpr(crate::error::SexprError::Empty)));
    }

    #[test]
    fn empty_but_valid_schematic_root_is_ok() {
        // A document with the right head but no entities is a valid empty scene.
        let scene = parse_document(Path::new("x.kicad_sch"), "(kicad_sch (version 20230121))").unwrap();
        assert_eq!(scene.entity_count(), 0);
        // net_names still has the reserved no-net slot at index 0.
        assert_eq!(scene.net_names.first().map(String::as_str), Some(""));
        // Flags initialized (all empty) — init_flags was called.
        assert!(scene.pad_flags.is_empty());
    }

    #[test]
    fn garbage_numbers_do_not_panic() {
        // `(at)` with no coords, a wire with one point, a pad with no size —
        // every reader must tolerate missing fields rather than panic.
        let src = r#"(kicad_sch (version 1)
            (junction (at))
            (wire (pts (xy 0 0)))
            (label "X" (at)))"#;
        let scene = parse_document(Path::new("x.kicad_sch"), src).unwrap();
        // The one-point wire is dropped (needs two endpoints); junction+label kept.
        assert_eq!(scene.wires.len(), 0);
        assert_eq!(scene.junctions.len(), 1);
        assert_eq!(scene.labels.len(), 1);
        assert_eq!(scene.junctions[0].position, Point::ORIGIN);
    }

    #[test]
    fn footprint_library_parses() {
        let mod_src = r#"
(footprint "Capacitor_SMD:C_0805" (layer "F.Cu")
  (pad "1" smd rect (at -1 0) (size 1.0 1.25) (layers "F.Cu"))
  (pad "2" smd rect (at 1 0) (size 1.0 1.25) (layers "F.Cu")))
"#;
        let scene = parse_document(Path::new("C_0805.kicad_mod"), mod_src).unwrap();
        assert_eq!(scene.components.len(), 1);
        assert_eq!(scene.pads.len(), 2);
        assert_eq!(scene.pads[0].position, Point::new(-1.0, 0.0));
        assert!(scene.pads.iter().all(|p| p.net_id.is_none()));
    }

    #[test]
    fn same_named_global_labels_merge_nets() {
        // Two disjoint wire islands each carrying a global "GND" label collapse to
        // ONE net (SPEC §1: global labels connect by name).
        let src = r#"(kicad_sch (version 1)
            (wire (pts (xy 0 0) (xy 0 10)))
            (global_label "GND" (at 0 0 0))
            (wire (pts (xy 50 0) (xy 50 10)))
            (global_label "GND" (at 50 0 0)))"#;
        let scene = parse_document(Path::new("x.kicad_sch"), src).unwrap();
        let gnd = scene.net_by_name("GND");
        assert!(gnd.is_some());
        // Both wires resolve to the single GND net.
        assert!(scene.wires.iter().all(|w| w.net_id == gnd));
        // Exactly one "GND" entry in net_names.
        assert_eq!(scene.net_names.iter().filter(|n| *n == "GND").count(), 1);
    }

    // ---- in-tree fuzz: deterministic panic-freedom (SPEC §1/§7) -----------
    //
    // Companion to the cargo-fuzz target in `fuzz/fuzz_targets/parse_document.rs`
    // (which needs nightly + libFuzzer and may not run on a given box). This
    // exercises `parse_document` — the full KiCad document layer for every
    // supported extension — on many adversarial inputs from a SEEDED LCG (no
    // system RNG; failures reproduce from the seed), asserting only the
    // panic-freedom contract from the module docs: arbitrary input yields a
    // `Result`, never a panic / stack overflow / abort. The test harness fails
    // the build on any panic.

    /// Seeded linear-congruential generator (self-contained; deterministic).
    struct Lcg(u64);
    impl Lcg {
        fn new(seed: u64) -> Self {
            Lcg(seed ^ 0x9E37_79B9_7F4A_7C15)
        }
        fn next_u32(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 32) as u32
        }
        fn below(&mut self, n: u32) -> u32 {
            if n == 0 {
                0
            } else {
                self.next_u32() % n
            }
        }
    }

    /// Generate one adversarial source. Same intent as the sexpr-layer fuzz
    /// generator but biased toward KiCad-document shapes (root heads, `(at ...)`,
    /// `(pad ...)`, nets) so the document interpreter — not just the lexer — is
    /// stressed, while still throwing deep/unbalanced parens, huge atoms, and
    /// unterminated strings at it.
    fn adversarial_source(rng: &mut Lcg, max_len: usize) -> String {
        const HEADS: &[&str] = &[
            "kicad_sch", "kicad_pcb", "symbol", "footprint", "at", "pad", "net", "wire", "pts",
            "xy", "segment", "via", "label", "junction", "lib_symbols", "pin", "size", "layers",
            "stroke", "property",
        ];
        const SOUP: &[char] = &[
            '(', ')', '"', '\\', ' ', '\t', '\n', ';', '.', '-', '+', 'e', '0', '9', 'a', '_',
            ':', '/', '\0', '\u{7f}', 'é', '中', '𝄞', '\u{FFFD}',
        ];
        let mut s = String::new();
        let len = rng.below(max_len as u32) as usize;
        while s.len() < len {
            match rng.below(20) {
                0 => {
                    let depth = rng.below(2200); // can exceed sexpr MAX_DEPTH
                    for _ in 0..depth {
                        s.push('(');
                    }
                }
                1 => {
                    for _ in 0..rng.below(64) {
                        s.push(')');
                    }
                }
                2 => {
                    for _ in 0..rng.below(4096) {
                        s.push('9'); // a huge numeric-ish atom (stresses f64 parse too)
                    }
                }
                3 => {
                    s.push('"');
                    for _ in 0..rng.below(96) {
                        s.push(SOUP[rng.below(SOUP.len() as u32) as usize]);
                    }
                    if rng.below(2) == 0 {
                        s.push('"');
                    }
                }
                // Emit a plausible-looking node head to drive the interpreter
                // deeper than the lexer (so field readers run on garbage args).
                4..=6 => {
                    s.push('(');
                    s.push_str(HEADS[rng.below(HEADS.len() as u32) as usize]);
                    s.push(' ');
                    for _ in 0..rng.below(6) {
                        s.push(SOUP[rng.below(SOUP.len() as u32) as usize]);
                        s.push(' ');
                    }
                    if rng.below(3) != 0 {
                        s.push(')');
                    }
                }
                _ => s.push(SOUP[rng.below(SOUP.len() as u32) as usize]),
            }
        }
        s
    }

    #[test]
    fn fuzz_parse_document_never_panics() {
        // Wrap most inputs in a valid-ish root so the body, not just the failing
        // root-head check, gets exercised; feed the rest raw.
        for seed in 0u64..4000 {
            let mut rng = Lcg::new(seed);
            let body = adversarial_source(&mut rng, 768);

            let raw = body.clone();
            let wrapped_sch = format!("(kicad_sch (version 20230121) {body})");
            let wrapped_pcb = format!("(kicad_pcb (version 20221018) {body})");

            for src in [&raw, &wrapped_sch, &wrapped_pcb] {
                // Every supported extension dispatches to a different grammar;
                // none may panic on arbitrary input.
                for ext in ["kicad_sch", "kicad_pcb", "kicad_sym", "kicad_mod", "txt"] {
                    let p = format!("fuzz.{ext}");
                    let _ = parse_document(Path::new(&p), src);
                }
            }
        }
    }

    #[test]
    fn fuzz_parse_document_hand_picked_inputs_never_panic() {
        // Adversarial shapes aimed at the document layer's field readers (missing
        // coords, junk net ordinals, one-point geometry, deep nesting). Each must
        // yield a Result, never panic.
        let cases: &[&str] = &[
            "",
            "(",
            "(kicad_sch",
            "(kicad_sch (version))",
            "(kicad_sch (symbol (at)))",
            "(kicad_pcb (net x \"\") (segment (start) (end) (net y)))",
            "(kicad_pcb (via (at) (size) (drill) (layers) (net -1)))",
            "(kicad_sch (wire (pts (xy))) (label \"\" (at)))",
            "(footprint \"\" (pad \"1\" smd rect (at) (size) (layers)))",
            "(symbol \"\" (pin (at) (length)))",
            "(kicad_sch (label \"\\\" (at 0 0)))", // unterminated string in body
        ];
        for c in cases {
            for ext in ["kicad_sch", "kicad_pcb", "kicad_sym", "kicad_mod"] {
                let p = format!("fuzz.{ext}");
                let _ = parse_document(Path::new(&p), c);
            }
        }
        // A deeply nested but balanced root must error (depth guard), not overflow.
        let deep = format!("(kicad_sch {}{})", "(".repeat(4000), ")".repeat(4000));
        let _ = parse_document(Path::new("deep.kicad_sch"), &deep);
    }
}
