//! DATA -> CHART (#41) — a structured numeric series the HUD plots EXACTLY.
//!
//! A data-producing path ("chart this", or an op that surfaces a numeric series)
//! emits a [`ChartSpec`] as a `chart.data` telemetry envelope. The HUD's Chart
//! component renders the series EXACTLY: every emitted point plotted, line segments
//! drawn only BETWEEN the given points, axis labels + ranges derived from the data,
//! and an honest-empty state when the series is empty. This module owns the daemon
//! side: the [`ChartSpec`] vocabulary, its serialization to the telemetry JSON the
//! HUD consumes, and the [`emit_chart`] fire-and-forget emit.
//!
//! ## The CONTRACT (non-negotiable)
//!   * EXACT: the spec carries EXACTLY the data points produced — no interpolation,
//!     no invented/extrapolated point, no resampling. What is emitted is what the
//!     producer computed. The HUD draws segments between the given points and
//!     nothing else.
//!   * HONEST AXES: the axis labels are the producer's; the HUD derives the ranges
//!     from the actual point values (the daemon does not pre-bake a misleading
//!     range). An empty series emits cleanly and the HUD shows an honest-empty
//!     state.
//!   * NEUTRAL: emitting a chart is a PURE presentation act — fire-and-forget over
//!     the existing telemetry hub, dropped silently when no HUD is connected
//!     (exactly like every other telemetry envelope). It changes no gate, takes no
//!     action, reaches no network. The `chart this` op is OFF by default
//!     ([`ChartConfig`]); the emit itself is neutral.
//!
//! Nothing here speaks or acts. It surfaces numbers the daemon already has.

use serde_json::{json, Value};

use crate::telemetry;

/// The kind of chart the HUD renders. All three plot the SAME exact points; the
/// kind only tells the HUD how to draw between them (bars, a connected line, or a
/// compact sparkline) — never to invent a point. `Line`/`Sparkline` are part of the
/// ChartSpec vocabulary the HUD consumes (and a data-producing op selects) — the
/// live system-load op happens to emit `Bar`, so the other two read as unused in
/// the binary while exercised by the spec tests.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartKind {
    /// Discrete bars, one per point.
    Bar,
    /// A line connecting the given points with straight segments (no smoothing).
    Line,
    /// A compact inline line — same data, minimal chrome.
    Sparkline,
}

impl ChartKind {
    /// The stable wire string the HUD switches on.
    pub fn as_str(self) -> &'static str {
        match self {
            ChartKind::Bar => "bar",
            ChartKind::Line => "line",
            ChartKind::Sparkline => "sparkline",
        }
    }
}

/// One named series of (x, y) points. The points are EXACTLY what the producer
/// computed — the HUD plots each one and draws segments only between consecutive
/// given points; it never interpolates a missing one or extrapolates beyond the
/// range.
#[derive(Debug, Clone, PartialEq)]
pub struct ChartSeries {
    /// The series label (shown in a legend / tooltip).
    pub label: String,
    /// The exact data points, in producer order: (x, y). No invented points.
    pub points: Vec<(f64, f64)>,
}

impl ChartSeries {
    /// Build a series.
    pub fn new(label: impl Into<String>, points: Vec<(f64, f64)>) -> Self {
        ChartSeries { label: label.into(), points }
    }
}

/// A structured chart the HUD renders: the kind, the series, honest axis labels,
/// and a title. Serialized to the `chart.data` telemetry envelope; the HUD's Chart
/// component is the EXACT renderer.
#[derive(Debug, Clone, PartialEq)]
pub struct ChartSpec {
    /// How the HUD draws between points (bars / line / sparkline).
    pub kind: ChartKind,
    /// The series to plot (one or more). An empty `series` (or all-empty series) is
    /// an HONEST-EMPTY chart — the HUD shows a "no data" state.
    pub series: Vec<ChartSeries>,
    /// The x-axis label.
    pub x_axis: String,
    /// The y-axis label.
    pub y_axis: String,
    /// The chart title.
    pub title: String,
}

