#!/usr/bin/env python3
"""Benchmark sabre against titiler over HTTP, request for request.

Both servers read the same COG from the same origin. The harness builds one
list of tiles, points and polygons, sends the equivalent request to each
server at each concurrency level, and reports latency percentiles,
throughput, and how many range requests and bytes each server pulled from
the origin (from the origin's access log). Before timing anything it fetches
a handful of tiles from both and diffs them, so the numbers describe equal
work.

Standard library only. With the Docker stack up:

    python3 bench/run.py

Without Docker, run the origin, sabre and titiler yourself and point the
harness at them:

    python3 bench/origin.py --port 8081 &
    cargo run --release -p sabre-server -- --bind 127.0.0.1:8787 &
    uvicorn titiler.application.main:app --port 8000 &
    python3 bench/run.py --origin http://127.0.0.1:8081

Results land in bench/out/results.md and results.json. Pass --titiler none
or --sabre none to benchmark one server on its own.
"""

from __future__ import annotations

import argparse
import http.client
import json
import math
import os
import random
import shutil
import subprocess
import sys
import threading
import time
import urllib.parse
from dataclasses import dataclass, field

HERE = os.path.dirname(os.path.abspath(__file__))


# ── Requests ──────────────────────────────────────────────────────────────────

@dataclass
class Req:
    method:  str
    path:    str
    body:    bytes | None = None
    headers: dict = field(default_factory=dict)


@dataclass
class Style:
    colormap: str
    vmin:     float
    vmax:     float
    band:     int   # 1-based


def wkt_of(geometry: dict) -> str:
    """GeoJSON Polygon or MultiPolygon -> the WKT sabre's `polygon`/`mask` take.

    Interior rings are kept. A farm block with a dam or a rocky patch cut out
    of it is the case where a mask implementation is either right or quietly
    averaging in pixels that are not part of the field.
    """
    def ring(r) -> str:
        return "(" + ",".join(f"{x} {y}" for x, y in r) + ")"

    t = geometry["type"]
    if t == "Polygon":
        return "POLYGON(" + ",".join(ring(r) for r in geometry["coordinates"]) + ")"
    if t == "MultiPolygon":
        return "MULTIPOLYGON(" + ",".join(
            "(" + ",".join(ring(r) for r in poly) + ")" for poly in geometry["coordinates"]) + ")"
    raise ValueError(f"cannot express {t} as WKT")


def geometry_bbox(geometry: dict) -> tuple[float, float, float, float]:
    polys = [geometry["coordinates"]] if geometry["type"] == "Polygon" else geometry["coordinates"]
    xs = [p[0] for poly in polys for r in poly for p in r]
    ys = [p[1] for poly in polys for r in poly for p in r]
    return min(xs), min(ys), max(xs), max(ys)


def tile_containing(lon: float, lat: float, z: int) -> tuple[int, int]:
    """Web Mercator tile holding a point, the inverse of tile_bbox."""
    n = 2 ** z
    lat = max(-85.05112878, min(85.05112878, lat))
    r = math.radians(lat)
    x = int(math.floor((lon + 180.0) / 360.0 * n))
    y = int(math.floor((1.0 - math.log(math.tan(r) + 1.0 / math.cos(r)) / math.pi) / 2.0 * n))
    return max(0, min(n - 1, x)), max(0, min(n - 1, y))


def zoom_for_span(span_deg: float, fill: float = 0.7) -> int:
    """The zoom at which something `span_deg` wide fills `fill` of one tile.

    A field is served at the zoom a person looking at that field would use.
    Benchmarking a 1.5 ha block at z=6, where it is a fraction of a pixel, is
    a measurement of overview selection rather than of anything real.
    """
    if span_deg <= 0:
        return 18
    z = math.log2(360.0 * fill / span_deg)
    return max(0, min(22, int(round(z))))


def tile_for_geometry(geometry: dict, max_zoom: int = 22) -> tuple[int, int, int]:
    """The deepest tile that wholly contains `geometry`.

    Starting from the zoom where the field roughly fills a tile is not enough:
    a field 96% as wide as a tile usually straddles two of them, and half a
    field is not the request anyone makes. Zoom out until one tile holds it.
    """
    west, south, east, north = geometry_bbox(geometry)
    lon, lat = (west + east) / 2, (south + north) / 2
    z = min(max_zoom, zoom_for_span(max(east - west, north - south)))
    while z > 0:
        x, y = tile_containing(lon, lat, z)
        tw, ts, te, tn = tile_bbox(z, x, y)
        if tw <= west and east <= te and ts <= south and north <= tn:
            return z, x, y
        z -= 1
    return 0, 0, 0


