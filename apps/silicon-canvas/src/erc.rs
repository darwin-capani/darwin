//! Electrical rule checks (SPEC §5).
//!
//! Runs the schematic-side electrical rules over a [`crate::scene::Scene`] and
//! produces a list of [`crate::ops::ErcMarker`]. Invoked at import and on demand
//! via the `erc.run` op (SPEC §6). ERC is ADVISORY and CONSERVATIVE — it flags
//! ONLY what is provable from the netlist + pin metadata, never anything
//! heuristic (SPEC §5). The bias is toward zero false positives: a clean
//! netlist yields zero findings.
//!
//! # Connectivity source
//!
//! The checks reason over the per-entity `net_id` arrays the import pass
//! ([`crate::parser`] + [`crate::graph`]) bakes into the [`crate::scene::Scene`]:
//! every connectable entity (pad, wire, junction, label, track, via, zone)
//! carries the [`crate::ids::NetId`] it was assigned by position-quantized
//! endpoint matching ([`crate::scene::Point::quantize`]). Net membership is
//! therefore fully derivable from the frozen `Scene` itself, which is exactly
//! what these rules need — there is no dependence on the (still-unfrozen)
//! `crate::graph::Graph` type. The single geometric rule that does not key off a
//! net — [`crate::ops::ErcCode::DanglingWire`] — re-derives endpoint coincidence
//! with the same `Point::quantize` key the graph uses, so it agrees with import.
//!
//! # The rules (each maps to an [`crate::ops::ErcCode`])
//!
//!   - [`ErcCode::UnconnectedPin`]    a non-NC pin with no net (warning)
//!   - [`ErcCode::OutputConflict`]    two or more hard drivers on one net (error)
//!   - [`ErcCode::PowerNoDriver`]     a power-in pin on a net with no source (error)
//!   - [`ErcCode::DanglingWire`]      a wire end touching nothing (warning)
//!   - [`ErcCode::DuplicateReference`] two components, one designator (error)
//!   - [`ErcCode::LabelTypo`]         a single-ended named net (warning)
//!
//! Each marker's `at` is the fault coordinate in scene space (the badge anchor;
//! SPEC §5); `code` is [`ErcCode::as_str`]; `severity` is
//! [`ErcCode::default_severity`].
//!
//! # Panic-freedom
//!
//! `run` never panics on any scene (including the empty scene and adversarial
//! parser output): it indexes through `.get(..)`/iterators only and treats the
//! `net_names` table as advisory (a missing entry yields a synthetic name).

use std::collections::HashMap;

use crate::ids::NetId;
use crate::ops::{ErcCode, ErcMarker};
use crate::scene::{LabelKind, PinType, Point, QuantKey, Scene};

/// Run every electrical rule over `scene` and return the findings (SPEC §5).
///
/// The list is ADVISORY and CONSERVATIVE — only provable faults. A clean
/// schematic returns an empty `Vec` (no false positives). Findings are returned
/// in rule order (unconnected → conflict → power → dangling → duplicate → typo)
/// so the panel-side list (SPEC §5) is stable across runs of the same scene.
///
/// The `graph` parameter from the SPEC's suggested signature is intentionally
/// omitted: every rule here is provable from the frozen [`Scene`]'s `net_id`
/// arrays + pin metadata, and the `crate::graph::Graph` type is not part of the
/// frozen contract this module compiles against. If the `ipc` caller already
/// holds a graph it simply ignores it for the ERC call.
pub fn run(scene: &Scene) -> Vec<ErcMarker> {
    let mut markers = Vec::new();

    // Build the per-net summary once; every net-keyed rule reads from it.
    let nets = NetSummaries::build(scene);

    check_unconnected_pins(scene, &nets, &mut markers);
    check_output_conflicts(scene, &nets, &mut markers);
    check_power_no_driver(scene, &nets, &mut markers);
    check_dangling_wires(scene, &mut markers);
    check_duplicate_references(scene, &mut markers);
    check_label_typos(scene, &nets, &mut markers);

    markers
}

/// Convenience constructor used by every rule so the `code`/`severity` pairing
/// stays consistent with [`ErcCode::default_severity`].
fn marker(code: ErcCode, at: Point, message: String) -> ErcMarker {
    ErcMarker {
        code: code.as_str().to_string(),
        severity: code.default_severity(),
        at,
        message,
    }
}

/// The net name for display, falling back to a synthetic `net N` when the
/// `net_names` table is short of the id (defensive against partial scenes).
fn net_label(scene: &Scene, net: NetId) -> String {
    let name = scene.net_name(net);
    if name.is_empty() {
        format!("net {}", net.raw())
    } else {
        name.to_string()
    }
}

// ===========================================================================
// Per-net aggregation.
//
// Several rules need to know what kinds of pin sit on a given net. We tabulate
// that once: for each *named* net (NetId::NONE is the unconnected sentinel and
// is never aggregated here) we count drivers, power producers/consumers, total
// connection points, and remember a representative fault coordinate.
// ===========================================================================

