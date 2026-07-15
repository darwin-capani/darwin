//! CASSANDRA's forecast & simulation engine: a SEEDED, deterministic
//! Monte-Carlo / scenario core. PURE — no clock, no network, no globals — so it
//! is fully unit-testable and reproducible: the same seed yields byte-identical
//! paths every run.
//!
//! The cardinal honesty rule, carried straight into the code and the persona:
//! this is a MODEL over the user's (or default) ASSUMPTIONS, not a prediction of
//! reality and NOT financial advice. Inputs are assumptions (drift, volatility,
//! a variable's range); outputs are DISTRIBUTIONS (percentile bands, an expected
//! value), never "the price will be X." A summary that reads like a forecast of
//! the world would be a lie the same way EDITH's round-A overclaim was — so every
//! public summary here is framed as "under these assumptions, the model says."
//!
//! Two pieces, both pure and both seeded:
//!   - [`gbm_paths`] / [`gbm_forecast`] — Geometric-Brownian-Motion price paths
//!     (drift + volatility + horizon + N paths) reduced to p5/p50/p95 percentile
//!     bands and summary stats over the terminal values. GBM is the standard
//!     log-normal model of a price under constant drift and vol; we simulate it
//!     exactly per step (the closed-form log-Euler update), so there is no
//!     discretization drift to muddy the statistical tests.
//!   - [`sample_scenario`] — a generic "what-if" sampler: a set of independent
//!     [`Variable`]s (each a range with a distribution), sampled jointly, reduced
//!     by an injected combine fn to one outcome per draw, then summarized as a
//!     distribution (percentiles + expected value).
//!
//! RNG: a small, self-contained SplitMix64 generator ([`Rng`]) — no `rand`
//! dependency added, deterministic across platforms, and good enough for a
//! scenario model (this is NOT cryptography and is never claimed to be). The
//! Box-Muller transform turns its uniforms into standard normals for the GBM
//! shocks. All randomness threads through one seed; nothing reads the ambient
//! clock or entropy pool, so a test pins exact numbers.

/// A deterministic SplitMix64 PRNG. Tiny, fast, self-contained (no external
/// crate), and fully reproducible from its seed — the property the whole module
/// rests on. NOT a CSPRNG and never used as one; it seeds a scenario model, not
/// a key. Public so the tool layer can construct one from a caller-or-default
/// seed and the tests can pin sequences.
#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    /// Seed the generator. The same seed always produces the same stream — the
    /// guarantee every "seeded determinism" test pins.
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// Next raw u64 (the SplitMix64 step). Advances the state.
    fn next_u64(&mut self) -> u64 {
        // SplitMix64 (Vigna): a well-distributed 64-bit mixer with a full
        // 2^64 period. Constants are the published reference values.
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Next f64 uniformly in the half-open unit interval [0, 1). Uses the top 53
    /// bits (the f64 mantissa width) so every representable value is reachable
    /// with the right probability.
    pub fn next_unit(&mut self) -> f64 {
        // 53 high bits / 2^53 -> [0, 1).
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Next standard-normal sample (mean 0, variance 1) via the Box-Muller
    /// transform. We clamp the first uniform off exact 0 so `ln` never hits
    /// negative infinity (the second uniform may legitimately be 0). Returns one
    /// of the pair; the other is discarded for simplicity (the generator is
    /// cheap and the discard does not bias the kept value).
    pub fn next_normal(&mut self) -> f64 {
        let u1 = self.next_unit().max(f64::MIN_POSITIVE);
        let u2 = self.next_unit();
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }
}

// ---------------------------------------------------------------------------
// Distribution summary (the shared "output is a distribution" shape)
// ---------------------------------------------------------------------------

/// A summary of a sampled distribution: the percentile bands CASSANDRA reports
/// plus the mean and the sample size. This is the OUTPUT shape — a distribution,
/// never a single "the answer is X" — for both the GBM forecast (over terminal
/// prices) and the generic scenario sampler (over outcomes).
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    /// 5th percentile — a pessimistic-tail reading under the assumptions.
    pub p5: f64,
    /// 50th percentile (median) — the central reading.
    pub p50: f64,
    /// 95th percentile — an optimistic-tail reading under the assumptions.
    pub p95: f64,
    /// Arithmetic mean of the samples (the expected value under the model).
    pub mean: f64,
    /// Smallest sampled value.
    pub min: f64,
    /// Largest sampled value.
    pub max: f64,
    /// Number of samples the summary is built from.
    pub samples: usize,
}

