//! The routing matrix: the authoritative [`MatrixState`] (FROZEN shared type),
//! its mutation API (the state-machine seam module agents flesh out), and the
//! lock-free [`SnapshotRing`] that hands immutable [`MatrixSnapshot`]s across
//! the realtime boundary (SPEC §1: "no shared mutable state across the realtime
//! boundary (SPSC ring of state snapshots)").
//!
//! Ownership split (DO NOT collapse it):
//!   - [`MatrixState`] lives on the CONTROL side. Every IPC op mutates it.
//!   - [`MatrixSnapshot`] is an immutable, `Copy`-of-the-grid value the control
//!     side `publish()`es into the ring; the audio thread `load()`s the latest
//!     one each callback. The audio thread NEVER touches `MatrixState`.
//!   - The ring is SINGLE-producer (control) / SINGLE-consumer (audio), wait-
//!     free on the consumer and lock-free on the producer (one `AtomicU64` seq +
//!     a small slot array). The publish writes a slot then bumps the sequence;
//!     the load reads the newest committed sequence's slot. No alloc, no lock,
//!     no syscall on either side — safe to call from the IOProc.
//!
//! The grid is fixed-capacity (`MAX_CHANNELS` x `MAX_CHANNELS`) so a snapshot is
//! a flat POD blob — no heap, no `Arc`, copyable into a ring slot. Active
//! input/output counts gate which crosspoints the mix actually reads.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{NexusError, Result};
use crate::types::{GAIN_MAX_DB, GAIN_OFF_DB, MAX_CHANNELS};

/// One crosspoint's gain in dB, or [`GAIN_OFF_DB`] (`-inf`) when the route is
/// off. SPEC §1: "float per crosspoint, -inf to +12 dB. Routes are crosspoints
/// above -inf." A transparent alias so call sites read as audio, not as bare f32.
pub type CrosspointGainDb = f32;

/// The flat crosspoint grid: `grid[input][output]` gain in dB. Fixed
/// `MAX_CHANNELS` square so the whole thing is POD and snapshot-copyable. Unused
/// rows/cols (beyond the active counts) hold [`GAIN_OFF_DB`].
pub type CrosspointGrid = [[CrosspointGainDb; MAX_CHANNELS]; MAX_CHANNELS];

/// Per-channel mute flags (true = muted). Inputs and outputs each get a row.
pub type MuteFlags = [bool; MAX_CHANNELS];

/// The authoritative routing-matrix state (SPEC §1 "State machine"). ONE of
/// these exists on the control side; every mutation goes through its methods so
/// validation + invariants live in one place. It is NOT `Copy` (it is the
/// mutable master); the realtime side reads [`MatrixSnapshot`]s instead.
#[derive(Debug, Clone, PartialEq)]
pub struct MatrixState {
    /// Number of active input channels (`<= MAX_CHANNELS`).
    inputs: usize,
    /// Number of active output channels (`<= MAX_CHANNELS`).
    outputs: usize,
    /// Crosspoint gains in dB; `grid[i][o]` routes input `i` to output `o`.
    grid: CrosspointGrid,
    /// Per-input mutes (true = that input contributes nothing).
    input_mutes: MuteFlags,
    /// Per-output mutes (true = that output is silenced).
    output_mutes: MuteFlags,
    /// The output index designated as the monitor bus, if any (SPEC §1 "monitor
    /// assignment"; the direct-monitor route lands here). `None` = no monitor.
    monitor_output: Option<usize>,
    /// Monotonic revision bumped on every accepted mutation — lets the publisher
    /// skip a snapshot when nothing changed, and lets telemetry coalesce.
    revision: u64,
}

impl MatrixState {
    /// A new matrix with `inputs` x `outputs` active channels, every crosspoint
    /// OFF (`-inf`), nothing muted, no monitor assigned. Errors if either count
    /// exceeds [`MAX_CHANNELS`].
    pub fn new(inputs: usize, outputs: usize) -> Result<Self> {
        if inputs > MAX_CHANNELS {
            return Err(NexusError::OutOfBounds { what: "inputs", got: inputs, limit: MAX_CHANNELS });
        }
        if outputs > MAX_CHANNELS {
            return Err(NexusError::OutOfBounds { what: "outputs", got: outputs, limit: MAX_CHANNELS });
        }
        Ok(Self {
            inputs,
            outputs,
            grid: [[GAIN_OFF_DB; MAX_CHANNELS]; MAX_CHANNELS],
            input_mutes: [false; MAX_CHANNELS],
            output_mutes: [false; MAX_CHANNELS],
            monitor_output: None,
            revision: 0,
        })
    }