/// Aggregated, per-net facts the net-keyed rules read.
#[derive(Debug, Default, Clone)]
struct NetFacts {
    /// Pads on this net that are HARD drivers — `Output` or `PowerOut`. Two or
    /// more of these on one net is a provable output conflict. (Open-collector,
    /// open-emitter, tri-state, and bidirectional pins are NOT counted: a
    /// wired-OR / shared-bus arrangement of those is legal, so flagging it would
    /// be heuristic, not provable.)
    hard_drivers: Vec<DriverSite>,
    /// Count of `PowerOut` pads on this net — a power *source*.
    power_sources: u32,
    /// Count of `PowerIn` pads on this net — a power *sink* needing a source.
    power_sinks: Vec<Point>,
    /// True if any non-power signal driver (`Output`) sits on the net; such a
    /// driver also satisfies a power-in pin's need for a source.
    has_signal_driver: bool,
    /// True if a power-flag label names this net. KiCad power flags are modeled
    /// as net-bearing labels whose text matches a power-rail convention, or any
    /// label on the net carrying a flag-style kind. A flag satisfies a power-in
    /// pin's source requirement (it tells ERC "this rail is driven elsewhere").
    has_power_flag: bool,
    /// Total connection points on this net (pads + wire ends + junctions +
    /// labels). Used by the single-ended-named-net rule.
    connection_points: u32,
    /// Count of net-bearing labels on this net.
    label_count: u32,
}

/// A driving pin's location + identity, kept so an output-conflict marker can
/// name the offending pin in its message.
#[derive(Debug, Clone)]
struct DriverSite {
    at: Point,
    /// `Reference.pin` for the human message, e.g. "U1.7".
    label: String,
}

/// The per-net table, keyed by raw net index. Built once per `run`.
struct NetSummaries {
    facts: HashMap<u32, NetFacts>,
}

impl NetSummaries {
    fn build(scene: &Scene) -> Self {
        let mut facts: HashMap<u32, NetFacts> = HashMap::new();

        // Pads: drivers, power producers/consumers, connection points.
        for pad in &scene.pads {
            if pad.net_id.is_none() {
                continue;
            }
            let f = facts.entry(pad.net_id.raw()).or_default();
            f.connection_points += 1;
            match pad.pin_type {
                PinType::Output => {
                    f.has_signal_driver = true;
                    f.hard_drivers.push(DriverSite {
                        at: pad.position,
                        label: pin_label(scene, pad),
                    });
                }
                PinType::PowerOut => {
                    f.power_sources += 1;
                    f.hard_drivers.push(DriverSite {
                        at: pad.position,
                        label: pin_label(scene, pad),
                    });
                }
                PinType::PowerIn => {
                    f.power_sinks.push(pad.position);
                }
                _ => {}
            }
        }

        // Wire endpoints: each end is a connection point on the wire's net.
        for wire in &scene.wires {
            if wire.net_id.is_none() {
                continue;
            }
            let f = facts.entry(wire.net_id.raw()).or_default();
            f.connection_points += 2;
        }

        // Junctions.
        for j in &scene.junctions {
            if j.net_id.is_none() {
                continue;
            }
            let f = facts.entry(j.net_id.raw()).or_default();
            f.connection_points += 1;
        }

        // Labels: a connection point, and possibly a power flag for the rail.
        for label in &scene.labels {
            if label.net_id.is_none() {
                continue;
            }
            let f = facts.entry(label.net_id.raw()).or_default();
            f.connection_points += 1;
            f.label_count += 1;
            if is_power_flag(label) {
                f.has_power_flag = true;
            }
        }

        NetSummaries { facts }
    }

    fn get(&self, net: NetId) -> Option<&NetFacts> {
        self.facts.get(&net.raw())
    }
}

/// "Reference.pinname" for a pad, e.g. "U1.7" — used in marker messages. Falls
/// back to "?" when the owning component is out of range (defensive).
fn pin_label(scene: &Scene, pad: &crate::scene::Pad) -> String {
    let reference = scene
        .components
        .get(pad.component.index())
        .map(|c| c.reference.as_str())
        .filter(|r| !r.is_empty())
        .unwrap_or("?");
    format!("{}.{}", reference, pad.name)
}

/// Whether a label functions as a power flag for its net (a marker that says
/// "this rail is driven elsewhere", satisfying a `PowerIn` pin's source need).
///
/// Conservative: KiCad's explicit `PWR_FLAG` symbol is the canonical case, and
/// global power-rail labels (`Global` kind) name a rail that is fed somewhere in
/// the design. We treat a `Global` net label as a power flag, plus any label
/// whose text matches the `PWR_FLAG` convention. We do NOT treat plain local
/// labels as flags — that would let a typo'd local label silence a real
/// power-no-driver error.
fn is_power_flag(label: &crate::scene::Label) -> bool {
    if label.kind == LabelKind::Global {
        return true;
    }
    let t = label.text.trim();
    t.eq_ignore_ascii_case("pwr_flag") || t.eq_ignore_ascii_case("#flg")
}

