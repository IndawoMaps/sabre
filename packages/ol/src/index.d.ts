import DataTile, { type Options as DataTileOptions } from 'ol/source/DataTile.js';
import type { ProjectionLike } from 'ol/proj.js';
import type { Extent } from 'ol/extent.js';
import type { Raster, RasterInfo, Style } from '@sabremaps/browser';

export type { Style, RasterInfo, Geometry, GeometryEntries } from '@sabremaps/browser';
export { configure, geometries } from '@sabremaps/browser';

export interface Options extends Omit<DataTileOptions, 'loader' | 'tileGrid' | 'projection' | 'tileSize' | 'maxZoom' | 'minZoom'> {
  /** How to draw it. Names and defaults follow sabre's HTTP API. */
  style?: Style;
  /**
   * Highest zoom sabre renders; past it OpenLayers stretches existing tiles.
   * Default: the zoom that matches the raster's own resolution.
   */
  maxZoom?: number;
}

/** Open a COG and make an OpenLayers source of it. */
export function sabreSource(url: string | URL, options?: Options): Promise<SabreSource>;

export class SabreSource extends DataTile {
  constructor(raster: Raster, options?: Options);
  /** The raster's own description: size, bands, EPSG, WGS84 extent, native zoom. */
  getInfo(): RasterInfo;
  /** The raster's extent in `projection` (default EPSG:3857), for `view.fit()`. */
  getExtent(projection?: ProjectionLike): Extent | undefined;
  getStyle(): Style;
  /**
   * Restyle without reading the raster again. `tile_size` cannot change.
   * Old tiles are dropped at once unless `keepStale` is set, in which case
   * they stay until their replacements arrive.
   */
  setStyle(style: Style, options?: { keepStale?: boolean }): void;
}
