#!/usr/bin/env python3
"""Turn GeoJSON field boundaries into a block set for the real-workload suites.

The `blocks`, `clip` and `imagery` suites need polygons shaped like real
requests: field-sized, clustered, and sitting on the Sentinel-2 scene that
`fetch_scenes.py` picks for them. This writes each source as a set in
`bench/blocks/`, with the workload's shape worked out from the geometry, and
rebuilds `bench/blocks/index.json` from what is on disk.

Only geometry is kept. Each feature becomes

    {"type": "Feature", "properties": {"block": 0, "area_m2": 20046.23}, ...}

Usage:

    python3 bench/make_blocks.py fields.geojson --name orchard-utm35s
    python3 bench/make_blocks.py a.geojson b.geojson   # names from the shape
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
OUT = os.path.join(HERE, "blocks")


def ring_area_m2(ring: list) -> float:
    """Shoelace area on a local equal-scale approximation.

    Only used to describe the workload in the index. Field-scale accuracy is
    plenty.
    """
    lat0 = sum(p[1] for p in ring) / len(ring)
    k = math.cos(math.radians(lat0))
    a = 0.0
    for i in range(len(ring) - 1):
        x1, y1 = ring[i][0] * k, ring[i][1]
        x2, y2 = ring[i + 1][0] * k, ring[i + 1][1]
        a += x1 * y2 - x2 * y1
    return abs(a) / 2 * (111_320.0 ** 2)


def polygons(geom: dict) -> list:
    """Every polygon in a Polygon or MultiPolygon, as a list of rings."""
    if geom["type"] == "Polygon":
        return [geom["coordinates"]]
    if geom["type"] == "MultiPolygon":
        return list(geom["coordinates"])
    raise SystemExit(f"unsupported geometry type {geom['type']!r}; expected Polygon or MultiPolygon")


def utm_epsg(lon: float, lat: float) -> int:
    """UTM zone for a coordinate, as the EPSG the covering Sentinel-2 scene uses."""
    zone = int((lon + 180) / 6) + 1
    return (32700 if lat < 0 else 32600) + zone


def describe(features: list) -> dict:
    """Workload shape, from geometry alone."""
    areas, verts = [], []
    minx = miny = float("inf")
    maxx = maxy = float("-inf")
    holes = 0
    for f in features:
        a = 0.0
        n = 0
        for poly in polygons(f["geometry"]):
            for i, ring in enumerate(poly):
                n += len(ring)
                a += ring_area_m2(ring) * (1 if i == 0 else -1)
                if i:
                    holes += 1
                for x, y in ring:
                    minx, maxx = min(minx, x), max(maxx, x)
                    miny, maxy = min(miny, y), max(maxy, y)
        areas.append(a)
        verts.append(n)
    areas.sort()
    verts.sort()
    mid = len(areas) // 2
    cx, cy = (minx + maxx) / 2, (miny + maxy) / 2
    return {
        "blocks": len(features),
        "bbox": [round(v, 6) for v in (minx, miny, maxx, maxy)],
        "centroid": [round(cx, 6), round(cy, 6)],
        "utm_epsg": utm_epsg(cx, cy),
        "area_ha": {
            "min": round(areas[0] / 1e4, 2),
            "p50": round(areas[mid] / 1e4, 2),
            "max": round(areas[-1] / 1e4, 2),
            "total": round(sum(areas) / 1e4, 1),
        },
        "vertices": {"min": verts[0], "p50": verts[mid], "max": verts[-1], "total": sum(verts)},
        "holes": holes,
        "span_km": [
            round((maxx - minx) * 111.32 * math.cos(math.radians(cy)), 2),
            round((maxy - miny) * 111.32, 2),
        ],
    }


def default_name(shape: dict) -> str:
    """A slug from the workload's shape: `blocks-86x1.5ha-utm35s`.

    The UTM zone is there because it selects the Sentinel-2 scene.
    """
    epsg = shape["utm_epsg"]
    zone = f"utm{epsg - 32700}s" if epsg > 32700 else f"utm{epsg - 32600}n"
    p50 = shape["area_ha"]["p50"]
    size = f"{p50:g}ha" if p50 < 10 else f"{round(p50)}ha"
    return f"blocks-{shape['blocks']}x{size}-{zone}"


def build(src: str) -> dict:
    with open(src) as fh:
        raw = json.load(fh)
    if raw.get("type") != "FeatureCollection":
        raise SystemExit(f"{src}: expected a FeatureCollection, got {raw.get('type')!r}")
    features = raw.get("features") or []
    if not features:
        raise SystemExit(f"{src}: no features")

    blocks = []
    # Sorted by position, so a block's number is stable across re-exports.
    ordered = sorted(features, key=lambda f: describe([f])["centroid"])
    for i, f in enumerate(ordered):
        area = sum(
            ring_area_m2(ring) * (1 if j == 0 else -1)
            for poly in polygons(f["geometry"])
            for j, ring in enumerate(poly)
        )
        blocks.append({
            "type": "Feature",
            "properties": {"block": i, "area_m2": round(area, 2)},
            "geometry": f["geometry"],
        })
    return {"type": "FeatureCollection", "name": "", "shape": describe(features), "features": blocks}


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("sources", nargs="+", help="GeoJSON FeatureCollections of Polygon or MultiPolygon")
    ap.add_argument("--name", help="set name; only valid with a single source")
    ap.add_argument("--out", default=OUT)
    args = ap.parse_args()

    if args.name and len(args.sources) > 1:
        sys.exit("--name takes one source at a time")

    os.makedirs(args.out, exist_ok=True)
    for src in args.sources:
        doc = build(src)
        name = args.name or default_name(doc["shape"])
        if not re.fullmatch(r"[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]", name):
            sys.exit(f"{name!r} is not a usable object key: lowercase, digits, dots and hyphens.")
        doc["name"] = name
        dest = os.path.join(args.out, f"{name}.json")
        with open(dest, "w") as fh:
            json.dump(doc, fh, separators=(",", ":"))
        shape = doc["shape"]
        print(f"{name}")
        print(f"  {os.path.relpath(dest, REPO)}")
        print(f"  {shape['blocks']} blocks, {shape['area_ha']['p50']} ha median, "
              f"{shape['vertices']['p50']} vertices median, "
              f"{shape['span_km'][0]}×{shape['span_km'][1]} km, EPSG:{shape['utm_epsg']}"
              + (f", {shape['holes']} holes" if shape["holes"] else ""))

    # Rebuilt from what is on disk, not appended to, so a deleted set does not
    # linger in the index the way it would if this only ever added entries.
    sets = []
    for fn in sorted(os.listdir(args.out)):
        if not fn.endswith(".json") or fn == "index.json":
            continue
        with open(os.path.join(args.out, fn), "rb") as fh:
            body = fh.read()
        doc = json.loads(body)
        sets.append({
            "name": doc["name"],
            "file": fn,
            "sha256": hashlib.sha256(body).hexdigest(),
            "bytes": len(body),
            "shape": doc["shape"],
        })
    with open(os.path.join(args.out, "index.json"), "w") as fh:
        json.dump({"kind": "sabre-bench-blocks", "sets": sets}, fh, indent=2)
        fh.write("\n")


if __name__ == "__main__":
    main()
