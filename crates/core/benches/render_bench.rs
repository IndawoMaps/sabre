#[path = "support/common.rs"]
mod common;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use sabre_core::render::{render_tile, StyleMode, TileRequest};

fn bench_render_tile(c: &mut Criterion) {
    let cfg = common::setup(None);

    // Take up to 8 tiles to keep measurement time reasonable
    let tiles: Vec<_> = cfg.tiles.iter().copied().take(8).collect();
    assert!(!tiles.is_empty(), "no tiles to benchmark");

    let req_for = |(z, x, y): (u32, u32, u32)| TileRequest {
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

    let mut group = c.benchmark_group("render_tile");

    // Single-tile benchmark for each available tile (up to 4)
    for &tile in tiles.iter().take(4) {
        let req = req_for(tile);
        group.bench_with_input(
            BenchmarkId::new("single", format!("{}/{}/{}", tile.0, tile.1, tile.2)),
            &tile,
            |b, _| {
                b.iter(|| {
                    pollster::block_on(render_tile(&req, &cfg.reader, &cfg.meta))
                        .expect("render_tile failed")
                })
            },
        );
    }

    group.finish();
}

fn bench_render_n_tiles(c: &mut Criterion) {
    let cfg = common::setup(None);
    let tiles: Vec<_> = cfg.tiles.iter().copied().take(16).collect();

    let req_for = |(z, x, y): (u32, u32, u32)| TileRequest {
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

    let mut group = c.benchmark_group("render_n_tiles");

    for &n in &[1usize, 4, 8, 16] {
        if n > tiles.len() { break; }
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter(|| {
                for &tile in tiles.iter().take(n) {
                    let req = req_for(tile);
                    pollster::block_on(render_tile(&req, &cfg.reader, &cfg.meta))
                        .expect("render_tile failed");
                }
            })
        });
    }

    group.finish();
}

/// The same tiles with and without a farm's worth of clip geometry, so the
/// difference is what clipping costs. Most telling over a UTM raster at
/// field zoom, where the mask has to meet the raster's projection:
///
///     SABRE_BENCH_FILE=crates/core/tests/fixtures/s2_b04_predictor2.tif \
///     SABRE_BENCH_ZOOM=16 cargo bench -p sabre-core --bench render_bench -- masked
fn bench_render_masked(c: &mut Criterion) {
    let cfg = common::setup(None);
    let tiles: Vec<_> = cfg.tiles.iter().copied().take(8).collect();
    let farm = std::sync::Arc::new(common::make_farm(&cfg.extent, 86));

    let mut group = c.benchmark_group("render_masked");
    for (label, mask) in [("none", None), ("farm_86", Some(farm))] {
        let reqs: Vec<TileRequest> = tiles.iter().map(|&(z, x, y)| TileRequest {
            z, x, y,
            tile_size: 256,
            style: StyleMode::Colormap {
                name: "viridis".to_string(),
                min: cfg.vmin,
                max: cfg.vmax,
                nodata: cfg.nodata,
            },
            bilinear: false,
            mask: mask.clone(),
        }).collect();
        group.bench_function(BenchmarkId::from_parameter(label), |b| {
            b.iter(|| {
                for req in &reqs {
                    pollster::block_on(render_tile(req, &cfg.reader, &cfg.meta))
                        .expect("render_tile failed");
                }
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench_render_tile, bench_render_n_tiles, bench_render_masked);
criterion_main!(benches);
