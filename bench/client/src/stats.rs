//! Turning raw per-request samples into a summary.
//!
//! Samples are kept whole rather than folded into a histogram as they arrive.
//! A 10-minute run at 5,000 req/s is 3 million samples, about 70 MB -- cheap
//! enough that it is not worth losing the ability to re-derive a different
//! quantile, or to emit a full CDF, after the fact.
//!
//! That is also why the server's phase breakdown is interned into a fixed
//! array rather than kept as the `(name, ms)` pairs it arrives as: a Vec of
//! Strings per sample would take the same run from 70 MB to something over a
//! gigabyte, for a set of phase names that never changes.

use serde::Serialize;

/// Phases a server may report, in the order they happen. An unrecognised
/// metric is dropped rather than growing this: the load client is not the
/// place to discover that a server started reporting something new.
pub const PHASES: [&str; 11] = [
    "geom", "meta", "plan", "fetch", "decode", "blit", "resample", "mask", "style", "encode",
    "stats",
];

/// Milliseconds per phase, indexed by [`PHASES`]. `f32` because these are
/// milliseconds recorded to the microsecond, and a NaN marks a phase the
/// server did not report -- distinct from one it reported as zero.
pub type PhaseMs = [f32; PHASES.len()];

pub fn no_phases() -> PhaseMs {
    [f32::NAN; PHASES.len()]
}

/// Parse a `Server-Timing` value into per-phase milliseconds.
///
/// Only the `dur` parameter is read; `desc` and anything else is ignored, and
/// a metric with no `dur` is skipped rather than counted as zero.
pub fn parse_server_timing(value: &str) -> PhaseMs {
    let mut out = no_phases();
    for metric in value.split(',') {
        let mut parts = metric.split(';');
        let Some(name) = parts.next().map(str::trim) else { continue };
        let Some(idx) = PHASES.iter().position(|p| *p == name) else { continue };
        let dur = parts.find_map(|p| {
            let (k, v) = p.split_once('=')?;
            (k.trim() == "dur").then(|| v.trim().parse::<f32>().ok())?
        });
        if let Some(ms) = dur {
            out[idx] = ms;
        }
    }
    out
}

#[derive(Debug, Clone, Copy)]
pub struct Sample {
    /// Time from the request going on the wire to its body being read.
    pub service_ms: f64,

    /// Time from when the load model *intended* to send it. Identical to
    /// `service_ms` in closed-loop; larger in open-loop once the client or the
    /// server falls behind the schedule.
    pub corrected_ms: f64,

    /// How late the request went out against its scheduled slot.
    pub lag_ms: f64,

    pub status: u16,
    pub bytes: u64,
    pub ok: bool,

    /// What the server said it spent where, from `Server-Timing`. All NaN
    /// when the response carried no header -- every error, and every server
    /// that does not measure.
    pub timing: PhaseMs,
}

#[derive(Debug, Clone, Serialize)]
pub struct Percentiles {
    pub min: f64,
    pub mean: f64,
    pub p50: f64,
    pub p75: f64,
    pub p90: f64,
    pub p95: f64,
    pub p99: f64,
    pub p999: f64,
    pub max: f64,
}

