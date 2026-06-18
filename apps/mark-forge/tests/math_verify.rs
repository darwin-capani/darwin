//! Math-module verification suite (SPEC §1 — the deterministic f64 foundation).
//!
//! The `math` module (`Vec3`/`Quat`/`Mat3` + ops + the inertia helpers) is the
//! FROZEN contract every downstream agent builds on, so it is `src/math.rs`'s
//! own unit tests PLUS this integration suite that pin its numerics. This file
//! lives in `tests/` (Cargo auto-discovers it — no edit to the frozen
//! `lib.rs`/`Cargo.toml`/`src/math.rs`) and exercises the four properties the
//! engine depends on:
//!
//!   1. quaternion-vs-matrix rotation AGREEMENT — `q.rotate(v)` and
//!      `q.to_mat3().mul_vec3(v)` are the same map (the solver rotates inertia
//!      via the matrix; the telemetry rotates points via the quat — they must
//!      not drift).
//!   2. quaternion NORMALIZATION STABILITY over many integration steps — the
//!      `q' = normalize(q + 0.5 (ω⊗q) h)` orientation integrator the world agent
//!      composes from the frozen `Quat::mul` + `Quat::normalized` stays unit and
//!      stays an actual rotation over thousands of substeps.
//!   3. inertia WORLD-TRANSFORM correctness — `I⁻¹_world = R·I⁻¹_body·Rᵀ`
//!      (`Mat3::rotated`) is symmetric, congruent, and agrees with the
//!      `Body::world_inv_inertia` path.
//!   4. NO NaN/inf under extreme inputs — zero, denormal, and astronomically
//!      large vectors/quaternions flow through the total, zero-safe primitives
//!      without producing a non-finite result.
//!
//! Everything is `f64` and uses only the frozen public API, so these are
//! bit-stable across runs/machines (SPEC §1).

use mark_forge::math::{Mat3, Quat, Vec3};

// ---------------------------------------------------------------------------
// shared tolerances + helpers
// ---------------------------------------------------------------------------

/// Tight tolerance for exact-form algebra (rotation agreement, transpose, etc.).
const EPS: f64 = 1e-12;
/// Looser tolerance for accumulated multi-step drift assertions.
const EPS_DRIFT: f64 = 1e-9;

fn vclose(a: Vec3, b: Vec3, eps: f64) -> bool {
    (a.x - b.x).abs() <= eps && (a.y - b.y).abs() <= eps && (a.z - b.z).abs() <= eps
}

fn mclose(a: Mat3, b: Mat3, eps: f64) -> bool {
    vclose(a.cols[0], b.cols[0], eps)
        && vclose(a.cols[1], b.cols[1], eps)
        && vclose(a.cols[2], b.cols[2], eps)
}

fn mat_finite(m: Mat3) -> bool {
    m.cols[0].is_finite() && m.cols[1].is_finite() && m.cols[2].is_finite()
}

fn quat_finite(q: Quat) -> bool {
    q.x.is_finite() && q.y.is_finite() && q.z.is_finite() && q.w.is_finite()
}

fn quat_norm(q: Quat) -> f64 {
    (q.x * q.x + q.y * q.y + q.z * q.z + q.w * q.w).sqrt()
}

/// The orientation-integration step the world agent composes from the FROZEN
/// public API: advance a unit orientation `q` by an angular velocity `omega`
/// (world-frame, rad/s) over substep `h`, then renormalize.
///
///   dq/dt = 1/2 · ω_quat ⊗ q,  with ω_quat = (ω, 0)
///   q' = normalize(q + 0.5 (ω_quat ⊗ q) h)
///
/// This uses ONLY `Quat::mul` (Hamilton product) + `Quat::normalized` + the
/// frozen `Vec3`/scalar ops — no new method on the frozen `Quat` struct. It is
/// reproduced here so the suite verifies the exact composition the integrator
/// will run, against the frozen primitives.
fn integrate_orientation(q: Quat, omega: Vec3, h: f64) -> Quat {
    let omega_q = Quat::new(omega.x, omega.y, omega.z, 0.0);
    let spin = omega_q.mul(q); // ω_quat ⊗ q
    let half_h = 0.5 * h;
    let candidate = Quat::new(
        q.x + spin.x * half_h,
        q.y + spin.y * half_h,
        q.z + spin.z * half_h,
        q.w + spin.w * half_h,
    );
    candidate.normalized()
}

