//! Sequential-impulse constraint solver (SPEC §6) — restitution, Coulomb
//! friction, and Baumgarte / split-impulse position correction.
//!
//! Owns the [`solve_velocity`] + [`correct_positions`] contract the integrator
//! calls per substep. Both mutate [`World::bodies`] in place and read the
//! narrowphase [`Manifold`]s + the [`crate::world::SolverParams`]; the
//! signatures are FROZEN — downstream agents must NOT change them.
//!
//! # The constraint model (SPEC §6)
//!
//! For each contact we solve a non-penetration constraint plus two friction
//! constraints, via the standard *sequential impulse* method (Catto / Box2D /
//! Bullet lineage):
//!
//! * **Normal constraint** — the relative velocity of the two bodies *at the
//!   contact point*, projected on the contact normal, must be `>= 0` (separating
//!   or resting). The corrective impulse is
//!   `Δλ = -(v_rel·n + bias) / k_n`, where `k_n` is the effective mass along the
//!   normal (linear `inv_mass` plus the angular `(r×n)·I⁻¹·(r×n)` term for both
//!   bodies). The accumulated normal impulse `λ_n` is clamped `>= 0` so a contact
//!   can only push, never pull.
//!
//! * **Restitution** — a bounce is injected as a *velocity bias*: if the
//!   approaching normal speed exceeds [`SolverParams::restitution_threshold`],
//!   the target separating speed becomes `e * v_approach` (with `e` the
//!   pairwise-combined restitution, [`Material::combine_restitution`]). Below the
//!   threshold restitution is suppressed so resting stacks do not jitter-bounce.
//!   The bias is computed once from the *pre-solve* velocity (a fixed snapshot)
//!   so iterating the solver does not pump energy in.
//!
//! * **Friction** — two tangent constraints drive the tangential relative
//!   velocity to zero, each clamped to the Coulomb cone `|λ_t| <= μ·λ_n` (μ the
//!   pairwise-combined friction, [`Material::combine_friction`]). Friction is
//!   solved *after* the normal pass each iteration so the cone uses the current
//!   normal impulse.
//!
//! * **Position correction** — the velocity solve leaves residual penetration;
//!   [`correct_positions`] resolves it with split-impulse-style *positional*
//!   pushout (a pseudo-velocity that never touches the real velocities, so no
//!   energy enters the velocity solve). Penetration up to
//!   [`SolverParams::penetration_slop`] is left uncorrected and only
//!   `baumgarte_beta` of the excess is removed per call, so resting stacks settle
//!   smoothly instead of popping.
//!
//! # Determinism (SPEC §1/§6)
//!
//! Manifolds are iterated in their given (broadphase) order; contacts within a
//! manifold in their stored order; for a fixed
//! [`SolverParams::velocity_iterations`] count. The pairwise combine rules are
//! fixed (`restitution = max`, `friction = sqrt(a*b)`), the tangent basis is a
//! deterministic branch on the normal's smallest component, and every operation
//! is `f64` with a fixed evaluation order — no RNG, no wall-clock, no
//! float-identity branching.

use crate::body::{Body, Material};
use crate::math::{Mat3, Vec3};
use crate::narrowphase::Manifold;
use crate::world::World;

/// Build a right-handed orthonormal tangent basis `(t1, t2)` for a unit
/// `normal`, deterministically. We pick the world axis least aligned with the
/// normal (by smallest absolute component) as the seed so the cross product is
/// well-conditioned, then Gram-Schmidt. The branch is on a fixed `<` comparison
/// of the normal's components, so the basis is a pure function of the normal
/// (SPEC §1 — no RNG, fixed evaluation order).
#[inline]
fn tangent_basis(normal: Vec3) -> (Vec3, Vec3) {
    // Seed = the cardinal axis with which `normal` is *least* aligned.
    let seed = if normal.x.abs() <= normal.y.abs() && normal.x.abs() <= normal.z.abs() {
        Vec3::X
    } else if normal.y.abs() <= normal.z.abs() {
        Vec3::Y
    } else {
        Vec3::Z
    };
    let t1 = normal.cross(seed).normalized();
    let t2 = normal.cross(t1);
    (t1, t2)
}

/// The world-space velocity of `body` at world point `p` (`v + ω × r`, with
/// `r = p − center_of_mass`). Static bodies report zero (their velocities are
/// always zero, but this keeps the call total).
#[inline]
fn point_velocity(body: &Body, r: Vec3) -> Vec3 {
    body.lin_vel + body.ang_vel.cross(r)
}

/// The effective inverse mass of a contact constraint along unit `dir` at lever
/// arms `ra`/`rb` for bodies `a`/`b`: `k = invMa + invMb + (ra×dir)·Ia⁻¹·(ra×dir)
/// + (rb×dir)·Ib⁻¹·(rb×dir)`. The reciprocal is the impulse-to-velocity scale.
#[inline]
fn effective_mass(
    inv_mass_a: f64,
    inv_mass_b: f64,
    inv_inertia_a: Mat3,
    inv_inertia_b: Mat3,
    ra: Vec3,
    rb: Vec3,
    dir: Vec3,
) -> f64 {
    let rn_a = ra.cross(dir);
    let rn_b = rb.cross(dir);
    let ang_a = inv_inertia_a.mul_vec3(rn_a).dot(rn_a);
    let ang_b = inv_inertia_b.mul_vec3(rn_b).dot(rn_b);
    inv_mass_a + inv_mass_b + ang_a + ang_b
}

/// Apply an impulse `j * dir` at world lever arms `ra`/`rb` to the two bodies'
/// linear + angular velocities (equal and opposite, SPEC §6). The normal
/// convention points A→B, so B receives `+`, A receives `−`.
#[inline]
fn apply_impulse(
    a: &mut Body,
    b: &mut Body,
    inv_inertia_a: Mat3,
    inv_inertia_b: Mat3,
    ra: Vec3,
    rb: Vec3,
    impulse: Vec3,
) {
    a.lin_vel -= impulse * a.inv_mass;
    a.ang_vel -= inv_inertia_a.mul_vec3(ra.cross(impulse));
    b.lin_vel += impulse * b.inv_mass;
    b.ang_vel += inv_inertia_b.mul_vec3(rb.cross(impulse));
}

