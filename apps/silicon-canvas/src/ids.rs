//! Index newtypes for the struct-of-arrays [`crate::scene::Scene`].
//!
//! Every entity in the scene is addressed by a `#[repr(transparent)]` `u32`
//! index into its typed array — NOT a heap pointer (SPEC §1: "No per-entity heap
//! objects; ids are indices"). A newtype per entity class makes the type system
//! enforce that a [`NetId`] is never used to index the wire array, etc., at zero
//! runtime cost (the wrapper is laid out identically to a `u32`).
//!
//! Conventions shared by every id:
//!   - `repr(transparent)` over a single `u32` — same ABI/layout as a `u32`, so
//!     a `&[NetId]` can be cast to `&[u32]` and uploaded to the GPU as-is.
//!   - `new(u32)` / `index(self) -> usize` / `raw(self) -> u32` accessors.
//!   - `Copy`, `Ord`, `Hash`, `serde` — ids are keys (graph nodes, R-tree
//!     payloads, selection sets) and travel over IPC.
//!   - A sentinel [`NetId::NONE`] etc. (`u32::MAX`) for "no net" without an
//!     `Option<NetId>` per entity (a billion entities each paying 4 wasted bytes
//!     for the niche is avoided; the SoA arrays store the bare id).
//!
//! These types are the CONTRACT. Downstream module agents (parser, graph, rtree,
//! erc, trace, render, ipc) build against them verbatim and must NOT change them.

use serde::{Deserialize, Serialize};

/// Declare a `#[repr(transparent)]` u32 index newtype with the shared accessor
/// surface, a `NONE` sentinel, and the derives every id needs.
macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash,
            Serialize, Deserialize,
        )]
        #[repr(transparent)]
        #[serde(transparent)]
        pub struct $name(pub u32);

        impl $name {
            /// The "absent" sentinel (`u32::MAX`). Used in the SoA arrays for
            /// entities that carry no value of this id class (e.g. a wire on no
            /// net) so no per-entity `Option` is needed.
            pub const NONE: $name = $name(u32::MAX);

            /// Wrap a raw index.
            #[inline]
            pub const fn new(raw: u32) -> Self {
                $name(raw)
            }

            /// The raw `u32` value.
            #[inline]
            pub const fn raw(self) -> u32 {
                self.0
            }

            /// The value as a `usize` array index.
            #[inline]
            pub const fn index(self) -> usize {
                self.0 as usize
            }

            /// True when this is the [`Self::NONE`] sentinel.
            #[inline]
            pub const fn is_none(self) -> bool {
                self.0 == u32::MAX
            }

            /// True when this id refers to a real entity (not the sentinel).
            #[inline]
            pub const fn is_some(self) -> bool {
                self.0 != u32::MAX
            }

            /// Convert to `Option`, mapping the [`Self::NONE`] sentinel to
            /// `None` for ergonomic call sites that prefer the option form.
            #[inline]
            pub const fn to_option(self) -> Option<$name> {
                if self.is_none() {
                    None
                } else {
                    Some(self)
                }
            }
        }

        impl From<u32> for $name {
            #[inline]
            fn from(raw: u32) -> Self {
                $name(raw)
            }
        }

        impl From<usize> for $name {
            #[inline]
            fn from(raw: usize) -> Self {
                $name(raw as u32)
            }
        }
    };
}

define_id! {
    /// A net (electrical node) — the unit of highlighting and tracing. Carried
    /// by every connectable entity (pad/pin, wire, junction, track, via).
    NetId
}
define_id! {
    /// A placed component / symbol instance (a schematic symbol or a PCB
    /// footprint). Cross-probe uses the shared `ComponentId` across views.
    ComponentId
}
define_id! {
    /// A pad (PCB) or pin (schematic). Both are the connectable terminal of a
    /// [`ComponentId`]; the scene stores them in one `pads` array, with the
    /// owning component recorded per-pad.
    PadId
}
define_id! {
    /// A wire segment (schematic) — a single straight electrical wire.
    WireId
}
define_id! {
    /// A junction dot (schematic) — an explicit connection of crossing wires.
    JunctionId
}
define_id! {
    /// A net/label/text annotation (reference designator, value, net label).
    LabelId
}
define_id! {
    /// A hierarchical sheet symbol (schematic) — a sub-sheet placeholder.
    SheetId
}
define_id! {
    /// A copper track segment (PCB).
    TrackId
}
define_id! {
    /// A via (PCB) — a plated through/blind/buried layer transition.
    ViaId
}
define_id! {
    /// A copper zone / filled polygon (PCB), tessellated once at import.
    ZoneId
}
define_id! {
    /// A graph node id in the connectivity graph ([`crate::graph`]). Distinct
    /// from the entity ids: one logical net contact may map to a graph node that
    /// is a pad, a wire endpoint, or a via. The graph stores the originating
    /// [`EntityRef`] per node.
    NodeId
}