    /// Active input count.
    pub fn inputs(&self) -> usize {
        self.inputs
    }
    /// Active output count.
    pub fn outputs(&self) -> usize {
        self.outputs
    }
    /// Current revision (bumped per accepted mutation).
    pub fn revision(&self) -> u64 {
        self.revision
    }
    /// The crosspoint gain in dB for `input` -> `output` (no bounds mutation;
    /// returns `Err` if either index is out of range).
    pub fn crosspoint(&self, input: usize, output: usize) -> Result<CrosspointGainDb> {
        self.check_in(input)?;
        self.check_out(output)?;
        Ok(self.grid[input][output])
    }
    /// The monitor output index, if assigned.
    pub fn monitor_output(&self) -> Option<usize> {
        self.monitor_output
    }

    /// Set one crosspoint's gain (SPEC §5 `route.set`). `gain_db == -inf`
    /// ([`GAIN_OFF_DB`]) clears the route. Validates indices and that the value
    /// is either the `-inf` sentinel or finite and `<= +12 dB`. Bumps revision.
    pub fn set_crosspoint(&mut self, input: usize, output: usize, gain_db: f32) -> Result<()> {
        self.check_in(input)?;
        self.check_out(output)?;
        // -inf is the explicit "off" sentinel; any other non-finite is invalid.
        if !(gain_db == GAIN_OFF_DB || (gain_db.is_finite() && gain_db <= GAIN_MAX_DB)) {
            return Err(NexusError::InvalidParam {
                param: "gain_db",
                reason: "must be -inf (off) or finite and <= +12 dB",
            });
        }
        self.grid[input][output] = gain_db;
        self.revision += 1;
        Ok(())
    }

    /// Mute/unmute an input (SPEC §5 — a `gain.set`/voice "mute the mic" lands
    /// here). Bumps revision.
    pub fn set_input_mute(&mut self, input: usize, muted: bool) -> Result<()> {
        self.check_in(input)?;
        self.input_mutes[input] = muted;
        self.revision += 1;
        Ok(())
    }

    /// Mute/unmute an output. Bumps revision.
    pub fn set_output_mute(&mut self, output: usize, muted: bool) -> Result<()> {
        self.check_out(output)?;
        self.output_mutes[output] = muted;
        self.revision += 1;
        Ok(())
    }

    /// Assign (or clear, with `None`) the monitor output bus (SPEC §1/§5
    /// `monitor.set`). Bumps revision.
    pub fn set_monitor_output(&mut self, output: Option<usize>) -> Result<()> {
        if let Some(o) = output {
            self.check_out(o)?;
        }
        self.monitor_output = output;
        self.revision += 1;
        Ok(())
    }

    /// Materialize an immutable [`MatrixSnapshot`] of the current state for the
    /// realtime side. Pure copy; cheap (a fixed POD blob).
    pub fn snapshot(&self) -> MatrixSnapshot {
        MatrixSnapshot {
            inputs: self.inputs,
            outputs: self.outputs,
            grid: self.grid,
            input_mutes: self.input_mutes,
            output_mutes: self.output_mutes,
            monitor_output: self.monitor_output,
            revision: self.revision,
        }
    }

    fn check_in(&self, input: usize) -> Result<()> {
        if input >= self.inputs {
            return Err(NexusError::OutOfBounds { what: "input index", got: input, limit: self.inputs });
        }
        Ok(())
    }
    fn check_out(&self, output: usize) -> Result<()> {
        if output >= self.outputs {
            return Err(NexusError::OutOfBounds { what: "output index", got: output, limit: self.outputs });
        }
        Ok(())
    }
}