/// Borrow two distinct bodies from the world by index, mutably. `a != b` is a
/// precondition (a manifold never relates a body to itself; broadphase emits
/// `a.0 < b.0`). Returns `None` if either index is out of range, so a stale
/// manifold can never panic the solver.
#[inline]
fn pair_mut(bodies: &mut [Body], ia: usize, ib: usize) -> Option<(&mut Body, &mut Body)> {
    if ia == ib || ia >= bodies.len() || ib >= bodies.len() {
        return None;
    }
    // Split the slice so we hold two disjoint mutable borrows.
    if ia < ib {
        let (lo, hi) = bodies.split_at_mut(ib);
        Some((&mut lo[ia], &mut hi[0]))
    } else {
        let (lo, hi) = bodies.split_at_mut(ia);
        Some((&mut hi[0], &mut lo[ib]))
    }
}

/// Per-contact constraint data precomputed once from the *pre-solve* state and
/// reused across every solver iteration. Holding the lever arms, effective
/// masses, the tangent basis, and the restitution bias fixed across iterations
/// is what keeps the solve energy-stable and deterministic (SPEC §6).
#[derive(Clone, Copy)]
struct ContactConstraint {
    /// Index into the manifold's `contacts` (write the accumulated impulses back).
    contact_idx: usize,
    /// Lever arm from A's center of mass to the contact point.
    ra: Vec3,
    /// Lever arm from B's center of mass to the contact point.
    rb: Vec3,
    /// Unit contact normal (A→B).
    normal: Vec3,
    /// Orthonormal friction tangents.
    t1: Vec3,
    t2: Vec3,
    /// `1 / k_n` along the normal (0 if the constraint has no mobile DOF).
    inv_k_normal: f64,
    /// `1 / k_t` along each tangent.
    inv_k_t1: f64,
    inv_k_t2: f64,
    /// Restitution target separating speed (`e * v_approach`, `>= 0`); already
    /// thresholded — `0.0` for a resting / sub-threshold contact.
    restitution_bias: f64,
    /// Coulomb friction coefficient μ for this contact.
    friction: f64,
    /// Accumulated normal impulse (warm-start seed + cone bound carrier).
    normal_impulse: f64,
    /// Accumulated tangent impulse along `t1` (cone-clamped each iteration).
    tangent_impulse_1: f64,
    /// Accumulated tangent impulse along `t2`.
    tangent_impulse_2: f64,
}

#[inline]
fn inv_or_zero(k: f64) -> f64 {
    if k > 0.0 {
        1.0 / k
    } else {
        0.0
    }
}

/// Build the per-contact constraints for one manifold from the current body
/// state. Returns the constraint list (one per contact). A manifold between two
/// static bodies (no mobile DOF) yields all-zero effective masses, so its
/// impulses are no-ops — but broadphase already skips static–static pairs, so
/// this is just defensive totality.
fn build_constraints(
    a: &Body,
    b: &Body,
    manifold: &Manifold,
    restitution_threshold: f64,
) -> Vec<ContactConstraint> {
    let inv_inertia_a = a.world_inv_inertia();
    let inv_inertia_b = b.world_inv_inertia();
    let e = Material::combine_restitution(a.material, b.material);
    let mu = Material::combine_friction(a.material, b.material);

    let mut out = Vec::with_capacity(manifold.contacts.len());
    for (idx, c) in manifold.contacts.iter().enumerate() {
        let normal = c.normal;
        let (t1, t2) = tangent_basis(normal);
        let ra = c.point - a.pos;
        let rb = c.point - b.pos;

        let k_n = effective_mass(a.inv_mass, b.inv_mass, inv_inertia_a, inv_inertia_b, ra, rb, normal);
        let k_t1 = effective_mass(a.inv_mass, b.inv_mass, inv_inertia_a, inv_inertia_b, ra, rb, t1);
        let k_t2 = effective_mass(a.inv_mass, b.inv_mass, inv_inertia_a, inv_inertia_b, ra, rb, t2);

        // Restitution from the PRE-SOLVE approach speed (fixed snapshot).
        let rel_vel = point_velocity(b, rb) - point_velocity(a, ra);
        let v_n = rel_vel.dot(normal); // < 0 => approaching (B moving toward A along +n)
        let restitution_bias = if v_n < -restitution_threshold {
            // Target separating speed after the bounce.
            -e * v_n
        } else {
            0.0
        };

        out.push(ContactConstraint {
            contact_idx: idx,
            ra,
            rb,
            normal,
            t1,
            t2,
            inv_k_normal: inv_or_zero(k_n),
            inv_k_t1: inv_or_zero(k_t1),
            inv_k_t2: inv_or_zero(k_t2),
            restitution_bias,
            friction: mu,
            // Accumulators start at zero each solve call (no cross-call
            // warm-starting): the accumulation across the velocity iterations
            // within THIS call is what converges the constraint, and a fresh
            // start keeps the solve a pure function of the input velocities +
            // contacts (SPEC §1 determinism). The frozen `Contact` impulse fields
            // are still written back for telemetry / a future warm-start agent.
            normal_impulse: 0.0,
            tangent_impulse_1: 0.0,
            tangent_impulse_2: 0.0,
        });
    }
    out
}