class Server:
    """One tile server: how to ask it for each operation and read the answer."""

    name        = ""
    health_path = "/"
    service     = ""   # compose service name

    def __init__(self, base: str, source: str) -> None:
        u = urllib.parse.urlparse(base)
        self.host = u.hostname or "127.0.0.1"
        self.scheme = u.scheme or "http"
        self.port = u.port or (443 if self.scheme == "https" else 80)
        self.base = base.rstrip("/")
        self.source = source
        self.source_path = urllib.parse.urlparse(source).path
        # What the origin log is matched on. nginx serves the same directory
        # under /sabre/ and /titiler/ precisely so path alone tells the two
        # servers apart, and a suite reading a Sentinel-2 scene instead of the
        # DEM changes the filename but not the prefix -- matching on
        # source_path would silently report zero traffic for those phases.
        self.log_prefix = f"/{self.name}/"

    def q(self, **params) -> str:
        return urllib.parse.urlencode({"url": self.source, **params})

    def tile(self, z: int, x: int, y: int, s: Style, url: str | None = None) -> Req:
        raise NotImplementedError
    def point(self, lon: float, lat: float, s: Style) -> Req: raise NotImplementedError
    def polygon(self, box: tuple, s: Style) -> Req: raise NotImplementedError
    def info(self) -> Req: raise NotImplementedError

    # Real-workload variants. Each takes `url` so one server object can be
    # pointed at a different raster per suite -- the synthetic DEM for the
    # control phases, a Sentinel-2 scene for the imagery ones -- without
    # standing up a second instance. `q()` puts **params last, so url wins.
    def zonal(self, geometry: dict, s: Style, url: str | None = None) -> Req:
        """Statistics over one real field, holes and all."""
        raise NotImplementedError

    def clip(self, geometry: dict, z: int, x: int, y: int, s: Style,
             size: int = 256, url: str | None = None) -> Req | None:
        """A `size`x`size` PNG of one field and nothing around it.

        Returns None where the server has no equivalent operation, which the
        suite records as a skip rather than a zero.
        """
        return None
    def parse_point(self, body: bytes) -> float | None: raise NotImplementedError
    def parse_polygon(self, body: bytes) -> dict | None: raise NotImplementedError

    def served(self, status: int, body: bytes) -> bool:
        """Whether a response answered the request. Statuses are still recorded."""
        return status == 200


class Sabre(Server):
    name        = "sabre"
    health_path = "/health"
    service     = "sabre"

    def tile(self, z, x, y, s, url=None):
        return Req("GET", f"/tiles/{z}/{x}/{y}?" + self.q(
            **({"url": url} if url else {}), colormap=s.colormap, min=s.vmin, max=s.vmax))

    def point(self, lon, lat, s):
        return Req("GET", "/query?" + self.q(lat=lat, lng=lon, band=s.band - 1))

    def polygon(self, box, s):
        w, so, e, n = box
        wkt = f"POLYGON(({w} {so},{e} {so},{e} {n},{w} {n},{w} {so}))"
        return Req("GET", "/query?" + self.q(polygon=wkt, band=s.band - 1))

    def info(self):
        return Req("GET", "/info?" + self.q())

    def zonal(self, geometry, s, url=None):
        return Req("GET", "/query?" + self.q(
            **({"url": url} if url else {}), polygon=wkt_of(geometry), band=s.band - 1))

    def clip(self, geometry, z, x, y, s, size=256, url=None):
        # sabre masks during the tile render, so this is an ordinary tile
        # request with a cutline attached.
        return Req("GET", f"/tiles/{z}/{x}/{y}?" + self.q(
            **({"url": url} if url else {}), colormap=s.colormap, min=s.vmin, max=s.vmax,
            tile_size=size, mask=wkt_of(geometry)))

    def parse_point(self, body):
        if body.strip() == b"point is nodata":
            return None
        return json.loads(body).get("value")

    def parse_polygon(self, body):
        r = json.loads(body)
        return {"min": r.get("min"), "max": r.get("max"), "mean": r.get("avg")}

    def served(self, status, body):
        # A point on nodata is a 400 with this text, where titiler answers a
        # 200 with a null value. Both are answers.
        return status == 200 or (status == 400 and body.strip() == b"point is nodata")


class Titiler(Server):
    name        = "titiler"
    health_path = "/healthz"
    service     = "titiler"

    def tile(self, z, x, y, s, url=None):
        return Req("GET", f"/cog/tiles/WebMercatorQuad/{z}/{x}/{y}.png?" + self.q(
            **({"url": url} if url else {}),
            bidx=s.band, rescale=f"{s.vmin},{s.vmax}", colormap_name=s.colormap, resampling="nearest"))

    def point(self, lon, lat, s):
        return Req("GET", f"/cog/point/{lon},{lat}?" + self.q(bidx=s.band))

    def polygon(self, box, s):
        w, so, e, n = box
        feature = {"type": "Feature", "properties": {}, "geometry": {
            "type": "Polygon", "coordinates": [[[w, so], [e, so], [e, n], [w, n], [w, so]]]}}
        return Req("POST", "/cog/statistics?" + self.q(bidx=s.band),
                   json.dumps(feature).encode(), {"Content-Type": "application/json"})

    def info(self):
        return Req("GET", "/cog/info?" + self.q())

    def zonal(self, geometry, s, url=None):
        feature = {"type": "Feature", "properties": {}, "geometry": geometry}
        return Req("POST", "/cog/statistics?" + self.q(**({"url": url} if url else {}), bidx=s.band),
                   json.dumps(feature).encode(), {"Content-Type": "application/json"})

    def clip(self, geometry, z, x, y, s, size=256, url=None):
        # titiler has no cutline on /cog/tiles, so the comparable operation is
        # /cog/feature: same output size, same work -- read the field's pixels,
        # mask, colourise, encode -- but framed on the feature's own bounds
        # rather than on the tile grid. The z/x/y are ignored here.
        feature = {"type": "Feature", "properties": {}, "geometry": geometry}
        return Req("POST", f"/cog/feature/{size}x{size}.png?" + self.q(
            **({"url": url} if url else {}), bidx=s.band,
            rescale=f"{s.vmin},{s.vmax}", colormap_name=s.colormap, resampling="nearest"),
                   json.dumps(feature).encode(), {"Content-Type": "application/json"})

    def parse_point(self, body):
        values = json.loads(body).get("values") or [None]
        return values[0]

    def parse_polygon(self, body):
        stats = json.loads(body)["properties"]["statistics"]
        band = next(iter(stats.values()))
        return {"min": band.get("min"), "max": band.get("max"), "mean": band.get("mean")}


