# @sabremaps/browser

Render Cloud-Optimized GeoTIFF tiles in the browser with sabre: range reads
straight from object
storage, five style modes, reprojection, all in a Web Worker. No tile server.

This is the part that is not tied to a map library. For OpenLayers, use
[`@sabremaps/ol`](https://www.npmjs.com/package/@sabremaps/ol), which is built on it.

```bash
npm install @sabremaps/browser
```

```js
import { open } from '@sabremaps/browser';

const raster = await open('https://data.example.com/farm.tif');
raster.info;   // { width, height, bands, epsg, extent: [w, s, e, n], native_zoom, … }

const bitmap = await raster.tile(15, 18707, 19609, { colormap: 'viridis', min: 0, max: 3000 });
context.drawImage(bitmap, 0, 0);
bitmap.close();
```

## API

**`open(url)`** reads the raster's header and resolves to a `Raster`.

**`raster.tile(z, x, y, style?, { signal }?)`** renders a Web Mercator z/x/y
tile to an `ImageBitmap`, which the caller owns.

**`raster.pixels(z, x, y, style?, { signal }?)`** gives `{ data, size }` instead:
unencoded RGBA, `size × size × 4` bytes.

Pass an `AbortSignal` to cancel. A tile still waiting its turn is dropped
without being rendered.

**`raster.close()`** frees the raster's cache once no other `open()` of the same
URL still holds it.

**`configure({ wasmUrl, concurrency })`** changes where the wasm is loaded from
(beside the package by default) and how many tiles are in flight at once
(default 6). Call it before the first `open()`.

`style` is an object:

| Field | Default | |
| --- | --- | --- |
| `mode` | `colormap` | `colormap`, `rgb`, `hillshade`, `contour` or `classified` |
| `colormap` | `viridis` | Ramp for `colormap` and `contour` |
| `min`, `max` | `0`, `255` | Value range mapped across the ramp |
| `nodata` | from the file | Values drawn transparent |
| `stops` | | `classified`: `[[threshold, "#rrggbb" or [r, g, b]], …]` |
| `azimuth`, `altitude`, `z_factor` | `315`, `45`, `1` | `hillshade`: light direction, elevation, exaggeration |
| `hillshade_colormap` | | `hillshade`: a ramp blended under the shading |
| `contour_level` | `10` | `contour`: bands between `min` and `max` |
| `rgb_min_r` … `rgb_max_b` | `0` … `255` | `rgb`: per-channel black and white points |
| `tile_size` | `256` | Output size, 64 to 512 |
| `interpolation` | nearest | `bilinear` for smooth resampling |
| `mask` | | WKT polygon in WGS84; pixels outside it are transparent |
| `geometry_provider`, `geometry_id` | | Clip to geometry put in with `geometries` (below) instead of a `mask` |

Colormaps: `viridis`, `plasma`, `turbo`, `greys` (or `gray`), `rdylbu`,
`spectral`, `reds`, `blues`, `greens`, `ylgnbu` and `hot`.

### Clip geometry by name

A `mask` travels with every tile: copied to the worker, into wasm, and
compared against the last one, each time. For a block that is nothing; for a
farm of them it is tens of kilobytes per tile. Send the geometry once instead,
and have tiles name it:

```js
import { geometries } from '@sabremaps/browser';

await geometries.set('blocks', { 19519: twkbBytes, 19520: geojson, 19521: 'POLYGON((…))' });

raster.tile(z, x, y, { colormap: 'rdylgn', geometry_provider: 'blocks', geometry_id: [19519, 19520] });
```

`set()` adds or overwrites the ids it names and leaves the rest;
`replace()` swaps the provider's whole contents in one step; `delete()`
removes ids. Geometry is WGS84, as TWKB bytes, WKT, or GeoJSON. Several ids
clip to their union, and an id that is not there is an error rather than a
tile drawn without it.

A change does not redraw tiles already on the map. Each one resolves to the
provider's new revision and calls `geometries.subscribe()` listeners, which
is the signal to redraw layers that name it. The names are those of a sabre
server's geometry providers, so the same style works against either.

## How it runs

One Web Worker per page holds the wasm (about 190 KB brotli) and one cache per
raster URL. Tiles come back as transferred `ImageBitmap`s, so nothing is copied
and the main thread never decodes. Bundlers find the worker and the wasm
through `new URL(…, import.meta.url)` with no configuration. That is tested
with Vite 8, in dev and production builds.

The raster's host must allow CORS `GET` with the `Range` header, exposing
`Content-Range`.

## License

[Apache License 2.0](./LICENSE.md), included in the package.