/// The linear-interpolated `q`-quantile (q in [0,1]) of `sorted` (ascending,
/// non-empty). Interpolates between the two bracketing order statistics so the
/// percentile is smooth in the sample size — the convention NumPy's default uses.
fn quantile_sorted(sorted: &[f64], q: f64) -> f64 {
    debug_assert!(!sorted.is_empty());
    let n = sorted.len();
    if n == 1 {
        return sorted[0];
    }
    let q = q.clamp(0.0, 1.0);
    let pos = q * (n - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    if lo == hi {
        sorted[lo]
    } else {
        let frac = pos - lo as f64;
        sorted[lo] * (1.0 - frac) + sorted[hi] * frac
    }
}

/// Summarize a set of samples into the [`Summary`] band. Sorts a copy (so the
/// caller's slice is untouched) and reports p5/p50/p95, mean, min, max. Returns
/// `None` for an empty input — there is no distribution to summarize, and we
/// never fabricate one. NaNs are dropped before summarizing so a single bad draw
/// cannot poison the sort/quantiles.
pub fn summarize(samples: &[f64]) -> Option<Summary> {
    let mut sorted: Vec<f64> = samples.iter().copied().filter(|x| x.is_finite()).collect();
    if sorted.is_empty() {
        return None;
    }
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("finite after filter"));
    let n = sorted.len();
    let mean = sorted.iter().sum::<f64>() / n as f64;
    Some(Summary {
        p5: quantile_sorted(&sorted, 0.05),
        p50: quantile_sorted(&sorted, 0.50),
        p95: quantile_sorted(&sorted, 0.95),
        mean,
        min: sorted[0],
        max: sorted[n - 1],
        samples: n,
    })
}

// ---------------------------------------------------------------------------
// Geometric Brownian Motion (price paths)
// ---------------------------------------------------------------------------

/// The assumptions a GBM forecast runs over. Every field is an ASSUMPTION the
/// caller supplies (or a default stands in) — NOT a measured fact about any real
/// instrument. Drift and vol are per-horizon-unit rates (e.g. per year if the
/// horizon is in years); the model is unit-agnostic, so "what could happen" is
/// always relative to the inputs, never an absolute prediction of a real market.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GbmParams {
    /// Starting value (price at step 0). Must be > 0 for a log-normal model.
    pub spot: f64,
    /// Drift: the assumed mean log-return per unit time (mu). Can be negative.
    pub drift: f64,
    /// Volatility: the assumed std-dev of log-returns per unit time (sigma). >= 0.
    pub volatility: f64,
    /// Horizon in the SAME time unit drift/vol are quoted in (e.g. 1.0 = one
    /// year). Must be > 0.
    pub horizon: f64,
    /// Number of discrete steps the horizon is split into (>= 1). More steps =
    /// a finer path; the terminal distribution is exact regardless (log-Euler).
    pub steps: usize,
    /// Number of independent paths to simulate (>= 1). Larger N tightens the
    /// Monte-Carlo estimate of the bands toward the model's true distribution.
    pub paths: usize,
}

impl Default for GbmParams {
    /// Neutral, clearly-a-placeholder defaults: spot 100, zero drift, 20% vol,
    /// one-unit horizon, daily-ish granularity, a modest path count. These are
    /// ASSUMPTIONS to be overridden, never a claim about any instrument.
    fn default() -> Self {
        Self {
            spot: 100.0,
            drift: 0.0,
            volatility: 0.20,
            horizon: 1.0,
            steps: 252,
            paths: 1000,
        }
    }
}

impl GbmParams {
    /// Validate the assumptions, returning a human-readable reason when they are
    /// not simulable (so the tool layer can report it instead of producing
    /// garbage or panicking). Pure.
    pub fn validate(&self) -> Result<(), String> {
        if !(self.spot.is_finite() && self.spot > 0.0) {
            return Err("spot must be a positive, finite number".to_string());
        }
        if !self.drift.is_finite() {
            return Err("drift must be a finite number".to_string());
        }
        if !(self.volatility.is_finite() && self.volatility >= 0.0) {
            return Err("volatility must be a finite, non-negative number".to_string());
        }
        if !(self.horizon.is_finite() && self.horizon > 0.0) {
            return Err("horizon must be a positive, finite number".to_string());
        }
        if self.steps == 0 {
            return Err("steps must be at least 1".to_string());
        }
        if self.paths == 0 {
            return Err("paths must be at least 1".to_string());
        }
        Ok(())
    }
}

