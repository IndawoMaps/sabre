import { requireNativeModule } from "expo";

/** Where the in-app tile server is listening. */
export interface Endpoint {
  port: number;
  token: string;
  /** `http://127.0.0.1:{port}/{token}` -- the prefix of every route. */
  baseUrl: string;
  /** The directory `file://` sources resolve inside. */
  fileRoot: string;
}

interface SabreNativeModule {
  start(fileRoot: string | null, cacheBytes: number): Promise<Endpoint>;
  stop(): void;
  listRasters(fileRoot: string | null): string[];
}

const Sabre = requireNativeModule<SabreNativeModule>("Sabre");

/**
 * Start the tile server, or get the one already running. `fileRoot` defaults
 * to the app's private files directory (Android) or Documents (iOS).
 */
export function start(options: { fileRoot?: string; cacheBytes?: number } = {}): Promise<Endpoint> {
  return Sabre.start(options.fileRoot ?? null, options.cacheBytes ?? 32 * 1024 * 1024);
}

export function stop(): void {
  Sabre.stop();
}

/** GeoTIFF file names directly inside the file root. */
export function listRasters(fileRoot?: string): string[] {
  return Sabre.listRasters(fileRoot ?? null);
}

/**
 * Style parameters, named as the HTTP API names them (crates/core/src/params.rs).
 * Anything left out takes the server's default.
 */
export type Style =
  | { mode: "colormap"; colormap?: string; min?: number; max?: number; nodata?: number }
  | { mode: "hillshade"; hillshade_colormap?: string; min?: number; max?: number;
      z_factor?: number; azimuth?: number; altitude?: number; nodata?: number }
  | { mode: "contour"; colormap?: string; min?: number; max?: number; contour_level?: number; nodata?: number }
  | { mode: "classified"; stops: [number, string][]; nodata?: number }
  | { mode: "rgb"; [param: string]: string | number | undefined };

function query(params: Record<string, unknown>): string {
  return Object.entries(params)
    .filter(([, v]) => v !== undefined && v !== null)
    .map(([k, v]) => `${encodeURIComponent(k)}=${encodeURIComponent(typeof v === "object" ? JSON.stringify(v) : String(v))}`)
    .join("&");
}

/** A MapLibre tile URL template for `file` (a name inside the file root) in `style`. */
export function tileUrl(endpoint: Endpoint, file: string, style: Style): string {
  return `${endpoint.baseUrl}/tiles/{z}/{x}/{y}?${query({ url: `file://${file}`, ...style })}`;
}

/** The raster's `/info`: size, type, extent, statistics. */
export async function info(endpoint: Endpoint, file: string): Promise<RasterInfo> {
  const res = await fetch(`${endpoint.baseUrl}/info?${query({ url: `file://${file}` })}`);
  if (!res.ok) throw new Error(`${res.status} ${await res.text()}`);
  return res.json();
}

export interface RasterInfo {
  width: number;
  height: number;
  bands: number;
  dtype: string;
  nodata: number | null;
  extent: { west: number; south: number; east: number; north: number };
  center: [number, number];
  stats_min: number | null;
  stats_max: number | null;
}
