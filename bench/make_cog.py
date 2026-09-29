#!/usr/bin/env python3
"""Produce a Cloud-Optimized GeoTIFF that both sabre and titiler can read.

Either synthesises a float32 terrain-like raster, or re-encodes an existing
raster, as a tiled DEFLATE COG with overviews and no predictor. The predictor
matters: sabre does not decode PREDICTOR=2/3 yet (see the README's known
limitations), and most float DEMs ship with one, so a stock COG would
benchmark garbage output rather than equal work.

Needs rasterio and numpy. The titiler image has both, so with Docker (it runs as
an unprivileged user, hence --user so it can write into the mounted directory):

    mkdir -p bench/data
    docker compose -f bench/docker-compose.yml run --rm --no-deps --user "$(id -u):$(id -g)" \\
        --entrypoint python titiler /bench/make_cog.py --out /bench/data/dem.tif

or, with a local rasterio:

    python3 bench/make_cog.py --out bench/data/dem.tif --size 4096
    python3 bench/make_cog.py --input path/to/any.tif --out bench/data/dem.tif
"""

from __future__ import annotations

import argparse
import os

import numpy as np
import rasterio
from rasterio.shutil import copy as rio_copy
from rasterio.transform import from_bounds

NODATA = -9999.0
COG_OPTIONS = dict(
    driver="COG",
    COMPRESS="DEFLATE",
    PREDICTOR="NO",
    LEVEL=6,
    BLOCKSIZE=512,
    OVERVIEWS="IGNORE_EXISTING",
    OVERVIEW_RESAMPLING="NEAREST",
    BIGTIFF="IF_SAFER",
    NUM_THREADS="ALL_CPUS",
)


def terrain(size: int, seed: int) -> np.ndarray:
    """A plausible DEM: layered sinusoids for relief, noise for texture, and
    one nodata lake so masking is exercised. Values run roughly 0–3000."""
    rng = np.random.default_rng(seed)
    y, x = np.mgrid[0:size, 0:size].astype(np.float32) / size
    z = np.zeros((size, size), dtype=np.float32)
    for octave in range(1, 7):
        freq = 2.0 ** octave
        phase_x, phase_y = rng.uniform(0, 2 * np.pi, 2)
        amp = 1.0 / freq
        z += amp * np.sin(freq * np.pi * x + phase_x) * np.cos(freq * np.pi * y + phase_y)
    z += rng.normal(0, 0.01, z.shape).astype(np.float32)
    z -= z.min()
    z *= 3000.0 / z.max()
    cx, cy, r = rng.uniform(0.3, 0.7, 2).tolist() + [0.08]
    lake = ((x - cx) ** 2 + (y - cy) ** 2) < r ** 2
    z[lake] = NODATA
    return z.astype(np.float32)


def synthesise(out: str, size: int, seed: int) -> None:
    west, south, east, north = 10.0, 0.0, 12.0, 2.0
    data = terrain(size, seed)
    with rasterio.open(
        out, "w",
        width=size, height=size, count=1, dtype="float32",
        crs="EPSG:4326", transform=from_bounds(west, south, east, north, size, size),
        nodata=NODATA, **COG_OPTIONS,
    ) as dst:
        dst.write(data, 1)


def convert(src_path: str, out: str) -> None:
    with rasterio.open(src_path) as src:
        rio_copy(src, out, **COG_OPTIONS)


def describe(path: str) -> None:
    with rasterio.open(path) as ds:
        structure = ds.tags(ns="IMAGE_STRUCTURE")
        print(f"{path}: {ds.width}×{ds.height}, {ds.count} band(s), {ds.dtypes[0]}, "
              f"{os.path.getsize(path) / 1e6:.1f} MB")
        print(f"  crs={ds.crs}  nodata={ds.nodata}  bounds={tuple(round(b, 6) for b in ds.bounds)}")
        print(f"  blocks={ds.block_shapes[0]}  overviews={ds.overviews(1)}")
        print(f"  compression={structure.get('COMPRESSION')}  predictor={structure.get('PREDICTOR', '1 (none)')}  "
              f"byte order={'little' if ds.tags(ns='IMAGE_STRUCTURE').get('BYTEORDER', 'LSB') != 'MSB' else 'BIG'}")
        if structure.get("PREDICTOR", "1") not in ("1", "NO"):
            print("  WARNING: predictor set; sabre will not decode this correctly")


def main() -> None:
    ap = argparse.ArgumentParser(description="Make a sabre-compatible COG for the benchmark")
    ap.add_argument("--out", default=os.path.join(os.path.dirname(__file__), "data", "dem.tif"))
    ap.add_argument("--input", help="re-encode this raster instead of synthesising one")
    ap.add_argument("--size", type=int, default=4096, help="synthetic raster side in pixels")
    ap.add_argument("--seed", type=int, default=42)
    args = ap.parse_args()

    os.makedirs(os.path.dirname(os.path.abspath(args.out)), exist_ok=True)
    if args.input:
        convert(args.input, args.out)
    else:
        synthesise(args.out, args.size, args.seed)
    describe(args.out)


if __name__ == "__main__":
    main()