/// The result of a GBM forecast: the distribution of TERMINAL values (the price
/// at the horizon) summarized into bands, plus the assumptions it ran over so the
/// caller can echo them back honestly. The bands are "under these assumptions"
/// outcomes, not a prediction of any real price.
#[derive(Debug, Clone, PartialEq)]
pub struct GbmForecast {
    /// The summarized distribution of terminal prices across the paths.
    pub terminal: Summary,
    /// The assumptions this forecast ran over (echoed for honest reporting).
    pub params: GbmParams,
}

/// Simulate `params.paths` GBM paths and return each path's TERMINAL value. Pure
/// and seeded: the same (`params`, `seed`) always yields the same vector. Uses
/// the EXACT log-Euler update per step,
///   S_{t+dt} = S_t * exp((mu - sigma^2/2) dt + sigma sqrt(dt) Z),
/// which is the closed-form GBM increment (no discretization bias), so the
/// terminal distribution is exactly log-normal in the limit of many paths. The
/// per-step shocks Z are standard normals from the injected [`Rng`].
///
/// Returns `Err` with a reason if the assumptions don't validate — never panics
/// on bad input.
pub fn gbm_terminals(params: &GbmParams, seed: u64) -> Result<Vec<f64>, String> {
    params.validate()?;
    let dt = params.horizon / params.steps as f64;
    let sqrt_dt = dt.sqrt();
    let drift_term = (params.drift - 0.5 * params.volatility * params.volatility) * dt;
    let vol_term = params.volatility * sqrt_dt;
    let mut rng = Rng::new(seed);
    let mut terminals = Vec::with_capacity(params.paths);
    for _ in 0..params.paths {
        let mut s = params.spot;
        for _ in 0..params.steps {
            let z = rng.next_normal();
            s *= (drift_term + vol_term * z).exp();
        }
        terminals.push(s);
    }
    Ok(terminals)
}

/// Simulate full GBM paths (every step retained) — `params.paths` rows, each of
/// length `params.steps + 1` (including the spot at index 0). Pure and seeded,
/// SHARING the exact step update with [`gbm_terminals`], so a path's last column
/// equals the corresponding terminal. Used where the full trajectory matters
/// (e.g. percentile bands over time); the tool path uses the lighter
/// [`gbm_terminals`]. Returns `Err` on invalid assumptions.
///
/// Exercised by the unit tests and kept as a documented part of the engine's
/// surface for a future bands-over-time view; `allow(dead_code)` in the non-test
/// build keeps the unused-in-binary lint quiet without masking real warnings.
#[cfg_attr(not(test), allow(dead_code))]
pub fn gbm_paths(params: &GbmParams, seed: u64) -> Result<Vec<Vec<f64>>, String> {
    params.validate()?;
    let dt = params.horizon / params.steps as f64;
    let sqrt_dt = dt.sqrt();
    let drift_term = (params.drift - 0.5 * params.volatility * params.volatility) * dt;
    let vol_term = params.volatility * sqrt_dt;
    let mut rng = Rng::new(seed);
    let mut paths = Vec::with_capacity(params.paths);
    for _ in 0..params.paths {
        let mut row = Vec::with_capacity(params.steps + 1);
        let mut s = params.spot;
        row.push(s);
        for _ in 0..params.steps {
            let z = rng.next_normal();
            s *= (drift_term + vol_term * z).exp();
            row.push(s);
        }
        paths.push(row);
    }
    Ok(paths)
}

/// Run a GBM forecast: simulate terminal prices and summarize them into bands.
/// Pure, seeded, and honest — the [`GbmForecast`] carries both the distribution
/// and the assumptions it ran over. Returns `Err` on invalid assumptions.
pub fn gbm_forecast(params: &GbmParams, seed: u64) -> Result<GbmForecast, String> {
    let terminals = gbm_terminals(params, seed)?;
    let terminal = summarize(&terminals)
        .ok_or_else(|| "no terminal samples to summarize".to_string())?;
    Ok(GbmForecast { terminal, params: *params })
}

