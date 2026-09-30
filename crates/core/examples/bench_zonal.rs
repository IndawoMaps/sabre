//! Throughput benchmark: run zonal stats over N polygons and print a summary table.
//!
//! Usage:
//!   SABRE_BENCH_FILE=path/to.tif cargo run -p sabre-core --example bench_zonal --release
//!   SABRE_BENCH_FILE=path/to.tif cargo run -p sabre-core --example bench_zonal --release -- --n 100 --size 0.1

#[path = "../benches/support/common.rs"]
mod common;

use sabre_core::query::query_polygon;
use std::time::Instant;

fn main() {
    let mut n: usize = 100;
    let mut size: f64 = 0.1;  // polygon side length in degrees
    let mut zoom: Option<u32> = None;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--n"    => { i += 1; n    = args[i].parse().expect("--n must be a number"); }
            "--size" => { i += 1; size = args[i].parse().expect("--size must be a number"); }
            "--zoom" => { i += 1; zoom = Some(args[i].parse().expect("--zoom must be a number")); }
            other    => eprintln!("unknown arg: {other}"),
        }
        i += 1;
    }

    let cfg = common::setup(zoom);
    let polys = common::make_polygons(&cfg.extent, n, size);

    let mut times_ms: Vec<f64> = Vec::with_capacity(n);
    let wall = Instant::now();

    for wkt in &polys {
        // Parsed outside the timed region: the server parses a geometry once
        // and reuses it, so timing the parse here would not describe anything
        // production does.
        let mask = sabre_core::mask::parse_wkt_mask(wkt).expect("polygon");
        let t0 = Instant::now();
        pollster::block_on(query_polygon(&mask, 0, cfg.nodata, false, &cfg.reader, &cfg.meta))
            .expect("query_polygon failed");
        times_ms.push(t0.elapsed().as_secs_f64() * 1_000.0);
    }

    let total_ms = wall.elapsed().as_secs_f64() * 1_000.0;
    times_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let throughput = n as f64 / (total_ms / 1_000.0);
    let median     = percentile(&times_ms, 50.0);
    let p95        = percentile(&times_ms, 95.0);
    let min        = *times_ms.first().unwrap();
    let max        = *times_ms.last().unwrap();

    println!("Polygons queried:      {n}");
    println!("Total time:            {total_ms:.1} ms");
    println!("Throughput:            {throughput:.1} polys/sec");
    println!("Median:                {median:.1} ms/poly");
    println!("p95:                   {p95:.1} ms/poly");
    println!("Min:                   {min:.1} ms/poly");
    println!("Max:                   {max:.1} ms/poly");
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() { return 0.0; }
    let rank = p / 100.0 * (sorted.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = (lo + 1).min(sorted.len() - 1);
    let frac = rank - lo as f64;
    sorted[lo] * (1.0 - frac) + sorted[hi] * frac
}