/// An immutable, `Copy` snapshot of [`MatrixState`] the audio thread reads each
/// callback. POD (fixed-size grid + flags) so it copies into a ring slot with no
/// allocation. The audio thread treats `input_mutes`/`output_mutes` and the grid
/// as the complete routing truth for the block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MatrixSnapshot {
    /// Active input count (the mix reads rows `0..inputs`).
    pub inputs: usize,
    /// Active output count (the mix writes cols `0..outputs`).
    pub outputs: usize,
    /// `grid[input][output]` gain in dB ([`GAIN_OFF_DB`] = no route).
    pub grid: CrosspointGrid,
    /// Per-input mutes.
    pub input_mutes: MuteFlags,
    /// Per-output mutes.
    pub output_mutes: MuteFlags,
    /// Monitor output bus, if assigned.
    pub monitor_output: Option<usize>,
    /// The state revision this snapshot was taken at.
    pub revision: u64,
}

impl MatrixSnapshot {
    /// An all-off snapshot (nothing routed) for initializing the ring before the
    /// control side publishes the first real state.
    pub fn silent(inputs: usize, outputs: usize) -> Self {
        Self {
            inputs,
            outputs,
            grid: [[GAIN_OFF_DB; MAX_CHANNELS]; MAX_CHANNELS],
            input_mutes: [false; MAX_CHANNELS],
            output_mutes: [false; MAX_CHANNELS],
            monitor_output: None,
            revision: 0,
        }
    }
}

/// Number of snapshot slots in the lock-free ring. A small power of two: the
/// producer is slow (one publish per accepted op, human-rate) and the consumer
/// is fast (one load per ~1.33 ms callback), so a tiny ring with seqlock-style
/// versioning never starves. Sized so a publish in flight while the audio thread
/// loads cannot tear (the consumer retries on an odd/again-changed sequence).
pub const SNAPSHOT_RING_SLOTS: usize = 4;

/// A single-producer / single-consumer lock-free ring of [`MatrixSnapshot`]s —
/// the realtime boundary from SPEC §1. The CONTROL thread is the sole producer
/// ([`SnapshotRing::publish`]); the AUDIO thread is the sole consumer
/// ([`SnapshotRing::load`], wait-free, no alloc/lock/syscall).
///
/// Mechanism (seqlock per the SPSC discipline): a monotonic `seq` counts
/// published snapshots. `publish` writes into slot `seq % SLOTS` then stores the
/// incremented `seq` with `Release`. `load` reads `seq` with `Acquire`, copies
/// the slot, re-reads `seq`, and retries if it moved — so the consumer never
/// observes a half-written slot. With only this one producer and a 4-slot ring,
/// a retry is vanishingly rare and always bounded.
pub struct SnapshotRing {
    /// Published-count / version. Even cadence isn't required (it's a count, not
    /// a classic even/odd seqlock) because there is exactly ONE producer; the
    /// consumer's re-read-and-compare guards against a slot being overwritten
    /// mid-copy.
    seq: AtomicU64,
    /// The snapshot slots. `UnsafeCell` because the producer writes while the
    /// consumer may read; correctness is provided by the seq protocol, not by
    /// the borrow checker. SAFETY is documented at each access.
    slots: [std::cell::UnsafeCell<MatrixSnapshot>; SNAPSHOT_RING_SLOTS],
}

// SAFETY: the ring is SPSC. The producer (`publish`) only ever runs on the
// control thread; the consumer (`load`) only on the audio thread. The seq
// protocol synchronizes the single in-flight slot. Sharing the ring (e.g. via an
// Arc) across exactly those two threads is sound.
unsafe impl Sync for SnapshotRing {}
unsafe impl Send for SnapshotRing {}

impl SnapshotRing {
    /// A ring pre-filled with `initial` in every slot and `seq = 0`, so a
    /// `load()` before the first `publish()` returns a valid (silent) snapshot
    /// rather than garbage.
    pub fn new(initial: MatrixSnapshot) -> Self {
        Self {
            seq: AtomicU64::new(0),
            slots: std::array::from_fn(|_| std::cell::UnsafeCell::new(initial)),
        }
    }

    /// CONTROL thread only: publish a new snapshot. Writes the slot, then bumps
    /// the sequence with `Release` so the consumer's `Acquire` read sees a fully
    /// written slot. Not wait-free-required here (the producer is human-rate) but
    /// it is lock-free and allocation-free.
    pub fn publish(&self, snap: MatrixSnapshot) {
        let cur = self.seq.load(Ordering::Relaxed);
        let next = cur.wrapping_add(1);
        let slot = (next as usize) % SNAPSHOT_RING_SLOTS;
        // SAFETY: single producer; no other writer touches this slot, and the
        // consumer will only read it AFTER the Release store of `next` below.
        unsafe {
            *self.slots[slot].get() = snap;
        }
        self.seq.store(next, Ordering::Release);
    }