// ---------------------------------------------------------------------------
// Generic scenario sampler (the "what-if" engine)
// ---------------------------------------------------------------------------

/// How a scenario [`Variable`] is sampled across its range. All are bounded by
/// `[low, high]` so a draw can never wander outside the assumption the user
/// stated.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Distribution {
    /// Every value in `[low, high]` equally likely.
    Uniform,
    /// Symmetric triangular on `[low, high]` peaking at the midpoint — a simple
    /// "most likely in the middle, extremes rarer" assumption with no extra
    /// parameter. The mean equals the midpoint.
    Triangular,
}

/// One independent input variable for a what-if scenario: a name, a bounded
/// range, and how it is distributed within that range. Every field is an
/// ASSUMPTION the caller supplies.
#[derive(Debug, Clone, PartialEq)]
pub struct Variable {
    /// Human-readable label (echoed in reporting).
    pub name: String,
    /// Inclusive lower bound of the range.
    pub low: f64,
    /// Inclusive upper bound of the range.
    pub high: f64,
    /// The sampling distribution within `[low, high]`.
    pub dist: Distribution,
}

impl Variable {
    /// Draw one sample for this variable from the injected RNG, honoring its
    /// distribution and staying within `[low, high]`. A degenerate range
    /// (`low == high`, or `high < low`) collapses to `low` — a fixed
    /// assumption, never a panic.
    fn sample(&self, rng: &mut Rng) -> f64 {
        let (lo, hi) = (self.low, self.high);
        // Degenerate range (hi <= lo, or a NaN bound) collapses to lo — a fixed
        // assumption, never a panic. partial_cmp keeps the NaN case explicit.
        if !matches!(hi.partial_cmp(&lo), Some(std::cmp::Ordering::Greater)) {
            return lo;
        }
        match self.dist {
            Distribution::Uniform => lo + (hi - lo) * rng.next_unit(),
            Distribution::Triangular => {
                // Average of two uniforms over [lo,hi] is symmetric-triangular
                // on [lo,hi] peaking at the midpoint — exact, no inverse-CDF.
                let a = lo + (hi - lo) * rng.next_unit();
                let b = lo + (hi - lo) * rng.next_unit();
                0.5 * (a + b)
            }
        }
    }
}