# ── HTTP ──────────────────────────────────────────────────────────────────────

class Client:
    """One keep-alive connection, reopened if the server drops it."""

    # Sent on every request. http.client sends no User-Agent at all, and a
    # WAF in front of a hosted titiler treats that as a bot and answers 403 --
    # which looks exactly like the server being down.
    USER_AGENT = "sabre-bench/0.0.1"

    def __init__(self, server: Server, timeout: float = 120.0) -> None:
        self.server, self.timeout, self.conn = server, timeout, None

    def send(self, req: Req) -> tuple[int, bytes, float]:
        for attempt in (0, 1):
            try:
                if self.conn is None:
                    # https so the correctness job can point at a titiler that
                    # is not on this machine — a hosted one is still a second
                    # implementation, which is the whole reason to keep it.
                    connect = (http.client.HTTPSConnection if self.server.scheme == "https"
                               else http.client.HTTPConnection)
                    self.conn = connect(self.server.host, self.server.port, timeout=self.timeout)
                t0 = time.perf_counter()
                headers = {"User-Agent": self.USER_AGENT, **(req.headers or {})}
                self.conn.request(req.method, req.path, body=req.body, headers=headers)
                resp = self.conn.getresponse()
                data = resp.read()
                return resp.status, data, time.perf_counter() - t0
            except (http.client.HTTPException, OSError):
                self.close()
                if attempt:
                    raise
        raise AssertionError("unreachable")

    def close(self) -> None:
        if self.conn is not None:
            self.conn.close()
            self.conn = None


def fetch(server: Server, req: Req) -> tuple[int, bytes]:
    c = Client(server)
    try:
        status, body, _ = c.send(req)
        return status, body
    finally:
        c.close()


def wait_healthy(server: Server, seconds: float) -> float:
    """Poll the health endpoint; return the seconds it took, or exit."""
    t0 = time.perf_counter()
    while time.perf_counter() - t0 < seconds:
        try:
            status, _ = fetch(server, Req("GET", server.health_path))
            if status == 200:
                return time.perf_counter() - t0
        except OSError:
            pass
        time.sleep(0.25)
    sys.exit(f"{server.name} at {server.base} did not become healthy within {seconds:.0f}s")


# ── Load generation ───────────────────────────────────────────────────────────

def percentile(sorted_values: list[float], p: float) -> float:
    if not sorted_values:
        return math.nan
    k = max(0, min(len(sorted_values) - 1, math.ceil(p / 100 * len(sorted_values)) - 1))
    return sorted_values[k]


def run_load(server: Server, reqs: list[Req], concurrency: int, warmup: int) -> dict:
    """Send `reqs` through `concurrency` keep-alive connections and time each one.

    The first `warmup` requests are sent untimed at the same concurrency so the
    header cache, GDAL's block cache and any JIT-style first-call cost are paid
    before the clock starts.
    """
    def drive(work: list[Req], record: list | None) -> float:
        lock, cursor = threading.Lock(), [0]

        def worker() -> None:
            client = Client(server)
            while True:
                with lock:
                    i = cursor[0]
                    cursor[0] += 1
                if i >= len(work):
                    break
                try:
                    status, data, dt = client.send(work[i])
                    if record is not None:
                        record.append((dt, status, len(data), server.served(status, data)))
                except Exception:
                    if record is not None:
                        record.append((math.nan, 0, 0, False))
            client.close()

        threads = [threading.Thread(target=worker, daemon=True) for _ in range(concurrency)]
        t0 = time.perf_counter()
        for t in threads: t.start()
        for t in threads: t.join()
        return time.perf_counter() - t0

    warm_t0 = time.time()
    if warmup:
        drive([reqs[i % len(reqs)] for i in range(warmup)], None)
    warm_t1 = time.time()

    results: list[tuple[float, int, int, bool]] = []
    t0 = time.time()
    wall = drive(reqs, results)
    t1 = time.time()
    ok = sorted(dt * 1000 for dt, _, _, served in results if served)
    nbytes = sum(n for _, _, n, served in results if served)
    statuses: dict[int, int] = {}
    for _, status, _, _ in results:
        statuses[status] = statuses.get(status, 0) + 1
    return {
        "requests": len(reqs), "ok": len(ok), "errors": len(reqs) - len(ok), "statuses": statuses,
        "wall_s": wall, "rps": len(ok) / wall if wall else math.nan,
        "mb_per_s": nbytes / wall / 1e6 if wall else math.nan,
        "p50_ms": percentile(ok, 50), "p95_ms": percentile(ok, 95), "p99_ms": percentile(ok, 99),
        "max_ms": ok[-1] if ok else math.nan, "mean_ms": sum(ok) / len(ok) if ok else math.nan,
        # Wall-clock windows, for matching origin log lines by their timestamps.
        "window": [t0, t1], "warmup_window": [warm_t0, warm_t1],
    }