// ===========================================================================
// Rule 1 — UnconnectedPin (warning).
//
// A non-NoConnect pad whose net has nothing else electrically continuing it:
// no OTHER pad, wire, junction, label, etc. sits on the pad's net. This is the
// real-pipeline-correct test. The import pass ([`crate::graph::apply_nets`])
// hands every ISOLATED pin its OWN synthetic singleton net — so a pin "with a
// net" is NOT proof of connection; the proof is net OCCUPANCY. We reuse the
// per-net connection-point tally `NetSummaries` already computes:
//
//   - `NetId::NONE`  -> no facts entry -> occupancy 0 -> unconnected.
//   - synthetic singleton (the pad alone on its net) -> occupancy 1 ->
//     unconnected. (`connection_points` counts this pad as +1, nothing else.)
//   - net shared with another pad / a wire / a junction / a label -> occupancy
//     >= 2 -> CONNECTED, never flagged.
//
// An NC pin SHOULD float, so it is never flagged (SPEC §5 / PinType doc). The
// rule stays conservative (no false positives): only an occupancy of <= 1 is
// provably unconnected. The single-ended-NAMED-net case is NOT double-flagged
// here — a pad sharing its net with a naming label has occupancy >= 2 (pad +
// label), so it never trips this rule; that case is the separate LabelTypo
// rule, which itself requires occupancy == 1 (the label alone, no pad).
// ===========================================================================

fn check_unconnected_pins(scene: &Scene, nets: &NetSummaries, out: &mut Vec<ErcMarker>) {
    for pad in &scene.pads {
        if pad.pin_type == PinType::NoConnect {
            continue; // an NC pin is meant to float — never a fault.
        }
        // Occupancy of this pad's net: how many connection points sit on it.
        // `NetId::NONE` (or any net absent from the tally) is occupancy 0; a
        // synthetic singleton net carries exactly this pad (occupancy 1). The
        // pad is unconnected iff nothing ELSE continues its net (occupancy <= 1).
        let occupancy = nets.get(pad.net_id).map_or(0, |f| f.connection_points);
        if occupancy > 1 {
            continue; // something else is on the net — provably connected.
        }
        let who = pin_label(scene, pad);
        out.push(marker(
            ErcCode::UnconnectedPin,
            pad.position,
            format!("pin {who} is unconnected"),
        ));
    }
}

// ===========================================================================
// Rule 2 — OutputConflict (error).
//
// Two or more HARD drivers (Output / PowerOut) tied to the same net. Two outputs
// fighting on one node is a provable conflict. Tri-state / open-collector /
// open-emitter / bidirectional are excluded (wired-OR and shared buses are
// legal) — conservative per SPEC §5.
// ===========================================================================

fn check_output_conflicts(scene: &Scene, nets: &NetSummaries, out: &mut Vec<ErcMarker>) {
    // Deterministic order: walk nets by id.
    let mut ids: Vec<u32> = nets.facts.keys().copied().collect();
    ids.sort_unstable();
    for raw in ids {
        let net = NetId::new(raw);
        let f = match nets.get(net) {
            Some(f) => f,
            None => continue,
        };
        if f.hard_drivers.len() < 2 {
            continue;
        }
        let name = net_label(scene, net);
        let drivers: Vec<&str> = f.hard_drivers.iter().map(|d| d.label.as_str()).collect();
        // Anchor the badge at the first driver's location.
        let at = f.hard_drivers[0].at;
        out.push(marker(
            ErcCode::OutputConflict,
            at,
            format!(
                "net {name} has {} drivers ({}) — outputs conflict",
                f.hard_drivers.len(),
                drivers.join(", ")
            ),
        ));
    }
}

// ===========================================================================
// Rule 3 — PowerNoDriver (error).
//
// A net carrying a PowerIn pin but no source: no PowerOut pin, no signal driver,
// and no power-flag label. A power input with nothing feeding it is a provable
// fault. The badge anchors at the first power-in sink.
// ===========================================================================

fn check_power_no_driver(scene: &Scene, nets: &NetSummaries, out: &mut Vec<ErcMarker>) {
    let mut ids: Vec<u32> = nets.facts.keys().copied().collect();
    ids.sort_unstable();
    for raw in ids {
        let net = NetId::new(raw);
        let f = match nets.get(net) {
            Some(f) => f,
            None => continue,
        };
        if f.power_sinks.is_empty() {
            continue; // no power-in pin on this net — rule does not apply.
        }
        let has_source = f.power_sources > 0 || f.has_signal_driver || f.has_power_flag;
        if has_source {
            continue;
        }
        let name = net_label(scene, net);
        let at = f.power_sinks[0];
        out.push(marker(
            ErcCode::PowerNoDriver,
            at,
            format!("power net {name} has a power-input pin but no driving source or power flag"),
        ));
    }
}

// ===========================================================================
// Rule 4 — DanglingWire (warning).
//
// A wire endpoint that coincides with NOTHING else: no pad, no other wire end,
// no junction, no label at that quantized point. A wire stub touching nothing is
// a provable dangle. We re-derive coincidence with Point::quantize so the result
// agrees with the import-time graph (SPEC §1).
//
// Note: an endpoint shared by the SAME wire's other end is still its own point;
// we count occurrences of each quantized point across ALL connection sites, and
// a wire end is dangling only if its point occurs exactly once in the whole
// scene (i.e. only this wire end sits there).
// ===========================================================================