/// The class of a scene entity. Pairs with a raw `u32` in [`EntityRef`] to name
/// any entity uniformly (R-tree payloads, hit-test results, graph nodes, ERC
/// fault locations, selection reports) without a trait object.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    Component,
    /// Pad (PCB) or pin (schematic) — the connectable terminal.
    Pad,
    Wire,
    Junction,
    Label,
    Sheet,
    Track,
    Via,
    Zone,
}

/// A type-tagged reference to one scene entity: its [`EntityKind`] plus its raw
/// index into that class's array. This is the uniform "any entity" handle used
/// by the R-tree payloads, hit-test results, the connectivity graph's per-node
/// origin, ERC fault sites, and the cross-probe path. Lossless: `kind` selects
/// the array, `index` selects the slot.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct EntityRef {
    pub kind: EntityKind,
    pub index: u32,
}

impl EntityRef {
    #[inline]
    pub const fn new(kind: EntityKind, index: u32) -> Self {
        EntityRef { kind, index }
    }

    /// The raw index as a `usize` for array addressing.
    #[inline]
    pub const fn idx(self) -> usize {
        self.index as usize
    }

    #[inline]
    pub fn component(id: ComponentId) -> Self {
        EntityRef::new(EntityKind::Component, id.raw())
    }
    #[inline]
    pub fn pad(id: PadId) -> Self {
        EntityRef::new(EntityKind::Pad, id.raw())
    }
    #[inline]
    pub fn wire(id: WireId) -> Self {
        EntityRef::new(EntityKind::Wire, id.raw())
    }
    #[inline]
    pub fn junction(id: JunctionId) -> Self {
        EntityRef::new(EntityKind::Junction, id.raw())
    }
    #[inline]
    pub fn label(id: LabelId) -> Self {
        EntityRef::new(EntityKind::Label, id.raw())
    }
    #[inline]
    pub fn sheet(id: SheetId) -> Self {
        EntityRef::new(EntityKind::Sheet, id.raw())
    }
    #[inline]
    pub fn track(id: TrackId) -> Self {
        EntityRef::new(EntityKind::Track, id.raw())
    }
    #[inline]
    pub fn via(id: ViaId) -> Self {
        EntityRef::new(EntityKind::Via, id.raw())
    }
    #[inline]
    pub fn zone(id: ZoneId) -> Self {
        EntityRef::new(EntityKind::Zone, id.raw())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_transparent_u32() {
        // repr(transparent) over a single u32: same size/align as u32, so SoA id
        // arrays can be reinterpreted as &[u32] for GPU upload.
        assert_eq!(std::mem::size_of::<NetId>(), std::mem::size_of::<u32>());
        assert_eq!(std::mem::align_of::<NetId>(), std::mem::align_of::<u32>());
    }

    #[test]
    fn none_sentinel_roundtrips() {
        assert!(NetId::NONE.is_none());
        assert!(!NetId::NONE.is_some());
        assert_eq!(NetId::NONE.to_option(), None);
        let n = NetId::new(7);
        assert!(n.is_some());
        assert_eq!(n.index(), 7);
        assert_eq!(n.to_option(), Some(NetId::new(7)));
    }

    #[test]
    fn id_serde_is_bare_number() {
        // #[serde(transparent)]: an id serializes as a plain number, not {"0":n}.
        let j = serde_json::to_string(&ComponentId::new(42)).unwrap();
        assert_eq!(j, "42");
        let back: ComponentId = serde_json::from_str("42").unwrap();
        assert_eq!(back, ComponentId::new(42));
    }

    #[test]
    fn entity_ref_constructors() {
        let r = EntityRef::pad(PadId::new(3));
        assert_eq!(r.kind, EntityKind::Pad);
        assert_eq!(r.idx(), 3);
    }
}