/// Nearest-rank on a sorted slice. With N below a few thousand the upper
/// quantiles are reporting individual samples; `samples` is in the output so
/// that is visible rather than implied.
fn quantile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let rank = (q * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

impl Percentiles {
    pub fn of(values: &mut [f64]) -> Self {
        values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mean = if values.is_empty() {
            f64::NAN
        } else {
            values.iter().sum::<f64>() / values.len() as f64
        };
        Self {
            min: values.first().copied().unwrap_or(f64::NAN),
            mean,
            p50: quantile(values, 0.50),
            p75: quantile(values, 0.75),
            p90: quantile(values, 0.90),
            p95: quantile(values, 0.95),
            p99: quantile(values, 0.99),
            p999: quantile(values, 0.999),
            max: values.last().copied().unwrap_or(f64::NAN),
        }
    }
}

/// A CDF thinned to `points` entries, for plotting without shipping every sample.
fn cdf(sorted: &[f64], points: usize) -> Vec<(f64, f64)> {
    if sorted.is_empty() {
        return Vec::new();
    }
    (0..points)
        .map(|i| {
            let q = (i + 1) as f64 / points as f64;
            (q, quantile(sorted, q))
        })
        .collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub name: String,
    pub mode: String,

    /// Requests per second the load model aimed for. Null in closed-loop,
    /// where offered load is whatever the server permits.
    pub offered_rps: Option<f64>,
    pub achieved_rps: f64,

    pub requests: usize,
    pub ok: usize,
    pub errors: usize,
    pub statuses: std::collections::BTreeMap<String, usize>,

    pub wall_s: f64,

    /// Unix timestamps bracketing the timed window, warmup excluded. The
    /// harness attributes origin-side log lines to a phase by these, because
    /// the origin writes its line after the server under test has already
    /// answered and a phase can be over before all of its lines have landed.
    pub window: [f64; 2],

    pub bytes: u64,
    pub mb_per_s: f64,

    /// Time on the wire. Compare servers with this.
    pub service_ms: Percentiles,

    /// Time from the intended send. In open-loop this is the number a user
    /// would experience, and the gap to `service_ms` is queueing the naive
    /// measurement would have hidden.
    pub corrected_ms: Percentiles,

    /// How far behind schedule the generator ran. Open-loop only.
    pub lag_ms: Option<Percentiles>,

    /// True when the generator could not keep to its own schedule, which makes
    /// the server-side numbers a lower bound rather than a measurement.
    pub client_saturated: bool,

    /// True when scheduling jitter is larger than the response being measured.
    /// Not a failure -- the service_ms figures are still sound -- but it means
    /// corrected_ms is mostly the generator's own timer granularity, so
    /// open-loop is the wrong tool for an endpoint this fast.
    pub lag_exceeds_service: bool,

    /// True when in-flight requests hit `max_inflight` and scheduling stopped.
    pub overloaded: bool,

    pub cdf_service_ms: Vec<(f64, f64)>,

    /// What the server said it spent where, per phase, over served requests.
    /// Absent when no response carried `Server-Timing`.
    ///
    /// These are the server's own view and do not account for the whole of
    /// `service_ms`: the network, the accept, and anything the server does
    /// outside the instrumented path are in the gap between them. The gap is
    /// as interesting as the phases -- a large one means time is going
    /// somewhere nothing is measuring.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub phases: Vec<PhaseSummary>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PhaseSummary {
    pub name: String,
    /// How many served requests reported this phase. A phase reported by some
    /// requests and not others is not an average over all of them.
    pub samples: usize,
    pub ms: Percentiles,
    /// Share of the median request's server-side time. Of the phases, not of
    /// `service_ms`, so these sum to 100%.
    pub share_pct: f64,
}

pub struct SummaryInput<'a> {
    pub name: &'a str,
    pub mode: &'a str,
    pub offered_rps: Option<f64>,
    pub wall_s: f64,
    pub window: [f64; 2],
    pub open_loop: bool,
    pub overloaded: bool,
}