fn check_dangling_wires(scene: &Scene, out: &mut Vec<ErcMarker>) {
    // Tally every connection point in the scene by quantized key.
    let mut tally: HashMap<QuantKey, u32> = HashMap::new();
    fn bump(p: Point, tally: &mut HashMap<QuantKey, u32>) {
        *tally.entry(p.quantize()).or_insert(0) += 1;
    }

    for pad in &scene.pads {
        bump(pad.position, &mut tally);
    }
    for wire in &scene.wires {
        bump(wire.a, &mut tally);
        bump(wire.b, &mut tally);
    }
    for j in &scene.junctions {
        bump(j.position, &mut tally);
    }
    for label in &scene.labels {
        // Only net-bearing labels make an electrical contact; pure text
        // (Reference/Value/Text) does not anchor a wire.
        if matches!(
            label.kind,
            LabelKind::Local | LabelKind::Global | LabelKind::Hierarchical
        ) {
            bump(label.position, &mut tally);
        }
    }

    // A wire end is dangling iff its quantized point is occupied only by itself.
    for wire in &scene.wires {
        for end in [wire.a, wire.b] {
            let count = tally.get(&end.quantize()).copied().unwrap_or(0);
            if count <= 1 {
                out.push(marker(
                    ErcCode::DanglingWire,
                    end,
                    "wire end connects to nothing".to_string(),
                ));
            }
        }
    }
}

// ===========================================================================
// Rule 5 — DuplicateReference (error).
//
// Two (or more) components sharing one non-empty reference designator. Unique
// designators are a hard schematic invariant, so a collision is a provable
// error. One marker per extra collision, anchored at the duplicate's position.
// ===========================================================================

fn check_duplicate_references(scene: &Scene, out: &mut Vec<ErcMarker>) {
    // References seen so far; a second sighting is the duplicate.
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for comp in &scene.components {
        let r = comp.reference.trim();
        if r.is_empty() {
            continue; // an unannotated symbol is not a duplicate of anything.
        }
        if !seen.insert(r) {
            // insert returned false -> `r` was already present: a duplicate.
            out.push(marker(
                ErcCode::DuplicateReference,
                comp.position,
                format!("duplicate reference designator {r:?}"),
            ));
        }
    }
}

// ===========================================================================
// Rule 6 — LabelTypo / single-ended named net (warning).
//
// A named net that is single-ended: a net label appears, but the net it names
// has exactly one connection point in total (just that label) — a likely typo
// where the label does not actually join the wire/pin it was meant to. We flag
// ONLY when:
//   - the net has at least one net-bearing label, AND
//   - the net's total connection-point count is exactly 1.
// That is provably single-ended; any net with two or more contacts is a real
// connection and never flagged (no false positive on legitimate nets).
// ===========================================================================

fn check_label_typos(scene: &Scene, nets: &NetSummaries, out: &mut Vec<ErcMarker>) {
    for label in &scene.labels {
        if label.net_id.is_none() {
            continue;
        }
        // Only net-naming labels participate (not Reference/Value/Text).
        if !matches!(
            label.kind,
            LabelKind::Local | LabelKind::Global | LabelKind::Hierarchical
        ) {
            continue;
        }
        let f = match nets.get(label.net_id) {
            Some(f) => f,
            None => continue,
        };
        // Single-ended: the label is the ONLY thing on its net.
        if f.label_count >= 1 && f.connection_points == 1 {
            let name = net_label(scene, label.net_id);
            out.push(marker(
                ErcCode::LabelTypo,
                label.position,
                format!("net label {name:?} names a single-ended net (connects to nothing else)"),
            ));
        }
    }
}

// ===========================================================================
// Tests — small embedded fixtures. A clean netlist yields ZERO findings; each
// crafted netlist triggers exactly its one rule.
// ===========================================================================
#[cfg(test)]
mod tests {
    // `super::*` already brings in NetId, ErcCode, ErcMarker, LabelKind, PinType,
    // Point, Scene. Pull the remaining scene/ops types the fixtures need.
    use super::*;
    use crate::ids::ComponentId;
    use crate::ops::ErcSeverity;
    use crate::scene::{Aabb, Component, Label, LayerId, Pad, PadShape, SceneKind, Wire};

    // ---- fixture builders ------------------------------------------------

    fn comp(reference: &str, at: Point) -> Component {
        Component {
            reference: reference.to_string(),
            value: String::new(),
            lib_id: "Device:R".to_string(),
            position: at,
            rotation: 0.0,
            mirror: false,
            bbox: Aabb::EMPTY,
            layer: LayerId::SCHEMATIC,
        }
    }

    fn pad(component: u32, name: &str, at: Point, pin_type: PinType, net: NetId) -> Pad {
        Pad {
            component: ComponentId::new(component),
            name: name.to_string(),
            position: at,
            size: (1.0, 1.0),
            shape: PadShape::Circle,
            pin_type,
            layer: LayerId::SCHEMATIC,
            net_id: net,
        }
    }

    fn wire(a: Point, b: Point, net: NetId) -> Wire {
        Wire { a, b, net_id: net }
    }

