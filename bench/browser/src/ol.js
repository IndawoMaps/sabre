// OpenLayers' own: the GeoTIFF source (geotiff.js, decoding in its worker
// pool) on a WebGLTile layer, reprojected from the COG's UTM zone by
// OpenLayers. Configured as its docs show, with only what parity needs:
// raw values rather than normalised, and no interpolation.

import WebGLTileLayer from 'ol/layer/WebGLTile.js';
import GeoTIFF from 'ol/source/GeoTIFF.js';
import { register } from 'ol/proj/proj4.js';
import proj4 from 'proj4';

// sabre carries its own projections; OpenLayers needs the scenes' UTM zones
// registered, or it cannot place the GeoTIFF at all.
for (const [code, zone] of [[32734, 34], [32735, 35], [32750, 50]]) {
  proj4.defs(`EPSG:${code}`, `+proj=utm +zone=${zone} +south +datum=WGS84 +units=m +no_defs`);
}
register(proj4);

// sabre's viridis, every 17th of its 256 entries.
const VIRIDIS = [
  [68, 1, 84], [72, 26, 108], [71, 47, 125], [65, 68, 135], [57, 86, 140], [49, 104, 142],
  [42, 120, 142], [35, 136, 142], [31, 152, 139], [34, 168, 132], [53, 183, 121], [84, 197, 104],
  [122, 209, 81], [165, 219, 54], [210, 226, 27], [253, 231, 37],
];

export async function makeLayer(url, max, errors) {
  const source = new GeoTIFF({ sources: [{ url }], normalize: false, interpolate: false });
  source.on('tileloaderror', () => errors.push('tile'));
  await source.getView();
  const stops = VIRIDIS.flatMap((c, i) => [i / (VIRIDIS.length - 1), c]);
  const layer = new WebGLTileLayer({
    source,
    style: {
      variables: { max },
      color: [
        'case',
        // Band 2 is the alpha OpenLayers adds for the COG's nodata.
        ['==', ['band', 2], 0], [0, 0, 0, 0],
        ['interpolate', ['linear'], ['clamp', ['/', ['band', 1], ['var', 'max']], 0, 1], ...stops],
      ],
    },
  });
  return { layer, restyle: (max) => layer.updateStyleVariables({ max }) };
}
