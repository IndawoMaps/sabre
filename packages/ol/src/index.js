// @sabremaps/ol -- an OpenLayers source for a Cloud-Optimized GeoTIFF, rendered
// in the browser by sabre.
//
//   import TileLayer from 'ol/layer/Tile.js';
//   import { sabreSource } from '@sabremaps/ol';
//
//   map.addLayer(new TileLayer({
//     source: await sabreSource('https://…/farm.tif', {
//       style: { colormap: 'viridis', min: 0, max: 3000 },
//     }),
//   }));

import DataTile from 'ol/source/DataTile.js';
import TileGrid from 'ol/tilegrid/TileGrid.js';
import { transformExtent } from 'ol/proj.js';
import { open } from '@sabremaps/browser';

export { configure } from '@sabremaps/browser';

/** Half the width of the Web Mercator world, in metres. */
const HALF = 20037508.342789244;
/** Where Web Mercator stops. */
const MAX_LAT = 85.0511287798066;

/** Open a COG and make an OpenLayers source of it. */
export async function sabreSource(url, options = {}) {
  return new SabreSource(await open(url), options);
}

/**
 * Tiles of one raster, drawn in a Web Worker.
 *
 * A DataTile source whose tiles are ImageBitmaps rather than arrays, which is
 * what lets the plain canvas `ol/layer/Tile` draw it. Only tiles over the
 * raster are ever requested, and past the raster's own resolution OpenLayers
 * stretches the tiles it has rather than asking for more.
 */
export class SabreSource extends DataTile {
  #raster;
  #style;

  /** @param {import('@sabremaps/browser').Raster} raster */
  constructor(raster, options = {}) {
    const { style = {}, maxZoom, ...rest } = options;
    const tileSize = style.tile_size ?? 256;
    const tileGrid = gridFor(raster.info, tileSize, maxZoom);
    super({
      projection: 'EPSG:3857',
      tileGrid,
      wrapX: false,
      ...rest,
    });
    this.#raster = raster;
    this.#style = { ...style };
    this.setKey(JSON.stringify(this.#style));
    this.setLoader((z, x, y, { signal }) => this.#raster.tile(z, x, y, this.#style, { signal }));
  }

  /** The raster's own description: size, bands, EPSG, WGS84 extent, native zoom. */
  getInfo() {
    return this.#raster.info;
  }

  /** The WGS84 extent transformed to `projection`, for `view.fit()`. */
  getExtent(projection = 'EPSG:3857') {
    const e = this.#raster.info.extent;
    return e ? transformExtent(clampLat(e), 'EPSG:4326', projection) : undefined;
  }

  getStyle() {
    return { ...this.#style };
  }

  /**
   * Restyle. No bytes are read again: the raster is cached in the worker,
   * only the drawing is redone.
   *
   * By default every tile in the old style is dropped at once, so the layer
   * is briefly empty rather than a patchwork of two styles. With
   * `{ keepStale: true }` old tiles stay until their replacements arrive,
   * which suits small changes made continuously, like dragging `max`.
   */
  setStyle(style, { keepStale = false } = {}) {
    if ((style.tile_size ?? 256) !== (this.#style.tile_size ?? 256)) {
      throw new Error('@sabremaps/ol: tile_size is fixed when the source is made');
    }
    this.#style = { ...style };
    if (keepStale) {
      // A new key: OpenLayers keeps tiles under the old one as placeholders,
      // at this zoom and others, until new ones load.
      this.setKey(JSON.stringify(this.#style));
    } else {
      // Same key, new revision: the tile layer renderer clears its cache.
      this.changed();
    }
  }

  /** Release the raster. The source cannot draw after this. */
  disposeInternal() {
    this.#raster.close();
    super.disposeInternal();
  }
}

/**
 * The standard XYZ grid, cut to the raster.
 *
 * The origin and resolutions are exactly XYZ's, so OpenLayers' tile
 * coordinates are the z/x/y sabre renders; only the extent is narrowed, which
 * is what stops requests for tiles with nothing in them.
 */
function gridFor(info, tileSize, maxZoom) {
  const top = maxZoom ?? (info.native_zoom == null ? 22 : Math.ceil(info.native_zoom));
  const resolutions = [];
  for (let z = 0; z <= Math.min(Math.max(top, 0), 30); z++) {
    resolutions.push((2 * HALF) / (tileSize * 2 ** z));
  }
  const extent = info.extent
    ? transformExtent(clampLat(info.extent), 'EPSG:4326', 'EPSG:3857')
    : [-HALF, -HALF, HALF, HALF];
  return new TileGrid({ origin: [-HALF, HALF], extent, resolutions, tileSize });
}

function clampLat([w, s, e, n]) {
  return [w, Math.max(s, -MAX_LAT), e, Math.min(n, MAX_LAT)];
}