    fn label(text: &str, at: Point, kind: LabelKind, net: NetId) -> Label {
        Label {
            text: text.to_string(),
            position: at,
            rotation: 0.0,
            kind,
            net_id: net,
        }
    }

    /// Codes present in a finding list, for terse assertions.
    fn codes(markers: &[ErcMarker]) -> Vec<&str> {
        markers.iter().map(|m| m.code.as_str()).collect()
    }

    fn count(markers: &[ErcMarker], code: ErcCode) -> usize {
        markers
            .iter()
            .filter(|m| m.code == code.as_str())
            .count()
    }

    /// A clean two-net schematic: a driver→sink signal net and a power net with
    /// a real source. No rule should fire.
    ///
    /// Layout (net ids: 1 = SIG, 2 = 3V3):
    ///   U1.1 (Output)  --wire-- R1.1 (Input)        on net SIG, both at shared pts
    ///   U2.1 (PowerOut)--wire-- R1.2 (PowerIn)       on net 3V3
    fn clean_scene() -> Scene {
        let mut s = Scene::new(SceneKind::Schematic);
        s.net_names = vec![
            String::new(),     // 0: no-net
            "SIG".to_string(), // 1
            "3V3".to_string(), // 2
        ];
        let sig = NetId::new(1);
        let v3 = NetId::new(2);

        s.components.push(comp("U1", Point::new(0.0, 0.0)));
        s.components.push(comp("U2", Point::new(0.0, 10.0)));
        s.components.push(comp("R1", Point::new(20.0, 0.0)));

        // SIG net: an output driving an input, wired between the two pin points.
        let u1_out = Point::new(2.0, 0.0);
        let r1_in = Point::new(18.0, 0.0);
        s.pads.push(pad(0, "1", u1_out, PinType::Output, sig));
        s.pads.push(pad(2, "1", r1_in, PinType::Input, sig));
        s.wires.push(wire(u1_out, r1_in, sig));

        // 3V3 net: a power source feeding a power-in pin, with a wire between.
        let u2_pwr = Point::new(2.0, 10.0);
        let r1_pwr = Point::new(18.0, 10.0);
        s.pads.push(pad(1, "1", u2_pwr, PinType::PowerOut, v3));
        s.pads.push(pad(2, "2", r1_pwr, PinType::PowerIn, v3));
        s.wires.push(wire(u2_pwr, r1_pwr, v3));

        s.init_flags();
        s
    }

    #[test]
    fn clean_netlist_has_zero_findings() {
        let s = clean_scene();
        let markers = run(&s);
        assert!(
            markers.is_empty(),
            "clean netlist must produce no findings, got: {:?}",
            codes(&markers)
        );
    }

    #[test]
    fn empty_scene_is_clean_and_does_not_panic() {
        let s = Scene::new(SceneKind::Schematic);
        assert!(run(&s).is_empty());
        // Default (never-initialized) scene must also be safe.
        let s = Scene::default();
        assert!(run(&s).is_empty());
    }

    #[test]
    fn unconnected_pin_flagged_nc_pin_ignored() {
        let mut s = clean_scene();
        // Add an input pin on no net (unconnected) — should flag.
        s.components.push(comp("R2", Point::new(40.0, 0.0)));
        s.pads
            .push(pad(3, "1", Point::new(42.0, 0.0), PinType::Input, NetId::NONE));
        // Add an NC pin on no net — must NOT flag.
        s.pads
            .push(pad(3, "2", Point::new(42.0, 5.0), PinType::NoConnect, NetId::NONE));
        s.init_flags();

        let markers = run(&s);
        assert_eq!(
            count(&markers, ErcCode::UnconnectedPin),
            1,
            "exactly one unconnected pin (NC must be ignored): {:?}",
            markers
        );
        // No OTHER rule should have fired.
        assert_eq!(markers.len(), 1, "only the unconnected-pin rule: {:?}", codes(&markers));
        let m = &markers[0];
        assert_eq!(m.severity, ErcSeverity::Warning);
        assert_eq!(m.at, Point::new(42.0, 0.0));
        assert!(m.message.contains("R2.1"), "message names the pin: {}", m.message);
    }