# ── Origin accounting ─────────────────────────────────────────────────────────

class OriginLog:
    """Reads the origin access log written by nginx.conf or origin.py:

        <time> <method> "<uri>" <status> <bytes> "<Range>" "<User-Agent>"

    A line is attributed to a phase by the timestamp in its first field, not by
    where the file pointer stood when the phase started and ended. The origin
    writes a line when it has finished a request, which can be after the server
    under test has already answered the client, so a phase can be over before
    all of its lines have landed; reading between two file offsets then drops
    them, or hands them to the next phase. The timestamp is the origin's clock,
    which for the compose stack and origin.py is the same host clock the
    harness reads.

    Lines that fall inside no phase window at all are counted separately and
    reported, so a broken origin count shows up as a warning rather than as a
    plausible-looking zero.
    """

    SETTLE   = 0.5    # the file must stop growing for this long before it is read
    MIN_WAIT = 2.0    # and at least this long is allowed for late lines to land
    MAX_WAIT = 10.0   # but never longer than this

    def __init__(self, path: str) -> None:
        self.path = path
        self.available = os.path.isfile(path)
        self.start = os.path.getsize(path) if self.available else 0   # skip earlier runs
        self.run_start: float | None = None
        self.windows: list[tuple[float, float]] = []

    def begin(self) -> None:
        """Mark the start of the timed suites; leftovers are counted from here."""
        self.run_start = time.time()

    def note(self, *windows: list[float]) -> None:
        """Remember a phase's warmup and timed windows for the leftover check."""
        self.windows.extend((float(a), float(b)) for a, b in windows)

    def settle(self) -> float:
        """Wait for late lines to land: at least MIN_WAIT, then until the file
        has stopped growing for SETTLE, capped at MAX_WAIT. Returns the wait."""
        if not self.available:
            return 0.0
        t0 = time.time()
        size, stable_since = os.path.getsize(self.path), t0
        while True:
            time.sleep(0.1)
            now = os.path.getsize(self.path)
            if now != size:
                size, stable_since = now, time.time()
            waited = time.time() - t0
            if waited >= self.MAX_WAIT or (waited >= self.MIN_WAIT and time.time() - stable_since >= self.SETTLE):
                return waited

    def _lines(self) -> list[tuple[float, str, str, int]]:
        with open(self.path, "rb") as f:
            f.seek(self.start)
            raw = f.read().decode("utf-8", "replace").splitlines()
        out = []
        for line in raw:
            parts = line.split(" ", 4)
            if len(parts) < 5:
                continue
            try:
                ts = float(parts[0])
            except ValueError:
                continue
            uri = parts[2].strip('"')
            status, sent = parts[3], parts[4].split(" ", 1)[0]
            out.append((ts, uri, status, int(sent) if sent.isdigit() else 0))
        return out

    @staticmethod
    def _tally(lines) -> dict:
        requests = nbytes = 0
        statuses: dict[str, int] = {}
        for _, _, status, sent in lines:
            requests += 1
            nbytes += sent
            statuses[status] = statuses.get(status, 0) + 1
        return {"requests": requests, "bytes": nbytes, "statuses": statuses}

    def between(self, window: list[float], prefix: str) -> dict | None:
        """Origin traffic for `prefix` whose timestamps fall inside `window`."""
        if not self.available:
            return None
        t0, t1 = window
        return self._tally(l for l in self._lines() if t0 <= l[0] <= t1 and l[1].startswith(prefix))

    def unattributed(self, prefix: str) -> dict | None:
        """Traffic for `prefix` since begin() that fell inside no noted window."""
        if not self.available or self.run_start is None:
            return None
        def inside(ts: float) -> bool:
            return any(a <= ts <= b for a, b in self.windows)
        return self._tally(l for l in self._lines()
                           if l[0] >= self.run_start and l[1].startswith(prefix) and not inside(l[0]))


# ── Docker helpers (all optional) ─────────────────────────────────────────────

class Compose:
    def __init__(self, file: str) -> None:
        self.file = file
        self.ok = bool(shutil.which("docker")) and os.path.isfile(file)
        if self.ok:
            self.ok = self._run(["ps", "-q"]) is not None

    def _run(self, args: list[str], timeout: float = 120) -> str | None:
        try:
            out = subprocess.run(["docker", "compose", "-f", self.file, *args],
                                 capture_output=True, text=True, timeout=timeout)
        except (OSError, subprocess.TimeoutExpired):
            return None
        return out.stdout if out.returncode == 0 else None

    def memory_mb(self, service: str) -> float | None:
        cid = (self._run(["ps", "-q", service]) or "").strip().splitlines()
        if not cid:
            return None
        try:
            out = subprocess.run(["docker", "stats", "--no-stream", "--format", "{{.MemUsage}}", cid[0]],
                                 capture_output=True, text=True, timeout=30)
        except (OSError, subprocess.TimeoutExpired):
            return None
        used = out.stdout.split("/")[0].strip()
        for suffix, scale in (("GiB", 1024), ("MiB", 1), ("KiB", 1 / 1024), ("B", 1e-6)):
            if used.endswith(suffix):
                try:
                    return float(used[:-len(suffix)]) * scale
                except ValueError:
                    return None
        return None

    def restart(self, service: str) -> bool:
        return self._run(["restart", service], timeout=300) is not None