impl ChartSpec {
    /// Build a chart spec.
    pub fn new(
        kind: ChartKind,
        series: Vec<ChartSeries>,
        x_axis: impl Into<String>,
        y_axis: impl Into<String>,
        title: impl Into<String>,
    ) -> Self {
        ChartSpec {
            kind,
            series,
            x_axis: x_axis.into(),
            y_axis: y_axis.into(),
            title: title.into(),
        }
    }

    /// True when there is NOTHING to plot — no series, or every series empty. The
    /// HUD renders an honest-empty state for an empty spec. Honest by construction:
    /// an empty producer surfaces an empty chart, never a fabricated point.
    pub fn is_empty(&self) -> bool {
        self.series.iter().all(|s| s.points.is_empty())
    }

    /// Serialize to the EXACT JSON the HUD's Chart component consumes. The points
    /// are emitted as `[x, y]` pairs verbatim — no rounding beyond f64, no
    /// resampling, no invented point. `empty` is carried explicitly so the HUD's
    /// honest-empty branch needs no client-side inference.
    pub fn to_telemetry(&self) -> Value {
        json!({
            "kind": self.kind.as_str(),
            "title": self.title,
            "x_axis": self.x_axis,
            "y_axis": self.y_axis,
            "empty": self.is_empty(),
            "series": self
                .series
                .iter()
                .map(|s| json!({
                    "label": s.label,
                    "points": s
                        .points
                        .iter()
                        .map(|(x, y)| json!([x, y]))
                        .collect::<Vec<_>>(),
                }))
                .collect::<Vec<_>>(),
        })
    }
}

/// Emit a [`ChartSpec`] as the `chart.data` telemetry envelope so the HUD's Chart
/// component can render the exact series. Fire-and-forget through the existing
/// telemetry hub; dropped silently when no HUD is connected. Neutral presentation
/// — no gate, no action, no network. Emits even an empty spec (the HUD shows the
/// honest-empty state) so a "chart this" with no data is honestly surfaced rather
/// than silently swallowed.
pub fn emit_chart(spec: &ChartSpec) {
    telemetry::emit("system", "chart.data", spec.to_telemetry());
}

// ---------------------------------------------------------------------------
// CONFIG — the OFF-by-default flag the live "chart this" op reads
// ---------------------------------------------------------------------------

/// The chart op's runtime knob, mirrored from [`crate::config::ChartConfig`].
/// `enabled` is the master gate (ships OFF): with it false the live "chart this"
/// op declines (it never emits) and behavior is byte-for-byte today's. The pure
/// [`ChartSpec`] + [`emit_chart`] do not consult `enabled` — the gate lives at the
/// op boundary; the type is always available to tests / structured-series
/// producers that already have a HUD panel. The router reads the flag straight off
/// `cfg.chart.enabled` (the chart emit takes no ChartConfig), so this op-boundary
/// mirror reads as unused in the binary while its off-by-default invariant is
/// asserted by its own test.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy)]
pub struct ChartConfig {
    /// Whether the live "chart this" op is enabled. OFF by default.
    pub enabled: bool,
}

impl Default for ChartConfig {
    fn default() -> Self {
        // SHIPS OFF — the chart op is opt-in; nothing emits a user-driven chart
        // until the operator turns it on. Neutral when off.
        Self { enabled: false }
    }
}

// ---------------------------------------------------------------------------
// INTENT — "chart this" / "chart the system load" (explicit, phrase-anchored)
// ---------------------------------------------------------------------------

/// A "chart this" intent parsed from an utterance. CONSERVATIVE — only an explicit
/// "chart / plot / graph" cue trips it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChartIntent;