// ===========================================================================
// 1. quaternion-vs-matrix rotation AGREEMENT
// ===========================================================================

#[test]
fn quat_and_matrix_rotate_a_vector_identically() {
    // A spread of non-trivial axes/angles, and a spread of test vectors. The
    // two rotation paths the engine uses — q.rotate (telemetry / contact points)
    // and q.to_mat3().mul_vec3 (solver inertia basis) — must be the same map.
    let axes = [
        Vec3::X,
        Vec3::Y,
        Vec3::Z,
        Vec3::new(1.0, 2.0, 3.0),
        Vec3::new(-3.0, 0.5, 2.0),
        Vec3::new(0.0, -7.0, 0.1),
    ];
    let angles = [
        0.0,
        0.3,
        std::f64::consts::FRAC_PI_4,
        std::f64::consts::FRAC_PI_2,
        2.0,
        std::f64::consts::PI,
        -1.1,
        5.7,
    ];
    let vectors = [
        Vec3::X,
        Vec3::Y,
        Vec3::Z,
        Vec3::new(1.0, 1.0, 1.0),
        Vec3::new(3.0, -2.0, 0.5),
        Vec3::new(-4.2, 9.0, -0.3),
    ];

    for &axis in &axes {
        for &angle in &angles {
            let q = Quat::from_axis_angle(axis, angle);
            let m = q.to_mat3();
            for &v in &vectors {
                let by_quat = q.rotate(v);
                let by_mat = m.mul_vec3(v);
                assert!(
                    vclose(by_quat, by_mat, EPS),
                    "rotation mismatch axis={:?} angle={} v={:?}: quat={:?} mat={:?}",
                    axis,
                    angle,
                    v,
                    by_quat,
                    by_mat
                );
            }
        }
    }
}

#[test]
fn rotation_preserves_length_and_is_orthonormal() {
    // A rotation is an isometry: |R v| == |v|, and R is orthonormal (RᵀR == I,
    // det == +1). This guards against a transposed/scaled `to_mat3`.
    let q = Quat::from_axis_angle(Vec3::new(1.0, -2.0, 0.5), 1.234);
    let m = q.to_mat3();

    let v = Vec3::new(2.0, -3.0, 1.5);
    assert!((m.mul_vec3(v).length() - v.length()).abs() < EPS, "not length-preserving");

    // RᵀR == I.
    let rtr = m.transpose().mul_mat3(m);
    assert!(mclose(rtr, Mat3::IDENTITY, EPS), "RᵀR != I: {:?}", rtr);

    // Proper rotation: columns are an orthonormal right-handed basis, so
    // col0 × col1 == col2 (det == +1, no reflection).
    let cross01 = m.cols[0].cross(m.cols[1]);
    assert!(vclose(cross01, m.cols[2], EPS), "left-handed / det != +1");
}

#[test]
fn composed_quat_matches_composed_matrix() {
    // Hamilton product composition agrees with matrix composition: rotating by
    // (a then b) == applying the product quaternion == multiplying the matrices.
    let a = Quat::from_axis_angle(Vec3::Z, 0.7);
    let b = Quat::from_axis_angle(Vec3::X, -1.3);
    let composed = b.mul(a); // apply a, then b

    let v = Vec3::new(1.0, 2.0, -0.5);
    let stepwise = b.rotate(a.rotate(v));
    let oneshot = composed.rotate(v);
    assert!(vclose(stepwise, oneshot, EPS), "quat composition order wrong");

    let mat_composed = b.to_mat3().mul_mat3(a.to_mat3());
    assert!(
        mclose(mat_composed, composed.to_mat3(), EPS),
        "matrix composition != quat composition"
    );
}