/// One sequential-impulse VELOCITY solve over all contacts (SPEC §6).
///
/// Runs [`SolverParams::velocity_iterations`] passes. Each pass, for every
/// contact in fixed order, applies the normal impulse (with the pre-snapshotted
/// restitution bias) and then the two clamped Coulomb-friction tangent impulses,
/// mutating the bodies' `lin_vel`/`ang_vel` in place. Accumulated impulses are
/// stored back on each [`crate::narrowphase::Contact`] for warm-starting +
/// diagnostics.
///
/// `dt` is the SUBSTEP duration `h = world.dt / world.substeps`. (The classic
/// velocity-only restitution formulation used here does not need it for the
/// bounce; it is part of the frozen signature for Baumgarte-velocity variants
/// and is accepted but unused here. Underscored to keep the build warning-free.)
///
/// [`SolverParams::velocity_iterations`] is read from `world.params`; the
/// per-contact effective masses, lever arms, and restitution bias are computed
/// once from the pre-solve state and held fixed across the iterations (the
/// energy-stable, deterministic formulation).
pub fn solve_velocity(world: &mut World, manifolds: &mut [Manifold], _dt: f64) {
    let iterations = world.params.velocity_iterations;
    let restitution_threshold = world.params.restitution_threshold;

    for manifold in manifolds.iter_mut() {
        let ia = manifold.a.index();
        let ib = manifold.b.index();

        // Snapshot constraints from the pre-solve state (immutable borrow), then
        // drop the borrow before taking the mutable pair.
        let mut constraints = {
            let (a, b) = match (world.bodies.get(ia), world.bodies.get(ib)) {
                (Some(a), Some(b)) if ia != ib => (a, b),
                _ => continue,
            };
            build_constraints(a, b, manifold, restitution_threshold)
        };

        let (a, b) = match pair_mut(&mut world.bodies, ia, ib) {
            Some(pair) => pair,
            None => continue,
        };
        let inv_inertia_a = a.world_inv_inertia();
        let inv_inertia_b = b.world_inv_inertia();

        for _ in 0..iterations {
            for cc in constraints.iter_mut() {
                // ---- normal impulse (with restitution bias) -----------------
                let rel_vel = point_velocity(b, cc.rb) - point_velocity(a, cc.ra);
                let v_n = rel_vel.dot(cc.normal);
                // Drive v_n up to the restitution target.
                let mut d_lambda = -(v_n - cc.restitution_bias) * cc.inv_k_normal;
                // Clamp the ACCUMULATED normal impulse to be non-negative (a
                // contact can only push). The applied delta is the clamp diff.
                let old_n = cc.normal_impulse;
                cc.normal_impulse = (old_n + d_lambda).max(0.0);
                d_lambda = cc.normal_impulse - old_n;
                if d_lambda != 0.0 {
                    let impulse = cc.normal * d_lambda;
                    apply_impulse(a, b, inv_inertia_a, inv_inertia_b, cc.ra, cc.rb, impulse);
                }

                // ---- friction: two tangents, clamped to the Coulomb cone ----
                // The cone bound uses the CURRENT accumulated normal impulse, so
                // a contact that is only lightly loaded can apply only a little
                // friction. We clamp the ACCUMULATED tangent impulse (not the raw
                // delta) so the friction constraint converges and never reverses
                // the body — the applied amount is the change in the clamped
                // accumulator (standard Catto/Box2D friction).
                let max_friction = cc.friction * cc.normal_impulse;

                // Tangent 1.
                let rel_vel = point_velocity(b, cc.rb) - point_velocity(a, cc.ra);
                let v_t1 = rel_vel.dot(cc.t1);
                let dj_t1 = -v_t1 * cc.inv_k_t1;
                let old_t1 = cc.tangent_impulse_1;
                cc.tangent_impulse_1 = (old_t1 + dj_t1).clamp(-max_friction, max_friction);
                let applied_t1 = cc.tangent_impulse_1 - old_t1;
                if applied_t1 != 0.0 {
                    let impulse = cc.t1 * applied_t1;
                    apply_impulse(a, b, inv_inertia_a, inv_inertia_b, cc.ra, cc.rb, impulse);
                }

                // Tangent 2.
                let rel_vel = point_velocity(b, cc.rb) - point_velocity(a, cc.ra);
                let v_t2 = rel_vel.dot(cc.t2);
                let dj_t2 = -v_t2 * cc.inv_k_t2;
                let old_t2 = cc.tangent_impulse_2;
                cc.tangent_impulse_2 = (old_t2 + dj_t2).clamp(-max_friction, max_friction);
                let applied_t2 = cc.tangent_impulse_2 - old_t2;
                if applied_t2 != 0.0 {
                    let impulse = cc.t2 * applied_t2;
                    apply_impulse(a, b, inv_inertia_a, inv_inertia_b, cc.ra, cc.rb, impulse);
                }
            }
        }

        // Write accumulated impulses back onto the manifold's contacts (warm-start
        // + diagnostics). The tangent magnitude is the resultant of both tangents.
        for cc in constraints.iter() {
            let contact = &mut manifold.contacts[cc.contact_idx];
            contact.normal_impulse = cc.normal_impulse;
            contact.tangent_impulse =
                (cc.tangent_impulse_1 * cc.tangent_impulse_1 + cc.tangent_impulse_2 * cc.tangent_impulse_2).sqrt();
        }
    }
}

