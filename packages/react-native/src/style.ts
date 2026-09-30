/**
 * How to draw a raster. The names and defaults are sabre's HTTP API's, and
 * the same as `@sabremaps/browser`'s, so a style works unchanged in each.
 */
export interface Style {
  mode?: "colormap" | "rgb" | "hillshade" | "contour" | "classified";
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
  /** `bilinear` for smooth resampling; nearest otherwise. */
  interpolation?: "nearest" | "bilinear";
  /** WKT polygon or multipolygon in WGS84; pixels outside it are transparent. */
  mask?: string;
  /**
   * Clip to geometry put in with `geometries.set()` instead of a `mask`: the
   * provider it was put under, and one or more of its ids. Several ids clip
   * to their union. Not with `mask`.
   */
  geometry_provider?: string;
  geometry_id?: GeometryId;
}

/** One geometry id, or several. */
export type GeometryId = string | number | Array<string | number>;

export function query(params: Record<string, unknown>): string {
  // Several geometry ids go as the comma-separated list the server takes.
  const ids = params.geometry_id;
  if (Array.isArray(ids)) params = { ...params, geometry_id: ids.join(",") };
  return Object.entries(params)
    .filter(([, v]) => v !== undefined && v !== null)
    .map(([k, v]) => `${encodeURIComponent(k)}=${encodeURIComponent(typeof v === "object" ? JSON.stringify(v) : String(v))}`)
    .join("&");
}
