# Browser benchmark: sabre against OpenLayers' GeoTIFF source

The same COG drawn by two engines in the same real browser, walking the same
views:

- **sabre**: `@sabremaps/ol` on a canvas `TileLayer`. Decoding, reprojection
  and styling run in sabre's wasm, in a Web Worker.
- **ol**: OpenLayers' own `ol/source/GeoTIFF` on a `WebGLTile` layer.
  geotiff.js decodes in its worker pool; OpenLayers reprojects from the COG's
  UTM zone and styles on the GPU.

```bash
just npm                                # build the wasm into @sabremaps/browser
just bench-browser                      # 5 runs, local, no added latency
just bench-browser --latency 40         # 40 ms before every response
node bench/browser/run.ts --scene 34JDP --runs 9   # flags are listed at the top of run.ts
```

Needs the scenes in `bench/data/scenes` (`bench/fetch_scenes.py`) and Chrome
installed. Other browsers: `--browser chromium|firefox|webkit` after
`pnpm exec playwright install <browser>` in this directory.

## What it does

Playwright drives the installed Chrome at 1280×800, device pixel ratio 1. For
every run, each engine gets a **fresh browser context**: an empty HTTP cache,
new workers, and the wasm compiled again. Runs alternate the engines' order, and
one warm-up run is discarded. Results are medians, with the min–max range.

1. **First render**: the page loads and shows the whole 110 km scene at z10. The
   time runs from navigation to OpenLayers' first `rendercomplete`, so it
   includes downloading the code and reading the header.
2. **The walk**: then, one view at a time, waiting for `rendercomplete` after
   each:
   - zoom to z12, then to z14;
   - three pans, of one viewport width or height each;
   - zoom to z16, past the raster's own resolution;
   - back out to z10;
   - change the colour range's maximum from 3000 to 2000.

A local HTTP/1.1 server hands out the COG with range support, like S3. It sits
on a different port from the page, so reads are cross-origin as in production.
It can hold every response back (`--latency`), and it counts every byte it
sends. Reads are counted at the server, not in the browser, so both engines'
workers are counted alike.

Main-thread blocking is the sum of each long task's time past 50 ms, from the
page's `longtask` observer. That is Chromium only; other browsers report 0.

Screenshots of both engines at z10 and z14 are saved with every result, to
check they drew the same thing.

### Keeping it fair

- **The same picture.** Both engines draw band 1 through the same viridis ramp
  (OpenLayers gets 16 of sabre's 256 stops), over 0 to 3000, nodata
  transparent, nearest-neighbour, on Web Mercator, with no basemap.
- **OpenLayers as its docs show it.** It is configured as its documentation
  shows. The only changes are what parity needs:
  - `normalize: false`, so the style sees raw values;
  - `interpolate: false`, for nearest-neighbour.

  geotiff.js keeps its defaults, including its block size and cache.
- **Only its own code.** Each engine is its own chunk, so a page downloads only
  the engine it uses.
- **One thing is not equal: the resolution level each reads.** sabre reads the
  first level at least as fine as the view, as GDAL does. OpenLayers often
  takes the next coarser one. At z10 and z12, sabre therefore reads about 4×
  the bytes and draws a visibly sharper picture; compare the z10 screenshots.
  At z14 and beyond, both read exactly the same bytes.

## Results

This is a snapshot, not a guarantee. It was taken on 2026-09-23 on an Apple M5
with Chrome 153, WebGL on Metal, using scene 35HLD (10980², uint16, Deflate,
1024² tiles, UTM 35S). Medians of 5 runs; times in ms.

| | sabre | ol, 0 ms | sabre | ol, 40 ms |
| --- | ---: | ---: | ---: | ---: |
| first render, from navigation | 383 | 532 | 602 | 618 |
| zoom to z12 | 264 | 581 | 864 | 829 |
| zoom to z14 | 383 | 765 | 433 | 832 |
| each pan at z14 | 283–316 | 648–698 | 283–366 | 649–749 |
| z16, past native | 16 | 414 | 16 | 414 |
| back out to z10 (cached) | 16 | 15 | 16 | 16 |
| restyle | 366 | 16 | 350 | 15 |
| **whole walk** | **1927** | **3771** | **2676** | **4203** |
| main thread blocked, whole walk | 0 | 594 | 0 | 648 |
| MB read, whole walk | 17.6 | 13.1 | 17.6 | 13.1 |
| code, brotli KB | 259 | 189 | 259 | 189 |

What they say:

- **New views draw about twice as fast in sabre.** This holds for every zoom
  and pan where both read the same bytes, and with or without latency.
- **sabre never blocks the main thread; OpenLayers blocked it for about 0.6 s
  over the walk.** That is time in which the map cannot respond. Where
  OpenLayers spends it has not been profiled yet; reprojection and texture
  upload are the likely candidates.
- **Past the raster's resolution, sabre stretches the tiles it already has.**
  OpenLayers reprojects again, at 0.4 s.
- **Restyling is where OpenLayers wins outright.** Its style is a shader
  uniform, so a new colour range costs one frame. sabre redraws every tile in
  view, from cached bytes, at about 0.35 s.
- **With latency, sabre's first render and z12 are no faster.** The finer
  resolution level means 3–4× the requests there, and each one now waits 40 ms.
  The picture is sharper for it.
- **sabre's code is 70 KB larger** once compressed, mostly the wasm.

## Caveats

- One machine, one browser, one scene, so far. Firefox and WebKit runs, the
  other scenes (`--scene 34JDP|50HPK`), and a slower machine are the obvious
  next data points. In headless Chromium without a GPU,
  WebGL falls back to software, which would be unfair to OpenLayers. The GPU in
  use is printed at the top of every summary.
- Times are measured to the frame (about 16.7 ms), because `rendercomplete`
  fires on one.
- The server is local and HTTP/1.1. Real object storage adds bandwidth limits
  and variable latency; `--latency` models only the latency.
