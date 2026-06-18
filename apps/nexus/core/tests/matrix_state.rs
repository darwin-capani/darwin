//! Integration tests for the matrix-state module (SPEC §1).
//!
//! These exercise the FROZEN `nexus_core::matrix` surface — the authoritative
//! `MatrixState` machine, its immutable `MatrixSnapshot`, and the lock-free SPSC
//! `SnapshotRing` that hands snapshots across the realtime boundary — strictly
//! through the crate's PUBLIC API (the `rlib`). They add NO public surface,
//! touch NO frozen source file, and never weaken the in-module unit tests; they
//! ADD the coverage the matrix-state contract requires but that the in-module
//! tests do not yet assert:
//!
//!   - route.set ABOVE -inf adds a crosspoint; AT -inf clears it (SPEC §1
//!     "Routes are crosspoints above -inf").
//!   - gain.set clamps the accepted range to [-inf, +12] dB — values above +12
//!     and non-`-inf` non-finite values are REJECTED, state unchanged.
//!   - input/output mute toggles and is reflected in the snapshot.
//!   - monitor assignment set / clear, with out-of-range rejection.
//!   - a snapshot is a consistent IMMUTABLE copy: mutating the state after
//!     taking a snapshot does not mutate the snapshot, and the snapshot's grid
//!     is internally consistent with the revision it was stamped at.
//!   - concurrent single-producer / single-consumer snapshot hand-off through
//!     the ring is RACE-FREE: the consumer NEVER observes a torn snapshot (a
//!     grid that mixes two revisions), modeled deterministically by checking an
//!     invariant the producer maintains on EVERY published snapshot, hammered
//!     across real threads AND replayed deterministically with an interleaved,
//!     single-thread schedule.
//!
//! HARD-BOUNDARY honored: no device, no socket, no audio output, no server —
//! pure in-memory state machine + atomics over synthesized values.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

use nexus_core::matrix::{MatrixSnapshot, MatrixState, SnapshotRing, SNAPSHOT_RING_SLOTS};
use nexus_core::{GAIN_MAX_DB, GAIN_OFF_DB, MAX_CHANNELS};

// ---------------------------------------------------------------------------
// route.set — add a crosspoint above -inf, clear it at -inf.
// ---------------------------------------------------------------------------

#[test]
fn route_set_above_inf_adds_a_crosspoint() {
    let mut m = MatrixState::new(4, 4).unwrap();
    // Everything starts OFF: a crosspoint at the -inf sentinel is NOT a route.
    for i in 0..m.inputs() {
        for o in 0..m.outputs() {
            assert_eq!(m.crosspoint(i, o).unwrap(), GAIN_OFF_DB);
            assert!(m.crosspoint(i, o).unwrap().is_infinite());
        }
    }
    // route.set with a finite gain ADDS the route (a crosspoint above -inf).
    m.set_crosspoint(2, 3, -6.0).unwrap();
    let g = m.crosspoint(2, 3).unwrap();
    assert_eq!(g, -6.0);
    assert!(g.is_finite(), "an added route must be a finite (above -inf) gain");
    // Unity (0 dB) is a route too.
    m.set_crosspoint(0, 0, 0.0).unwrap();
    assert_eq!(m.crosspoint(0, 0).unwrap(), 0.0);
    // Only the touched crosspoints changed; the rest are still off.
    assert_eq!(m.crosspoint(1, 1).unwrap(), GAIN_OFF_DB);
}

#[test]
fn route_set_at_inf_clears_the_crosspoint() {
    let mut m = MatrixState::new(2, 2).unwrap();
    m.set_crosspoint(1, 1, 3.0).unwrap();
    assert_eq!(m.crosspoint(1, 1).unwrap(), 3.0);
    // route.set at the -inf sentinel CLEARS the route (back to off).
    m.set_crosspoint(1, 1, GAIN_OFF_DB).unwrap();
    let cleared = m.crosspoint(1, 1).unwrap();
    assert_eq!(cleared, GAIN_OFF_DB);
    assert!(
        cleared.is_infinite() && cleared.is_sign_negative(),
        "clearing must restore the -inf OFF sentinel"
    );
}