/// Sample a what-if scenario: draw `draws` joint samples of the independent
/// `vars`, reduce each joint draw to a single outcome via `combine`, and
/// summarize the resulting outcomes into a distribution. Pure and seeded — the
/// same (`vars`, `combine`, `draws`, `seed`) always yields the same [`Summary`].
///
/// `combine` is injected so the engine is generic over the question: a sum of
/// costs, a product of growth factors, a profit = revenue - cost, etc. The
/// engine never assumes what the variables MEAN — it samples assumptions and
/// reports the outcome distribution, leaving interpretation (honestly) to the
/// caller. Returns `Err` when there are no variables or no draws (nothing to
/// sample — we don't fabricate a distribution).
pub fn sample_scenario(
    vars: &[Variable],
    combine: impl Fn(&[f64]) -> f64,
    draws: usize,
    seed: u64,
) -> Result<Summary, String> {
    if vars.is_empty() {
        return Err("a scenario needs at least one variable".to_string());
    }
    if draws == 0 {
        return Err("a scenario needs at least one draw".to_string());
    }
    let mut rng = Rng::new(seed);
    let mut row = vec![0.0; vars.len()];
    let mut outcomes = Vec::with_capacity(draws);
    for _ in 0..draws {
        for (slot, v) in row.iter_mut().zip(vars.iter()) {
            *slot = v.sample(&mut rng);
        }
        outcomes.push(combine(&row));
    }
    summarize(&outcomes).ok_or_else(|| "no scenario outcomes to summarize".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- RNG: seeded determinism + basic distribution shape ----------------

    #[test]
    fn rng_is_deterministic_for_a_seed_and_differs_across_seeds() {
        let a: Vec<f64> = {
            let mut r = Rng::new(42);
            (0..8).map(|_| r.next_unit()).collect()
        };
        let b: Vec<f64> = {
            let mut r = Rng::new(42);
            (0..8).map(|_| r.next_unit()).collect()
        };
        assert_eq!(a, b, "same seed must produce identical streams");
        let c: Vec<f64> = {
            let mut r = Rng::new(43);
            (0..8).map(|_| r.next_unit()).collect()
        };
        assert_ne!(a, c, "a different seed must produce a different stream");
    }

    #[test]
    fn next_unit_stays_in_the_unit_interval() {
        let mut r = Rng::new(7);
        for _ in 0..10_000 {
            let u = r.next_unit();
            assert!((0.0..1.0).contains(&u), "uniform out of [0,1): {u}");
        }
    }

    #[test]
    fn next_normal_has_roughly_zero_mean_unit_variance() {
        let mut r = Rng::new(123);
        let n = 50_000;
        let xs: Vec<f64> = (0..n).map(|_| r.next_normal()).collect();
        let mean = xs.iter().sum::<f64>() / n as f64;
        let var = xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n as f64;
        // Loose bounds — a sanity check on the Box-Muller normals, not a strict
        // statistical test.
        assert!(mean.abs() < 0.05, "normal mean off: {mean}");
        assert!((var - 1.0).abs() < 0.1, "normal variance off: {var}");
    }

    // ---- summarize / quantiles --------------------------------------------

    #[test]
    fn summarize_reports_monotonic_percentiles_and_correct_extremes() {
        let xs: Vec<f64> = (1..=100).map(|i| i as f64).collect();
        let s = summarize(&xs).expect("non-empty");
        assert_eq!(s.samples, 100);
        assert_eq!(s.min, 1.0);
        assert_eq!(s.max, 100.0);
        // Percentiles are monotonically ordered.
        assert!(s.p5 <= s.p50, "p5 <= p50");
        assert!(s.p50 <= s.p95, "p50 <= p95");
        // Mean of 1..=100 is 50.5; median is between 50 and 51.
        assert!((s.mean - 50.5).abs() < 1e-9, "mean: {}", s.mean);
        assert!(s.p50 > 50.0 && s.p50 < 51.0, "median: {}", s.p50);
    }

    #[test]
    fn summarize_drops_nans_and_rejects_empty() {
        assert!(summarize(&[]).is_none(), "empty -> None");
        assert!(summarize(&[f64::NAN, f64::INFINITY]).is_none(), "all-nonfinite -> None");
        let s = summarize(&[1.0, f64::NAN, 3.0]).expect("two finite remain");
        assert_eq!(s.samples, 2, "NaN dropped");
        assert_eq!(s.min, 1.0);
        assert_eq!(s.max, 3.0);
    }

    #[test]
    fn quantile_endpoints_and_interpolation() {
        let xs = [10.0, 20.0, 30.0, 40.0, 50.0];
        assert_eq!(quantile_sorted(&xs, 0.0), 10.0);
        assert_eq!(quantile_sorted(&xs, 1.0), 50.0);
        // Median of 5 sorted values is the middle one.
        assert_eq!(quantile_sorted(&xs, 0.5), 30.0);
        // q=0.25 -> pos=1.0 -> exactly the second value.
        assert_eq!(quantile_sorted(&xs, 0.25), 20.0);
        // Single value: any quantile is that value.
        assert_eq!(quantile_sorted(&[7.0], 0.95), 7.0);
    }

    // ---- GBM: validation, determinism, statistical properties --------------

    #[test]
    fn gbm_validate_rejects_bad_assumptions() {
        let ok = GbmParams::default();
        assert!(ok.validate().is_ok());
        assert!(GbmParams { spot: 0.0, ..ok }.validate().is_err(), "spot>0");
        assert!(GbmParams { spot: -1.0, ..ok }.validate().is_err());
        assert!(GbmParams { volatility: -0.1, ..ok }.validate().is_err(), "vol>=0");
        assert!(GbmParams { horizon: 0.0, ..ok }.validate().is_err(), "horizon>0");
        assert!(GbmParams { steps: 0, ..ok }.validate().is_err(), "steps>=1");
        assert!(GbmParams { paths: 0, ..ok }.validate().is_err(), "paths>=1");
        // A bad-input simulate returns Err, never panics.
        assert!(gbm_terminals(&GbmParams { spot: 0.0, ..ok }, 1).is_err());
    }

    #[test]
    fn gbm_is_seeded_deterministic() {
        let p = GbmParams { paths: 200, steps: 50, ..Default::default() };
        let a = gbm_terminals(&p, 99).unwrap();
        let b = gbm_terminals(&p, 99).unwrap();
        assert_eq!(a, b, "same seed = identical terminal prices");
        let c = gbm_terminals(&p, 100).unwrap();
        assert_ne!(a, c, "a different seed = different prices");
        // The forecast summary is likewise reproducible.
        assert_eq!(gbm_forecast(&p, 99).unwrap(), gbm_forecast(&p, 99).unwrap());
    }

    #[test]
    fn gbm_terminal_column_matches_full_path_last_value() {
        // gbm_paths and gbm_terminals share the exact step update under the same
        // seed, so a path's final value equals the terminal.
        let p = GbmParams { paths: 30, steps: 20, ..Default::default() };
        let terminals = gbm_terminals(&p, 5).unwrap();
        let paths = gbm_paths(&p, 5).unwrap();
        assert_eq!(paths.len(), p.paths);
        for (path, t) in paths.iter().zip(terminals.iter()) {
            assert_eq!(path.len(), p.steps + 1, "path includes the spot + every step");
            assert_eq!(path[0], p.spot, "path starts at spot");
            assert_eq!(*path.last().unwrap(), *t, "last column == terminal");
        }
    }

    #[test]
    fn gbm_terminal_mean_converges_toward_the_lognormal_theory() {
        // For GBM, E[S_T] = spot * exp(drift * horizon) exactly (independent of
        // vol). With many paths the Monte-Carlo mean should land near it. Loose
        // bounds — this is a convergence sanity check, not a tight estimator.
        let p = GbmParams {
            spot: 100.0,
            drift: 0.05,
            volatility: 0.2,
            horizon: 1.0,
            steps: 252,
            paths: 20_000,
        };
        let theory = p.spot * (p.drift * p.horizon).exp();
        let f = gbm_forecast(&p, 2024).unwrap();
        let rel_err = (f.terminal.mean - theory).abs() / theory;
        assert!(rel_err < 0.05, "MC mean {} vs theory {theory} (rel {rel_err})", f.terminal.mean);
    }

    #[test]
    fn gbm_terminal_variance_shrinks_as_paths_grow() {
        // The Monte-Carlo estimate of the mean tightens with N: the spread of
        // the mean across independent seeds should shrink as paths grow. Compare
        // the across-seed std-dev of the mean at small vs large N (loose bound).
        let base = GbmParams { volatility: 0.3, steps: 50, ..Default::default() };
        let mean_spread = |paths: usize| -> f64 {
            let means: Vec<f64> = (0..12)
                .map(|seed| gbm_forecast(&GbmParams { paths, ..base }, seed).unwrap().terminal.mean)
                .collect();
            let m = means.iter().sum::<f64>() / means.len() as f64;
            (means.iter().map(|x| (x - m).powi(2)).sum::<f64>() / means.len() as f64).sqrt()
        };
        let small = mean_spread(100);
        let large = mean_spread(4000);
        assert!(large < small, "estimator spread should shrink: small={small} large={large}");
    }

    #[test]
    fn gbm_bands_are_monotonic_and_bracket_the_median() {
        let p = GbmParams { paths: 5000, volatility: 0.25, ..Default::default() };
        let f = gbm_forecast(&p, 7).unwrap();
        let t = &f.terminal;
        assert!(t.min <= t.p5, "min <= p5");
        assert!(t.p5 <= t.p50, "p5 <= p50");
        assert!(t.p50 <= t.p95, "p50 <= p95");
        assert!(t.p95 <= t.max, "p95 <= max");
        // GBM terminals are strictly positive (a log-normal).
        assert!(t.min > 0.0, "terminal prices stay positive: {}", t.min);
    }

    #[test]
    fn zero_vol_gbm_is_deterministic_drift_only() {
        // With volatility 0, every path is the deterministic compounding of the
        // drift: S_T = spot * exp(drift * horizon), so the whole distribution
        // collapses to a point.
        let p = GbmParams {
            spot: 100.0,
            drift: 0.1,
            volatility: 0.0,
            horizon: 2.0,
            steps: 10,
            paths: 50,
        };
        let f = gbm_forecast(&p, 1).unwrap();
        let expected = p.spot * (p.drift * p.horizon).exp();
        assert!((f.terminal.min - expected).abs() < 1e-6, "min");
        assert!((f.terminal.max - expected).abs() < 1e-6, "max");
        assert!((f.terminal.p50 - expected).abs() < 1e-6, "all bands collapse to the point");
    }

    // ---- scenario sampler --------------------------------------------------

    #[test]
    fn scenario_is_seeded_deterministic_and_validates() {
        let vars = vec![
            Variable { name: "a".into(), low: 0.0, high: 10.0, dist: Distribution::Uniform },
            Variable { name: "b".into(), low: 5.0, high: 5.0, dist: Distribution::Uniform },
        ];
        let sum = |row: &[f64]| row.iter().sum();
        let a = sample_scenario(&vars, sum, 1000, 11).unwrap();
        let b = sample_scenario(&vars, sum, 1000, 11).unwrap();
        assert_eq!(a, b, "same seed = identical scenario summary");
        let c = sample_scenario(&vars, sum, 1000, 12).unwrap();
        assert_ne!(a.mean, c.mean, "different seed = different draw");
        // Empty inputs are rejected, never fabricated.
        assert!(sample_scenario(&[], sum, 10, 1).is_err(), "no variables");
        assert!(sample_scenario(&vars, sum, 0, 1).is_err(), "no draws");
    }

    #[test]
    fn uniform_scenario_mean_lands_near_the_range_midpoint() {
        // A single Uniform[0,100] summed (one var) has expected value 50. Many
        // draws -> the sampled mean lands near it. Loose bound.
        let vars = vec![Variable {
            name: "x".into(),
            low: 0.0,
            high: 100.0,
            dist: Distribution::Uniform,
        }];
        let s = sample_scenario(&vars, |row| row[0], 20_000, 3).unwrap();
        assert!((s.mean - 50.0).abs() < 2.0, "uniform mean near midpoint: {}", s.mean);
        // Samples stay inside the stated range — never outside the assumption.
        assert!(s.min >= 0.0 && s.max <= 100.0, "draws stay in [0,100]: {}..{}", s.min, s.max);
        // Monotonic bands.
        assert!(s.p5 <= s.p50 && s.p50 <= s.p95);
    }

    #[test]
    fn triangular_concentrates_around_the_midpoint_more_than_uniform() {
        // Both share the same midpoint mean (50), but the triangular's central
        // mass is denser: its interquartile-ish band (p5..p95) is NARROWER than
        // the uniform's. A distribution-shape sanity check.
        let mk = |dist| {
            vec![Variable { name: "x".into(), low: 0.0, high: 100.0, dist }]
        };
        let uni = sample_scenario(&mk(Distribution::Uniform), |r| r[0], 20_000, 8).unwrap();
        let tri = sample_scenario(&mk(Distribution::Triangular), |r| r[0], 20_000, 8).unwrap();
        // Both center near 50.
        assert!((uni.mean - 50.0).abs() < 2.0);
        assert!((tri.mean - 50.0).abs() < 2.0);
        // Triangular's tail-to-tail band is tighter.
        let uni_band = uni.p95 - uni.p5;
        let tri_band = tri.p95 - tri.p5;
        assert!(tri_band < uni_band, "triangular tighter: tri={tri_band} uni={uni_band}");
        // Triangular stays in range too.
        assert!(tri.min >= 0.0 && tri.max <= 100.0);
    }

    #[test]
    fn degenerate_variable_collapses_to_low_without_panicking() {
        // high < low and high == low both collapse to `low` — a fixed
        // assumption, never a crash.
        let vars = vec![
            Variable { name: "fixed".into(), low: 3.0, high: 3.0, dist: Distribution::Uniform },
            Variable { name: "inverted".into(), low: 9.0, high: 1.0, dist: Distribution::Triangular },
        ];
        let s = sample_scenario(&vars, |r| r[0] + r[1], 100, 1).unwrap();
        // Every draw is 3 + 9 = 12, so the whole distribution is the point 12.
        assert_eq!(s.min, 12.0);
        assert_eq!(s.max, 12.0);
        assert_eq!(s.p50, 12.0);
    }
}