    #[test]
    fn unconnected_pin_real_pipeline_synthetic_nets() {
        // THE crux test: drive the REAL import pipeline so net ids are the
        // SYNTHETIC ones graph::apply_nets mints (every isolated pin gets its
        // OWN singleton net) — NOT hand-set NetId::NONE. The rule must still
        // flag a truly isolated non-NC pin, never flag a pin that shares its
        // net with a wire/another pin, and never flag an NC pin.
        let mut s = Scene::new(SceneKind::Schematic);
        s.components.push(comp("U1", Point::new(0.0, 0.0)));
        s.components.push(comp("U2", Point::new(20.0, 0.0)));
        s.components.push(comp("U3", Point::new(40.0, 0.0)));

        // CONNECTED: two pins joined by a wire at their pin points — one shared
        // net carrying 2 pads + 1 wire. Passive type so no other rule fires.
        let ca = Point::new(2.0, 0.0);
        let cb = Point::new(18.0, 0.0);
        s.pads.push(pad(0, "1", ca, PinType::Passive, NetId::NONE));
        s.pads.push(pad(1, "1", cb, PinType::Passive, NetId::NONE));
        s.wires.push(wire(ca, cb, NetId::NONE));

        // ISOLATED non-NC pin: an Input touching nothing — must flag.
        let lonely = Point::new(42.0, 0.0);
        s.pads.push(pad(2, "IN", lonely, PinType::Input, NetId::NONE));

        // ISOLATED NC pin: floats by design — must NOT flag.
        let nc = Point::new(42.0, 5.0);
        s.pads.push(pad(2, "NC", nc, PinType::NoConnect, NetId::NONE));

        // Run the REAL pipeline: graph build + apply_nets writes SYNTHETIC nets
        // back into the scene (the isolated pins get singleton nets, NOT NONE).
        s.init_flags();
        let g = crate::graph::Graph::build(&s);
        g.apply_nets(&mut s);

        // Sanity: the isolated pins did NOT stay NONE — they got real synthetic
        // singleton nets. This is exactly the condition that defeated the old
        // `is_none()`-only rule.
        assert!(
            s.pads[2].net_id.is_some(),
            "isolated input pin must have a synthetic singleton net, not NONE"
        );
        assert!(
            s.pads[3].net_id.is_some(),
            "isolated NC pin must have a synthetic singleton net, not NONE"
        );
        // The two connected pads landed on ONE shared net.
        assert_eq!(
            s.pads[0].net_id, s.pads[1].net_id,
            "the wired pair must share a net"
        );

        let markers = run(&s);
        assert_eq!(
            count(&markers, ErcCode::UnconnectedPin),
            1,
            "exactly the isolated non-NC pin flags (NC ignored, wired pair connected): {:?}",
            markers
        );
        assert_eq!(
            markers.len(),
            1,
            "only the unconnected-pin rule fires on this scene: {:?}",
            codes(&markers)
        );
        let m = &markers[0];
        assert_eq!(m.severity, ErcSeverity::Warning);
        assert_eq!(m.at, lonely, "badge anchors at the isolated pin");
        assert!(m.message.contains("U3.IN"), "message names the pin: {}", m.message);
    }

    #[test]
    fn output_conflict_flagged_two_drivers() {
        let mut s = Scene::new(SceneKind::Schematic);
        s.net_names = vec![String::new(), "BUS".to_string()];
        let bus = NetId::new(1);
        s.components.push(comp("U1", Point::new(0.0, 0.0)));
        s.components.push(comp("U2", Point::new(10.0, 0.0)));
        // Two outputs on one net, joined by a wire so neither is unconnected
        // and the net is not single-ended.
        let a = Point::new(2.0, 0.0);
        let b = Point::new(8.0, 0.0);
        s.pads.push(pad(0, "Y", a, PinType::Output, bus));
        s.pads.push(pad(1, "Y", b, PinType::Output, bus));
        s.wires.push(wire(a, b, bus));
        s.init_flags();

        let markers = run(&s);
        assert_eq!(count(&markers, ErcCode::OutputConflict), 1, "{:?}", codes(&markers));
        assert_eq!(markers.len(), 1, "only the conflict rule: {:?}", codes(&markers));
        let m = markers.iter().find(|m| m.code == ErcCode::OutputConflict.as_str()).unwrap();
        assert_eq!(m.severity, ErcSeverity::Error);
        assert!(m.message.contains("U1.Y") && m.message.contains("U2.Y"), "{}", m.message);
    }

    #[test]
    fn output_conflict_not_flagged_for_open_collector_wired_or() {
        // Two open-collector outputs on one net = legal wired-OR, NOT a conflict.
        let mut s = Scene::new(SceneKind::Schematic);
        s.net_names = vec![String::new(), "OD".to_string()];
        let od = NetId::new(1);
        s.components.push(comp("U1", Point::new(0.0, 0.0)));
        s.components.push(comp("U2", Point::new(10.0, 0.0)));
        let a = Point::new(2.0, 0.0);
        let b = Point::new(8.0, 0.0);
        s.pads.push(pad(0, "Y", a, PinType::OpenCollector, od));
        s.pads.push(pad(1, "Y", b, PinType::OpenCollector, od));
        s.wires.push(wire(a, b, od));
        s.init_flags();

        assert_eq!(
            count(&run(&s), ErcCode::OutputConflict),
            0,
            "open-collector wired-OR must not be flagged"
        );
    }