#[test]
fn route_set_bumps_revision_each_accepted_mutation() {
    let mut m = MatrixState::new(2, 2).unwrap();
    let r0 = m.revision();
    m.set_crosspoint(0, 0, -1.0).unwrap();
    m.set_crosspoint(0, 0, GAIN_OFF_DB).unwrap(); // clear is still a mutation
    assert_eq!(m.revision(), r0 + 2);
    // A REJECTED mutation must NOT advance the revision.
    let before = m.revision();
    assert!(m.set_crosspoint(0, 0, 99.0).is_err());
    assert_eq!(m.revision(), before, "a rejected op must not bump the revision");
}

// ---------------------------------------------------------------------------
// gain.set — clamp the accepted crosspoint range to [-inf, +12] dB.
// ---------------------------------------------------------------------------

#[test]
fn gain_set_accepts_full_inf_to_plus12_range() {
    let mut m = MatrixState::new(1, 1).unwrap();
    // The two range endpoints both accepted: the -inf sentinel and exactly +12.
    m.set_crosspoint(0, 0, GAIN_OFF_DB).unwrap();
    assert_eq!(m.crosspoint(0, 0).unwrap(), GAIN_OFF_DB);
    m.set_crosspoint(0, 0, GAIN_MAX_DB).unwrap();
    assert_eq!(m.crosspoint(0, 0).unwrap(), GAIN_MAX_DB);
    assert_eq!(GAIN_MAX_DB, 12.0);
    // A representative interior value.
    m.set_crosspoint(0, 0, -24.0).unwrap();
    assert_eq!(m.crosspoint(0, 0).unwrap(), -24.0);
}

#[test]
fn gain_set_rejects_above_ceiling_and_leaves_state_unchanged() {
    let mut m = MatrixState::new(1, 1).unwrap();
    m.set_crosspoint(0, 0, 6.0).unwrap();
    // Above +12 dB is rejected; the previous value is preserved.
    let r_before = m.revision();
    assert!(m.set_crosspoint(0, 0, 12.000001).is_err());
    assert!(m.set_crosspoint(0, 0, 48.0).is_err());
    assert_eq!(m.crosspoint(0, 0).unwrap(), 6.0, "rejected value must not mutate state");
    assert_eq!(m.revision(), r_before, "rejected value must not bump revision");
}

#[test]
fn gain_set_rejects_non_inf_non_finite() {
    let mut m = MatrixState::new(1, 1).unwrap();
    m.set_crosspoint(0, 0, -3.0).unwrap();
    // +inf and NaN are NOT the -inf OFF sentinel and are not finite -> rejected.
    assert!(m.set_crosspoint(0, 0, f32::INFINITY).is_err());
    assert!(m.set_crosspoint(0, 0, f32::NAN).is_err());
    // -inf alone (the OFF sentinel) is the ONE non-finite value accepted.
    assert!(m.set_crosspoint(0, 0, f32::NEG_INFINITY).is_ok());
    assert_eq!(m.crosspoint(0, 0).unwrap(), GAIN_OFF_DB);
}

#[test]
fn crosspoint_index_out_of_range_is_rejected() {
    let mut m = MatrixState::new(2, 3).unwrap();
    assert!(m.set_crosspoint(2, 0, 0.0).is_err(), "input index == count is OOB");
    assert!(m.set_crosspoint(0, 3, 0.0).is_err(), "output index == count is OOB");
    assert!(m.crosspoint(2, 0).is_err());
    assert!(m.crosspoint(0, 3).is_err());
    // The last valid indices are accepted.
    assert!(m.set_crosspoint(1, 2, 0.0).is_ok());
}

// ---------------------------------------------------------------------------
// mutes.
// ---------------------------------------------------------------------------

#[test]
fn input_and_output_mute_toggle_and_reflect_in_snapshot() {
    let mut m = MatrixState::new(3, 3).unwrap();
    m.set_input_mute(1, true).unwrap();
    m.set_output_mute(2, true).unwrap();
    let s = m.snapshot();
    assert!(s.input_mutes[1] && !s.input_mutes[0] && !s.input_mutes[2]);
    assert!(s.output_mutes[2] && !s.output_mutes[0] && !s.output_mutes[1]);
    // Un-mute restores.
    m.set_input_mute(1, false).unwrap();
    assert!(!m.snapshot().input_mutes[1]);
    // Out-of-range mute is rejected.
    assert!(m.set_input_mute(3, true).is_err());
    assert!(m.set_output_mute(3, true).is_err());
}

// ---------------------------------------------------------------------------
// monitor assignment.
// ---------------------------------------------------------------------------

