#!/usr/bin/env python3
"""Benchmark tile rendering throughput."""

from __future__ import annotations
import argparse
import math
import random
import time

import numpy as np
import rasterio
from pyproj import Transformer
from rasterio.crs import CRS

import sys
import os
sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "src"))

from sabre_py.geo import tile_to_bbox, Bbox
from sabre_py.render import render_tile


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


def _tiles_at_zoom(bbox: Bbox, z: int) -> list[tuple[int, int, int]]:
    n = 2 ** z

    def lon_to_x(lon: float) -> int:
        return int(math.floor((lon + 180.0) / 360.0 * n))

    def lat_to_y(lat: float) -> int:
        lat_r = math.radians(lat)
        return int(math.floor((1.0 - math.log(math.tan(lat_r) + 1.0 / math.cos(lat_r)) / math.pi) / 2.0 * n))

    x0 = max(0, lon_to_x(bbox.west))
    x1 = min(n - 1, lon_to_x(bbox.east))
    y0 = max(0, lat_to_y(bbox.north))
    y1 = min(n - 1, lat_to_y(bbox.south))

    tiles = []
    for x in range(x0, x1 + 1):
        for y in range(y0, y1 + 1):
            tiles.append((z, x, y))
    return tiles


def main() -> None:
    parser = argparse.ArgumentParser(description="Benchmark tile rendering")
    parser.add_argument("tiff", help="Path to GeoTIFF")
    parser.add_argument("--n", type=int, default=50, help="Number of tiles to render")
    parser.add_argument("--zoom", type=int, default=8, help="Zoom level")
    parser.add_argument("--style", default="colormap", choices=["colormap", "rgb", "hillshade"])
    parser.add_argument("--colormap", default="viridis")
    parser.add_argument("--tile-size", type=int, default=256)
    parser.add_argument("--seed", type=int, default=42)
    args = parser.parse_args()

    rng = random.Random(args.seed)
    extent = _raster_bbox_wgs84(args.tiff)
    all_tiles = _tiles_at_zoom(extent, args.zoom)

    if not all_tiles:
        print(f"No tiles found at zoom {args.zoom} for this raster. Try a lower zoom.")
        return

    tiles = rng.choices(all_tiles, k=args.n) if len(all_tiles) < args.n else rng.sample(all_tiles, args.n)
    print(f"Rendering {len(tiles)} tiles at z={args.zoom} ({len(all_tiles)} total available) …")

    times: list[float] = []
    for z, x, y in tiles:
        t0 = time.perf_counter()
        png = render_tile(
            args.tiff, z, x, y,
            style=args.style,
            colormap=args.colormap,
            tile_size=args.tile_size,
        )
        times.append(time.perf_counter() - t0)

    times_ms = np.array(times) * 1000
    total = sum(times)
    print(f"\n{'Tiles rendered:':<22} {len(tiles)}")
    print(f"{'Total time:':<22} {total*1000:.1f} ms")
    print(f"{'Throughput:':<22} {len(tiles)/total:.1f} tiles/sec")
    print(f"{'Median:':<22} {np.median(times_ms):.1f} ms/tile")
    print(f"{'p95:':<22} {np.percentile(times_ms, 95):.1f} ms/tile")
    print(f"{'Min:':<22} {times_ms.min():.1f} ms/tile")
    print(f"{'Max:':<22} {times_ms.max():.1f} ms/tile")


if __name__ == "__main__":
    main()
