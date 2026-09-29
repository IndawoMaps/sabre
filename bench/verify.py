#!/usr/bin/env python3
"""Does sabre give the right answers? A correctness job, not a benchmark.

The load suite stopped measuring titiler once the performance question was
settled — Rust against Python/GDAL/FastAPI was never going to be close, and
quantifying it every week taught us nothing. What titiler is still good for is
being a second opinion. It is an independent implementation of the same
operations over the same bytes, and that is the only thing in this repo that
can catch sabre being *consistently* wrong.

It earned that keep. The pixel columns caught the viridis colormap being a
16-point approximation of a 256-entry table, and the statistics columns turned
up a window-convention difference that looked like a bug and was not.

So this runs on its own schedule — before a release, after anything touching
rendering, decoding, reprojection or masking — and says pass or fail rather
than producing numbers to compare.

    python3 bench/verify.py                     # against the running stack
    python3 bench/verify.py --blocks-dir bench/blocks --scenes-dir bench/data/scenes

Exits non-zero if anything fails, so it can be a CI step.

What is a failure, and what is a known difference, is decided per check and
written down next to each one. Two servers agreeing to the last bit on every
operation is not the goal; two servers disagreeing in a way nobody can explain
is the thing worth catching.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import math
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)


def _load_run_module():
    spec = importlib.util.spec_from_file_location("benchrun", os.path.join(HERE, "run.py"))
    mod = importlib.util.module_from_spec(spec)
    sys.modules["benchrun"] = mod
    spec.loader.exec_module(mod)
    return mod


R = _load_run_module()

try:
    import numpy as np
    from PIL import Image
except ImportError:
    np = Image = None


# ── Result accounting ─────────────────────────────────────────────────────────

class Checks:
    """Every check, its verdict, and why that verdict is the right one.

    A check can pass, fail, or be *expected* — a difference that is understood
    and documented. Expected differences are printed, never hidden, but they do
    not fail the job: burying a known convention difference under a tolerance
    is how it stops being known.
    """

    def __init__(self) -> None:
        self.rows: list[tuple[str, str, str, str]] = []

    def record(self, verdict: str, group: str, what: str, detail: str = "") -> None:
        self.rows.append((verdict, group, what, detail))

    def ok(self, group: str, what: str, detail: str = "") -> None:
        self.record("pass", group, what, detail)

    def fail(self, group: str, what: str, detail: str) -> None:
        self.record("FAIL", group, what, detail)

    def expected(self, group: str, what: str, detail: str) -> None:
        self.record("known", group, what, detail)

    def skip(self, group: str, what: str, detail: str) -> None:
        self.record("skip", group, what, detail)

    @property
    def failures(self) -> list[tuple[str, str, str, str]]:
        return [r for r in self.rows if r[0] == "FAIL"]

    def summary(self) -> str:
        counts: dict[str, int] = {}
        for verdict, *_ in self.rows:
            counts[verdict] = counts.get(verdict, 0) + 1
        return "  ".join(f"{v} {k}" for k, v in sorted(counts.items()))


def rgba(body: bytes, path: str) -> "np.ndarray | None":
    if Image is None:
        return None
    with open(path, "wb") as fh:
        fh.write(body)
    try:
        return np.asarray(Image.open(path).convert("RGBA")).astype(int)
    except Exception:  # noqa: BLE001
        return None


# ── Checks ────────────────────────────────────────────────────────────────────

def check_tiles(c: Checks, servers, tiles, style, out_dir: str, rgb_budget: int) -> None:
    """Rendered pixels.

    Two independent renderers will not agree bit for bit, and demanding it
    produces a check that cries wolf. What they differ by, and where, is what
    separates a bug from arithmetic:

    * **Almost every pixel off by one.** Rounding in the colormap index. Seen
      on every tile here and not interesting.
    * **A scattering off by more, in whole rows or at edges.** The two picked
      different source pixels — nearest-neighbour tie-breaking on a half-pixel
      boundary, or a different overview level. The colours are neighbours on
      the ramp, so the ramp is agreed; the sample is not.
    * **A large mean across the whole tile.** The ramps themselves disagree.
      This is what the 16-point viridis table looked like: max 32, **mean
      6.96**. After the 256-entry fix the same tiles read mean 0.63-1.05.

    So the mean is what decides, because it is the statistic that moved by two
    orders of magnitude when there was a real bug. The max is reported but not
    judged on its own: one pixel at a tile edge is not evidence of anything.
    A large *fraction* of badly-differing pixels is judged, because that is a
    systematic sampling difference rather than an edge.
    """
    os.makedirs(out_dir, exist_ok=True)
    if Image is None:
        c.skip("tiles", "pixel diff", "install numpy and Pillow to compare pixels")
        return

    for (z, x, y) in tiles:
        name = f"{z}/{x}/{y}"
        images, sizes = {}, {}
        bad = False
        for s in servers:
            status, body = R.fetch(s, s.tile(z, x, y, style))
            sizes[s.name] = len(body)
            if status != 200:
                c.fail("tiles", name, f"{s.name} answered HTTP {status}")
                bad = True
                continue
            img = rgba(body, os.path.join(out_dir, f"{z}_{x}_{y}_{s.name}.png"))
            if img is None:
                c.fail("tiles", name, f"{s.name} returned something that is not a PNG")
                bad = True
            else:
                images[s.name] = img
        if bad or len(images) != 2:
            continue

        a, b = images[servers[0].name], images[servers[1].name]
        if a.shape != b.shape:
            c.fail("tiles", name, f"different sizes: {a.shape} vs {b.shape}")
            continue
        alpha_diff = 100 * (a[..., 3] != b[..., 3]).mean()
        both = (a[..., 3] == 255) & (b[..., 3] == 255)
        per_px = np.abs(a[..., :3] - b[..., :3]).max(axis=2)[both]
        rgb_max = int(per_px.max()) if per_px.size else 0
        rgb_mean = float(per_px.mean()) if per_px.size else 0.0
        # How much of the tile is off by more than rounding can explain.
        coarse = 100 * float((per_px > 4 * rgb_budget).mean()) if per_px.size else 0.0
        detail = (f"alpha {alpha_diff:.2f}%, rgb mean {rgb_mean:.2f}, "
                  f"max {rgb_max}, {coarse:.2f}% coarse")

        if alpha_diff > 0.5:
            c.fail("tiles", name, f"nodata masks disagree — {detail}")
        elif rgb_mean > 4 * rgb_budget:
            c.fail("tiles", name, f"colour ramps disagree — {detail}")
        elif coarse > 1.0:
            c.fail("tiles", name, f"more than 1% of the tile sampled differently — {detail}")
        elif rgb_mean > rgb_budget or rgb_max > 4 * rgb_budget:
            c.expected("tiles", name,
                       f"rounding, and edges sampled differently — {detail}")
        else:
            c.ok("tiles", name, detail)


def check_points(c: Checks, servers, points, style) -> None:
    """Point sampling.

    Unambiguous: both read one pixel at one coordinate, so any difference is a
    difference in geotransform arithmetic or decoding. Exact agreement, no
    tolerance.
    """
    for lon, lat in points:
        where = f"{lon:.5f},{lat:.5f}"
        vals = {}
        for s in servers:
            status, body = R.fetch(s, s.point(lon, lat, style))
            vals[s.name] = s.parse_point(body) if s.served(status, body) else f"HTTP {status}"
        a, b = vals[servers[0].name], vals[servers[1].name]
        if isinstance(a, str) or isinstance(b, str):
            c.fail("points", where, f"{a} vs {b}")
        elif R.close(a, b, rel=0):
            c.ok("points", where, R.fmt(a, 4))
        else:
            c.fail("points", where, f"{R.fmt(a, 6)} vs {R.fmt(b, 6)}")


def check_polygons(c: Checks, servers, boxes, style) -> None:
    """Zonal statistics over axis-aligned boxes.

    sabre matches rio-tiler's defaults: pixel centres inside the polygon, a
    window rounded outward. titiler's `/cog/statistics` passes
    `align_bounds_with_dataset=True`, which snaps the window to the raster grid
    — 513x513 where sabre reads 512x512. That is 0.39% more pixels, which
    barely moves a mean and can move a min or a max outright, because those are
    order statistics and one extra row can contain a new extreme.

    So min and max differing is *expected* and reported; the mean is the one
    that has to agree, because no convention difference of that size can move
    it by a percent.
    """
    for box in boxes:
        where = f"{box[0]:.4f},{box[1]:.4f}"
        vals = {}
        for s in servers:
            status, body = R.fetch(s, s.polygon(box, style))
            vals[s.name] = s.parse_polygon(body) if status == 200 else None
        a, b = vals[servers[0].name], vals[servers[1].name]
        if a is None or b is None:
            c.fail("polygons", where, f"{servers[0].name}={a} {servers[1].name}={b}")
            continue
        if not R.close(a.get("mean"), b.get("mean"), rel=1e-2):
            c.fail("polygons", f"{where} mean",
                   f"{R.fmt(a.get('mean'), 4)} vs {R.fmt(b.get('mean'), 4)}")
            continue
        extremes = [k for k in ("min", "max") if not R.close(a.get(k), b.get(k), rel=1e-6)]
        if extremes:
            c.expected("polygons", f"{where} {'/'.join(extremes)}",
                       "window convention: titiler aligns to the raster grid, sabre does not")
        else:
            c.ok("polygons", where, f"mean {R.fmt(a.get('mean'), 3)}")


def scene_url(origin: str, flat: str, server_name: str, scene: dict, asset: str) -> str:
    file = scene["assets"][asset]["file"]
    return f"{flat}/{file}" if flat else f"{origin}/{server_name}/scenes/{file}"


def check_real_blocks(c: Checks, servers, farms, origin, flat, asset, style, limit: int) -> None:
    """The same statistics over real field boundaries on real imagery.

    Worth doing separately from the box check: a farm block is small, irregular
    and sometimes has holes, and a 150-pixel block moves its mean by ~0.7% for
    every boundary pixel either way. The box check cannot see any of that.
    """
    for farm in farms:
        scene = farm["scene"]
        for i, feature in enumerate(farm["features"][:limit]):
            where = f"{farm['name']}#{feature['properties']['block']}"
            vals = {}
            for s in servers:
                url = scene_url(origin, flat, s.name, scene, asset)
                status, body = R.fetch(s, s.zonal(feature["geometry"], style, url))
                vals[s.name] = s.parse_polygon(body) if status == 200 else None
            a, b = vals[servers[0].name], vals[servers[1].name]
            if a is None or b is None:
                c.fail("blocks", where, f"{servers[0].name}={a} {servers[1].name}={b}")
                continue
            # 2% rather than 1%: these are far smaller than the synthetic
            # boxes, so one boundary pixel is worth proportionally more.
            if not R.close(a.get("mean"), b.get("mean"), rel=2e-2):
                c.fail("blocks", f"{where} mean",
                       f"{R.fmt(a.get('mean'), 4)} vs {R.fmt(b.get('mean'), 4)}")
            elif [k for k in ("min", "max") if not R.close(a.get(k), b.get(k), rel=1e-6)]:
                c.expected("blocks", where, "min/max differ by one boundary pixel")
            else:
                c.ok("blocks", where, f"mean {R.fmt(a.get('mean'), 3)}")
            del i


def check_geometry_provider(c: Checks, sabre, farms, origin, flat, asset, style,
                            provider: str, limit: int) -> None:
    """The provider path against the same geometry sent inline.

    Not a comparison with titiler — titiler has no equivalent. This is sabre
    against itself, and it is the check that TWKB at precision 6 has not moved
    anything that matters. Quantisation is 11 cm, so a statistic may shift by a
    boundary pixel; the answer must not shift by more than that.
    """
    for farm in farms:
        scene = farm["scene"]
        url = scene_url(origin, flat, sabre.name, scene, asset)
        for feature in farm["features"][:limit]:
            bid = feature["properties"]["block"]
            where = f"{farm['name']}#{bid}"

            s1, b1 = R.fetch(sabre, sabre.zonal(feature["geometry"], style, url))
            req = R.Req("GET", "/query?" + sabre.q(
                url=url, geometry_provider=provider, geometry_id=str(bid), band=style.band - 1))
            s2, b2 = R.fetch(sabre, req)
            if s1 != 200 or s2 != 200:
                c.fail("provider", where, f"inline HTTP {s1}, by reference HTTP {s2}")
                continue
            inline, named = sabre.parse_polygon(b1), sabre.parse_polygon(b2)
            if not R.close(inline.get("mean"), named.get("mean"), rel=5e-3):
                c.fail("provider", f"{where} mean",
                       f"inline {R.fmt(inline.get('mean'), 5)} vs "
                       f"by reference {R.fmt(named.get('mean'), 5)} — more than p6 can explain")
            else:
                c.ok("provider", where, f"mean {R.fmt(named.get('mean'), 3)}")


# ── Main ──────────────────────────────────────────────────────────────────────

def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--sabre", default="http://127.0.0.1:8787")
    ap.add_argument("--titiler", default="http://127.0.0.1:8000")
    ap.add_argument("--origin", default="http://origin")
    ap.add_argument("--cog", default="dem.tif")
    ap.add_argument("--out", default=os.path.join(HERE, "out", "verify"))
    ap.add_argument("--tiles", type=int, default=12, help="tiles to compare per run")
    ap.add_argument("--points", type=int, default=25)
    ap.add_argument("--polygons", type=int, default=10)
    ap.add_argument("--blocks", type=int, default=8, help="real blocks per farm")
    ap.add_argument("--zooms", default="6,8,10")
    ap.add_argument("--rgb-budget", type=int, default=1,
                    help="one channel step. Mean difference above 4x this fails the tile; "
                         "above 1x it is reported as a known rounding difference.")
    ap.add_argument("--blocks-dir", default=os.path.join(HERE, "blocks"))
    ap.add_argument("--scenes-dir", default=os.path.join(HERE, "data", "scenes"))
    ap.add_argument("--imagery-asset", default="red")
    ap.add_argument("--scenes-base", default="",
                    help="where imagery is served from. Empty means <origin>/<server>/scenes, "
                         "which is what the bench stack's nginx serves so the origin log can "
                         "tell the two servers apart. Set it when reading a flat layout, such "
                         "as the Space directly.")
    ap.add_argument("--geometry-provider", default="",
                    help="provider name to check the by-reference path against inline geometry")
    ap.add_argument("--provider-farm", default="",
                    help="which block set that provider is backed by. Without it every farm is "
                         "tried, and a provider holding only one of them fails on ids it has "
                         "never heard of — which is a fixture problem wearing a bug's clothes.")
    ap.add_argument("--colormap", default="viridis")
    ap.add_argument("--band", type=int, default=1)
    ap.add_argument("--seed", type=int, default=42)
    ap.add_argument("--wait", type=float, default=120)
    args = ap.parse_args()

    os.makedirs(args.out, exist_ok=True)
    servers = [R.Sabre(args.sabre, f"{args.origin}/sabre/{args.cog}"),
               R.Titiler(args.titiler, f"{args.origin}/titiler/{args.cog}")]

    print("Waiting for servers …")
    for s in servers:
        print(f"  {s.name:8} {s.base}  ready in {R.wait_healthy(s, args.wait):.1f}s")

    import random
    rng = random.Random(args.seed)
    extent = (10.0, 0.0, 12.0, 2.0)
    pixel_deg = (extent[2] - extent[0]) / 4096
    style = R.Style(colormap=args.colormap, vmin=0.0, vmax=3000.0, band=args.band)

    zooms = [int(z) for z in args.zooms.split(",") if z.strip()]
    pool = [t for z in zooms for t in R.tiles_at_zoom(extent, z)]
    tiles = R.sample(rng, pool, args.tiles)
    inset = pixel_deg
    points = [(rng.uniform(extent[0] + inset, extent[2] - inset),
               rng.uniform(extent[1] + inset, extent[3] - inset)) for _ in range(args.points)]
    side = 512 * pixel_deg
    boxes = []
    for _ in range(args.polygons):
        w = rng.uniform(extent[0], max(extent[0], extent[2] - side))
        s_ = rng.uniform(extent[1], max(extent[1], extent[3] - side))
        boxes.append((w, s_, min(w + side, extent[2]), min(s_ + side, extent[3])))

    c = Checks()
    print("\nRendered tiles")
    check_tiles(c, servers, tiles, style, os.path.join(args.out, "tiles"), args.rgb_budget)
    print("Point sampling")
    check_points(c, servers, points, style)
    print("Zonal statistics")
    check_polygons(c, servers, boxes, style)

    # Real imagery, when it has been fetched.
    sys.path.insert(0, HERE)
    suite = importlib.util.spec_from_file_location("benchsuite", os.path.join(HERE, "suite.py"))
    mod = importlib.util.module_from_spec(suite)
    suite.loader.exec_module(mod)
    farms = mod.load_farms(args.blocks_dir, args.scenes_dir)
    if farms:
        print("Real farm blocks on real imagery")
        check_real_blocks(c, servers, farms, args.origin, args.scenes_base,
                          args.imagery_asset, style, args.blocks)
        if args.geometry_provider:
            print("Geometry provider against inline geometry")
            backed = ([f for f in farms if f["name"] == args.provider_farm]
                      if args.provider_farm else farms)
            if args.provider_farm and not backed:
                c.fail("provider", "configuration",
                       f"--provider-farm {args.provider_farm!r} is not one of "
                       f"{', '.join(f['name'] for f in farms)}")
            check_geometry_provider(c, servers[0], backed, args.origin, args.scenes_base,
                                    args.imagery_asset, style, args.geometry_provider,
                                    args.blocks)
        else:
            c.skip("provider", "by-reference path",
                   "pass --geometry-provider <name> to check it against inline geometry")
    else:
        c.skip("blocks", "real imagery",
               "run bench/make_blocks.py and bench/fetch_scenes.py to include them")

    # ── Report ───────────────────────────────────────────────────────────────
    groups: dict[str, list] = {}
    for verdict, group, what, detail in c.rows:
        groups.setdefault(group, []).append((verdict, what, detail))

    lines = ["# sabre correctness check", "",
             "sabre against titiler on identical requests over identical bytes. "
             "A *known* verdict is a difference that is understood and written down "
             "next to the check that produced it; only *FAIL* means something is wrong.",
             ""]
    for group, rows in groups.items():
        counts: dict[str, int] = {}
        for v, _, _ in rows:
            counts[v] = counts.get(v, 0) + 1
        lines += [f"## {group} — " + ", ".join(f"{n} {v}" for v, n in sorted(counts.items())), ""]
        shown = [r for r in rows if r[0] != "pass"] or rows[:3]
        lines.append(R.table(["", "what", "detail"], [[v, w, d] for v, w, d in shown]))
        if len(shown) < len(rows):
            lines.append(f"\n…and {len(rows) - len(shown)} more passing.")
        lines.append("")

    report = "\n".join(lines)
    with open(os.path.join(args.out, "verify.md"), "w") as fh:
        fh.write(report + "\n")
    with open(os.path.join(args.out, "verify.json"), "w") as fh:
        json.dump({"kind": "sabre-verify", "rows": c.rows, "ok": not c.failures}, fh, indent=2)

    print("\n" + report)
    print(f"\n{c.summary()}")
    print(f"Wrote {args.out}/verify.md")

    if c.failures:
        print(f"\n{len(c.failures)} check(s) failed.")
        sys.exit(1)
    print("\nEverything agrees, or differs for a reason that is written down.")


if __name__ == "__main__":
    main()