#[test]
fn monitor_assignment_set_and_clear() {
    let mut m = MatrixState::new(2, 4).unwrap();
    assert_eq!(m.monitor_output(), None);
    m.set_monitor_output(Some(3)).unwrap();
    assert_eq!(m.monitor_output(), Some(3));
    assert_eq!(m.snapshot().monitor_output, Some(3));
    // Clearing with None.
    m.set_monitor_output(None).unwrap();
    assert_eq!(m.monitor_output(), None);
    assert_eq!(m.snapshot().monitor_output, None);
    // An out-of-range monitor output is rejected and leaves the assignment alone.
    m.set_monitor_output(Some(1)).unwrap();
    assert!(m.set_monitor_output(Some(4)).is_err());
    assert_eq!(m.monitor_output(), Some(1), "rejected monitor set must not mutate");
}

// ---------------------------------------------------------------------------
// snapshot is a consistent immutable copy.
// ---------------------------------------------------------------------------

#[test]
fn snapshot_is_an_independent_immutable_copy() {
    let mut m = MatrixState::new(2, 2).unwrap();
    m.set_crosspoint(0, 0, -3.0).unwrap();
    m.set_input_mute(0, true).unwrap();
    m.set_monitor_output(Some(1)).unwrap();
    let snap = m.snapshot();
    let rev_at_snap = snap.revision;
    assert_eq!(rev_at_snap, m.revision());

    // Mutating the state AFTER the snapshot must not retroactively change it.
    m.set_crosspoint(0, 0, 6.0).unwrap();
    m.set_input_mute(0, false).unwrap();
    m.set_monitor_output(None).unwrap();

    // The snapshot still holds the values (and revision) it was taken at.
    assert_eq!(snap.grid[0][0], -3.0, "snapshot grid must be a frozen copy");
    assert!(snap.input_mutes[0], "snapshot mutes must be a frozen copy");
    assert_eq!(snap.monitor_output, Some(1));
    assert_eq!(snap.revision, rev_at_snap);
    // The live state moved on.
    assert_eq!(m.crosspoint(0, 0).unwrap(), 6.0);
    assert!(m.revision() > rev_at_snap);

    // Two snapshots taken at the same revision are byte-equal (POD copy).
    let a = m.snapshot();
    let b = m.snapshot();
    assert_eq!(a, b);
}

#[test]
fn silent_snapshot_has_no_routes() {
    let s = MatrixSnapshot::silent(8, 8);
    assert_eq!(s.inputs, 8);
    assert_eq!(s.outputs, 8);
    assert_eq!(s.revision, 0);
    assert_eq!(s.monitor_output, None);
    for i in 0..MAX_CHANNELS {
        for o in 0..MAX_CHANNELS {
            assert_eq!(s.grid[i][o], GAIN_OFF_DB);
        }
        assert!(!s.input_mutes[i] && !s.output_mutes[i]);
    }
}

// ---------------------------------------------------------------------------
// concurrent producer/consumer snapshot hand-off is race-free.
//
// We model "no torn snapshot" as a checkable per-snapshot INVARIANT the
// producer maintains: EVERY active crosspoint in a published snapshot holds the
// exact gain that the snapshot's OWN revision encodes (`rev_to_gain(revision)`).
// The producer builds each snapshot so this holds. A torn read — a grid whose
// bytes come from one publish but whose `revision` field comes from another, or
// a grid mixing two producer passes — would surface as a crosspoint whose value
// disagrees with `rev_to_gain(snap.revision)`. The consumer asserts the
// invariant on every `load()`; if the seqlock retry protocol failed to exclude a
// mid-write copy, this catches it.
//
// Making the snapshot internally coherent is the subtle part: every accepted
// `set_crosspoint` bumps the revision by one, so a full uniform grid write of
// `D = inputs*outputs` crosspoints advances the revision by exactly `D`. So we
// read the start revision, compute the gain the FINAL revision (`start + D`)
// will encode, and write the whole grid to THAT gain — landing the snapshot's
// revision and its grid in agreement by construction.
// ---------------------------------------------------------------------------