# ── Geometry ──────────────────────────────────────────────────────────────────

def tiles_at_zoom(extent: tuple, z: int) -> list[tuple[int, int, int]]:
    west, south, east, north = extent
    n = 2 ** z

    def lon_to_x(lon: float) -> int:
        return int(math.floor((lon + 180.0) / 360.0 * n))

    def lat_to_y(lat: float) -> int:
        lat = max(-85.05112878, min(85.05112878, lat))
        r = math.radians(lat)
        return int(math.floor((1.0 - math.log(math.tan(r) + 1.0 / math.cos(r)) / math.pi) / 2.0 * n))

    x0, x1 = max(0, lon_to_x(west)), min(n - 1, lon_to_x(east))
    y0, y1 = max(0, lat_to_y(north)), min(n - 1, lat_to_y(south))
    tiles = []
    for x in range(x0, x1 + 1):
        for y in range(y0, y1 + 1):
            tw, ts, te, tn = tile_bbox(z, x, y)
            if min(te, east) - max(tw, west) > 1e-9 and min(tn, north) - max(ts, south) > 1e-9:
                tiles.append((z, x, y))
    return tiles


def tile_bbox(z: int, x: int, y: int) -> tuple[float, float, float, float]:
    """WGS84 bounds (west, south, east, north) of a Web Mercator tile."""
    n = 2 ** z
    west, east = x / n * 360.0 - 180.0, (x + 1) / n * 360.0 - 180.0
    lat = lambda yy: math.degrees(math.atan(math.sinh(math.pi * (1 - 2 * yy / n))))
    return west, lat(y + 1), east, lat(y)


def sample(rng: random.Random, items: list, k: int) -> list:
    return rng.sample(items, k) if len(items) >= k else [rng.choice(items) for _ in range(k)]


# ── Reporting ─────────────────────────────────────────────────────────────────

def fmt(v, digits: int = 1) -> str:
    if v is None:
        return "n/a"
    if isinstance(v, float):
        return "n/a" if math.isnan(v) else f"{v:,.{digits}f}"
    return str(v)


def table(headers: list[str], rows: list[list]) -> str:
    cells = [[str(h) for h in headers]] + [[fmt(c) if not isinstance(c, str) else c for c in r] for r in rows]
    widths = [max(len(r[i]) for r in cells) for i in range(len(headers))]
    line = lambda r: "| " + " | ".join(c.ljust(w) if i == 0 else c.rjust(w) for i, (c, w) in enumerate(zip(r, widths))) + " |"
    sep = "|" + "|".join("-" * (w + 2) for w in widths) + "|"
    return "\n".join([line(cells[0]), sep] + [line(r) for r in cells[1:]])


def load_rows(entries: list[dict], origin: bool = True) -> str:
    """One row per server. Origin columns are attributed after the whole run,
    so the live print during the run leaves them out."""
    headers = ["server", "ok", "err", "p50 ms", "p95 ms", "p99 ms", "max ms", "req/s", "MB/s"]
    if origin:
        headers += ["origin req", "origin MB", "origin req/req", "origin KB/req"]
    headers.append("RSS MB")
    rows = []
    for e in entries:
        m, o = e["load"], e.get("origin")
        per = e["load"]["requests"] or 1
        row = [e["server"], m["ok"], m["errors"], m["p50_ms"], m["p95_ms"], m["p99_ms"], m["max_ms"],
               m["rps"], m["mb_per_s"]]
        if origin:
            row += [o["requests"] if o else None, o["bytes"] / 1e6 if o else None,
                    o["requests"] / per if o else None, o["bytes"] / per / 1e3 if o else None]
        row.append(e.get("memory_mb"))
        rows.append(row)
    return table(headers, rows)


def close(a, b, rel: float = 1e-3) -> bool:
    if a is None or b is None:
        return a is None and b is None
    try:
        a, b = float(a), float(b)
    except (TypeError, ValueError):
        return False
    if math.isnan(a) or math.isnan(b):
        return math.isnan(a) and math.isnan(b)
    return abs(a - b) <= rel * max(1.0, abs(a), abs(b))


# ── Main ──────────────────────────────────────────────────────────────────────

