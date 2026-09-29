# sabre in a browser

`sabre-core` compiled to WebAssembly, rendering COG tiles client-side from
`fetch` range requests. No server in the path.

```bash
just browser                      # build the bundle into examples/wasm/pkg
just browser-test                 # run the tests in headless Chrome
(cd examples/wasm && python3 -m http.server 8080)
```

## Testing

`cargo check --target wasm32-unknown-unknown` proves almost nothing here.
`std::time::Instant::now()` compiles for this target and panics when it runs —
the whole render path built cleanly and then trapped with
`RuntimeError: unreachable` on the first tile. So the tests execute in a real
browser:

```bash
CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner \
    cargo test -p sabre-browser --target wasm32-unknown-unknown
```

Reintroduce that `Instant::now()` and `cargo check` still passes while
`a_tile_renders_and_reprojects` fails. CI runs both, and enforces a **300 KB
brotli budget** on the bundle — the client path stops being worth it if the
decoder costs more than the imagery.

## What it answers

**It works, and the output is exact.** A tile rendered in the browser is
**byte-identical** to the same tile from `sabre-server` — 75,080 bytes, zero
differing channels across all 65,536 pixels. That covers the whole path:
`PREDICTOR=2` decode, UTM reprojection through `proj4rs`, colormap, PNG encode.

**The download is small.** 443 KB of wasm, **159 KB brotli**. Smaller than most
of the JavaScript already on a map page.

**It is barely slower than native.** Same tile, same COG, warm:

| phase | server | browser | |
| --- | --- | --- | --- |
| decode | 29.05 ms | 36.7 ms | |
| blit | 0.28 | 0.3 | |
| resample | 0.17 | 0.1 | |
| style | 0.24 | 0.7 | |
| encode | 0.29 | 0.9 | |
| **total** | **~30 ms** | **38.1 ms** | 1.3x |

Module init is 11.7 ms, paid once.

**The cache earns more here than on the server.** A tab is long-lived, which is
exactly what the Workers isolate was not. Panning seven neighbouring tiles
after the first cost **zero** range requests, against 11 with the cache off:

| | time | range requests |
| --- | --- | --- |
| 8 tiles, cache on | 92 ms | 0 after the first |
| 8 tiles, cache off | 150 ms | 11 |

## What it also showed

**Decode is the bottleneck everywhere.** 96% of a warm browser tile and 97% of
a warm server one. On real Sentinel-2 — 1024 px tiles of uint16 with a
predictor — nothing else is close. Work spent there pays off in both runtimes.

**A cold read over a long network is the real cost**, not the rendering. The
first measurement, before anything was cached, spent **3,313 ms in `fetch`**
across three range requests to a Space on another continent. The browser path
puts that latency in front of the user with nothing to hide it behind, where a
server sits next to the data and absorbs it once for everyone.

**It needs CORS on the object store**, which the server path does not. The
bucket must allow `GET`/`HEAD` with the `Range` header and expose
`Content-Range`. Without it the browser blocks the read outright.

## What it cost to find out

Three mistakes worth recording, because all three were silent and all three
produced confident numbers that were wrong:

`std::time::Instant::now()` compiles for `wasm32-unknown-unknown` and panics
when it runs. The clock had already been abstracted for exactly this reason,
but five raw calls were left in the render and query paths — so
`cargo check --target wasm32-unknown-unknown` passed, and the first tile
trapped with `RuntimeError: unreachable` inside a future. `core/tests/wasm_safe.rs`
now fails the build if anything outside `timing.rs` reads a clock.

`[profile.release]` in a member manifest is **ignored** — Cargo only warns.
The first size and speed numbers came from a build that had none of the
settings written for it. With LTO actually applied a warm tile went from
126 ms to 38 ms, and `opt-level = 3` turned out both smaller *and* faster than
`"z"`. The profile now lives in the workspace root, and CI builds release so
the size it reports is the size that ships.

The first stream-versus-whole-file measurement ran on **localhost**, where a
round trip costs nothing and only the byte count is left to see. It pointed
the opposite way. The second put the first run's TLS handshake in one column
and not the other. Hence a warmed connection and medians of three.

## Farm scale is a different workload from a Sentinel scene

A COG covering one property at 1–3 m is single-digit megabytes; the same farm
at 0.5 m aerial is 12–143 MB, and a full Sentinel-2 scene is 107–325 MB. Those
are not the same problem, and they do not want the same answer.

| | farm extent, 1–3 m | Sentinel scene |
| --- | --- | --- |
| size | ~0.5–9 MB | 100–325 MB |
| shape | one property, panned around | 110 km tile, analytics over all of it |
| where | **client** | **server** |

### Measured on a real one

A farm-scale raster: **1101×1033 float32 at ~4 m/px, 651 KB**, EPSG:4326,
DEFLATE with no predictor, six overview levels — one property, about
4.3 × 4.6 km.

| | |
| --- | --- |
| whole map, z=13–16, **126 tiles** | **381 ms** |
| first tile | 7 ms |
| range requests for the session | 6 |
| bytes read | 636 KB — **all of it** |

**Three milliseconds a tile, no server, one 651 KB file.** A tile rendered in
the browser is byte-identical to the same tile from `sabre-server`: 28,040
bytes both, zero differing channels across 65,536 pixels — this time through
the float32, no-predictor, already-WGS84 path rather than the Sentinel one.

Below about a megabyte the streaming question stops mattering: a session reads
the whole file either way, so the only difference is 6 requests against 1.

`examples/wasm/farm.html` is where the question does matter. Warm connection, medians of
three, a pan across four zoom levels:

| file | mode | first tile | pan, 15 tiles | requests | bytes read |
| --- | --- | --- | --- | --- | --- |
| 2.25 MB | stream | **468 ms** | 1,079 ms | 3 | **32% of file** |
| 2.25 MB | whole | 1,989 ms | 73 ms | 1 | 100% |
| 7.93 MB | stream | **424 ms** | 725 ms | 3 | **6% of file** |
| 7.93 MB | whole | 5,013 ms | 81 ms | 1 | 100% |

**Stream, do not bulk download.** The obvious move at this size is to fetch the
whole file once and serve every tile from memory, and it is wrong: a pan reads
**6% of an 8 MB COG**, in three range requests, because the overviews are doing
what they exist for. Whole-file only wins the pan, having already paid for
every byte of it — and loses first tile, the thing a user feels, by 4–12x.

`Cog::preload()` stays for the case it is good for: taking a property offline,
or pre-warming a known extent on purpose. It is not the default.

## What this is not

It is not a replacement for the server, it is the other half of a pair. The
server caches for everyone, sits next to the data, and answers in 30 ms warm
without asking the client to download a decoder or wait out a cold
transatlantic read — which is what a 100–325 MB scene needs, and what an
analytics or ETL job over one wants in a container.

The client wins where the raster is small enough that one property is a few
megabytes: no backend to run, no per-tile egress, and a tab that keeps its
cache as long as the map is open.