pub fn summarise(samples: &[Sample], input: SummaryInput<'_>) -> Summary {
    let mut statuses = std::collections::BTreeMap::new();
    let mut bytes = 0u64;
    let mut ok = 0usize;
    for s in samples {
        *statuses.entry(s.status.to_string()).or_insert(0) += 1;
        if s.ok {
            ok += 1;
            bytes += s.bytes;
        }
    }

    // Percentiles are over served requests only: a connection refused in 0.1 ms
    // is not a fast response, and letting it into the distribution would make a
    // failing server look quick.
    let mut service: Vec<f64> = samples
        .iter()
        .filter(|s| s.ok)
        .map(|s| s.service_ms)
        .collect();
    let mut corrected: Vec<f64> = samples
        .iter()
        .filter(|s| s.ok)
        .map(|s| s.corrected_ms)
        .collect();
    let mut lag: Vec<f64> = samples.iter().filter(|s| s.ok).map(|s| s.lag_ms).collect();

    let service_p = Percentiles::of(&mut service);
    let corrected_p = Percentiles::of(&mut corrected);
    let lag_p = Percentiles::of(&mut lag);

    // Sleeping until a deadline on a general-purpose OS lands a millisecond or
    // two late, every time, at any rate. That floor is not saturation, so the
    // threshold sits well above it: a generator that is genuinely losing the
    // race falls tens of milliseconds behind and keeps going. Requiring the
    // slip to also exceed the server's median stops a fast server from being
    // blamed for the scheduler's noise.
    let client_saturated = input.open_loop
        && lag_p.p99.is_finite()
        && service_p.p50.is_finite()
        && lag_p.p99 > 10.0
        && lag_p.p99 > service_p.p50;

    // Separate, weaker signal: the timer noise is bigger than the measurement.
    let lag_exceeds_service = input.open_loop
        && lag_p.p50.is_finite()
        && service_p.p50.is_finite()
        && lag_p.p50 > service_p.p50;

    // Per phase, over served requests that reported it.
    let mut phases: Vec<PhaseSummary> = Vec::new();
    for (i, name) in PHASES.iter().enumerate() {
        let mut values: Vec<f64> = samples
            .iter()
            .filter(|s| s.ok)
            .map(|s| s.timing[i])
            .filter(|ms| ms.is_finite())
            .map(f64::from)
            .collect();
        if values.is_empty() {
            continue;
        }
        let p = Percentiles::of(&mut values);
        phases.push(PhaseSummary {
            name: (*name).to_string(),
            samples: values.len(),
            ms: p,
            share_pct: 0.0,
        });
    }
    let median_total: f64 = phases.iter().map(|p| p.ms.p50).sum();
    if median_total > 0.0 {
        for p in &mut phases {
            p.share_pct = 100.0 * p.ms.p50 / median_total;
        }
    }

    Summary {
        name: input.name.to_string(),
        mode: input.mode.to_string(),
        offered_rps: input.offered_rps,
        achieved_rps: if input.wall_s > 0.0 {
            ok as f64 / input.wall_s
        } else {
            0.0
        },
        requests: samples.len(),
        ok,
        errors: samples.len() - ok,
        statuses,
        wall_s: input.wall_s,
        window: input.window,
        bytes,
        mb_per_s: if input.wall_s > 0.0 {
            bytes as f64 / 1e6 / input.wall_s
        } else {
            0.0
        },
        service_ms: service_p,
        corrected_ms: corrected_p,
        lag_ms: if input.open_loop { Some(lag_p) } else { None },
        client_saturated,
        lag_exceeds_service,
        overloaded: input.overloaded,
        cdf_service_ms: cdf(&service, 50),
        phases,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_timing_is_parsed_into_the_phases_it_names() {
        let got = parse_server_timing("meta;dur=1.417, decode;dur=0.346, mask;dur=2.114");
        let idx = |n: &str| PHASES.iter().position(|p| *p == n).unwrap();
        assert_eq!(got[idx("meta")], 1.417);
        assert_eq!(got[idx("decode")], 0.346);
        assert_eq!(got[idx("mask")], 2.114);
        assert!(got[idx("style")].is_nan(), "a phase not reported is not a phase at zero");
    }

    #[test]
    fn metrics_we_do_not_know_about_are_dropped_rather_than_guessed_at() {
        let got = parse_server_timing("cache;dur=1.0, meta;dur=2.0, db;desc=\"x\"");
        let idx = |n: &str| PHASES.iter().position(|p| *p == n).unwrap();
        assert_eq!(got[idx("meta")], 2.0);
        assert_eq!(got.iter().filter(|v| v.is_finite()).count(), 1);
    }

    #[test]
    fn a_metric_with_no_duration_is_skipped_not_counted_as_zero() {
        let got = parse_server_timing("meta, decode;desc=\"deflate\", plan;dur=0.5");
        let idx = |n: &str| PHASES.iter().position(|p| *p == n).unwrap();
        assert!(got[idx("meta")].is_nan());
        assert!(got[idx("decode")].is_nan());
        assert_eq!(got[idx("plan")], 0.5);
    }

    #[test]
    fn nonsense_does_not_panic() {
        for bad in ["", ",,,", ";;", "dur=1", "meta;dur=", "meta;dur=abc", "=;=;="] {
            assert!(parse_server_timing(bad).iter().all(|v| v.is_nan()), "{bad:?}");
        }
    }

    #[test]
    fn phases_are_summarised_only_over_requests_that_reported_them() {
        let idx = |n: &str| PHASES.iter().position(|p| *p == n).unwrap();
        let mut samples: Vec<Sample> = Vec::new();
        for i in 0..10 {
            let mut s = sample(1.0);
            s.timing[idx("decode")] = 1.0 + i as f32;
            // Only half the requests clipped anything.
            if i % 2 == 0 {
                s.timing[idx("mask")] = 10.0;
            }
            samples.push(s);
        }
        let sum = summarise(&samples, SummaryInput {
            name: "t", mode: "closed", offered_rps: None, wall_s: 1.0,
            window: [0.0, 1.0], open_loop: false, overloaded: false,
        });
        let decode = sum.phases.iter().find(|p| p.name == "decode").unwrap();
        let mask = sum.phases.iter().find(|p| p.name == "mask").unwrap();
        assert_eq!(decode.samples, 10);
        assert_eq!(mask.samples, 5, "a phase half the requests skipped is not averaged over all");
        assert_eq!(mask.ms.p50, 10.0);
        assert!(sum.phases.iter().all(|p| p.name != "style"), "unreported phases are absent");
        let total: f64 = sum.phases.iter().map(|p| p.share_pct).sum();
        assert!((total - 100.0).abs() < 1e-6, "shares should sum to 100, got {total}");
    }

    fn sample(ms: f64) -> Sample {
        Sample {
            service_ms: ms,
            corrected_ms: ms,
            lag_ms: 0.0,
            status: 200,
            bytes: 10,
            ok: true,
            timing: no_phases(),
        }
    }

    #[test]
    fn quantiles_are_nearest_rank() {
        let mut v: Vec<f64> = (1..=100).map(|i| i as f64).collect();
        let p = Percentiles::of(&mut v);
        assert_eq!(p.min, 1.0);
        assert_eq!(p.p50, 50.0);
        assert_eq!(p.p99, 99.0);
        assert_eq!(p.max, 100.0);
    }

    #[test]
    fn single_sample_does_not_panic() {
        let mut v = vec![7.0];
        let p = Percentiles::of(&mut v);
        assert_eq!(p.p999, 7.0);
        assert_eq!(p.max, 7.0);
    }

    #[test]
    fn empty_is_nan_not_panic() {
        let mut v: Vec<f64> = vec![];
        let p = Percentiles::of(&mut v);
        assert!(p.p50.is_nan());
    }

    #[test]
    fn failed_requests_stay_out_of_the_latency_distribution() {
        let samples = vec![
            sample(10.0),
            Sample {
                service_ms: 0.01,
                corrected_ms: 0.01,
                lag_ms: 0.0,
                status: 502,
                bytes: 0,
                ok: false,
                timing: no_phases(),
            },
        ];
        let s = summarise(
            &samples,
            SummaryInput {
                name: "t",
                mode: "closed",
                offered_rps: None,
                wall_s: 1.0, window: [0.0, 1.0],
                open_loop: false,
                overloaded: false,
            },
        );
        assert_eq!(s.ok, 1);
        assert_eq!(s.errors, 1);
        assert_eq!(
            s.service_ms.min, 10.0,
            "the 502 must not become the fastest response"
        );
    }

    fn open_summary(service: f64, lag: f64) -> Summary {
        let samples: Vec<Sample> = (0..100)
            .map(|_| Sample {
                service_ms: service,
                corrected_ms: service + lag,
                lag_ms: lag,
                status: 200,
                bytes: 1,
                ok: true,
                timing: no_phases(),
            })
            .collect();
        summarise(
            &samples,
            SummaryInput {
                name: "t",
                mode: "open",
                offered_rps: Some(1000.0),
                wall_s: 1.0, window: [0.0, 1.0],
                open_loop: true,
                overloaded: false,
            },
        )
    }

    #[test]
    fn lag_flags_a_saturated_generator() {
        // 5 ms responses, 80 ms behind schedule: the generator is the limit.
        assert!(open_summary(5.0, 80.0).client_saturated);
    }

    #[test]
    fn timer_jitter_is_not_saturation() {
        // The case that made the first version of this flag useless: a couple
        // of milliseconds of sleep_until slip against a sub-millisecond
        // endpoint, at a rate the generator was comfortably keeping up with.
        let s = open_summary(0.21, 2.2);
        assert!(
            !s.client_saturated,
            "2ms of OS timer slip is the floor, not saturation"
        );
        assert!(
            s.lag_exceeds_service,
            "but it does swamp a 0.2ms response, and should say so"
        );
    }

    #[test]
    fn small_lag_against_slow_responses_is_neither() {
        let s = open_summary(500.0, 0.5);
        assert!(!s.client_saturated);
        assert!(!s.lag_exceeds_service);
    }

    #[test]
    fn closed_loop_never_flags_lag() {
        let samples = vec![sample(10.0)];
        let s = summarise(
            &samples,
            SummaryInput {
                name: "t",
                mode: "closed",
                offered_rps: None,
                wall_s: 1.0, window: [0.0, 1.0],
                open_loop: false,
                overloaded: false,
            },
        );
        assert!(!s.client_saturated);
        assert!(!s.lag_exceeds_service);
        assert!(s.lag_ms.is_none());
    }
}