/// The position-correction pass (SPEC §6): split-impulse positional pushout.
///
/// For each contact whose penetration exceeds
/// [`SolverParams::penetration_slop`], moves the two bodies apart along the
/// contact normal by `baumgarte_beta` of the excess, weighted by inverse mass
/// (so the lighter body moves more, an immovable body not at all). This is a
/// *positional* correction — it never touches `lin_vel`/`ang_vel`, so it injects
/// no energy into the velocity solve (the split-impulse property). Orientations
/// are left untouched (linear pushout only; rotational positional drift is left
/// to the velocity solver's angular term, the standard cheap approximation).
///
/// Returns the deepest penetration still remaining (the `last_penetration` stat,
/// SPEC §8) — measured from the *input* manifolds (pre-correction), which is the
/// settling indicator the HUD shows for the step.
pub fn correct_positions(world: &mut World, manifolds: &[Manifold]) -> f64 {
    let beta = world.params.baumgarte_beta;
    let slop = world.params.penetration_slop;
    let mut max_penetration = 0.0_f64;

    for manifold in manifolds.iter() {
        let ia = manifold.a.index();
        let ib = manifold.b.index();

        let (inv_mass_a, inv_mass_b) = match (world.bodies.get(ia), world.bodies.get(ib)) {
            (Some(a), Some(b)) if ia != ib => (a.inv_mass, b.inv_mass),
            _ => continue,
        };
        let inv_sum = inv_mass_a + inv_mass_b;
        if inv_sum <= 0.0 {
            // Two static bodies: nothing to push, but still report penetration.
            for c in &manifold.contacts {
                max_penetration = max_penetration.max(c.penetration);
            }
            continue;
        }

        let (a, b) = match pair_mut(&mut world.bodies, ia, ib) {
            Some(pair) => pair,
            None => continue,
        };

        for c in &manifold.contacts {
            max_penetration = max_penetration.max(c.penetration);
            let excess = c.penetration - slop;
            if excess <= 0.0 {
                continue;
            }
            // Total positional correction magnitude to apply this pass.
            let correction = beta * excess;
            let push = c.normal * (correction / inv_sum);
            // Normal points A→B: move B along +n, A along −n, mass-weighted.
            a.pos -= push * inv_mass_a;
            b.pos += push * inv_mass_b;
        }
    }

    max_penetration
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::{Body, BodyId, Material, Shape};
    use crate::math::Vec3;
    use crate::narrowphase::{Contact, Manifold};
    use crate::world::World;

    // -- helpers ------------------------------------------------------------

    /// A fresh world with default params; tests drive the solver directly with
    /// hand-built manifolds (the narrowphase/broadphase are filled by other
    /// agents, so this module is verified in isolation against the frozen
    /// contract rather than through the not-yet-built collision pipeline).
    fn empty_world() -> World {
        World::new()
    }

    /// Manually build a single-contact manifold between A and B with a unit
    /// normal A→B at `point` and the given penetration.
    fn one_contact(a: BodyId, b: BodyId, point: Vec3, normal: Vec3, penetration: f64) -> Manifold {
        let mut m = Manifold::new(a, b);
        m.contacts.push(Contact::new(point, normal.normalized(), penetration));
        m
    }

    /// A multi-point manifold (mirrors a box-on-plane face manifold) — several
    /// contacts sharing one normal, at the given world points + penetration.
    fn multi_contact(a: BodyId, b: BodyId, points: &[Vec3], normal: Vec3, penetration: f64) -> Manifold {
        let mut m = Manifold::new(a, b);
        let n = normal.normalized();
        for &p in points {
            m.contacts.push(Contact::new(p, n, penetration));
        }
        m
    }

    // -- restitution: a single normal-impulse bounce reflects velocity --------

    #[test]
    fn restitution_reflects_normal_velocity() {
        // A dynamic sphere moving DOWN onto a static plane (normal up). Set
        // restitution e=1 (perfectly elastic). After the solve the sphere's
        // downward velocity should reverse to ~ +v (within tolerance).
        let mut w = empty_world();
        // Drop restitution threshold to 0 so the bounce is not suppressed.
        w.params.restitution_threshold = 0.0;

        let plane = w.spawn(Body::plane(Vec3::Y, 0.0)); // BodyId(0), static
        let mat = Material { restitution: 1.0, friction: 0.0 };
        let ball = w.spawn(Body::new(Shape::Sphere { radius: 1.0 }, Vec3::new(0.0, 1.0, 0.0), 1.0, mat));
        // The plane material is default (restitution 0); combine = max(0,1)=1.
        w.bodies[ball.index()].lin_vel = Vec3::new(0.0, -5.0, 0.0);

        // Contact normal A->B. A = plane (id 0), B = ball (id 1). Normal points
        // from plane to ball = +Y. Contact point at the plane surface.
        let mut manifolds = vec![one_contact(plane, ball, Vec3::new(0.0, 0.0, 0.0), Vec3::Y, 0.0)];
        let h = w.dt / w.substeps as f64;
        solve_velocity(&mut w, &mut manifolds, h);

        let v = w.bodies[ball.index()].lin_vel.y;
        assert!((v - 5.0).abs() < 1e-9, "elastic bounce should reverse -5 -> +5, got {}", v);
    }

    #[test]
    fn restitution_half_gives_half_rebound() {
        let mut w = empty_world();
        w.params.restitution_threshold = 0.0;
        let plane = w.spawn(Body::plane(Vec3::Y, 0.0));
        let mat = Material { restitution: 0.5, friction: 0.0 };
        let ball = w.spawn(Body::new(Shape::Sphere { radius: 1.0 }, Vec3::new(0.0, 1.0, 0.0), 1.0, mat));
        w.bodies[ball.index()].lin_vel = Vec3::new(0.0, -4.0, 0.0);
        let mut manifolds = vec![one_contact(plane, ball, Vec3::ZERO, Vec3::Y, 0.0)];
        let dt = w.dt;
        solve_velocity(&mut w, &mut manifolds, dt);
        let v = w.bodies[ball.index()].lin_vel.y;
        assert!((v - 2.0).abs() < 1e-9, "e=0.5 on -4 should give +2, got {}", v);
    }

    #[test]
    fn restitution_suppressed_below_threshold() {
        // A slow approach (below the restitution threshold) should NOT bounce:
        // the contact just becomes resting (v_n driven to 0), no rebound.
        let mut w = empty_world();
        w.params.restitution_threshold = 0.5; // default-ish
        let plane = w.spawn(Body::plane(Vec3::Y, 0.0));
        let mat = Material { restitution: 1.0, friction: 0.0 };
        let ball = w.spawn(Body::new(Shape::Sphere { radius: 1.0 }, Vec3::new(0.0, 1.0, 0.0), 1.0, mat));
        w.bodies[ball.index()].lin_vel = Vec3::new(0.0, -0.2, 0.0); // |v| < threshold
        let mut manifolds = vec![one_contact(plane, ball, Vec3::ZERO, Vec3::Y, 0.0)];
        let dt = w.dt;
        solve_velocity(&mut w, &mut manifolds, dt);
        let v = w.bodies[ball.index()].lin_vel.y;
        assert!(v.abs() < 1e-9, "sub-threshold approach should rest at ~0, got {}", v);
    }

    // -- two-body momentum + KE conservation ---------------------------------

    #[test]
    fn head_on_elastic_conserves_momentum_and_energy() {
        // Two equal spheres approaching head-on along X; e=1. Elastic equal-mass
        // collision should SWAP velocities, conserving both momentum and KE.
        let mut w = empty_world();
        w.params.restitution_threshold = 0.0;
        w.params.velocity_iterations = 20;
        let mat = Material { restitution: 1.0, friction: 0.0 };

        // A at x=-1 moving +X; B at x=+1 moving -X. Touching at origin.
        let a = w.spawn(Body::new(Shape::Sphere { radius: 1.0 }, Vec3::new(-1.0, 0.0, 0.0), 1.0, mat));
        let b = w.spawn(Body::new(Shape::Sphere { radius: 1.0 }, Vec3::new(1.0, 0.0, 0.0), 1.0, mat));
        w.bodies[a.index()].lin_vel = Vec3::new(3.0, 0.0, 0.0);
        w.bodies[b.index()].lin_vel = Vec3::new(-3.0, 0.0, 0.0);

        let p_before = w.bodies[a.index()].lin_vel + w.bodies[b.index()].lin_vel;
        let ke_before = 0.5 * w.bodies[a.index()].lin_vel.length_squared()
            + 0.5 * w.bodies[b.index()].lin_vel.length_squared();

        // Normal A->B = +X.
        let mut manifolds = vec![one_contact(a, b, Vec3::ZERO, Vec3::X, 0.0)];
        let dt = w.dt;
        solve_velocity(&mut w, &mut manifolds, dt);

        let va = w.bodies[a.index()].lin_vel;
        let vb = w.bodies[b.index()].lin_vel;
        let p_after = va + vb;
        let ke_after = 0.5 * va.length_squared() + 0.5 * vb.length_squared();

        // Momentum conserved exactly (equal/opposite impulses).
        assert!((p_after.x - p_before.x).abs() < 1e-9, "momentum px {} -> {}", p_before.x, p_after.x);
        assert!((p_after.y).abs() < 1e-12 && (p_after.z).abs() < 1e-12);
        // KE conserved for e=1.
        assert!((ke_after - ke_before).abs() < 1e-6, "KE {} -> {}", ke_before, ke_after);
        // Equal-mass elastic head-on => velocities swap.
        assert!((va.x - (-3.0)).abs() < 1e-6, "A should end at -3, got {}", va.x);
        assert!((vb.x - 3.0).abs() < 1e-6, "B should end at +3, got {}", vb.x);
    }

    #[test]
    fn head_on_inelastic_conserves_momentum_loses_energy() {
        // e=0 perfectly inelastic: equal masses should end at the common
        // (center-of-mass) velocity; momentum conserved, KE strictly decreases.
        let mut w = empty_world();
        w.params.restitution_threshold = 0.0;
        w.params.velocity_iterations = 20;
        let mat = Material { restitution: 0.0, friction: 0.0 };
        let a = w.spawn(Body::new(Shape::Sphere { radius: 1.0 }, Vec3::new(-1.0, 0.0, 0.0), 1.0, mat));
        let b = w.spawn(Body::new(Shape::Sphere { radius: 1.0 }, Vec3::new(1.0, 0.0, 0.0), 1.0, mat));
        w.bodies[a.index()].lin_vel = Vec3::new(2.0, 0.0, 0.0);
        w.bodies[b.index()].lin_vel = Vec3::new(-2.0, 0.0, 0.0);
        let p_before = w.bodies[a.index()].lin_vel.x + w.bodies[b.index()].lin_vel.x;
        let ke_before = 0.5 * w.bodies[a.index()].lin_vel.length_squared()
            + 0.5 * w.bodies[b.index()].lin_vel.length_squared();

        let mut manifolds = vec![one_contact(a, b, Vec3::ZERO, Vec3::X, 0.0)];
        let dt = w.dt;
        solve_velocity(&mut w, &mut manifolds, dt);

        let va = w.bodies[a.index()].lin_vel;
        let vb = w.bodies[b.index()].lin_vel;
        assert!(((va.x + vb.x) - p_before).abs() < 1e-9, "momentum lost");
        // Common velocity = 0 (symmetric); both ~0.
        assert!(va.x.abs() < 1e-6 && vb.x.abs() < 1e-6, "inelastic should stick at COM vel, got {} {}", va.x, vb.x);
        let ke_after = 0.5 * va.length_squared() + 0.5 * vb.length_squared();
        assert!(ke_after < ke_before - 1e-6, "inelastic must lose KE: {} -> {}", ke_before, ke_after);
    }

    #[test]
    fn unequal_mass_elastic_matches_analytic() {
        // 1D elastic: m1=1 at +v, m2=3 at rest. Analytic results:
        // v1' = (m1-m2)/(m1+m2) v = (-2/4)*4 = -2 ; v2' = 2 m1/(m1+m2) v = (2/4)*4 = 2.
        let mut w = empty_world();
        w.params.restitution_threshold = 0.0;
        w.params.velocity_iterations = 30;
        let mat = Material { restitution: 1.0, friction: 0.0 };
        let a = w.spawn(Body::new(Shape::Sphere { radius: 1.0 }, Vec3::new(-1.0, 0.0, 0.0), 1.0, mat));
        let b = w.spawn(Body::new(Shape::Sphere { radius: 1.0 }, Vec3::new(1.0, 0.0, 0.0), 3.0, mat));
        w.bodies[a.index()].lin_vel = Vec3::new(4.0, 0.0, 0.0);
        // b at rest
        let mut manifolds = vec![one_contact(a, b, Vec3::ZERO, Vec3::X, 0.0)];
        let dt = w.dt;
        solve_velocity(&mut w, &mut manifolds, dt);
        let va = w.bodies[a.index()].lin_vel.x;
        let vb = w.bodies[b.index()].lin_vel.x;
        assert!((va - (-2.0)).abs() < 1e-6, "v1' expected -2, got {}", va);
        assert!((vb - 2.0).abs() < 1e-6, "v2' expected 2, got {}", vb);
        // Momentum: 1*4 = 4 ; after: 1*-2 + 3*2 = 4.
        assert!(((1.0 * va + 3.0 * vb) - 4.0).abs() < 1e-6);
    }

    // -- friction halts a sliding body ---------------------------------------

    #[test]
    fn friction_halts_sliding_body() {
        // A rotation-locked body sliding in +X on the floor under gravity. Each
        // integrate+solve cycle, gravity loads the contact (giving the friction
        // cone a real budget = μ·N), and Coulomb friction brakes the tangential
        // velocity. Over enough cycles the slide must HALT and never reverse.
        //
        // Rotation is locked (inv_inertia = ZERO) so this isolates the *sliding*
        // friction law from the tipping torque a single-point base contact would
        // otherwise induce — a clean, deterministic translational test.
        let mut w = empty_world();
        w.params.restitution_threshold = 0.5;
        w.params.velocity_iterations = 8;
        let h = w.dt / w.substeps as f64;

        let plane = w.spawn(Body::plane(Vec3::Y, 0.0));
        let mat = Material { restitution: 0.0, friction: 1.0 };
        let box_id = w.spawn(Body::new(
            Shape::Cuboid { half_extents: Vec3::splat(0.5) },
            Vec3::new(0.0, 0.5, 0.0),
            1.0,
            mat,
        ));
        // Lock rotation -> pure point-mass slide.
        w.bodies[box_id.index()].inv_inertia = Mat3::ZERO;
        w.bodies[box_id.index()].lin_vel = Vec3::new(3.0, 0.0, 0.0);

        for _ in 0..400 {
            // gravity loads the contact downward.
            w.bodies[box_id.index()].lin_vel += w.gravity * h;
            let base = w.bodies[box_id.index()].pos.y - 0.5;
            let pen = (-base).max(0.0);
            // single base contact (rotation is locked, so 1 point suffices).
            let cx = w.bodies[box_id.index()].pos.x;
            let mut manifolds = vec![one_contact(plane, box_id, Vec3::new(cx, 0.0, 0.0), Vec3::Y, pen)];
            solve_velocity(&mut w, &mut manifolds, h);
            let v = w.bodies[box_id.index()].lin_vel;
            w.bodies[box_id.index()].pos += v * h;
            correct_positions(&mut w, &manifolds);
        }

        let vx = w.bodies[box_id.index()].lin_vel.x;
        let omega = w.bodies[box_id.index()].ang_vel.length();
        // Started at 3.0; combined μ = sqrt(1.0 * 0.5) ≈ 0.707 must brake it to a
        // near halt and must NOT push it backwards. Rotation stays locked.
        assert!(vx < 3.0 - 1e-6, "friction must reduce sliding speed, got {}", vx);
        assert!(vx >= -1e-6, "friction must not reverse the body, got {}", vx);
        assert!(vx < 0.2, "friction should bring the slide nearly to rest, got {}", vx);
        assert!(omega < 1e-12, "rotation was locked; ang_vel must stay zero, got {}", omega);
    }

    #[test]
    fn zero_friction_preserves_tangential_velocity() {
        // mu=0 => no tangential change at all.
        let mut w = empty_world();
        w.params.restitution_threshold = 0.5;
        w.params.velocity_iterations = 20;
        let plane = w.spawn(Body::plane(Vec3::Y, 0.0));
        let mat = Material { restitution: 0.0, friction: 0.0 };
        let box_id = w.spawn(Body::new(
            Shape::Cuboid { half_extents: Vec3::splat(0.5) },
            Vec3::new(0.0, 0.5, 0.0),
            1.0,
            mat,
        ));
        w.bodies[box_id.index()].lin_vel = Vec3::new(2.0, -0.1, 0.0);
        let mut manifolds = vec![one_contact(plane, box_id, Vec3::ZERO, Vec3::Y, 0.0)];
        let dt = w.dt;
        solve_velocity(&mut w, &mut manifolds, dt);
        let vx = w.bodies[box_id.index()].lin_vel.x;
        assert!((vx - 2.0).abs() < 1e-9, "mu=0 must not change tangential velocity, got {}", vx);
    }

    // -- position correction --------------------------------------------------

    #[test]
    fn position_correction_pushes_penetration_out() {
        // A dynamic ball overlapping a static plane by 0.1. Correction should move
        // the ball OUT along +normal (the dynamic body absorbs the full push since
        // the plane is immovable), never the plane.
        let mut w = empty_world();
        w.params.baumgarte_beta = 1.0; // full correction for a crisp assert
        w.params.penetration_slop = 0.0;
        let plane = w.spawn(Body::plane(Vec3::Y, 0.0));
        let ball = w.spawn(Body::new(Shape::Sphere { radius: 1.0 }, Vec3::new(0.0, 0.9, 0.0), 1.0, Material::default()));
        let manifolds = vec![one_contact(plane, ball, Vec3::ZERO, Vec3::Y, 0.1)];
        let before_plane = w.bodies[plane.index()].pos;
        let remaining = correct_positions(&mut w, &manifolds);
        assert!((remaining - 0.1).abs() < 1e-12, "reports pre-correction max penetration");
        // Ball moved up by ~0.1 along +Y.
        assert!((w.bodies[ball.index()].pos.y - 1.0).abs() < 1e-9, "ball should be pushed to y=1.0, got {}", w.bodies[ball.index()].pos.y);
        // Plane (static) did not move.
        assert_eq!(w.bodies[plane.index()].pos, before_plane);
    }

    #[test]
    fn position_correction_respects_slop_and_beta() {
        // Penetration within slop => no movement. Beta scales the excess.
        let mut w = empty_world();
        w.params.penetration_slop = 0.05;
        w.params.baumgarte_beta = 0.5;
        let plane = w.spawn(Body::plane(Vec3::Y, 0.0));
        let ball = w.spawn(Body::new(Shape::Sphere { radius: 1.0 }, Vec3::new(0.0, 0.9, 0.0), 1.0, Material::default()));

        // Within slop: no move.
        let within = vec![one_contact(plane, ball, Vec3::ZERO, Vec3::Y, 0.04)];
        let y0 = w.bodies[ball.index()].pos.y;
        correct_positions(&mut w, &within);
        assert!((w.bodies[ball.index()].pos.y - y0).abs() < 1e-12, "within slop must not move");

        // Beyond slop: move beta*(pen-slop) = 0.5*(0.15-0.05)=0.05 along +Y.
        let beyond = vec![one_contact(plane, ball, Vec3::ZERO, Vec3::Y, 0.15)];
        correct_positions(&mut w, &beyond);
        let moved = w.bodies[ball.index()].pos.y - y0;
        assert!((moved - 0.05).abs() < 1e-9, "expected +0.05 pushout, got {}", moved);
    }

    #[test]
    fn position_correction_splits_by_inverse_mass() {
        // Two dynamic bodies of EQUAL mass overlapping: each should move half the
        // correction in opposite directions along the normal.
        let mut w = empty_world();
        w.params.baumgarte_beta = 1.0;
        w.params.penetration_slop = 0.0;
        let a = w.spawn(Body::new(Shape::Sphere { radius: 1.0 }, Vec3::new(-0.9, 0.0, 0.0), 1.0, Material::default()));
        let b = w.spawn(Body::new(Shape::Sphere { radius: 1.0 }, Vec3::new(0.9, 0.0, 0.0), 1.0, Material::default()));
        // Overlap 0.2 along X, normal A->B = +X.
        let manifolds = vec![one_contact(a, b, Vec3::ZERO, Vec3::X, 0.2)];
        correct_positions(&mut w, &manifolds);
        // Each moves 0.1: A to -1.0, B to +1.0.
        assert!((w.bodies[a.index()].pos.x - (-1.0)).abs() < 1e-9, "A x={}", w.bodies[a.index()].pos.x);
        assert!((w.bodies[b.index()].pos.x - 1.0).abs() < 1e-9, "B x={}", w.bodies[b.index()].pos.x);
    }

    #[test]
    fn multi_point_face_manifold_rests_stably() {
        // A box resting on the floor via a realistic 4-corner face manifold (as
        // the box-plane narrowphase emits). Under repeated gravity loading the box
        // must settle flat: stay near rest height with negligible spin (the four
        // symmetric normal impulses balance, so no net torque).
        let mut w = empty_world();
        w.params.velocity_iterations = 12;
        let h = w.dt / w.substeps as f64;
        let plane = w.spawn(Body::plane(Vec3::Y, 0.0));
        let mat = Material { restitution: 0.0, friction: 0.5 };
        let box_id = w.spawn(Body::new(
            Shape::Cuboid { half_extents: Vec3::splat(0.5) },
            Vec3::new(0.0, 0.5, 0.0),
            1.0,
            mat,
        ));

        for _ in 0..300 {
            w.bodies[box_id.index()].lin_vel += w.gravity * h;
            let cx = w.bodies[box_id.index()].pos.x;
            let cz = w.bodies[box_id.index()].pos.z;
            let base = w.bodies[box_id.index()].pos.y - 0.5;
            let pen = (-base).max(0.0);
            let corners = [
                Vec3::new(cx - 0.5, 0.0, cz - 0.5),
                Vec3::new(cx + 0.5, 0.0, cz - 0.5),
                Vec3::new(cx - 0.5, 0.0, cz + 0.5),
                Vec3::new(cx + 0.5, 0.0, cz + 0.5),
            ];
            let mut manifolds = vec![multi_contact(plane, box_id, &corners, Vec3::Y, pen)];
            solve_velocity(&mut w, &mut manifolds, h);
            let v = w.bodies[box_id.index()].lin_vel;
            w.bodies[box_id.index()].pos += v * h;
            correct_positions(&mut w, &manifolds);
        }

        let y = w.bodies[box_id.index()].pos.y;
        assert!(y > 0.5 - 0.05 && y < 0.5 + 0.05, "box did not rest near 0.5: y={}", y);
        // A symmetric face manifold must not spin the box UP; tiny residual from
        // the sequential (one-corner-at-a-time) order is allowed but must stay
        // bounded — no runaway rotation.
        assert!(w.bodies[box_id.index()].ang_vel.length() < 1e-2, "symmetric face manifold should not spin up: {:?}", w.bodies[box_id.index()].ang_vel);
    }

    // -- stack stability: positions stay bounded over many steps --------------

    #[test]
    fn resting_contact_does_not_sink_or_explode() {
        // A single resting box on the floor, driven by repeated gravity-velocity
        // + solve + position-correct cycles (mimicking the integrator inner loop
        // but local to the solver). Over many steps the box must neither sink far
        // through the floor nor fly off.
        let mut w = empty_world();
        let h = w.dt / w.substeps as f64;
        let plane = w.spawn(Body::plane(Vec3::Y, 0.0));
        let mat = Material { restitution: 0.0, friction: 0.5 };
        // Box half-extent 0.5 resting so its base is at y=0 (center y=0.5).
        let box_id = w.spawn(Body::new(
            Shape::Cuboid { half_extents: Vec3::splat(0.5) },
            Vec3::new(0.0, 0.5, 0.0),
            1.0,
            mat,
        ));

        for _ in 0..600 {
            // 1. gravity velocity integrate (dynamic body).
            w.bodies[box_id.index()].lin_vel += w.gravity * h;
            // 2. contact: base of the box vs the plane. Penetration = how far the
            //    base dipped below y=0.
            let base_y = w.bodies[box_id.index()].pos.y - 0.5;
            let penetration = (-base_y).max(0.0);
            let mut manifolds = if penetration >= 0.0 && base_y <= 0.01 {
                vec![one_contact(plane, box_id, Vec3::new(0.0, 0.0, 0.0), Vec3::Y, penetration)]
            } else {
                Vec::new()
            };
            // 3. velocity solve.
            solve_velocity(&mut w, &mut manifolds, h);
            // 4. position integrate.
            let v = w.bodies[box_id.index()].lin_vel;
            w.bodies[box_id.index()].pos += v * h;
            // 5. position correct.
            correct_positions(&mut w, &manifolds);
        }

        let y = w.bodies[box_id.index()].pos.y;
        // Must stay near the rest height 0.5: not sunk through, not launched.
        assert!(y > 0.5 - 0.05, "box sank through the floor: y={}", y);
        assert!(y < 0.5 + 0.05, "box bounced/exploded off the floor: y={}", y);
        // Velocity should be essentially settled.
        assert!(w.bodies[box_id.index()].lin_vel.length() < 0.5, "resting box velocity not settled: {:?}", w.bodies[box_id.index()].lin_vel);
    }

    #[test]
    fn stack_of_boxes_stays_bounded() {
        // Three stacked boxes over a floor, run through many integrate+solve
        // cycles. Assert the whole stack stays bounded (no sinking through the
        // floor, no explosion) — the classic stack-stability crown-jewel test.
        let mut w = empty_world();
        w.params.velocity_iterations = 12;
        let h = w.dt / w.substeps as f64;
        let he = 0.5; // half-extent
        let plane = w.spawn(Body::plane(Vec3::Y, 0.0));
        let mat = Material { restitution: 0.0, friction: 0.6 };
        // Centers at 0.5, 1.5, 2.5 (each box of full height 1.0 stacked).
        let b0 = w.spawn(Body::new(Shape::Cuboid { half_extents: Vec3::splat(he) }, Vec3::new(0.0, 0.5, 0.0), 1.0, mat));
        let b1 = w.spawn(Body::new(Shape::Cuboid { half_extents: Vec3::splat(he) }, Vec3::new(0.0, 1.5, 0.0), 1.0, mat));
        let b2 = w.spawn(Body::new(Shape::Cuboid { half_extents: Vec3::splat(he) }, Vec3::new(0.0, 2.5, 0.0), 1.0, mat));
        let ids = [b0, b1, b2];

        for _ in 0..800 {
            // gravity integrate
            for &id in &ids {
                w.bodies[id.index()].lin_vel += w.gravity * h;
            }

            // Build contacts: box0-plane, box1-box0, box2-box1, from current pos.
            let mut manifolds = Vec::new();
            // box0 vs plane
            {
                let base = w.bodies[b0.index()].pos.y - he;
                let pen = (-base).max(0.0);
                if base <= 0.02 {
                    manifolds.push(one_contact(plane, b0, Vec3::new(0.0, 0.0, 0.0), Vec3::Y, pen));
                }
            }
            // box(i) on box(i-1): normal A=lower -> B=upper = +Y.
            for pair in [(b0, b1), (b1, b2)] {
                let (lower, upper) = pair;
                let gap = (w.bodies[upper.index()].pos.y - w.bodies[lower.index()].pos.y) - 2.0 * he;
                let pen = (-gap).max(0.0);
                if gap <= 0.02 {
                    let contact_y = w.bodies[lower.index()].pos.y + he;
                    manifolds.push(one_contact(lower, upper, Vec3::new(0.0, contact_y, 0.0), Vec3::Y, pen));
                }
            }

            // Multiple solver passes over the manifold SET help a stack settle.
            solve_velocity(&mut w, &mut manifolds, h);

            // position integrate
            for &id in &ids {
                let v = w.bodies[id.index()].lin_vel;
                w.bodies[id.index()].pos += v * h;
            }

            // position correct (a couple of passes for the stack)
            correct_positions(&mut w, &manifolds);
            correct_positions(&mut w, &manifolds);
        }

        // Expected rest heights ~0.5, 1.5, 2.5 (allow generous settle band).
        let y0 = w.bodies[b0.index()].pos.y;
        let y1 = w.bodies[b1.index()].pos.y;
        let y2 = w.bodies[b2.index()].pos.y;
        assert!(y0 > 0.45 && y0 < 0.60, "box0 out of band: {}", y0);
        assert!(y1 > 1.40 && y1 < 1.70, "box1 out of band: {}", y1);
        assert!(y2 > 2.30 && y2 < 2.85, "box2 out of band: {}", y2);
        // Order preserved + not interpenetrating wildly.
        assert!(y1 > y0 && y2 > y1, "stack order broke: {} {} {}", y0, y1, y2);
        // No NaN/explosion.
        for &id in &ids {
            assert!(w.bodies[id.index()].pos.is_finite(), "stack body went non-finite");
        }
    }

    // -- determinism: identical scenes -> identical state --------------------

    #[test]
    fn solver_is_deterministic() {
        fn run() -> (Vec3, Vec3, Vec3, Vec3) {
            let mut w = empty_world();
            w.params.restitution_threshold = 0.0;
            w.params.velocity_iterations = 16;
            let mat = Material { restitution: 0.7, friction: 0.5 };
            let a = w.spawn(Body::new(Shape::Sphere { radius: 1.0 }, Vec3::new(-1.0, 0.3, 0.0), 1.0, mat));
            let b = w.spawn(Body::new(Shape::Sphere { radius: 1.0 }, Vec3::new(1.0, -0.2, 0.0), 2.0, mat));
            w.bodies[a.index()].lin_vel = Vec3::new(3.0, -1.0, 0.5);
            w.bodies[b.index()].lin_vel = Vec3::new(-2.0, 0.4, -0.3);
            w.bodies[a.index()].ang_vel = Vec3::new(0.2, -0.1, 0.3);
            w.bodies[b.index()].ang_vel = Vec3::new(-0.1, 0.2, 0.0);
            let mut manifolds = vec![one_contact(a, b, Vec3::new(0.05, 0.05, 0.0), Vec3::new(1.0, 0.2, 0.0), 0.08)];
            let dt = w.dt;
            for _ in 0..5 {
                solve_velocity(&mut w, &mut manifolds, dt);
                correct_positions(&mut w, &manifolds);
            }
            (
                w.bodies[a.index()].lin_vel,
                w.bodies[a.index()].ang_vel,
                w.bodies[b.index()].lin_vel,
                w.bodies[b.index()].ang_vel,
            )
        }
        let first = run();
        for _ in 0..8 {
            assert_eq!(run(), first, "solver must be bit-identical across runs");
        }
    }

    #[test]
    fn empty_manifolds_are_noop() {
        let mut w = empty_world();
        w.spawn_shape(Shape::Sphere { radius: 1.0 }, Vec3::ZERO, 1.0, Material::default());
        let before = w.bodies.clone();
        let dt = w.dt;
        solve_velocity(&mut w, &mut [], dt);
        assert_eq!(w.bodies, before);
        assert_eq!(correct_positions(&mut w, &[]), 0.0);
        assert_eq!(w.bodies, before);
    }

    #[test]
    fn angular_response_from_offset_contact() {
        // An off-center normal impulse on a free body must induce ANGULAR velocity
        // (torque = r × J), exercising the world_inv_inertia path.
        let mut w = empty_world();
        w.params.restitution_threshold = 0.0;
        let mat = Material { restitution: 0.0, friction: 0.0 };
        // Static anchor (plane) as A; a box as B moving down onto an OFFSET point.
        let plane = w.spawn(Body::plane(Vec3::Y, 0.0));
        let box_id = w.spawn(Body::new(Shape::Cuboid { half_extents: Vec3::splat(0.5) }, Vec3::new(0.0, 0.5, 0.0), 1.0, mat));
        w.bodies[box_id.index()].lin_vel = Vec3::new(0.0, -2.0, 0.0);
        // Contact offset in +X from the center => impulse along +Y creates torque about Z.
        let mut manifolds = vec![one_contact(plane, box_id, Vec3::new(0.4, 0.0, 0.0), Vec3::Y, 0.0)];
        let dt = w.dt;
        solve_velocity(&mut w, &mut manifolds, dt);
        let w_ang = w.bodies[box_id.index()].ang_vel;
        assert!(w_ang.length() > 1e-6, "offset contact must induce spin, got {:?}", w_ang);
        // The box's downward velocity at the COM should be reduced (impulse acted up).
        assert!(w.bodies[box_id.index()].lin_vel.y > -2.0 + 1e-9, "normal impulse should slow the descent");
    }
}
