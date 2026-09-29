# @sabremaps/react-native

Cloud-Optimized GeoTIFFs on a [MapLibre React Native](https://github.com/maplibre/maplibre-react-native)
map, from files on the device, restyled at runtime. No network, no pre-rendered
tiles: sabre's tile server runs inside the app and renders each tile from the
raster as MapLibre asks for it.

> **Android only.** iOS is not supported yet: on iOS the native module is
> absent, and the component reports that through `onError`.

```bash
npx expo install @sabremaps/react-native @maplibre/maplibre-react-native
```

Add the config plugin to `app.json`, then rebuild the app (`npx expo run:android`
or a new dev client; Expo Go cannot load native code):

```json
{ "expo": { "plugins": ["@maplibre/maplibre-react-native", "@sabremaps/react-native"] } }
```

The plugin lets the app talk plain HTTP to `127.0.0.1`, and to nowhere else, so
MapLibre can reach the in-app server. Bare React Native apps need
[Expo Modules](https://docs.expo.dev/bare/installing-expo-modules/) installed.

```tsx
import { Camera, Map } from "@maplibre/maplibre-react-native";
import { SabreRasterSource, useRasterInfo } from "@sabremaps/react-native";

function DemMap() {
  const { info } = useRasterInfo("dem.tif");
  const [colormap, setColormap] = useState("viridis");
  if (!info?.extent) return null;
  const { west, south, east, north } = info.extent;

  return (
    <Map mapStyle={offlineStyle}>
      <Camera initialViewState={{ bounds: [west, south, east, north] }} />
      <SabreRasterSource
        id="dem"
        source="dem.tif"
        style={{ mode: "colormap", colormap, min: info.stats_min ?? 0, max: info.stats_max ?? 3000 }}
      />
    </Map>
  );
}
```

## Where rasters come from

Local rasters are read from one directory, the **file root**: the app's files
directory by default (`Paths.document` in expo-file-system). Put GeoTIFFs there,
by downloading them, copying them out of the app bundle, or from a document
picker, and name them by file name:

```tsx
import { File, Paths } from "expo-file-system";
await File.downloadFileAsync("https://example.com/dem.tif", new File(Paths.document, "dem.tif"));

<SabreRasterSource id="dem" source="dem.tif" />
```

A `source` can be:

- a file name in the file root: `"dem.tif"`, `"surveys/2026/dem.tif"`;
- a `file://` URI of a file inside the file root, as expo-file-system gives;
- an `https://` URL, read with range requests and cached in memory.

Anything outside the file root is refused. Cloud-Optimized GeoTIFFs (tiled,
with overviews) render fastest at every zoom; a plain striped GeoTIFF works but
reads more at low zooms. Compression: none, LZW, Deflate or PackBits. JPEG,
ZSTD, LERC and WebP tiles are not decoded yet, so convert with
`gdal_translate -co COMPRESS=DEFLATE -co PREDICTOR=2`.

## Styles

`style` takes sabre's style parameters, the same names and defaults as its HTTP
API and [`@sabremaps/browser`](https://www.npmjs.com/package/@sabremaps/browser):

| Field | Default | |
| --- | --- | --- |
| `mode` | `colormap` | `colormap`, `rgb`, `hillshade`, `contour` or `classified` |
| `colormap` | `viridis` | `viridis`, `plasma`, `turbo`, `greys`, `rdylbu`, `spectral`, `reds`, `blues`, `greens`, `ylgnbu`, `hot` |
| `min`, `max` | `0`, `255` | Value range mapped across the ramp |
| `nodata` | from the file | Values drawn transparent |
| `stops` | | `classified`: `[[threshold, "#rrggbb" or [r, g, b]], …]` |
| `azimuth`, `altitude`, `z_factor` | `315`, `45`, `1` | `hillshade`: light direction, elevation, exaggeration (elevations in metres) |
| `hillshade_colormap` | | `hillshade`: a ramp blended under the shading |
| `contour_level` | `10` | `contour`: bands between `min` and `max` |
| `rgb_min_r` … `rgb_max_b` | `0` … `255` | `rgb`: per-channel black and white points |
| `interpolation` | nearest | `bilinear` for smooth resampling |
| `mask` | | WKT polygon in WGS84; pixels outside it are transparent |

Changing `style` redraws the visible tiles. MapLibre builds a raster source once
and ignores later changes to its tiles, so the component remounts the source
under the same ids; layers placed relative to it with `beforeId` stay put.

## API

**`<SabreRasterSource>`** is a MapLibre raster source and, unless you pass
`children`, a raster layer `${id}-layer` drawing it.

| Prop | |
| --- | --- |
| `id` | Source id |
| `source` | File name, `file://` URI or `https://` URL |
| `style` | How to draw it |
| `tileSize` | Tile size in pixels, default 256 |
| `minzoom`, `maxzoom`, `attribution` | As on MapLibre's `RasterSource` |
| `paint`, `beforeId`, `afterId`, `layerIndex` | For the default layer |
| `children` | Your own layers instead of the default one |
| `onError` | The server could not start |

**`useRasterInfo(source)`** gives `{ info, error, loading }`, where `info` has the
raster's size, data type, nodata, EPSG code, WGS84 `extent` and `center`, and
`stats_min`/`stats_max` when the file carries statistics.

**`info(source)`** is the same as a promise. **`queryRaster(source, { lng, lat })`**
reads the value at a point, and **`queryRaster(source, { polygon })`** gives
min, max, mean and standard deviation inside a WKT polygon.

**`listRasters()`** lists the GeoTIFFs directly inside the file root.

**`configure({ fileRoot, cacheBytes })`** changes the file root and the memory
cache for `https://` sources (default 64 MB). Call it before anything else.

**`useEndpoint()`**, **`getEndpoint()`** and **`tileUrl(endpoint, source, style)`**
are for building your own MapLibre sources. The endpoint can change: when the app
returns to the foreground the server is checked, and restarted on a new port if
the system closed it. `useEndpoint()` re-renders when that happens.

## How it works

`libsabre_mobile.so` (about 4 MB on arm64) holds sabre's tile server. On first
use it listens on `127.0.0.1` on a random port, under a random per-launch token
that other apps on the device cannot guess, and MapLibre fetches tiles from it
like any other tile URL. Tiles are sent `Cache-Control: no-store`, so MapLibre's
disk cache does not fill with copies of tiles the device can render again.

## License

[Apache License 2.0](./LICENSE.md), included in the package.