def main() -> None:
    ap = argparse.ArgumentParser(description="Benchmark sabre against titiler over HTTP.",
                                 formatter_class=argparse.ArgumentDefaultsHelpFormatter)
    ap.add_argument("--sabre",   default="http://127.0.0.1:8787", help="sabre base URL, or 'none'")
    ap.add_argument("--titiler", default="http://127.0.0.1:8000", help="titiler base URL, or 'none'")
    ap.add_argument("--origin",  default="http://origin", help="origin base URL as the servers see it")
    ap.add_argument("--cog",     default="dem.tif", help="file name under bench/data on the origin")
    ap.add_argument("--origin-log", default=os.path.join(HERE, "out", "logs", "access.log"))
    ap.add_argument("--compose-file", default=os.path.join(HERE, "docker-compose.yml"))
    ap.add_argument("--out",     default=os.path.join(HERE, "out"))
    ap.add_argument("--suites",  default="tiles,point,polygon,info")
    ap.add_argument("--zooms",   default="6,8,10")
    ap.add_argument("--tiles",   type=int, default=100, help="tiles per zoom level")
    ap.add_argument("--points",  type=int, default=200)
    ap.add_argument("--polygons", type=int, default=50)
    ap.add_argument("--polygon-px", type=int, default=512, help="polygon side in source pixels")
    ap.add_argument("--info-requests", type=int, default=100)
    ap.add_argument("--concurrency", default="1,8,32")
    ap.add_argument("--warmup",  type=int, default=20, help="untimed requests before each phase")
    ap.add_argument("--compare", type=int, default=12, help="tiles to fetch from both and diff")
    ap.add_argument("--colormap", default="viridis")
    ap.add_argument("--band",    type=int, default=1, help="1-based band")
    ap.add_argument("--min",     type=float, help="colormap range (default: file stats, else 0)")
    ap.add_argument("--max",     type=float, help="colormap range (default: file stats, else 3000)")
    ap.add_argument("--seed",    type=int, default=42)
    ap.add_argument("--wait",    type=float, default=180, help="seconds to wait for servers")
    ap.add_argument("--cold",    action="store_true", help="restart each container and time its first tile (needs Docker)")
    args = ap.parse_args()

    servers: list[Server] = []
    if args.sabre.lower() != "none":
        servers.append(Sabre(args.sabre, f"{args.origin.rstrip('/')}/sabre/{args.cog}"))
    if args.titiler.lower() != "none":
        servers.append(Titiler(args.titiler, f"{args.origin.rstrip('/')}/titiler/{args.cog}"))
    if not servers:
        sys.exit("nothing to benchmark: both servers are 'none'")

    suites = [s.strip() for s in args.suites.split(",") if s.strip()]
    zooms = [int(z) for z in args.zooms.split(",")]
    concurrencies = [int(c) for c in args.concurrency.split(",")]
    rng = random.Random(args.seed)
    os.makedirs(args.out, exist_ok=True)
    origin_log = OriginLog(args.origin_log)
    compose = Compose(args.compose_file)

    print("Waiting for servers …")
    versions: dict[str, dict] = {}
    for s in servers:
        print(f"  {s.name:<8} {s.base}  ready in {wait_healthy(s, args.wait):.1f}s  source={s.source}")
        if isinstance(s, Titiler):
            try:
                versions[s.name] = json.loads(fetch(s, Req("GET", s.health_path))[1]).get("versions", {})
            except (ValueError, OSError):
                pass
    if not origin_log.available:
        print(f"  origin log not found at {args.origin_log}; origin traffic will be reported as n/a")
    if not compose.ok:
        print("  docker compose not reachable; memory and cold-start numbers will be n/a")

    # ── Dataset ───────────────────────────────────────────────────────────────
    extent = None
    width = height = None
    vmin, vmax = args.min, args.max
    for s in servers:
        status, body = fetch(s, s.info())
        if status != 200:
            sys.exit(f"{s.name} /info failed ({status}): {body[:300].decode('utf-8', 'replace')}")
        info = json.loads(body)
        if isinstance(s, Sabre):
            ext = info.get("extent") or {}
            extent = extent or (ext["west"], ext["south"], ext["east"], ext["north"])
            width, height = info.get("width"), info.get("height")
            vmin = vmin if vmin is not None else info.get("stats_min")
            vmax = vmax if vmax is not None else info.get("stats_max")
        else:
            b = info.get("bounds")
            if b and extent is None:
                extent = tuple(b)
            width, height = width or info.get("width"), height or info.get("height")
    if extent is None:
        sys.exit("could not determine the raster extent from either server")
    vmin = 0.0 if vmin is None else float(vmin)
    vmax = 3000.0 if vmax is None else float(vmax)
    style = Style(args.colormap, vmin, vmax, args.band)
    pixel_deg = (extent[2] - extent[0]) / width if width else 0.001
    print(f"\nDataset: {width}×{height}, extent {tuple(round(v, 4) for v in extent)}, "
          f"colormap {style.colormap} {vmin:g}–{vmax:g}, band {style.band}")

    results: dict = {
        "args": vars(args),
        "servers": {s.name: {"base": s.base, "source": s.source, "versions": versions.get(s.name)} for s in servers},
        "dataset": {"extent": extent, "width": width, "height": height, "vmin": vmin, "vmax": vmax},
        "started": time.strftime("%Y-%m-%dT%H:%M:%S"), "compare": [], "cold": [], "phases": [],
    }
    report: list = [f"# sabre vs titiler — {results['started']}", "",
                    f"Dataset {width}×{height}, extent {tuple(round(v, 4) for v in extent)}, "
                    f"colormap `{style.colormap}` {vmin:g}–{vmax:g}, band {style.band}. "
                    f"Servers: " + ", ".join(f"{s.name} at `{s.base}` reading `{s.source}`" for s in servers) + "."]
    for name, v in versions.items():
        report.append(f"{name} versions: " + ", ".join(f"{k} {val}" for k, val in v.items()) + ".")
    report.append("")

    # ── Work lists ────────────────────────────────────────────────────────────
    tiles = {z: sample(rng, tiles_at_zoom(extent, z), args.tiles) for z in zooms}
    # Points stay a pixel inside the extent: on the exact edge the two servers
    # round differently and one of them reports the point as outside.
    inset = pixel_deg
    points = [(rng.uniform(extent[0] + inset, extent[2] - inset), rng.uniform(extent[1] + inset, extent[3] - inset))
              for _ in range(args.points)]
    side = args.polygon_px * pixel_deg
    boxes = []
    for _ in range(args.polygons):
        w = rng.uniform(extent[0], max(extent[0], extent[2] - side))
        s_ = rng.uniform(extent[1], max(extent[1], extent[3] - side))
        boxes.append((w, s_, min(w + side, extent[2]), min(s_ + side, extent[3])))

    # ── Equal work: diff a few tiles, points and polygons across servers ──────
    if len(servers) == 2 and args.compare:
        print(f"\nComparing {args.compare} tiles from both servers …")
        cmp_dir = os.path.join(args.out, "compare")
        os.makedirs(cmp_dir, exist_ok=True)
        try:
            import numpy as np
            from PIL import Image
        except ImportError:
            np = Image = None
            print("  (install numpy and Pillow for a pixel diff; comparing sizes only)")
        unique = {z: list(dict.fromkeys(t)) for z, t in tiles.items()}
        zs = sorted(unique)
        picks = list(dict.fromkeys(
            unique[zs[i % len(zs)]][(i // len(zs)) % len(unique[zs[i % len(zs)]])] for i in range(args.compare)))
        rows = []
        for z, x, y in picks:
            row: dict = {"tile": f"{z}/{x}/{y}"}
            images = {}
            for s in servers:
                status, body = fetch(s, s.tile(z, x, y, style))
                row[f"{s.name}_status"], row[f"{s.name}_bytes"] = status, len(body)
                path = os.path.join(cmp_dir, f"{z}_{x}_{y}_{s.name}.png")
                with open(path, "wb") as f:
                    f.write(body)
                if status == 200 and Image is not None:
                    try:
                        images[s.name] = np.asarray(Image.open(path).convert("RGBA")).astype(int)
                    except Exception as e:  # noqa: BLE001
                        row[f"{s.name}_decode_error"] = str(e)
            if len(images) == 2:
                a, b = images[servers[0].name], images[servers[1].name]
                if a.shape == b.shape:
                    alpha_diff = (a[..., 3] != b[..., 3]).mean()
                    both = (a[..., 3] == 255) & (b[..., 3] == 255)
                    rgb = np.abs(a[..., :3] - b[..., :3])[both]
                    row.update(alpha_diff_pct=100 * alpha_diff,
                               rgb_max_diff=int(rgb.max()) if rgb.size else 0,
                               rgb_mean_diff=float(rgb.mean()) if rgb.size else 0.0,
                               opaque_pct=100 * both.mean())
                else:
                    row["shape_mismatch"] = f"{a.shape} vs {b.shape}"
            rows.append(row)
            results["compare"].append(row)
        headers = ["tile"] + [f"{s.name} status" for s in servers] + [f"{s.name} bytes" for s in servers]
        if Image is not None:
            headers += ["alpha diff %", "rgb max", "rgb mean", "both opaque %"]
        out_rows = []
        for r in rows:
            out_rows.append([r["tile"]] + [r[f"{s.name}_status"] for s in servers] + [r[f"{s.name}_bytes"] for s in servers]
                            + ([r.get("alpha_diff_pct"), r.get("rgb_max_diff"), r.get("rgb_mean_diff"), r.get("opaque_pct")] if Image is not None else []))
        t = table(headers, out_rows)
        print(t)
        report += ["## Equal work check", "", f"PNGs are in `{cmp_dir}`.", "", t, ""]

        # Point and polygon values.
        mismatches = []
        checked = 0
        for lon, lat in points[:20]:
            vals = {}
            for s in servers:
                status, body = fetch(s, s.point(lon, lat, style))
                vals[s.name] = s.parse_point(body) if s.served(status, body) else f"HTTP {status}"
            checked += 1
            a, b = vals[servers[0].name], vals[servers[1].name]
            if not close(a, b):
                mismatches.append(("point", f"{lon:.5f},{lat:.5f}", a, b))
        for box in boxes[:10]:
            vals = {}
            for s in servers:
                status, body = fetch(s, s.polygon(box, style))
                vals[s.name] = s.parse_polygon(body) if status == 200 else {"error": f"HTTP {status}"}
            checked += 1
            a, b = vals[servers[0].name], vals[servers[1].name]
            for k in ("min", "max", "mean"):
                if not close(a.get(k), b.get(k), rel=1e-2 if k == "mean" else 1e-3):
                    mismatches.append((f"polygon {k}", "%.4f,%.4f" % box[:2], a.get(k), b.get(k)))
        msg = f"{checked} point/polygon values compared, {len(mismatches)} mismatches."
        print("\n" + msg)
        report += ["", msg, ""]
        if mismatches:
            t = table(["check", "where", servers[0].name, servers[1].name],
                      [[k, w, fmt(a, 4) if not isinstance(a, str) else a, fmt(b, 4) if not isinstance(b, str) else b]
                       for k, w, a, b in mismatches[:15]])
            print(t)
            report += [t, ""]
        results["value_mismatches"] = [list(m) for m in mismatches]

    # ── Cold start ────────────────────────────────────────────────────────────
    if args.cold:
        print("\nCold start …")
        rows = []
        z = zooms[len(zooms) // 2]
        for s in servers:
            if not compose.ok or not compose.restart(s.service):
                print(f"  {s.name}: cannot restart via docker compose; skipped")
                continue
            ready = wait_healthy(s, args.wait)
            t0 = time.perf_counter()
            status, _ = fetch(s, s.tile(*tiles[z][0], style))
            first = (time.perf_counter() - t0) * 1000
            rows.append([s.name, ready, first, status])
            results["cold"].append({"server": s.name, "ready_s": ready, "first_tile_ms": first, "status": status})
        if rows:
            t = table(["server", "healthy after s", "first tile ms", "status"], rows)
            print(t)
            report += ["## Cold start", "", "Container restarted, time to a healthy check, then the first tile.", "", t, ""]

    # ── Timed suites ──────────────────────────────────────────────────────────
    origin_log.begin()
    pending_origin: list[tuple[dict, str]] = []   # (entry, source path) to attribute once the log has settled

    def phase(title: str, make: dict[str, list[Req]]) -> None:
        for c in concurrencies:
            entries = []
            for s in servers:
                reqs = make[s.name]
                load = run_load(s, reqs, c, args.warmup)
                origin_log.note(load["warmup_window"], load["window"])
                entry = {"server": s.name, "load": load, "origin": None,
                         "memory_mb": compose.memory_mb(s.service) if compose.ok else None}
                entries.append(entry)
                pending_origin.append((entry, s.log_prefix))
            results["phases"].append({"suite": title, "concurrency": c, "entries": entries})
            heading = f"{title} — concurrency {c}, {len(next(iter(make.values())))} requests"
            print(f"\n{heading}\n{load_rows(entries, origin=False)}")
            # The table with origin columns is rendered when the report is written.
            report.extend([f"### {heading}", "", entries, ""])

    if "tiles" in suites:
        report.append("## Tiles")
        for z in zooms:
            phase(f"tiles z={z}", {s.name: [s.tile(zz, x, y, style) for zz, x, y in tiles[z]] for s in servers})
    if "point" in suites:
        report.append("## Point queries")
        phase("point", {s.name: [s.point(lon, lat, style) for lon, lat in points] for s in servers})
    if "polygon" in suites:
        report.append("## Polygon statistics")
        phase(f"polygon {args.polygon_px}px", {s.name: [s.polygon(b, style) for b in boxes] for s in servers})
    if "info" in suites:
        report.append("## Info")
        phase("info", {s.name: [s.info() for _ in range(args.info_requests)] for s in servers})

    # ── Origin accounting ─────────────────────────────────────────────────────
    # The origin can write a line well after the server under test has answered
    # the client, so lines are attributed to phases by timestamp once the whole
    # run is over and the log has gone quiet, not phase by phase as they run.
    if origin_log.available and pending_origin:
        waited = origin_log.settle()
        for entry, path in pending_origin:
            entry["origin"] = origin_log.between(entry["load"]["window"], path)
        totals = {}
        for entry, _ in pending_origin:
            t = totals.setdefault(entry["server"], {"requests": 0, "bytes": 0})
            t["requests"] += entry["origin"]["requests"]
            t["bytes"] += entry["origin"]["bytes"]
        print(f"\nOrigin traffic during the timed requests, after waiting {waited:.1f}s for the log to settle "
              f"(per-phase columns are in results.md):")
        for name, t in totals.items():
            print(f"  {name}: {t['requests']} range requests, {t['bytes'] / 1e6:.1f} MB")

    # Every origin request made during the suites should sit inside a warmup or
    # timed window. Anything left over means the origin count in the tables is
    # incomplete, and says how badly.
    leftovers = {s.name: origin_log.unattributed(s.log_prefix) for s in servers}
    results["origin_unattributed"] = leftovers
    stray = {name: o for name, o in leftovers.items() if o and o["requests"]}
    if stray:
        lines = [f"  {name}: {o['requests']} requests, {o['bytes'] / 1e6:.1f} MB" for name, o in stray.items()]
        print("\nWARNING: origin requests outside every phase window, so the origin columns in results.md are incomplete:")
        print("\n".join(lines))
        report += ["## Origin accounting", "",
                   "Origin requests that fell outside every phase window. The origin columns above are "
                   "incomplete by this much; if it is large, the origin is logging late or its clock "
                   "disagrees with the harness's.", "", *lines, ""]

    # ── Write out ─────────────────────────────────────────────────────────────
    results["finished"] = time.strftime("%Y-%m-%dT%H:%M:%S")
    with open(os.path.join(args.out, "results.json"), "w") as f:
        json.dump(results, f, indent=2, default=str)
    rendered = [part if isinstance(part, str) else load_rows(part) for part in report]
    with open(os.path.join(args.out, "results.md"), "w") as f:
        f.write("\n".join(rendered).rstrip() + "\n")
    print(f"\nWrote {os.path.join(args.out, 'results.md')} and results.json")


if __name__ == "__main__":
    main()
