# Benchmarking sabre against titiler

An end-to-end, request-for-request comparison of `sabre-server` and
[titiler](https://github.com/developmentseed/titiler) over HTTP. Both servers read the
same Cloud-Optimized GeoTIFF from the same origin through the same HTTP range requests, get
the same tiles, points and polygons, and are measured the same way.

This is separate from `bench/py/`, which is an in-process rasterio port of sabre's own
operations, and from `cargo bench -p sabre-core`, which times the library alone.

## What is in here

| File | Purpose |
| --- | --- |
| `docker-compose.yml` | The stack: an nginx origin, sabre built from this repo, and the titiler image, all capped to the same CPUs and memory |
| `nginx.conf` | The origin. Serves `data/` under `/sabre/` and `/titiler/` and logs every range request |
| `make_cog.py` | Synthesises a terrain-like float32 COG, or re-encodes any raster, in a form sabre reads correctly |
| `run.py` | The harness. Standard library only; writes `out/results.md` and `out/results.json` |
| `origin.py` | A range-capable file server with the same log format, for running without Docker |
| `make_blocks.py` | Turns GeoJSON field boundaries into a block set for the `blocks`, `clip` and `imagery` suites |
| `fetch_scenes.py` | Picks and downloads the Sentinel-2 scene under each block set, byte-exact from AWS |
| `publish_scenes.py` | Uploads that imagery to the Space. Idempotent, so a failed run resumes by repeating |
| `suite.py` | The capacity suite: open-loop rate sweep, real workloads, publishable results |
| `verify.py` | The correctness job: sabre against titiler on identical requests. Its own schedule |
| `publish.py` | Uploads a result to the Space and rebuilds the index |

## What the suite measures, and why it changed

Production traffic for the deployment sabre was built for, over 31 days: 50.6 GB to users, 4.6M
requests, **10.9 KB average object**, and a clean weekly cycle — ~200K requests
a day on weekdays, ~45K at weekends. That is **5 req/s** across a working day
and perhaps 16 at a minute-level spike.

sabre sustained 104–683 req/s on 4 vCPUs. So the saturation knee is a cliff
nothing is driving towards, and probing for it was most of the suite's runtime.
`--knee` is now off by default.

What replaces it:

| | why |
| --- | --- |
| `--rates 5,20` held for `--soak` | latency and footprint where the server actually lives |
| `--rss pid:N` / `docker:service` | throughput headroom says nothing about memory |
| `--cold-start` | the weekend trough expires everything; this is Monday morning |
| `KB/req` in every row | egress is the cost line, not CPU |

### Memory is the column to read

Headroom does not bound footprint. Two things grow independently of the request
rate, and the suite exists partly to keep them honest:

- **Caches grow with variety, not rate.** The geometry cache was bounded by
  entry count, which bounds nothing you can put in a container — the entries
  are geometry and grow with how much land a request names. Measured at
  **8.9 KB an entry** for an 80-block farm view, the old 10,000-entry ceiling
  was 87 MB, and roughly 500 MB at the 512-id limit, beside a page cache whose
  256 MB was the number anyone would think they had configured. It is bounded
  in bytes now (`--geometry-cache-size`), and holds flat: +0.0 MB across 2,400
  further distinct entries.
- **Per-request buffers grow with concurrency.** A polygon query's working
  buffer is `pixels × bands × 4` bytes, and the cap was on pixels — 16 MB on a
  one-band DEM, 48 MB on a three-band scene, *each*. At 32 in flight that was
  1.5 GB, over the 2 GB this stack gives the container. The cap is on bytes
  now.

## Two jobs, not one

`suite.py` asks *what can sabre carry, and what does it cost to run*. It
measures sabre alone. Rust against Python, GDAL and FastAPI was never going to
be close, and re-establishing that every week taught us nothing.

`verify.py` asks *does sabre give the right answers*, and that is where titiler
earns its keep: it is an independent implementation of the same operations over
the same bytes, and the only thing here that can catch sabre being
*consistently* wrong. It found the viridis colormap being a 16-point
approximation of a 256-entry table, and a statistics window convention that
looked like a bug and was not.

```bash
python3 bench/verify.py --sabre http://127.0.0.1:8787 --titiler http://127.0.0.1:8000
bench/remote-bench.sh --verify        # provision, check, tear down
```

It exits non-zero on failure, so it can be a CI step. Run it before a release
and after anything touching rendering, decoding, reprojection or masking.

Each check decides for itself what counts as wrong, and says why next to the
check:

| Check | Fails when | Known difference |
| --- | --- | --- |
| tiles | mean channel difference > 4, or > 1% of the tile sampled differently | off-by-one rounding, and edges resolved to different source pixels |
| points | any difference at all — one pixel, one coordinate, no room for convention | — |
| polygon stats | mean differs by > 1% | min/max: titiler snaps the window to the raster grid, sabre does not |
| real blocks | mean differs by > 2% — a 150-pixel field moves ~0.7% per boundary pixel | min/max by one boundary pixel |
| geometry provider | by-reference differs from inline by > 0.5% | — |

The tile thresholds are calibrated against the bug that motivated them: the
16-point viridis table produced mean **6.96**, and the fixed one reads
**0.6–1.8** on the same tiles. The mean is what moved by two orders of
magnitude, so the mean is what decides; a single edge pixel at max 20 is not
evidence of anything.

The provider check is sabre against itself — titiler has no equivalent
operation — and it is what pins TWKB at precision 6 not having moved anything
that matters.

## Real workloads

The synthetic DEM is a control, not a workload. Everything about it is
convenient: float32, WGS84, `PREDICTOR=NO`, 4096 px, and the suites over it ask
for random 512×512 boxes scattered across two degrees. Nothing in production
looks like that.

Real Sentinel-2 L2A is uint16 in 1024 px blocks with `PREDICTOR=2`, in UTM, and
10980 px square. Real requests are farm blocks: a median field in one
set is **1.5 ha**, which at 10 m is a window of a few hundred pixels —
against the synthetic polygon suite's 262,144 — and a whole farm of 86 of them
lands in **two** of the scene's COG tiles. The requests are small and they are
clustered, and both of those decide how a tile server behaves.

Three steps, each pinned so a later run measures the same bytes:

```bash
# 1. Build a block set from field boundaries: any GeoJSON FeatureCollection of
#    Polygons or MultiPolygons. Only geometry is kept, and the set is named
#    from its shape unless you pass --name: blocks-86x1.5ha-utm35s.
python3 bench/make_blocks.py fields.geojson

# 2. Pick and download the scene under each set. The choice is written to the
#    manifest and reused; --repick chooses afresh.
python3 bench/fetch_scenes.py --dry-run
python3 bench/fetch_scenes.py

# 3. Publish. Terraform takes the Space, the DEM, the block sets and the
#    scene manifest; the imagery goes up separately, public-read like the
#    DEM because sabre speaks no S3 auth.
cd bench/terraform/dataset && terraform apply && cd -
export SPACES_ACCESS_KEY_ID=... SPACES_SECRET_ACCESS_KEY=...
python3 bench/publish_scenes.py --bucket <space> --region nyc3
```

The imagery is not a Terraform resource on purpose.
`digitalocean_spaces_bucket_object` has no import support, so an object
uploaded any other way can never be reconciled into state — and on a 325 MB
file, "any other way" is sometimes the only way that works. The resource makes
one attempt with no resume, so a 1.4 GB apply that dies two thirds through
leaves nothing to carry forward and nothing importable behind it.
`publish_scenes.py` skips anything already in the Space with the right size and
sha256, so repeating it after a failure picks up where it stopped.

This adds three suites, all skipped with a note when either half is missing:

| Suite | sabre | titiler |
| --- | --- | --- |
| `blocks <set>` | `GET /query?polygon=<WKT>` | `POST /cog/statistics` |
| `clip <set>` | `GET /tiles/z/x/y?mask=<WKT>` | `POST /cog/feature/256x256.png` |
| `imagery z=N` | `GET /tiles/z/x/y` on the scene | `GET /cog/tiles/…` on the scene |

`clip` asks the same question by two routes: sabre attaches a cutline to an
ordinary tile request, titiler has none on `/cog/tiles` and uses `/cog/feature`,
which frames the image on the field's own bounds rather than the tile grid.
Same output size and the same work, not the same framing — the report says so
where the numbers are.

`imagery` runs at z=13/15/17 rather than 6/8/10. A farm is 1–8 km across; at
z=6 a tile spans hundreds of kilometres and the measurement is of overview
selection, not of anything anyone asks for.

Interior rings are kept throughout. A block with a dam cut out of it is the
case where a mask is either right or quietly averaging in pixels that are not
part of the field.

Cross-checked against titiler on live AWS imagery, six real blocks: `min` and
`max` identical on four of six, means within 1.2%. The residual is the same
pixel-centre-versus-rasterisation convention difference documented under
*Fairness notes*, amplified because a 150-pixel field moves its mean by ~0.7%
for every boundary pixel either way.

## Quick start

```bash
# 0. Create the mounted directories yourself. They are gitignored, and if Docker
#    creates them for a bind mount they end up owned by root.
mkdir -p bench/data bench/out/logs

# 1. Make a test COG (the titiler image has rasterio; any rasterio works). The
#    image runs as an unprivileged user, so run it as yourself to write into data/.
docker compose -f bench/docker-compose.yml run --rm --no-deps --user "$(id -u):$(id -g)" \
    --entrypoint python titiler /bench/make_cog.py --out /bench/data/dem.tif --size 4096

# 2. Start the stack. BENCH_CPUS caps both servers and sets their worker counts.
BENCH_CPUS=4 docker compose -f bench/docker-compose.yml up --build -d

# 3. Run it
python3 bench/run.py
python3 bench/run.py --cold            # also restart each container and time its first tile

# 4. Read bench/out/results.md, then
docker compose -f bench/docker-compose.yml down
```

If step 1 fails with `Attempt to create new tiff file '/bench/data/dem.tif' failed:
Permission denied`, the directory was created by Docker on an earlier run and is owned by
root. Take it back and rerun: `sudo chown -R "$(id -u):$(id -g)" bench/data bench/out`.

Install `numpy` and `Pillow` for the harness if you want a pixel diff in the equal-work
check; without them it compares sizes only.

To benchmark a real raster, re-encode it first so both servers read identical bytes:

```bash
python3 bench/make_cog.py --input path/to/dem.tif --out bench/data/dem.tif
```

### Without Docker

Run the three pieces yourself and tell the harness where the origin is *as the servers see
it*:

```bash
python3 bench/origin.py --port 8081 &
SABRE_THREADS=4 cargo run --release -p sabre-server -- --bind 127.0.0.1:8787 &
uvicorn titiler.application.main:app --port 8000 --workers 4 &     # pip install titiler.application
python3 bench/run.py --origin http://127.0.0.1:8081
```

Set the GDAL environment variables listed in `docker-compose.yml` on the titiler process,
otherwise it runs without its recommended remote-COG settings. Memory and cold-start
columns need Docker and show `n/a` here.

## What the harness does

1. Waits for both health endpoints, reads the raster extent from `/info`, and picks the
   colormap range from the file's statistics if it has any, else 0–3000 (or `--min`/`--max`).
2. Builds one deterministic (`--seed`) work list: `--tiles` tiles at each of `--zooms`,
   `--points` random points, and `--polygons` random boxes `--polygon-px` pixels on a side.
3. **Equal-work check.** Fetches `--compare` tiles from both servers, saves them under
   `out/compare/`, and diffs them: the fraction of pixels whose alpha differs (geometry
   and nodata masking), and the RGB difference where both are opaque. Then compares 20
   point values and 10 polygon min/max/mean results.
4. **Timed suites**, each at every `--concurrency` level, each preceded by `--warmup`
   untimed requests: tiles per zoom, point queries, polygon statistics, info. Each request
   is sent over a keep-alive connection and timed individually.
5. Once every suite has run, it waits for the origin access log to go quiet and
   attributes every range request to a server by its path prefix and to a phase by its
   timestamp, warmup excluded. The origin can write a line after the server has already
   answered the client, which is why this is not done phase by phase; the live output
   therefore has no origin columns, `results.md` does. Anything that lands outside every
   window is reported at the end as unattributed, so an incomplete origin count is
   flagged rather than shown as zero.

The request each server receives:

| Operation | sabre | titiler |
| --- | --- | --- |
| Tile | `GET /tiles/{z}/{x}/{y}?url&colormap&min&max` | `GET /cog/tiles/WebMercatorQuad/{z}/{x}/{y}.png?url&bidx&rescale&colormap_name&resampling=nearest` |
| Point | `GET /query?url&lat&lng&band` | `GET /cog/point/{lon},{lat}?url&bidx` |
| Polygon | `GET /query?url&polygon=WKT&band` | `POST /cog/statistics?url&bidx` with a GeoJSON Feature |
| Info | `GET /info?url` | `GET /cog/info?url` |

## Reading the results

Each timed phase produces one row per server:

| Column | Meaning |
| --- | --- |
| ok / err | Requests that were answered, and everything else. sabre's 400 for a point on nodata counts as answered; every status is in `results.json` |
| p50 / p95 / p99 / max ms | Per-request latency as seen by the client |
| req/s, MB/s | Throughput over the phase's wall time |
| origin req, origin MB | Range requests and bytes the server pulled from the origin during the timed requests of the phase |
| origin req/req, KB/req | The same per client request |
| RSS MB | Container memory after the phase, from `docker stats` |

Things you will see, and why:

- **Both origin columns drop to zero once warm.** GDAL keeps a block cache
  (`GDAL_CACHEMAX`) and a curl cache (`CPL_VSIL_CURL_CACHE_SIZE`) per worker; sabre keeps
  one page cache (`SABRE_CACHE_SIZE`) shared by all its threads. Both are sized so a small
  raster is served entirely from memory. On a raster larger than the cache, or a wider tile
  set, the origin columns show the eviction behaviour instead; the `--cold` numbers and the
  first concurrency-1 phase are the fairer read of the origin cost per tile. Pass
  `SABRE_CACHE_SIZE=0` to measure sabre without its cache.
- **Colours differ slightly.** Both use viridis, but sabre interpolates a 16-stop ramp
  while titiler uses matplotlib's 256-entry table and quantises through `rescale` first.
  Expect an RGB mean difference of a few units and a max of tens. Alpha should match
  exactly on interior tiles; an alpha difference means the two disagree on geometry or
  nodata and the timing rows are comparing different work.
- **PNG sizes differ.** The encoders and compression levels are not the same, which shows
  up in `MB/s` and in the bytes column of the equal-work table. It is part of each server's
  cost, not noise.
- **A point on nodata is a 400 from sabre and a 200 with `null` from titiler.** The
  harness treats both as answered and as equal values; the status counts in
  `results.json` show how many there were.
- **Polygon min/max may differ at the boundary.** The two rasterise the polygon edge
  differently, so a pixel on the outline can be counted by one and not the other. Means
  should agree within a percent.
- **Low zooms have few distinct tiles.** A 2°×2° raster is two tiles at z=6, so a phase
  there repeats the same requests and mostly measures the warm path.
- **The servers do different amounts of work.** titiler supports any CRS through GDAL
  warping, every resampling method, expressions and more output formats. A raw speed gap
  partly reflects scope.

## Fairness notes

- Both containers get the same `cpus` and `mem_limit`, and the same number of workers:
  sabre's request threads and uvicorn's processes.
- sabre's page cache is given the same 200 MB as GDAL's curl cache. GDAL's is per worker
  and its decoded block cache comes on top, so titiler has more memory to cache with, not
  less.
- titiler runs with its own recommended GDAL settings for remote COGs, spelled out in
  `docker-compose.yml` so a run records them. `GDAL_HTTP_VERSION=2` has no effect
  against the HTTP/1.1 origin.
- sabre's polygon queries read full resolution and are capped at 4 million pixels;
  titiler's `POST /cog/statistics` also reads at full resolution unless `max_size` is
  passed. Keep `--polygon-px` at or below 2048 so sabre never returns an error row.
- The COG must be one sabre decodes correctly: little-endian, tiled, DEFLATE or LZW,
  **no predictor**. `make_cog.py` guarantees this and warns if a converted file has one.
- Run on an otherwise idle machine and run twice; the numbers are only as stable as the
  host.

---

## The suite (`suite.py`) versus the comparison (`run.py`)

`run.py` answers *"is sabre faster than titiler on these requests"*. `suite.py`
answers *"what can sabre carry, and did that change since last week"*. They
measure different things and neither replaces the other.

The difference is the load model. `run.py` is closed-loop: N threads, each
sending again the moment the last response lands. That gives one point per
concurrency level — a comparison. It cannot produce a latency-versus-load
curve, because offered load is whatever the server permits, so there is no
saturation knee to find.

`suite.py` adds an open-loop sweep: requests leave on a fixed schedule whether
or not earlier ones have returned, which is how real traffic arrives. Ramping
the offered rate until the server stops keeping up gives a capacity:

    | server | sustained req/s | first failing | p50 ms | p99 ms | probes | confirmed |
    |--------|-----------------|---------------|--------|--------|--------|-----------|
    | sabre  |           1,217 |         1,369 |    5.9 |   10.0 |      7 |       yes |

Load is generated by `bench-client`, a Rust binary (see `client/README.md`).
Orchestration stays in Python because it needs the tile arithmetic, docker and
the nginx access log, none of which belong in a load generator.

    cargo build --release -p sabre-bench-client     # or: bench/install-client.sh
    python3 bench/suite.py --knee --out bench/out/suite

With the generator on a second machine (see `terraform/README.md`), add
`--client-ssh`. The orchestrator stays on the server box — it needs docker, the
compose file and the nginx access log — and only the plan and its result cross
the wire:

    python3 bench/suite.py --knee --client-ssh bench-client \
        --client /root/sabre/target/release/bench-client \
        --sabre http://10.124.0.3:8787

Every run measures a round-trip floor first and marks any phase within 3x of it
as describing the path rather than the server. `info` stays on a local
generator even when split, because it answers faster than the network between
two boxes.

The ramp is seeded from the closed-loop throughput already measured rather than
from a fixed number, so a server that does 3,000 req/s is not probed at 100. It
then bisects between the last rate that tracked and the first that did not, and
re-runs the winner for longer to confirm. `info` is skipped: it answers in a
few hundred microseconds, below the generator's scheduling resolution, so an
open-loop figure there would describe the timer rather than the server.

Every result carries the git branch, commit and subject, the CPU model, and a
sha256 of the raster. A run made against a dirty working tree is labelled as
not attributable to its commit.

## Publishing results

    export SPACES_ACCESS_KEY_ID=... SPACES_SECRET_ACCESS_KEY=...
    python3 bench/publish.py --bucket sabre-bench-you --result bench/out/suite/suite.json
    python3 bench/publish.py --bucket sabre-bench-you --compare main

Layout:

    results/<branch>/<commit>/<timestamp>.json    one run, immutable
    index.json                                    every run, summarised

Results are immutable and the index is **rebuilt by listing them**, never
appended to. Two machines publishing at once would otherwise each read the same
index, add a row, and the second write would silently drop the first. Rebuilding
from a listing makes the objects the source of truth and the index a derived
cache, so a concurrent write costs at worst a stale index that the next publish
repairs.

`--compare` refuses to report a delta across different datasets or different
CPUs. A benchmark history that quietly compares an Apple M5 against a shared
Xeon is worse than no history, because the noise looks like a finding.

Publishing a run measured against a dirty tree needs `--allow-dirty`.
