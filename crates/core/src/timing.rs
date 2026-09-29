//! Where a request's time actually went.
//!
//! Client-side latency tells you a tile took 3 ms. It does not tell you that
//! 2.5 of those were spent testing every pixel against every polygon, which is
//! the kind of thing that stays hidden until someone writes a microbenchmark
//! to go looking for it. Phases are timed in place and reported as
//! `Server-Timing`, which browsers show in devtools and the load suite
//! aggregates into a distribution.
//!
//! Spans accumulate **by name**, not by call. `decode` entered once per COG
//! tile reports the total spent decoding, not the last one — per-item spans
//! would be both noisier and slow enough to distort what they measure.
//!
//! The cost is a clock read per span boundary, about 20 ns. Ten phases is
//! ~0.4 µs against a 3 ms request, so this is on by default: instrumentation
//! nobody switches on is instrumentation that is wrong when they do.
//!
//! # Clocks
//!
//! `std::time::Instant::now()` panics on `wasm32-unknown-unknown`, and this
//! crate compiles there so the renderer can run in a browser. So the clock is
//! a function rather than a hard dependency: natively there is a default one,
//! and anywhere else the host supplies it. A recorder with no clock still runs
//! the work and reports nothing, which is the right answer for a build that
//! cannot measure -- reporting zeroes would be worse than reporting nothing.
//!
//! In a browser that function is `performance.now()`, wired in by the host so
//! this crate need not depend on `wasm-bindgen` to tell the time.

use std::cell::RefCell;

/// Phase names, so a typo cannot quietly create a second bucket.
pub mod phase {
    /// Reading and parsing the COG header. Usually a cache hit.
    pub const META: &str = "meta";
    /// Resolving clip geometry: a provider fetch, a cache hit, or parsing WKT.
    pub const GEOMETRY: &str = "geom";
    /// Deciding which overview and which pixel window to read.
    pub const PLAN: &str = "plan";
    /// Byte ranges from the source, whether they came from the origin or the
    /// page cache. A large number here means the cache is not holding.
    pub const FETCH: &str = "fetch";
    /// Decompression and predictor.
    pub const DECODE: &str = "decode";
    /// Assembling decoded tiles into the read window.
    pub const BLIT: &str = "blit";
    /// Resampling the window onto the output grid.
    pub const RESAMPLE: &str = "resample";
    /// Reprojecting the clip geometry and testing pixels against it.
    pub const MASK: &str = "mask";
    /// Colormap, hillshade, contour or RGB stretch.
    pub const STYLE: &str = "style";
    /// PNG encoding.
    pub const ENCODE: &str = "encode";
    /// Accumulating statistics over the masked window.
    pub const STATS: &str = "stats";
}

/// Reads a monotonic clock, in milliseconds. Only differences are used, so
/// the origin does not matter.
pub type Clock = fn() -> f64;

/// The default clock, where there is one.
#[cfg(not(target_arch = "wasm32"))]
pub fn default_clock() -> Option<Clock> {
    fn now() -> f64 {
        // A fixed origin so successive reads are comparable. `Instant` has no
        // public epoch, so one is made here and leaked for the process.
        use std::sync::OnceLock;
        static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
        EPOCH.get_or_init(std::time::Instant::now).elapsed().as_secs_f64() * 1e3
    }
    Some(now)
}

/// No default on wasm: `Instant::now()` panics there. A host that can tell the
/// time -- a browser, through `performance.now()` -- passes one to
/// [`Timings::with_clock`].
#[cfg(target_arch = "wasm32")]
pub fn default_clock() -> Option<Clock> {
    None
}

/// Accumulated per-phase durations for one request.
#[derive(Default)]
pub struct Timings {
    spans: RefCell<Vec<(&'static str, f64)>>,
    clock: Option<Clock>,
}

impl Timings {
    /// A recorder using the platform's default clock, measuring only if there
    /// is one.
    pub fn new() -> Self {
        Self { spans: RefCell::new(Vec::with_capacity(8)), clock: default_clock() }
    }

    /// A recorder using a clock the host supplies.
    pub fn with_clock(clock: Clock) -> Self {
        Self { spans: RefCell::new(Vec::with_capacity(8)), clock: Some(clock) }
    }

