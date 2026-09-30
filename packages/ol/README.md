# @sabremaps/ol

An OpenLayers source for Cloud-Optimized GeoTIFFs, rendered in the browser by
sabre. No tile server: the page reads
the byte ranges it needs straight from object storage and draws tiles in a
Web Worker.

```bash
npm install @sabremaps/ol ol
```

```js
import Map from 'ol/Map.js';
import View from 'ol/View.js';
import TileLayer from 'ol/layer/Tile.js';
import { sabreSource } from '@sabremaps/ol';

const source = await sabreSource('https://data.example.com/farm.tif', {
  style: { colormap: 'viridis', min: 0, max: 3000 },
});

const map = new Map({
  target: 'map',
  layers: [new TileLayer({ source })],
  view: new View(),
});
map.getView().fit(source.getExtent());
```

## Styles

`style` picks one of five modes. Every field and its default is listed in the
[`@sabremaps/browser` README](https://www.npmjs.com/package/@sabremaps/browser#api).

```js
{ mode: 'colormap', colormap: 'viridis', min: 0, max: 3000 }
{ mode: 'hillshade', azimuth: 315, altitude: 45, z_factor: 1, hillshade_colormap: 'greys' }
{ mode: 'contour', colormap: 'spectral', min: 0, max: 3000, contour_level: 8 }
{ mode: 'classified', stops: [[0, '#2166ac'], [500, '#f7f7f7'], [2000, [178, 24, 43]]] }
{ mode: 'rgb', rgb_max_r: 3000, rgb_max_g: 3000, rgb_max_b: 3000 }   // three-band rasters
```

Also accepted: `nodata`, `interpolation: 'bilinear'`, `tile_size` (64 to 512),
and `mask`, a WKT polygon in WGS84 outside which pixels are transparent.

For geometry that is large or shared between layers, put it in once with
`geometries.set()` and clip by name instead of carrying WKT with every tile:

```js
import { geometries, sabreSource } from '@sabremaps/ol';

await geometries.set('blocks', { 19519: blockGeoJson, 19520: blockTwkb });
const source = await sabreSource(url, {
  style: { colormap: 'rdylgn', geometry_provider: 'blocks', geometry_id: [19519, 19520] },
});
```

A source redraws on its own when geometry under its `geometry_provider`
changes. See `@sabremaps/browser` for `set`, `replace` and `delete`.

Restyle with `source.setStyle(style)`. Nothing is downloaded again: the
raster's bytes are cached in the worker, only the drawing is redone. Tiles in
the old style are dropped at once, so the layer never shows two styles side by
side. For small, continuous changes, like a slider on `max`, pass
`{ keepStale: true }` to keep old tiles on screen until their replacements
arrive.

## What it does for you

- **Draws off the main thread.** Decoding is nearly all of a tile's cost, and
  in a worker a pan never waits on it.
- **Only asks for tiles over the raster**, and past the raster's own resolution
  lets OpenLayers stretch the tiles it has instead of rendering more.
- **Drops tiles you panned away from** before rendering them.
- **Reprojects** WGS84, Web Mercator and UTM rasters into the map.
- **Shares one cache per URL**, so two layers over the same raster read it once.

Tiles are `ImageBitmap`s, so the source works under the plain canvas
`ol/layer/Tile`.

## API

`sabreSource(url, options)` resolves to a `SabreSource` once the raster's
header has been read. `options` takes `style`, `maxZoom`, and the usual
`DataTile` options such as `attributions`, `transition` and `interpolate`.

`SabreSource` adds:

| | |
| --- | --- |
| `getInfo()` | Size, bands, EPSG code, WGS84 extent, native zoom |
| `getExtent(projection?)` | The raster's extent, EPSG:3857 by default |
| `getStyle()` / `setStyle(style, { keepStale }?)` | Read or change the style |

`configure({ wasmUrl, concurrency })`, re-exported from `@sabremaps/browser`,
changes where the wasm is loaded from or how many tiles are in flight. Call it
before the first `sabreSource()`.

## Requirements

The raster must be a Cloud-Optimized GeoTIFF, and its host must allow CORS
`GET` with the `Range` header, exposing `Content-Range`. The browser blocks the
read otherwise.

Tested with OpenLayers 10.9 and Vite 8, in dev and production builds. Other
bundlers that follow the `new Worker(new URL(…, import.meta.url))` convention
(webpack 5, Rollup, esbuild, Parcel) should work, but are not yet tested.

## License

[Apache License 2.0](./LICENSE.md), included in the package.