/// Detect a "chart this" intent. CONSERVATIVE and phrase-anchored: the utterance
/// must carry an explicit "chart"/"plot"/"graph" verb together with a "this"/the
/// system-load subject, so an ordinary question never trips it. Pure — unit-tested.
pub fn classify_chart_intent(utterance: &str) -> Option<ChartIntent> {
    let lower = utterance.to_lowercase();
    let lower = lower.trim();
    const VERBS: &[&str] = &["chart", "plot", "graph"];
    if !VERBS.iter().any(|v| lower.contains(v)) {
        return None;
    }
    // Anchored to a chartable subject: "this", or the system load / cpu / memory
    // (the only real series this op has on hand without a fetch).
    let chartable = lower.contains("this")
        || lower.contains("system load")
        || lower.contains("cpu")
        || lower.contains("memory")
        || lower.contains("load");
    if !chartable {
        return None;
    }
    Some(ChartIntent)
}

/// Build a [`ChartSpec`] from the latest REAL system snapshot the telemetry bus
/// already publishes (CPU %, memory used/total %) — a tiny two-bar chart of the
/// metrics the daemon already has on hand. PURE over the injected `snapshot`: it
/// plots EXACTLY the two real values (no history fabricated, no interpolation), and
/// with NO snapshot available it returns an HONEST-EMPTY spec (the HUD shows the
/// no-data state) rather than inventing numbers. This is the data path #41 surfaces
/// a ChartSpec from — wired live behind [`ChartConfig::enabled`] in the router.
pub fn chart_from_snapshot(snapshot: Option<crate::telemetry::SystemSnapshot>) -> ChartSpec {
    let Some(snap) = snapshot else {
        // No reading available -> honest empty (never fabricate a value).
        return ChartSpec::new(
            ChartKind::Bar,
            vec![],
            "metric",
            "percent",
            "System load",
        );
    };
    let mem_pct = if snap.mem_total_bytes > 0 {
        (snap.mem_used_bytes as f64 / snap.mem_total_bytes as f64) * 100.0
    } else {
        0.0
    };
    // EXACTLY the two real metrics, plotted at x=0 (cpu) and x=1 (memory). No
    // invented history, no interpolated point.
    ChartSpec::new(
        ChartKind::Bar,
        vec![ChartSeries::new(
            "load %",
            vec![(0.0, snap.cpu_percent as f64), (1.0, mem_pct)],
        )],
        "metric (0=cpu, 1=mem)",
        "percent",
        "System load",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_spec() -> ChartSpec {
        ChartSpec::new(
            ChartKind::Line,
            vec![ChartSeries::new(
                "cpu",
                vec![(0.0, 12.0), (1.0, 30.5), (2.0, 18.0)],
            )],
            "t (s)",
            "cpu %",
            "CPU over time",
        )
    }

    // ---- serialize: the EXACT points, no interpolation / invented point ----

    #[test]
    fn serializes_exactly_the_emitted_points() {
        let spec = line_spec();
        let v = spec.to_telemetry();
        assert_eq!(v["kind"], "line");
        assert_eq!(v["title"], "CPU over time");
        assert_eq!(v["x_axis"], "t (s)");
        assert_eq!(v["y_axis"], "cpu %");
        assert_eq!(v["empty"], false);
        let series = v["series"].as_array().unwrap();
        assert_eq!(series.len(), 1);
        let pts = series[0]["points"].as_array().unwrap();
        // EXACTLY the three given points, verbatim, in order — no 4th invented
        // point, no interpolated midpoint.
        assert_eq!(pts.len(), 3, "exactly the emitted points: {pts:?}");
        assert_eq!(pts[0], json!([0.0, 12.0]));
        assert_eq!(pts[1], json!([1.0, 30.5]));
        assert_eq!(pts[2], json!([2.0, 18.0]));
        assert_eq!(series[0]["label"], "cpu");
    }

    #[test]
    fn kinds_serialize_to_stable_strings() {
        assert_eq!(ChartKind::Bar.as_str(), "bar");
        assert_eq!(ChartKind::Line.as_str(), "line");
        assert_eq!(ChartKind::Sparkline.as_str(), "sparkline");
    }

    // ---- honest-empty: an empty series serializes cleanly ------------------

    #[test]
    fn empty_series_is_honest_empty() {
        let spec = ChartSpec::new(ChartKind::Bar, vec![], "x", "y", "Nothing");
        assert!(spec.is_empty());
        let v = spec.to_telemetry();
        assert_eq!(v["empty"], true, "an empty spec is flagged empty");
        assert_eq!(v["series"].as_array().unwrap().len(), 0);

        // A series carrying NO points is also honestly empty.
        let spec2 = ChartSpec::new(
            ChartKind::Line,
            vec![ChartSeries::new("blank", vec![])],
            "x",
            "y",
            "Blank",
        );
        assert!(spec2.is_empty());
        assert_eq!(spec2.to_telemetry()["empty"], true);
    }

    #[test]
    fn nonempty_series_is_not_empty() {
        assert!(!line_spec().is_empty());
    }

    // ---- emit: reaches the telemetry hub as chart.data ---------------------

    #[tokio::test]
    async fn emit_chart_publishes_a_chart_data_envelope() {
        // Subscribe to the hub, emit, and observe the exact envelope a HUD would
        // receive — hermetic, no WS client, no network.
        let mut rx = telemetry::subscribe_for_test();
        let spec = line_spec();
        emit_chart(&spec);
        let raw = rx.try_recv().expect("a chart.data envelope was published");
        let env: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(env["source"], "system");
        assert_eq!(env["event"], "chart.data");
        // The data is the EXACT serialized spec — the three points ride through.
        let pts = env["data"]["series"][0]["points"].as_array().unwrap();
        assert_eq!(pts.len(), 3, "the exact points reached the hub: {pts:?}");
        assert_eq!(pts[1], json!([1.0, 30.5]));
    }

    // ---- intent classification ---------------------------------------------

    #[test]
    fn classifies_explicit_chart_requests() {
        assert!(classify_chart_intent("chart this").is_some());
        assert!(classify_chart_intent("plot the system load").is_some());
        assert!(classify_chart_intent("graph the cpu").is_some());
        // No chart verb -> not an intent.
        assert!(classify_chart_intent("what's the cpu usage").is_none());
        // A chart verb with no chartable subject -> not an intent.
        assert!(classify_chart_intent("chart the unknowable cosmos").is_none());
    }

    // ---- chart_from_snapshot: exactly the real metrics, honest-empty -------

    #[test]
    fn charts_exactly_the_real_snapshot_metrics() {
        let snap = crate::telemetry::SystemSnapshot {
            cpu_percent: 42.0,
            mem_used_bytes: 4_000_000_000,
            mem_total_bytes: 8_000_000_000,
            disk_free_bytes: None,
            disk_total_bytes: None,
            uptime_secs: 100,
        };
        let spec = chart_from_snapshot(Some(snap));
        assert!(!spec.is_empty());
        let pts = &spec.series[0].points;
        // EXACTLY two points: cpu at x=0, mem% at x=1 — no fabricated history.
        assert_eq!(pts.len(), 2, "exactly the two real metrics: {pts:?}");
        assert_eq!(pts[0], (0.0, 42.0), "cpu plotted verbatim");
        assert_eq!(pts[1], (1.0, 50.0), "mem% = 4/8 GiB = 50%");
    }

    #[test]
    fn no_snapshot_is_honest_empty_never_fabricates() {
        let spec = chart_from_snapshot(None);
        assert!(spec.is_empty(), "no reading -> honest empty (no fabricated value)");
        assert_eq!(spec.to_telemetry()["empty"], true);
    }

    // ---- config: OFF by default --------------------------------------------

    #[test]
    fn chart_config_ships_off_by_default() {
        assert!(!ChartConfig::default().enabled, "the chart op ships OFF");
    }
}