    /// AUDIO thread only: load the most recently published snapshot. Wait-free
    /// in the common case; retries (bounded) only if the producer overwrote the
    /// slot mid-copy. No allocation, no lock, no syscall — safe in the IOProc.
    pub fn load(&self) -> MatrixSnapshot {
        loop {
            let s1 = self.seq.load(Ordering::Acquire);
            let slot = (s1 as usize) % SNAPSHOT_RING_SLOTS;
            // SAFETY: we copy the POD snapshot out; the seq re-check below
            // detects a concurrent overwrite of this slot and retries.
            let snap = unsafe { *self.slots[slot].get() };
            let s2 = self.seq.load(Ordering::Acquire);
            if s1 == s2 {
                return snap;
            }
        }
    }

    /// The number of snapshots published so far (for tests/telemetry).
    pub fn published(&self) -> u64 {
        self.seq.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_rejects_oversized() {
        assert!(MatrixState::new(MAX_CHANNELS + 1, 2).is_err());
        assert!(MatrixState::new(2, MAX_CHANNELS + 1).is_err());
        assert!(MatrixState::new(8, 8).is_ok());
    }

    #[test]
    fn crosspoint_set_and_clear() {
        let mut m = MatrixState::new(4, 4).unwrap();
        assert_eq!(m.crosspoint(0, 0).unwrap(), GAIN_OFF_DB);
        m.set_crosspoint(0, 1, -3.0).unwrap();
        assert_eq!(m.crosspoint(0, 1).unwrap(), -3.0);
        // Clear with the -inf sentinel.
        m.set_crosspoint(0, 1, GAIN_OFF_DB).unwrap();
        assert_eq!(m.crosspoint(0, 1).unwrap(), GAIN_OFF_DB);
    }

    #[test]
    fn crosspoint_validates_value_and_index() {
        let mut m = MatrixState::new(2, 2).unwrap();
        assert!(m.set_crosspoint(0, 0, 13.0).is_err()); // above +12 dB
        assert!(m.set_crosspoint(0, 0, f32::NAN).is_err()); // non-finite, not -inf
        assert!(m.set_crosspoint(2, 0, 0.0).is_err()); // input out of range
        assert!(m.set_crosspoint(0, 2, 0.0).is_err()); // output out of range
        assert!(m.set_crosspoint(0, 0, 12.0).is_ok()); // exactly +12 dB ok
    }

    #[test]
    fn revision_bumps_on_mutation() {
        let mut m = MatrixState::new(2, 2).unwrap();
        let r0 = m.revision();
        m.set_crosspoint(0, 0, 0.0).unwrap();
        m.set_input_mute(0, true).unwrap();
        m.set_monitor_output(Some(1)).unwrap();
        assert!(m.revision() > r0);
        assert_eq!(m.revision(), r0 + 3);
    }

    #[test]
    fn snapshot_reflects_state() {
        let mut m = MatrixState::new(2, 2).unwrap();
        m.set_crosspoint(1, 0, -6.0).unwrap();
        m.set_output_mute(1, true).unwrap();
        m.set_monitor_output(Some(0)).unwrap();
        let s = m.snapshot();
        assert_eq!(s.inputs, 2);
        assert_eq!(s.grid[1][0], -6.0);
        assert!(s.output_mutes[1]);
        assert_eq!(s.monitor_output, Some(0));
        assert_eq!(s.revision, m.revision());
    }

    #[test]
    fn ring_publish_then_load_roundtrips() {
        let ring = SnapshotRing::new(MatrixSnapshot::silent(2, 2));
        // Before any publish, load returns the silent initial snapshot.
        assert_eq!(ring.load().revision, 0);
        assert_eq!(ring.published(), 0);

        let mut m = MatrixState::new(2, 2).unwrap();
        m.set_crosspoint(0, 0, -1.5).unwrap();
        ring.publish(m.snapshot());
        let got = ring.load();
        assert_eq!(got.grid[0][0], -1.5);
        assert_eq!(ring.published(), 1);

        // A second publish supersedes the first.
        m.set_crosspoint(0, 0, -2.5).unwrap();
        ring.publish(m.snapshot());
        assert_eq!(ring.load().grid[0][0], -2.5);
        assert_eq!(ring.published(), 2);
    }
}