    /// A recorder that does not measure, for callers who want the same code
    /// path without the header.
    pub fn off() -> Self {
        Self::default()
    }

    pub fn enabled(&self) -> bool {
        self.clock.is_some()
    }

    /// Time a synchronous phase.
    pub fn time<T>(&self, name: &'static str, f: impl FnOnce() -> T) -> T {
        let Some(clock) = self.clock else { return f() };
        let start = clock();
        let out = f();
        self.add(name, clock() - start);
        out
    }

    /// Time an asynchronous phase.
    ///
    /// This measures wall-clock across the await, which for `fetch` is the
    /// point: what matters is how long the bytes took to arrive, not how much
    /// CPU was burned waiting for them.
    pub async fn time_async<F: std::future::Future>(&self, name: &'static str, f: F) -> F::Output {
        let Some(clock) = self.clock else { return f.await };
        let start = clock();
        let out = f.await;
        self.add(name, clock() - start);
        out
    }

    /// Open a span that is closed by [`Timings::since`].
    ///
    /// For phases that do not fit a closure — anything spanning a `?`, a
    /// `match` with early returns, or a block that borrows across the
    /// measurement. Returns None when there is no clock, which makes `since`
    /// a no-op and keeps callers off `Instant::now()`: a raw clock read in
    /// the render path compiles on wasm and panics there.
    pub fn start(&self) -> Option<f64> {
        self.clock.map(|c| c())
    }

    /// Close a span opened by [`Timings::start`].
    pub fn since(&self, name: &'static str, start: Option<f64>) {
        if let (Some(clock), Some(start)) = (self.clock, start) {
            self.add(name, clock() - start);
        }
    }

    /// Record a duration measured elsewhere.
    pub fn add(&self, name: &'static str, ms: f64) {
        if self.clock.is_none() {
            return;
        }
        let mut spans = self.spans.borrow_mut();
        match spans.iter_mut().find(|(n, _)| *n == name) {
            Some((_, total)) => *total += ms,
            None => spans.push((name, ms)),
        }
    }

    /// Phases in the order they were first entered, which is the order they
    /// happen in, which is the order that reads correctly.
    pub fn spans(&self) -> Vec<(&'static str, f64)> {
        self.spans.borrow().clone()
    }

    pub fn total_ms(&self) -> f64 {
        self.spans.borrow().iter().map(|(_, ms)| ms).sum()
    }