/// Encode a revision into a finite, in-range crosspoint gain so a consumer can
/// verify a loaded snapshot is internally consistent (untorn). Always finite,
/// always `<= +12`, always `> -inf` — a valid ROUTE whose value is a
/// deterministic function of the revision. The 64-wide window keeps the value in
/// a small band well under the +12 ceiling; distinct adjacent revisions map to
/// distinct gains, so a one-revision tear is visible.
fn rev_to_gain(rev: u64) -> f32 {
    let step = (rev % 64) as f32;
    GAIN_MAX_DB - 0.5 - step * 0.25
}

/// The invariant a NON-TORN snapshot must satisfy: every active crosspoint holds
/// exactly `rev_to_gain(snap.revision)` — i.e. the grid was written by a single
/// producer pass at that revision, not a mix of two.
fn assert_coherent(snap: &MatrixSnapshot) {
    let expected = rev_to_gain(snap.revision);
    for i in 0..snap.inputs {
        for o in 0..snap.outputs {
            assert_eq!(
                snap.grid[i][o], expected,
                "TORN snapshot: crosspoint ({i},{o}) = {} but revision {} encodes {}",
                snap.grid[i][o], snap.revision, expected
            );
        }
    }
}

#[test]
fn ring_handoff_threaded_is_race_free() {
    // One producer thread publishes a long stream of internally-coherent
    // snapshots; one consumer thread hammers load() and asserts coherence on
    // every read. A torn read would trip `assert_coherent`. We also confirm the
    // consumer actually saw a spread of revisions (the hand-off is live, not a
    // single stale value), and that revisions never go backwards.
    const INPUTS: usize = 6;
    const OUTPUTS: usize = 6;
    const PUBLISHES: u64 = 200_000;
    // Each publish is one full uniform grid rewrite = INPUTS*OUTPUTS revision
    // bumps, so the producer's final state revision is this:
    const FINAL_REVISION: u64 = PUBLISHES * (INPUTS as u64) * (OUTPUTS as u64);

    let ring = Arc::new(SnapshotRing::new(MatrixSnapshot::silent(INPUTS, OUTPUTS)));
    let done = Arc::new(AtomicBool::new(false));
    // For an external cross-check that the consumer kept pace with the producer.
    let max_seen = Arc::new(AtomicU64::new(0));

    let producer = {
        let ring = Arc::clone(&ring);
        let done = Arc::clone(&done);
        thread::spawn(move || {
            // One persistent authoritative state; each step rewrites the whole
            // grid to a value encoding the new revision, so every published
            // snapshot is internally coherent and the revision strictly climbs.
            let mut m = MatrixState::new(INPUTS, OUTPUTS).unwrap();
            for _ in 0..PUBLISHES {
                let snap = publish_coherent(&mut m);
                ring.publish(snap);
            }
            done.store(true, Ordering::Release);
        })
    };

    let consumer = {
        let ring = Arc::clone(&ring);
        let done = Arc::clone(&done);
        let max_seen = Arc::clone(&max_seen);
        thread::spawn(move || {
            let mut last_rev = 0u64;
            let mut reads = 0u64;
            loop {
                let snap = ring.load();
                // (1) never torn. The encoded-coherence invariant only holds for
                //     producer-built snapshots (revision > 0); revision 0 is the
                //     silent SEED the ring was constructed with, before the first
                //     publish has been observed — its grid is intentionally -inf.
                if snap.revision > 0 {
                    assert_coherent(&snap);
                }
                // (2) the published count is monotonic from the consumer's view:
                //     a load never reports an OLDER revision than a prior load
                //     (the ring always hands back the newest committed slot).
                assert!(
                    snap.revision >= last_rev,
                    "revision went backwards: {} after {}",
                    snap.revision,
                    last_rev
                );
                last_rev = snap.revision;
                reads += 1;
                if reads % 4096 == 0 {
                    max_seen.store(last_rev, Ordering::Relaxed);
                }
                if done.load(Ordering::Acquire) {
                    // Drain the final published value and check it too.
                    let fin = ring.load();
                    assert!(fin.revision > 0, "producer must have published by now");
                    assert_coherent(&fin);
                    assert!(fin.revision >= last_rev);
                    max_seen.store(fin.revision, Ordering::Relaxed);
                    break;
                }
            }
        })
    };

    producer.join().expect("producer panicked");
    consumer.join().expect("consumer panicked");

    // The ring's own published counter equals the number of publishes.
    assert_eq!(ring.published(), PUBLISHES);
    // The consumer observed the final state (it kept up enough to see the end).
    assert_eq!(
        max_seen.load(Ordering::Relaxed),
        FINAL_REVISION,
        "consumer never observed the final published revision"
    );
}