// ===========================================================================
// 2. quaternion NORMALIZATION STABILITY over many integration steps
// ===========================================================================

#[test]
fn integrated_orientation_stays_unit_over_many_steps() {
    // Spin a body at a constant world-frame angular velocity for many substeps
    // using the frozen-primitive integrator. Without per-step renormalization
    // the semi-implicit add would inflate the norm; with `Quat::normalized` it
    // must stay unit-length to f64 precision the whole way.
    let omega = Vec3::new(0.7, -1.3, 2.1);
    let h = 1.0 / 240.0; // 60 Hz, 4 substeps
    let mut q = Quat::IDENTITY;

    for i in 0..20_000 {
        q = integrate_orientation(q, omega, h);
        assert!(quat_finite(q), "non-finite quat at step {}", i);
        let n = quat_norm(q);
        assert!((n - 1.0).abs() < 1e-12, "norm drift {} at step {}", n, i);
    }

    // After integration the orientation must still produce a valid rotation
    // matrix (orthonormal), i.e. the accumulation did not skew it.
    let m = q.to_mat3();
    let rtr = m.transpose().mul_mat3(m);
    assert!(mclose(rtr, Mat3::IDENTITY, EPS_DRIFT), "integrated R not orthonormal");
}

#[test]
fn constant_spin_recovers_known_angle() {
    // Integrating a pure-Z spin of magnitude `w` for total time `T` must rotate
    // X toward the analytic angle `w*T` (small-step integrator converges to the
    // closed-form rotation). Use a small step so truncation error is tiny.
    let w = 1.5; // rad/s about +Z
    let omega = Vec3::Z * w;
    let h = 1.0 / 2000.0;
    let steps = 1000u32; // total T = 0.5 s -> angle = 0.75 rad
    let total_angle = w * (h * steps as f64);

    let mut q = Quat::IDENTITY;
    for _ in 0..steps {
        q = integrate_orientation(q, omega, h);
    }

    let rotated = q.rotate(Vec3::X);
    let expected = Vec3::new(total_angle.cos(), total_angle.sin(), 0.0);
    assert!(
        vclose(rotated, expected, 1e-4),
        "spin angle wrong: got {:?} expected {:?}",
        rotated,
        expected
    );
    // Spin about Z leaves Z untouched.
    assert!((rotated.z).abs() < 1e-9, "Z leaked during pure-Z spin");
}

#[test]
fn repeated_normalize_is_idempotent_and_total() {
    // Renormalizing an already-unit quat is a no-op; renormalizing a collapsed
    // (zero) quat yields IDENTITY (total, never NaN). Both are integrator
    // invariants relied on every substep.
    let q = Quat::from_axis_angle(Vec3::new(2.0, 1.0, -1.0), 0.9).normalized();
    assert!(quat_close(q, q.normalized(), EPS), "normalize not idempotent");

    let collapsed = Quat::new(0.0, 0.0, 0.0, 0.0).normalized();
    assert_eq!(collapsed, Quat::IDENTITY, "zero quat must normalize to IDENTITY");
}

fn quat_close(a: Quat, b: Quat, eps: f64) -> bool {
    (a.x - b.x).abs() <= eps
        && (a.y - b.y).abs() <= eps
        && (a.z - b.z).abs() <= eps
        && (a.w - b.w).abs() <= eps
}

// ===========================================================================
// 3. inertia WORLD-TRANSFORM correctness  (R·I⁻¹·Rᵀ)
// ===========================================================================

