#!/usr/bin/env python3
"""Pick and download the Sentinel-2 scenes the block sets sit on.

The synthetic DEM in `make_cog.py` is a fine control, but it is not what
production reads. Real imagery differs in every way that matters to a tile
server:

    PREDICTOR=2       every Sentinel-2 L2A COG on AWS carries it
    uint16, 1024 px   not float32 in 512 px blocks
    UTM, not WGS84    so every request reprojects
    10980 x 10980     five overview levels, and a farm occupies two tiles of it

Scenes come from the public `sentinel-cogs` bucket and are copied byte-exact:
the sha256 recorded here is the sha256 of what AWS served, so the copy in the
Space is provably the same file and a benchmark against it is a benchmark
against production bytes.

Choice is pinned, not re-run. The first invocation queries Earth Search for the
least cloudy recent scene over each block set and writes its id to the
manifest; later invocations reuse that id. A benchmark dataset that silently
becomes a different week's imagery is not a benchmark dataset.

    python3 bench/fetch_scenes.py                     # all sets in bench/blocks
    python3 bench/fetch_scenes.py --assets red,nir,visual
    python3 bench/fetch_scenes.py --repick            # choose fresh scenes

Needs nothing but the standard library.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import sys
import urllib.error
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
BLOCKS = os.path.join(HERE, "blocks")
SCENES = os.path.join(HERE, "data", "scenes")
MANIFEST = os.path.join(SCENES, "manifest.json")

STAC = "https://earth-search.aws.element84.com/v1/search"
COLLECTION = "sentinel-2-l2a"

# STAC asset keys, with what each one is for.
ASSETS = {
    "red":    "B04, 10 m, uint16 — the band NDVI and most zonal statistics read",
    "nir":    "B08, 10 m, uint16 — the other half of NDVI",
    "visual": "TCI, 10 m, 3-band uint8 — true colour, what a basemap tile serves",
    "scl":    "Scene classification, 20 m, uint8 — cloud and shadow masking",
}


def post_json(url: str, body: dict, timeout: float = 30) -> dict:
    req = urllib.request.Request(
        url, data=json.dumps(body).encode(), headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r)


def pick_scene(lon: float, lat: float, max_cloud: float) -> dict:
    """Least cloudy recent scene covering the point."""
    try:
        res = post_json(STAC, {
            "collections": [COLLECTION],
            "intersects": {"type": "Point", "coordinates": [lon, lat]},
            "query": {"eo:cloud_cover": {"lt": max_cloud}},
            "limit": 20,
            "sortby": [{"field": "properties.datetime", "direction": "desc"}],
        })
    except (urllib.error.URLError, TimeoutError) as exc:
        sys.exit(f"Earth Search is not answering: {exc}")

    feats = res.get("features") or []
    if not feats:
        sys.exit(f"no {COLLECTION} scene under {max_cloud}% cloud covers {lon},{lat}. "
                 f"Raise --max-cloud.")
    # Recency first, then the clearest of what came back: a farm under cloud is
    # a benchmark of nodata handling, not of tile serving.
    best = min(feats, key=lambda f: f["properties"].get("eo:cloud_cover", 100))
    return best


def download(url: str, dest: str, expect_bytes: int | None = None) -> tuple[str, int]:
    """Stream to disk, hashing as it goes. Returns (sha256, bytes)."""
    tmp = dest + ".part"
    h = hashlib.sha256()
    total = 0
    with urllib.request.urlopen(url, timeout=120) as r, open(tmp, "wb") as fh:
        while True:
            chunk = r.read(1 << 20)
            if not chunk:
                break
            fh.write(chunk)
            h.update(chunk)
            total += len(chunk)
            if expect_bytes:
                pct = 100 * total / expect_bytes
                print(f"\r    {total / 1e6:7.1f} / {expect_bytes / 1e6:.1f} MB  {pct:5.1f}%",
                      end="", file=sys.stderr, flush=True)
            else:
                print(f"\r    {total / 1e6:7.1f} MB", end="", file=sys.stderr, flush=True)
    print(file=sys.stderr)
    os.replace(tmp, dest)
    return h.hexdigest(), total


def content_length(url: str) -> int | None:
    try:
        req = urllib.request.Request(url, method="HEAD")
        with urllib.request.urlopen(req, timeout=30) as r:
            return int(r.headers.get("Content-Length") or 0) or None
    except (urllib.error.URLError, TimeoutError, ValueError):
        return None


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--blocks", default=BLOCKS, help="directory of block sets")
    ap.add_argument("--out", default=SCENES)
    ap.add_argument("--assets", default="red,visual",
                    help="comma-separated STAC asset keys: " + ", ".join(ASSETS))
    ap.add_argument("--max-cloud", type=float, default=5.0, help="percent")
    ap.add_argument("--repick", action="store_true",
                    help="choose scenes afresh instead of reusing the pinned ids")
    ap.add_argument("--dry-run", action="store_true", help="resolve and size, download nothing")
    args = ap.parse_args()

    wanted = [a.strip() for a in args.assets.split(",") if a.strip()]
    unknown = [a for a in wanted if a not in ASSETS]
    if unknown:
        sys.exit(f"unknown asset key(s) {', '.join(unknown)}. Known: {', '.join(ASSETS)}")

    index_path = os.path.join(args.blocks, "index.json")
    if not os.path.exists(index_path):
        sys.exit(f"no block index at {index_path}. Build one with bench/make_blocks.py.")
    with open(index_path) as fh:
        sets = json.load(fh)["sets"]

    os.makedirs(args.out, exist_ok=True)
    manifest = {"kind": "sabre-bench-scenes", "scenes": {}}
    if os.path.exists(MANIFEST):
        with open(MANIFEST) as fh:
            manifest = json.load(fh)
    scenes = manifest.setdefault("scenes", {})

    total_bytes = 0
    for s in sets:
        name = s["name"]
        lon, lat = s["shape"]["centroid"]
        entry = scenes.get(name)

        if entry and not args.repick:
            print(f"{name}: pinned to {entry['scene_id']} ({entry['datetime'][:10]}, "
                  f"{entry['cloud_cover']:.1f}% cloud)")
            feature = None
        else:
            feature = pick_scene(lon, lat, args.max_cloud)
            p = feature["properties"]
            entry = {
                "scene_id": feature["id"],
                "datetime": p["datetime"],
                "cloud_cover": round(p.get("eo:cloud_cover", 0.0), 3),
                "epsg": p.get("proj:epsg"),
                "platform": p.get("platform"),
                "block_set": name,
                "centroid": [lon, lat],
                "assets": {},
            }
            scenes[name] = entry
            print(f"{name}: picked {entry['scene_id']} ({entry['datetime'][:10]}, "
                  f"{entry['cloud_cover']:.1f}% cloud, EPSG:{entry['epsg']})")

        if entry.get("epsg") and entry["epsg"] != s["shape"]["utm_epsg"]:
            print(f"  note: scene is EPSG:{entry['epsg']}, blocks sit in "
                  f"EPSG:{s['shape']['utm_epsg']}. Both reproject from WGS84, so this is "
                  f"fine -- it just means the farm straddles a UTM zone boundary.")

        for key in wanted:
            href = (entry.get("assets", {}).get(key) or {}).get("href")
            if href is None:
                if feature is None:
                    sys.exit(f"{name}: asset {key!r} was not resolved when the scene was pinned. "
                             f"Re-run with --repick, or ask for only the assets already pinned: "
                             f"{', '.join(entry.get('assets', {})) or '(none)'}")
                asset = feature["assets"].get(key)
                if asset is None:
                    sys.exit(f"{name}: scene {entry['scene_id']} has no {key!r} asset")
                href = asset["href"]

            # One copy per scene, not per block set: nginx on the box serves the
            # same directory under /sabre/ and /titiler/, so the per-server
            # split that the origin log needs happens there, not here.
            fn = f"{entry['scene_id']}_{key}.tif"
            dest = os.path.join(args.out, fn)
            rec = entry.setdefault("assets", {}).setdefault(key, {})
            rec.update({"href": href, "file": fn})

            if args.dry_run:
                size = content_length(href)
                total_bytes += size or 0
                print(f"  {key:7} {fn}  {(size or 0) / 1e6:.1f} MB")
                continue

            if os.path.exists(dest) and rec.get("sha256"):
                with open(dest, "rb") as fh:
                    h = hashlib.sha256()
                    for chunk in iter(lambda: fh.read(1 << 20), b""):
                        h.update(chunk)
                if h.hexdigest() == rec["sha256"]:
                    print(f"  {key:7} {fn}  already here, sha256 matches")
                    total_bytes += rec["bytes"]
                    continue
                print(f"  {key:7} {fn}  on disk but hashes differently; re-downloading")

            print(f"  {key:7} {fn}")
            size = content_length(href)
            digest, got = download(href, dest, size)
            rec.update({"sha256": digest, "bytes": got})
            total_bytes += got

    with open(MANIFEST if args.out == SCENES else os.path.join(args.out, "manifest.json"), "w") as fh:
        json.dump(manifest, fh, indent=2, sort_keys=True)
        fh.write("\n")

    verb = "would download" if args.dry_run else "have"
    print(f"\n{verb} {total_bytes / 1e6:,.0f} MB across "
          f"{sum(len(e.get('assets', {})) for e in scenes.values())} objects")
    print(f"manifest: {os.path.relpath(os.path.join(args.out, 'manifest.json'), os.path.dirname(HERE))}")
    if not args.dry_run:
        print("\nPublish them with:  cd bench/terraform/dataset && terraform apply")


if __name__ == "__main__":
    main()
