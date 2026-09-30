/**
 * How to draw a tile. The names and defaults are sabre's HTTP API's, so a
 * style that works as a tile URL's query string works here unchanged.
 */
export interface Style {
  mode?: 'colormap' | 'rgb' | 'hillshade' | 'contour' | 'classified';
  /** Colormap for `colormap` and `contour`. Default `viridis`. */
  colormap?: string;
  /** Value range mapped across the ramp. Default 0 to 255. */
  min?: number;
  max?: number;
  /** Override the raster's nodata; matching pixels are transparent. */
  nodata?: number;
  /** `contour`: number of bands between `min` and `max`. Default 10. */
  contour_level?: number;
  /** `rgb`: per-channel black and white points. */
  rgb_min_r?: number;
  rgb_min_g?: number;
  rgb_min_b?: number;
  rgb_max_r?: number;
  rgb_max_g?: number;
  rgb_max_b?: number;
  /** `hillshade`: light direction and elevation in degrees, and exaggeration. */
  azimuth?: number;
  altitude?: number;
  z_factor?: number;
  /** `hillshade`: a ramp blended under the shading. */
  hillshade_colormap?: string;
  /** `classified`: `[threshold, color]` pairs; color is `"#rrggbb"` or `[r, g, b]`. */
  stops?: Array<[number, string | [number, number, number]]>;
  /** Output size in pixels, 64 to 512. Default 256. */
  tile_size?: number;
  /** `bilinear` for smooth resampling; nearest otherwise. */
  interpolation?: 'nearest' | 'bilinear';
  /** WKT polygon or multipolygon in WGS84; pixels outside it are transparent. */
  mask?: string;
  /**
   * Clip to geometry put in with `geometries.set()` instead of sending a
   * `mask`: the provider it was put under, and one or more of its ids.
   * Several ids clip to their union. Not with `mask`.
   */
  geometry_provider?: string;
  geometry_id?: string | number | Array<string | number>;
}

/**
 * WGS84 clip geometry: TWKB bytes, WKT, or GeoJSON (a Polygon, MultiPolygon,
 * Feature or FeatureCollection) as text or as an object.
 */
export type Geometry = ArrayBuffer | ArrayBufferView | string | object;

/** Geometry by id: `{ id: geometry }`, a `Map`, or `[id, geometry]` pairs. */
export type GeometryEntries =
  | Record<string, Geometry>
  | Map<string | number, Geometry>
  | Iterable<[string | number, Geometry]>;

/**
 * Clip geometry, sent to the worker once and named by tiles with
 * `geometry_provider` and `geometry_id`. Each change resolves to the
 * provider's new revision. Tiles already drawn are not redrawn: subscribe,
 * and redraw layers that name the provider.
 */
export const geometries: {
  /** Add these, or overwrite them if the ids exist. Others are left alone. */
  set(provider: string, entries: GeometryEntries): Promise<number>;
  /** Make these the provider's whole contents, in one step. `{}` empties it. */
  replace(provider: string, entries: GeometryEntries): Promise<number>;
  /** Remove these ids. Ids the provider does not have are ignored. */
  delete(provider: string, ids: Iterable<string | number>): Promise<number>;
  /** The provider's revision as of the last change that finished; 0 before any. */
  revision(provider: string): number;
  /** Called after each change. Returns an unsubscribe function. */
  subscribe(listener: (provider: string, revision: number) => void): () => void;
};

export interface RasterInfo {
  width: number;
  height: number;
  bands: number;
  overviews: number;
  epsg: number | null;
  compression: number;
  predictor: number;
  nodata: number | null;
  /** WGS84 `[west, south, east, north]`. */
  extent: [number, number, number, number] | null;
  /** The Web Mercator zoom whose pixels match the raster's own. */
  native_zoom: number | null;
}

export interface Options {
  /** Where to load `sabre_browser_bg.wasm` from, if not beside the package. */
  wasmUrl?: string | URL;
  /** Tiles rendering or fetching at once. Default 6. */
  concurrency?: number;
}

/** Change where the wasm comes from, or concurrency. Only before the first `open()`. */
export function configure(options: Options): void;

/** Open a raster by URL. Resolves once its header has been read. */
export function open(url: string | URL): Promise<Raster>;

export class Raster {
  readonly url: string;
  readonly info: RasterInfo;
  /** Render a z/x/y tile. The caller owns the ImageBitmap. */
  tile(z: number, x: number, y: number, style?: Style | string, options?: { signal?: AbortSignal }): Promise<ImageBitmap>;
  /** As `tile()`, but the RGBA pixels. */
  pixels(z: number, x: number, y: number, style?: Style | string, options?: { signal?: AbortSignal }): Promise<{ data: Uint8ClampedArray; size: number }>;
  /** Free the raster's cache once no other `open()` of the same URL holds it. */
  close(): void;
}