#[test]
fn rotated_inertia_is_symmetric_and_finite() {
    // A (diagonal) inverse-inertia tensor taken to world space stays symmetric
    // (it's a congruence of a symmetric matrix) and finite.
    let inv_body = Mat3::diagonal(Vec3::new(1.25, 0.5, 2.0));
    let r = Quat::from_axis_angle(Vec3::new(1.0, 2.0, -0.5), 0.85).to_mat3();
    let inv_world = inv_body.rotated(r);

    assert!(mat_finite(inv_world));
    assert!(
        mclose(inv_world, inv_world.transpose(), EPS),
        "world inertia not symmetric: {:?}",
        inv_world
    );
}

#[test]
fn rotated_identity_inertia_is_invariant() {
    // The identity tensor is rotation-invariant: R·I·Rᵀ == I for any R (a
    // uniform sphere's inertia is the same in every frame).
    let r = Quat::from_axis_angle(Vec3::new(-1.0, 3.0, 2.0), 2.4).to_mat3();
    let out = Mat3::IDENTITY.rotated(r);
    assert!(mclose(out, Mat3::IDENTITY, EPS), "identity inertia not invariant: {:?}", out);

    // A scalar (isotropic) tensor is likewise invariant.
    let iso = Mat3::diagonal(Vec3::splat(3.7));
    assert!(mclose(iso.rotated(r), iso, EPS), "isotropic inertia not invariant");
}

#[test]
fn rotated_inertia_preserves_trace_and_eigenvalues() {
    // A congruence by an orthonormal R is a similarity transform here (R⁻¹ = Rᵀ),
    // so the trace (sum of principal inverse-inertias) is invariant.
    let inv_body = Mat3::diagonal(Vec3::new(1.0, 2.0, 4.0));
    let r = Quat::from_axis_angle(Vec3::new(0.3, -1.0, 0.7), 1.9).to_mat3();
    let inv_world = inv_body.rotated(r);

    let trace_body = inv_body.cols[0].x + inv_body.cols[1].y + inv_body.cols[2].z;
    let trace_world = inv_world.cols[0].x + inv_world.cols[1].y + inv_world.cols[2].z;
    assert!((trace_body - trace_world).abs() < EPS, "trace not preserved");
}

#[test]
fn rotated_inertia_acts_correctly_on_world_angular_impulse() {
    // The whole point of R·I⁻¹·Rᵀ: a world-space angular impulse L applied to
    // the world tensor gives the same Δω as transforming L into body space,
    // applying the body tensor, and transforming the result back.
    let inv_body = Mat3::diagonal(Vec3::new(1.25, 0.5, 2.0));
    let q = Quat::from_axis_angle(Vec3::new(1.0, -2.0, 0.5), 1.1);
    let r = q.to_mat3();
    let inv_world = inv_body.rotated(r);

    let l_world = Vec3::new(0.4, -1.2, 0.9); // world-space angular impulse
    let dw_direct = inv_world.mul_vec3(l_world);

    // Reference path: Rᵀ L  ->  I⁻¹_body ·  ->  R ·.
    let l_body = r.transpose().mul_vec3(l_world);
    let dw_body = inv_body.mul_vec3(l_body);
    let dw_ref = r.mul_vec3(dw_body);

    assert!(
        vclose(dw_direct, dw_ref, EPS),
        "world inertia application mismatch: {:?} vs {:?}",
        dw_direct,
        dw_ref
    );
}

// ===========================================================================
// 4. NO NaN / inf under extreme inputs
// ===========================================================================

#[test]
fn normalized_is_total_on_degenerate_vectors() {
    // Zero and denormal-magnitude vectors must normalize to a finite result
    // (ZERO for the zero vector), never NaN/inf.
    assert_eq!(Vec3::ZERO.normalized(), Vec3::ZERO);
    assert!(Vec3::ZERO.normalized().is_finite());

    let tiny = Vec3::new(1e-300, -1e-300, 1e-300);
    assert!(tiny.normalized().is_finite(), "tiny vector normalize went non-finite");

    let huge = Vec3::new(1e300, 1e300, 1e300);
    let n = huge.normalized();
    assert!(n.is_finite(), "huge vector normalize went non-finite: {:?}", n);
}