    /// A `Server-Timing` header value, or None when nothing was recorded.
    ///
    /// Durations are given to a microsecond. Any less and the fast phases all
    /// read `0.0`, which looks like they were skipped rather than quick.
    pub fn header(&self) -> Option<String> {
        let spans = self.spans.borrow();
        if spans.is_empty() {
            return None;
        }
        Some(spans.iter()
            .map(|(name, ms)| format!("{name};dur={ms:.3}"))
            .collect::<Vec<_>>()
            .join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_accumulate_by_name_rather_than_replacing() {
        let t = Timings::new();
        for _ in 0..3 {
            t.add(phase::DECODE, 1.0);
        }
        t.add(phase::FETCH, 10.0);
        assert_eq!(t.spans(), vec![(phase::DECODE, 3.0), (phase::FETCH, 10.0)]);
    }

    #[test]
    fn phases_keep_the_order_they_were_first_entered() {
        let t = Timings::new();
        t.add(phase::FETCH, 1.0);
        t.add(phase::DECODE, 1.0);
        t.add(phase::FETCH, 1.0);
        assert_eq!(t.spans().iter().map(|(n, _)| *n).collect::<Vec<_>>(),
                   vec![phase::FETCH, phase::DECODE]);
    }

    /// Elapsed time on a shared CI runner has a floor and no ceiling: a 15 ms
    /// sleep measured 124 ms on a loaded macOS runner, which failed an earlier
    /// version of this asserting it stayed under 100.
    ///
    /// The floor is the real assertion — it proves something was measured
    /// rather than zero returned, and that the unit is milliseconds and not
    /// seconds. The ceiling is only wide enough to catch the other unit
    /// mistakes, microseconds or nanoseconds, which are out by three orders of
    /// magnitude and will never be confused with a busy machine.
    const SLEPT_MS: u64 = 15;
    const CEILING_MS: f64 = 10_000.0;

    #[test]
    fn timing_something_records_roughly_how_long_it_took() {
        let t = Timings::new();
        t.time(phase::STYLE, || std::thread::sleep(std::time::Duration::from_millis(SLEPT_MS)));
        let ms = t.spans()[0].1;
        assert!(ms >= SLEPT_MS as f64 * 0.8,
                "a {SLEPT_MS} ms sleep measured as {ms} ms — too small to be milliseconds");
        assert!(ms < CEILING_MS,
                "a {SLEPT_MS} ms sleep measured as {ms} ms — that is not milliseconds");
    }

    #[test]
    fn start_and_since_measure_the_same_thing_as_time() {
        let t = Timings::new();
        let s = t.start();
        std::thread::sleep(std::time::Duration::from_millis(SLEPT_MS));
        t.since(phase::PLAN, s);
        let ms = t.spans()[0].1;
        assert!(ms >= SLEPT_MS as f64 * 0.8, "a {SLEPT_MS} ms sleep measured as {ms} ms");
        assert!(ms < CEILING_MS, "a {SLEPT_MS} ms sleep measured as {ms} ms");
    }

    #[test]
    fn start_and_since_do_nothing_without_a_clock() {
        let t = Timings { spans: RefCell::new(Vec::new()), clock: None };
        let s = t.start();
        assert!(s.is_none(), "no clock means nothing to read");
        t.since(phase::PLAN, s);
        assert!(t.spans().is_empty());
    }

    #[test]
    fn a_host_may_supply_its_own_clock() {
        // What a browser does: performance.now() rather than Instant.
        fn fake() -> f64 {
            use std::cell::Cell;
            thread_local!(static T: Cell<f64> = const { Cell::new(0.0) });
            T.with(|t| { let v = t.get(); t.set(v + 7.0); v })
        }
        let t = Timings::with_clock(fake);
        t.time(phase::DECODE, || ());
        assert_eq!(t.spans(), vec![(phase::DECODE, 7.0)]);
    }

    #[test]
    fn without_a_clock_nothing_is_measured_and_nothing_panics() {
        // wasm32 has no default clock because Instant::now() panics there.
        // The work still has to happen, and the report has to be absent
        // rather than a column of zeroes.
        let t = Timings { spans: RefCell::new(Vec::new()), clock: None };
        let mut ran = false;
        t.time(phase::DECODE, || ran = true);
        assert!(ran);
        assert!(t.spans().is_empty());
        assert_eq!(t.header(), None);
        assert!(!t.enabled());
    }

    #[test]
    fn a_disabled_recorder_still_runs_the_work_and_reports_nothing() {
        let t = Timings::off();
        let mut ran = false;
        t.time(phase::STYLE, || ran = true);
        assert!(ran, "the work must happen either way");
        t.add(phase::FETCH, 5.0);
        assert!(t.spans().is_empty());
        assert_eq!(t.header(), None);
    }

    #[test]
    fn the_header_is_what_server_timing_says_it_should_be() {
        let t = Timings::new();
        t.add(phase::FETCH, 1.5);
        t.add(phase::DECODE, 0.25);
        assert_eq!(t.header().unwrap(), "fetch;dur=1.500, decode;dur=0.250");
    }

    #[test]
    fn microsecond_resolution_so_quick_phases_are_not_reported_as_skipped() {
        let t = Timings::new();
        t.add(phase::PLAN, 0.0004);
        assert_eq!(t.header().unwrap(), "plan;dur=0.000");
        t.add(phase::PLAN, 0.0016);
        assert_eq!(t.header().unwrap(), "plan;dur=0.002");
    }

    #[test]
    fn total_is_the_sum_of_the_phases() {
        let t = Timings::new();
        t.add(phase::FETCH, 2.0);
        t.add(phase::DECODE, 3.0);
        assert!((t.total_ms() - 5.0).abs() < 1e-9);
    }
}
