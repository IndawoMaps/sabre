import { type Endpoint, getEndpoint } from "./server";
import { query, type Style } from "./style";

/**
 * A raster to read: a file name inside the file root (`"dem.tif"`), a
 * `file://` URI of a file inside it -- as expo-file-system gives -- or an
 * `https://` URL.
 */
export type Source = string;

/** The URL sabre's server takes for `source`. */
export function sourceUrl(endpoint: Endpoint, source: Source): string {
  if (source.startsWith("http://") || source.startsWith("https://")) return source;
  if (source.startsWith("file://")) {
    // The server resolves file:// paths inside the file root, so an
    // absolute URI of a file in it becomes relative to it.
    const path = decodeURIComponent(source.slice("file://".length));
    const root = endpoint.fileRoot.replace(/\/+$/, "");
    if (path.startsWith(root + "/")) return `file://${path.slice(root.length + 1)}`;
    return source;
  }
  return `file://${source}`;
}

/** A MapLibre tile URL template for `source` drawn in `style`. */
export function tileUrl(endpoint: Endpoint, source: Source, style: Style = {}, tileSize = 256): string {
  return `${endpoint.baseUrl}/tiles/{z}/{x}/{y}?${query({
    url: sourceUrl(endpoint, source), tile_size: tileSize, ...style,
  })}`;
}

/** What `info()` reports about a raster. */
export interface RasterInfo {
  width: number;
  height: number;
  bands: number;
  dtype: string;
  nodata: number | null;
  compression: number;
  epsg: number | null;
  overview_count: number;
  /** WGS84. */
  extent: { west: number; south: number; east: number; north: number } | null;
  /** WGS84 `[lng, lat]`. */
  center: [number, number] | null;
  /** From the file's statistics metadata, when it has them. */
  stats_min: number | null;
  stats_max: number | null;
}

async function get<T>(route: string, params: Record<string, unknown>): Promise<T> {
  const ep = await getEndpoint();
  const res = await fetch(`${ep.baseUrl}/${route}?${query({ ...params, url: sourceUrl(ep, params.url as string) })}`);
  const body = await res.text();
  if (!res.ok) throw new Error(`sabre ${route}: ${body}`);
  return JSON.parse(body) as T;
}

/** Read a raster's header: size, type, extent, statistics. */
export function info(source: Source): Promise<RasterInfo> {
  return get("info", { url: source });
}

export type QueryResult =
  | { kind: "point"; value: number | null }
  | { kind: "polygon"; min: number; max: number; avg: number; stdev: number };

/** The value at a WGS84 point, or statistics inside a WKT polygon. */
export function queryRaster(
  source: Source,
  at: { lng: number; lat: number } | { polygon: string },
  options: { band?: number; nodata?: number } = {},
): Promise<QueryResult> {
  return get("query", { url: source, ...at, ...options });
}