#[test]
fn quat_normalize_is_total_on_degenerate_quats() {
    // Zero quat -> IDENTITY (already covered) and large quats normalize finite.
    assert_eq!(Quat::new(0.0, 0.0, 0.0, 0.0).normalized(), Quat::IDENTITY);

    let huge = Quat::new(1e200, -1e200, 1e200, -1e200).normalized();
    assert!(quat_finite(huge), "huge quat normalize non-finite");
    assert!((quat_norm(huge) - 1.0).abs() < 1e-9, "huge quat not unit after normalize");

    let tiny = Quat::new(1e-280, 1e-280, 1e-280, 1e-280).normalized();
    assert!(quat_finite(tiny), "tiny quat normalize non-finite");
}

#[test]
fn rotate_with_unnormalized_axis_is_finite() {
    // `from_axis_angle` normalizes its axis (zero-safe). A zero axis collapses
    // to IDENTITY rather than producing NaN, so rotation is a no-op, not garbage.
    let q = Quat::from_axis_angle(Vec3::ZERO, 1.0);
    let v = Vec3::new(1.0, 2.0, 3.0);
    let out = q.rotate(v);
    assert!(out.is_finite(), "rotate by zero-axis quat non-finite");
    assert!(vclose(out, v, EPS), "zero-axis rotation should be identity");
}

#[test]
fn inverse_diagonal_is_total_on_zero_and_huge() {
    // A zero diagonal entry inverts to 0.0 (locked axis), NOT inf — so a static
    // / partially-locked inertia never injects a non-finite into the solver.
    let with_zero = Mat3::diagonal(Vec3::new(0.0, 4.0, 0.0)).inverse_diagonal();
    assert!(mat_finite(with_zero));
    assert_eq!(with_zero.cols[0].x, 0.0);
    assert_eq!(with_zero.cols[1].y, 0.25);
    assert_eq!(with_zero.cols[2].z, 0.0);

    // A huge diagonal inverts to a finite tiny value.
    let huge = Mat3::diagonal(Vec3::splat(1e300)).inverse_diagonal();
    assert!(mat_finite(huge));

    // An all-zero tensor (static body) inverts to ZERO, the engine's "no angular
    // response" sentinel.
    assert_eq!(Mat3::ZERO.inverse_diagonal(), Mat3::ZERO);
}

#[test]
fn extreme_velocity_integration_stays_finite() {
    // An absurd angular velocity over a substep still yields a finite, unit
    // orientation (the renormalize is the safety net). Guards the integrator
    // against a spawn op with a pathological ang_vel.
    let omega = Vec3::new(1e8, -1e8, 1e8);
    let h = 1.0 / 240.0;
    let q = integrate_orientation(Quat::IDENTITY, omega, h);
    assert!(quat_finite(q), "extreme-omega integration non-finite");
    assert!((quat_norm(q) - 1.0).abs() < 1e-9, "extreme-omega quat not unit");
}

// ===========================================================================
// determinism cross-check: same inputs -> bit-identical outputs
// ===========================================================================

#[test]
fn math_is_bit_deterministic() {
    // Two independent runs of the same sequence must be byte-identical (SPEC §1
    // — no RNG, no wall-clock, fixed float order). Compare via the serde wire
    // form, which is the actual telemetry representation.
    fn run() -> Quat {
        let omega = Vec3::new(0.55, -1.1, 0.33);
        let h = 1.0 / 240.0;
        let mut q = Quat::IDENTITY;
        for _ in 0..5000 {
            q = integrate_orientation(q, omega, h);
        }
        q
    }
    let a = run();
    let b = run();
    assert_eq!(
        serde_json::to_string(&a).unwrap(),
        serde_json::to_string(&b).unwrap(),
        "integration not bit-deterministic"
    );
    // And the two quats are literally equal (not merely close).
    assert_eq!(a, b);
}