/// Advance a persistent [`MatrixState`] by one full, INTERNALLY-COHERENT grid
/// rewrite and return the resulting snapshot — the producer's per-step payload.
///
/// Coherence by construction: a uniform write of every active crosspoint is
/// `D = inputs*outputs` accepted mutations, each bumping the revision by one, so
/// the post-write revision is exactly `start + D`. We compute the encoded gain
/// for THAT final revision *before* writing and write the whole grid to it, so
/// the snapshot's `revision` and its uniform grid value agree exactly. The state
/// is persistent across calls, so the revision strictly increases publish over
/// publish — giving the consumer genuine forward motion to observe.
fn publish_coherent(m: &mut MatrixState) -> MatrixSnapshot {
    let inputs = m.inputs();
    let outputs = m.outputs();
    let d = (inputs * outputs) as u64;
    assert!(d > 0, "coherence model needs at least one crosspoint");
    let final_rev = m.revision() + d;
    let g = rev_to_gain(final_rev);
    for i in 0..inputs {
        for o in 0..outputs {
            m.set_crosspoint(i, o, g).unwrap();
        }
    }
    debug_assert_eq!(m.revision(), final_rev);
    let snap = m.snapshot();
    // Self-check the construction before handing it to the ring.
    assert_coherent(&snap);
    snap
}

#[test]
fn ring_handoff_deterministic_interleaving_is_race_free() {
    // Deterministic (single-thread) model of the SPSC seqlock hand-off: we drive
    // publish/load in a FIXED interleaving schedule and assert the consumer never
    // observes a torn snapshot, regardless of where its loads fall relative to
    // the producer's publishes. This pins the correctness argument without
    // relying on the OS scheduler to expose a race.
    const INPUTS: usize = 4;
    const OUTPUTS: usize = 5;

    let ring = SnapshotRing::new(MatrixSnapshot::silent(INPUTS, OUTPUTS));

    // Pre-publish round-trip: a load before any publish returns the silent init.
    // The silent grid is GAIN_OFF_DB (the encoded-coherence invariant does NOT
    // apply to it — that invariant only holds for snapshots the producer built
    // via `snapshot_at_revision`), so we assert the silent shape directly.
    let init = ring.load();
    assert_eq!(init.revision, 0);
    assert_eq!(init.grid[0][0], GAIN_OFF_DB);

    // A schedule that walks the published sequence PAST the ring slot count
    // several times so the consumer's loads land on every slot, including just
    // after a wrap. SNAPSHOT_RING_SLOTS is small; cover a few full cycles.
    let cycles = 5;
    let total = (SNAPSHOT_RING_SLOTS as u64) * cycles + 3;

    let mut m = MatrixState::new(INPUTS, OUTPUTS).unwrap();
    let mut last = 0u64;
    for _ in 1..=total {
        // Producer step: publish the next internally-coherent snapshot. Its
        // revision strictly exceeds the previous one (a full grid rewrite).
        let snap = publish_coherent(&mut m);
        let stamped = snap.revision;
        assert!(stamped > last, "producer revision must strictly advance");
        ring.publish(snap);

        // Consumer step A: an immediate load must see a coherent snapshot whose
        // revision is at least the just-published one (newest committed slot).
        let a = ring.load();
        assert_coherent(&a);
        assert!(a.revision >= stamped, "load after publish saw an older slot");
        assert!(a.revision >= last);
        last = a.revision;

        // Consumer step B: a second back-to-back load is stable (no producer ran
        // in between in this single-thread schedule) and still coherent.
        let b = ring.load();
        assert_eq!(a, b, "back-to-back loads with no publish must be identical");
        assert_coherent(&b);
    }

    assert_eq!(ring.published(), total);
}

#[test]
fn ring_load_before_publish_returns_initial() {
    // The ring is seeded so an audio-thread load BEFORE the first control-thread
    // publish returns the valid silent snapshot, not garbage (no torn read, no
    // uninitialized slot).
    let ring = SnapshotRing::new(MatrixSnapshot::silent(2, 2));
    let s = ring.load();
    assert_eq!(s.revision, 0);
    assert_eq!(s.grid[0][0], GAIN_OFF_DB);
    assert_eq!(ring.published(), 0);
    // And every slot held the same initial value, so repeated loads agree.
    assert_eq!(ring.load(), s);
}