    #[test]
    fn power_no_driver_flagged_and_satisfied_by_source() {
        // Net with a PowerIn pin and NO source -> flagged.
        let mut s = Scene::new(SceneKind::Schematic);
        s.net_names = vec![String::new(), "5V".to_string()];
        let v5 = NetId::new(1);
        s.components.push(comp("U1", Point::new(0.0, 0.0)));
        s.components.push(comp("U2", Point::new(10.0, 0.0)));
        let a = Point::new(2.0, 0.0);
        let b = Point::new(8.0, 0.0);
        // Two power-in pins, no source, wired together (so not unconnected, not
        // single-ended).
        s.pads.push(pad(0, "VCC", a, PinType::PowerIn, v5));
        s.pads.push(pad(1, "VCC", b, PinType::PowerIn, v5));
        s.wires.push(wire(a, b, v5));
        s.init_flags();

        let markers = run(&s);
        assert_eq!(count(&markers, ErcCode::PowerNoDriver), 1, "{:?}", codes(&markers));
        assert_eq!(markers.len(), 1, "only the power rule fires: {:?}", codes(&markers));
        assert_eq!(
            markers[0].severity,
            ErcSeverity::Error,
            "power-no-driver is an error"
        );

        // Now add a PowerOut source on the same net -> the rule must clear.
        s.components.push(comp("U3", Point::new(20.0, 0.0)));
        let src = Point::new(8.0, 0.0); // coincides with b, joining the net
        s.pads.push(pad(2, "OUT", src, PinType::PowerOut, v5));
        s.init_flags();
        assert_eq!(
            count(&run(&s), ErcCode::PowerNoDriver),
            0,
            "a PowerOut source must satisfy the power-in pins"
        );
    }

    #[test]
    fn power_no_driver_satisfied_by_power_flag_label() {
        // A global power-flag label feeds a power-in pin -> no power-no-driver.
        let mut s = Scene::new(SceneKind::Schematic);
        s.net_names = vec![String::new(), "3V3".to_string()];
        let v3 = NetId::new(1);
        s.components.push(comp("U1", Point::new(0.0, 0.0)));
        let p = Point::new(2.0, 0.0);
        s.pads.push(pad(0, "VDD", p, PinType::PowerIn, v3));
        // A global label on the same net = the rail is fed elsewhere.
        s.labels.push(label("3V3", p, LabelKind::Global, v3));
        s.init_flags();

        let markers = run(&s);
        assert_eq!(
            count(&markers, ErcCode::PowerNoDriver),
            0,
            "a global power-flag label satisfies the power-in pin: {:?}",
            codes(&markers)
        );
    }

    #[test]
    fn dangling_wire_flagged_connected_end_clean() {
        // One wire: end `a` lands on a pad (connected); end `b` lands on nothing.
        let mut s = Scene::new(SceneKind::Schematic);
        s.net_names = vec![String::new(), "N".to_string()];
        let n = NetId::new(1);
        s.components.push(comp("U1", Point::new(0.0, 0.0)));
        let connected = Point::new(0.0, 0.0);
        let dangling = Point::new(5.0, 0.0);
        // A pad on the net at `connected`, with a Passive type so no other rule
        // fires from it.
        s.pads.push(pad(0, "1", connected, PinType::Passive, n));
        s.wires.push(wire(connected, dangling, n));
        s.init_flags();

        let markers = run(&s);
        assert_eq!(count(&markers, ErcCode::DanglingWire), 1, "{:?}", codes(&markers));
        assert_eq!(markers.len(), 1, "only the dangling-wire rule: {:?}", codes(&markers));
        let m = &markers[0];
        assert_eq!(m.severity, ErcSeverity::Warning);
        assert_eq!(m.at, dangling, "badge anchors at the dangling end");
    }

    #[test]
    fn dangling_wire_not_flagged_when_ends_meet() {
        // Two wires meeting end-to-end at a shared point: neither end dangles.
        let mut s = Scene::new(SceneKind::Schematic);
        s.net_names = vec![String::new(), "N".to_string()];
        let n = NetId::new(1);
        s.components.push(comp("U1", Point::new(0.0, 0.0)));
        s.components.push(comp("U2", Point::new(20.0, 0.0)));
        let p0 = Point::new(0.0, 0.0);
        let mid = Point::new(10.0, 0.0);
        let p1 = Point::new(20.0, 0.0);
        // Pads cap both far ends so only the shared midpoint matters.
        s.pads.push(pad(0, "1", p0, PinType::Passive, n));
        s.pads.push(pad(1, "1", p1, PinType::Passive, n));
        s.wires.push(wire(p0, mid, n));
        s.wires.push(wire(mid, p1, n));
        s.init_flags();

        assert_eq!(
            count(&run(&s), ErcCode::DanglingWire),
            0,
            "wires that meet end-to-end (shared point) do not dangle"
        );
    }

    #[test]
    fn duplicate_reference_flagged_blanks_ignored() {
        let mut s = Scene::new(SceneKind::Schematic);
        // Two components both "R1" -> one duplicate finding.
        s.components.push(comp("R1", Point::new(0.0, 0.0)));
        s.components.push(comp("R1", Point::new(10.0, 0.0)));
        // Two unannotated (blank) components must NOT be flagged as dups.
        s.components.push(comp("", Point::new(20.0, 0.0)));
        s.components.push(comp("", Point::new(30.0, 0.0)));
        s.init_flags();

        let markers = run(&s);
        assert_eq!(count(&markers, ErcCode::DuplicateReference), 1, "{:?}", codes(&markers));
        assert_eq!(markers.len(), 1, "only the duplicate rule: {:?}", codes(&markers));
        let m = &markers[0];
        assert_eq!(m.severity, ErcSeverity::Error);
        assert_eq!(m.at, Point::new(10.0, 0.0), "anchors at the second (duplicate) R1");
        assert!(m.message.contains("R1"), "{}", m.message);
    }

