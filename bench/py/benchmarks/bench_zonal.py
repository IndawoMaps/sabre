#!/usr/bin/env python3
"""Benchmark zonal statistics throughput over N random polygons."""

from __future__ import annotations
import argparse
import random
import time

import numpy as np
import rasterio
from pyproj import Transformer
from rasterio.crs import CRS

import sys
import os
sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "src"))

from sabre_py.geo import Bbox
from sabre_py.query import zonal_stats


def _raster_bbox_wgs84(path: str) -> Bbox:
    with rasterio.open(path) as ds:
        b = ds.bounds
        if ds.crs and ds.crs != CRS.from_epsg(4326):
            t = Transformer.from_crs(ds.crs, CRS.from_epsg(4326), always_xy=True)
            w, s = t.transform(b.left, b.bottom)
            e, n = t.transform(b.right, b.top)
        else:
            w, s, e, n = b.left, b.bottom, b.right, b.top
        return Bbox(w, s, e, n)


def _random_box_wkt(rng: random.Random, extent: Bbox, size: float) -> str:
    margin = size / 2
    cx = rng.uniform(extent.west + margin, extent.east - margin)
    cy = rng.uniform(extent.south + margin, extent.north - margin)
    w, s, e, n = cx - margin, cy - margin, cx + margin, cy + margin
    return f"POLYGON(({w} {s}, {e} {s}, {e} {n}, {w} {n}, {w} {s}))"


def main() -> None:
    parser = argparse.ArgumentParser(description="Benchmark zonal statistics")
    parser.add_argument("tiff", help="Path to GeoTIFF")
    parser.add_argument("--n", type=int, default=100, help="Number of polygons")
    parser.add_argument("--size", type=float, default=0.1,
                        help="Polygon side length in WGS84 degrees")
    parser.add_argument("--band", type=int, default=1)
    parser.add_argument("--seed", type=int, default=42)
    parser.add_argument("--batch", action="store_true",
                        help="Run all polygons in one zonal_stats() call (vs one-by-one)")
    args = parser.parse_args()

    rng = random.Random(args.seed)
    extent = _raster_bbox_wgs84(args.tiff)

    span_x = extent.east - extent.west
    span_y = extent.north - extent.south
    if args.size >= min(span_x, span_y):
        print(f"Warning: --size {args.size} is larger than raster extent "
              f"({span_x:.3f}° × {span_y:.3f}°). Shrinking to 10% of extent.")
        args.size = min(span_x, span_y) * 0.1

    polygons = [_random_box_wkt(rng, extent, args.size) for _ in range(args.n)]
    print(f"Running zonal stats over {args.n} polygons "
          f"(size={args.size}°, {'batch' if args.batch else 'sequential'}) …")

    if args.batch:
        t0 = time.perf_counter()
        results = zonal_stats(args.tiff, polygons, band=args.band)
        elapsed = time.perf_counter() - t0
        times_ms = np.array([elapsed / len(polygons) * 1000] * len(polygons))
        total = elapsed
    else:
        times: list[float] = []
        results = []
        for wkt in polygons:
            t0 = time.perf_counter()
            r = zonal_stats(args.tiff, [wkt], band=args.band)
            times.append(time.perf_counter() - t0)
            results.extend(r)
        times_ms = np.array(times) * 1000
        total = sum(times)

    valid_results = [r for r in results if r["count"] > 0]
    print(f"\n{'Polygons:':<24} {len(polygons)}")
    print(f"{'Valid (non-empty):':<24} {len(valid_results)}")
    print(f"{'Total time:':<24} {total*1000:.1f} ms")
    print(f"{'Throughput:':<24} {len(polygons)/total:.1f} polys/sec")
    print(f"{'Median:':<24} {np.median(times_ms):.1f} ms/poly")
    print(f"{'p95:':<24} {np.percentile(times_ms, 95):.1f} ms/poly")

    if valid_results:
        sample = valid_results[0]
        print(f"\nSample result (first valid polygon):")
        print(f"  min={sample['min']:.4g}  max={sample['max']:.4g}  "
              f"mean={sample['mean']:.4g}  stdev={sample['stdev']:.4g}  "
              f"count={sample['count']}")


if __name__ == "__main__":
    main()
