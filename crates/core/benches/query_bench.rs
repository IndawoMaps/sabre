#[path = "support/common.rs"]
mod common;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use sabre_core::query::query_polygon;

fn bench_query_polygon_size(c: &mut Criterion) {
    let cfg = common::setup(None);
    let ext = &cfg.extent;

    // Three polygon sizes: small (10% of extent), medium (40%), large (80%)
    let span = f64::min(ext.east - ext.west, ext.north - ext.south);
    let cases: &[(&str, f64)] = &[
        ("small",  span * 0.10),
        ("medium", span * 0.40),
        ("large",  span * 0.80),
    ];

    let mut group = c.benchmark_group("query_polygon");

    for &(label, size) in cases {
        let polys = common::make_polygons(ext, 1, size);
        // Parsed once, outside the timed region: the server resolves a
        // geometry once and reuses it across requests, so folding the parse in
        // here would measure something production does not do.
        let mask = sabre_core::mask::parse_wkt_mask(&polys[0]).expect("polygon");
        group.bench_with_input(BenchmarkId::new("size", label), &mask, |b, mask| {
            b.iter(|| {
                pollster::block_on(query_polygon(mask, 0, cfg.nodata, &cfg.reader, &cfg.meta)).ok()
            })
        });
    }

    group.finish();
}

fn bench_query_n_polygons(c: &mut Criterion) {
    let cfg = common::setup(None);
    let span = f64::min(
        cfg.extent.east - cfg.extent.west,
        cfg.extent.north - cfg.extent.south,
    ) * 0.15;

    let mut group = c.benchmark_group("query_n_polygons");

    for &n in &[1usize, 10, 50, 100] {
        let masks: Vec<_> = common::make_polygons(&cfg.extent, n, span)
            .iter()
            .map(|wkt| sabre_core::mask::parse_wkt_mask(wkt).expect("polygon"))
            .collect();
        group.bench_with_input(BenchmarkId::from_parameter(n), &masks, |b, masks| {
            b.iter(|| {
                for mask in masks {
                    pollster::block_on(query_polygon(mask, 0, cfg.nodata, &cfg.reader, &cfg.meta)).ok();
                }
            })
        });
    }

    group.finish();
}

/// Statistics over a whole farm at once -- 86 blocks as one geometry, as a
/// multi-id `geometry_id` resolves to -- on a 1800×1800 raster of its own,
/// large enough that the per-pixel work is what is measured.
fn bench_query_farm(c: &mut Criterion) {
    let (w, h, px) = (1800u32, 1800u32, 0.0001);
    let pixels: Vec<f32> = (0..w * h).map(|i| (i % 997) as f32 + 1.0).collect();
    let tiff = common::make_geotiff(&pixels, w, h, 20.0, -30.0, px, 0.0);
    let reader = common::MemReader(tiff);
    let meta = pollster::block_on(sabre_core::cog::fetch_meta(&reader)).expect("synthetic COG");
    let extent = sabre_core::geo::Bbox {
        west: 20.0, south: -30.0 - h as f64 * px, east: 20.0 + w as f64 * px, north: -30.0,
    };
    let farm = common::make_farm(&extent, 86);

    let mut group = c.benchmark_group("query_farm");
    group.bench_function("farm_86", |b| {
        b.iter(|| pollster::block_on(query_polygon(&farm, 0, Some(0.0), &reader, &meta)).expect("stats"))
    });
    group.finish();
}

criterion_group!(benches, bench_query_polygon_size, bench_query_n_polygons, bench_query_farm);
criterion_main!(benches);