    #[test]
    fn label_typo_single_ended_net_flagged() {
        // A local label naming a net with NOTHING else on it -> single-ended.
        let mut s = Scene::new(SceneKind::Schematic);
        s.net_names = vec![String::new(), "MISO".to_string()];
        let miso = NetId::new(1);
        let at = Point::new(5.0, 5.0);
        s.labels.push(label("MISO", at, LabelKind::Local, miso));
        s.init_flags();

        let markers = run(&s);
        assert_eq!(count(&markers, ErcCode::LabelTypo), 1, "{:?}", codes(&markers));
        assert_eq!(markers.len(), 1, "only the label-typo rule: {:?}", codes(&markers));
        let m = &markers[0];
        assert_eq!(m.severity, ErcSeverity::Warning);
        assert_eq!(m.at, at);
        assert!(m.message.contains("MISO"), "{}", m.message);
    }

    #[test]
    fn label_typo_not_flagged_when_net_has_other_contacts() {
        // A label naming a net that ALSO carries a pad + wire = a real net.
        let mut s = Scene::new(SceneKind::Schematic);
        s.net_names = vec![String::new(), "SCK".to_string()];
        let sck = NetId::new(1);
        s.components.push(comp("U1", Point::new(0.0, 0.0)));
        let p = Point::new(0.0, 0.0);
        let lp = Point::new(3.0, 0.0);
        s.pads.push(pad(0, "1", p, PinType::Passive, sck));
        s.wires.push(wire(p, lp, sck));
        s.labels.push(label("SCK", lp, LabelKind::Local, sck));
        s.init_flags();

        assert_eq!(
            count(&run(&s), ErcCode::LabelTypo),
            0,
            "a named net with a pad + wire is a real connection, not a typo"
        );
    }

    #[test]
    fn markers_serialize_to_spec_shape() {
        // Sanity: a finding round-trips through the ops::ErcMarker wire shape
        // SPEC §5 declares ({code, severity, at, message}).
        let mut s = Scene::new(SceneKind::Schematic);
        s.net_names = vec![String::new(), "X".to_string()];
        s.labels
            .push(label("X", Point::new(1.0, 2.0), LabelKind::Local, NetId::new(1)));
        s.init_flags();
        let markers = run(&s);
        assert_eq!(markers.len(), 1);
        let j = serde_json::to_string(&markers[0]).unwrap();
        assert!(j.contains(r#""code":"label_typo""#), "{j}");
        assert!(j.contains(r#""severity":"warning""#), "{j}");
        assert!(j.contains(r#""at":{"x":1.0,"y":2.0}"#), "{j}");
        assert!(j.contains(r#""message""#), "{j}");
    }

    #[test]
    fn rule_ordering_is_stable() {
        // A scene that trips several rules: order must be unconnected, conflict,
        // power, dangling, duplicate, typo (rule order, SPEC §5 panel stability).
        let mut s = Scene::new(SceneKind::Schematic);
        s.net_names = vec![String::new(), "BUS".to_string(), "DEAD".to_string()];
        let bus = NetId::new(1);
        let dead = NetId::new(2);
        s.components.push(comp("U1", Point::new(0.0, 0.0)));
        s.components.push(comp("U2", Point::new(10.0, 0.0)));
        s.components.push(comp("U1", Point::new(20.0, 0.0))); // duplicate ref
        // Unconnected input pin.
        s.pads
            .push(pad(0, "IN", Point::new(0.0, 5.0), PinType::Input, NetId::NONE));
        // Output conflict on BUS.
        let a = Point::new(1.0, 0.0);
        let b = Point::new(9.0, 0.0);
        s.pads.push(pad(0, "Y", a, PinType::Output, bus));
        s.pads.push(pad(1, "Y", b, PinType::Output, bus));
        s.wires.push(wire(a, b, bus));
        // Dangling wire (end at 50,50 touches nothing).
        s.wires
            .push(wire(a, Point::new(50.0, 50.0), bus));
        // Single-ended named net DEAD.
        s.labels
            .push(label("DEAD", Point::new(30.0, 0.0), LabelKind::Local, dead));
        s.init_flags();

        let markers = run(&s);
        let got = codes(&markers);
        // Find the first index of each code; assert the rule-group ordering.
        let pos = |c: ErcCode| got.iter().position(|x| *x == c.as_str());
        let unc = pos(ErcCode::UnconnectedPin).expect("unconnected present");
        let conf = pos(ErcCode::OutputConflict).expect("conflict present");
        let dang = pos(ErcCode::DanglingWire).expect("dangling present");
        let dup = pos(ErcCode::DuplicateReference).expect("duplicate present");
        let typo = pos(ErcCode::LabelTypo).expect("typo present");
        assert!(unc < conf, "unconnected before conflict: {got:?}");
        assert!(conf < dang, "conflict before dangling: {got:?}");
        assert!(dang < dup, "dangling before duplicate: {got:?}");
        assert!(dup < typo, "duplicate before typo: {got:?}");
    }
}
