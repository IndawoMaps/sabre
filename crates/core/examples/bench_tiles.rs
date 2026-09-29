//! Throughput benchmark: render N tiles and print a summary table.
//!
//! Usage:
//!   SABRE_BENCH_FILE=path/to.tif cargo run -p sabre-core --example bench_tiles --release
//!   SABRE_BENCH_FILE=path/to.tif cargo run -p sabre-core --example bench_tiles --release -- --n 100 --zoom 9

#[path = "../benches/support/common.rs"]
mod common;

use sabre_core::render::{render_tile, StyleMode, TileRequest};
use std::time::Instant;

fn main() {
    let mut n: usize = 50;
    let mut zoom: Option<u32> = None;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--n"    => { i += 1; n    = args[i].parse().expect("--n must be a number"); }
            "--zoom" => { i += 1; zoom = Some(args[i].parse().expect("--zoom must be a number")); }
            other    => eprintln!("unknown arg: {other}"),
        }
        i += 1;
    }

    let cfg = common::setup(zoom);
    assert!(!cfg.tiles.is_empty(), "no tiles found for this file/zoom");

    let tile_list: Vec<_> = cfg.tiles.iter().copied().cycle().take(n).collect();

    let mut times_ms: Vec<f64> = Vec::with_capacity(n);
    let wall = Instant::now();

    for &(z, x, y) in &tile_list {
        let req = TileRequest {
            z, x, y,
            tile_size: 256,
            style: StyleMode::Colormap {
                name: "viridis".to_string(),
                min: cfg.vmin,
                max: cfg.vmax,
                nodata: cfg.nodata,
            },
            bilinear: false,
            mask: None,
        };
        let t0 = Instant::now();
        pollster::block_on(render_tile(&req, &cfg.reader, &cfg.meta)).expect("render_tile failed");
        times_ms.push(t0.elapsed().as_secs_f64() * 1_000.0);
    }

    let total_ms = wall.elapsed().as_secs_f64() * 1_000.0;
    times_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let throughput = n as f64 / (total_ms / 1_000.0);
    let median     = percentile(&times_ms, 50.0);
    let p95        = percentile(&times_ms, 95.0);
    let min        = *times_ms.first().unwrap();
    let max        = *times_ms.last().unwrap();

    println!("Tiles rendered:        {n}");
    println!("Total time:            {total_ms:.1} ms");
    println!("Throughput:            {throughput:.1} tiles/sec");
    println!("Median:                {median:.1} ms/tile");
    println!("p95:                   {p95:.1} ms/tile");
    println!("Min:                   {min:.1} ms/tile");
    println!("Max:                   {max:.1} ms/tile");
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() { return 0.0; }
    let rank = p / 100.0 * (sorted.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = (lo + 1).min(sorted.len() - 1);
    let frac = rank - lo as f64;
    sorted[lo] * (1.0 - frac) + sorted[hi] * frac
}
