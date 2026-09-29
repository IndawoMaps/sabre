// The whole integration: an OSM basemap, and a COG rendered in the browser.
// No sabre-server running -- the tiles are drawn in a Web Worker from range
// requests straight to object storage.

import Map from 'ol/Map.js';
import View from 'ol/View.js';
import TileLayer from 'ol/layer/Tile.js';
import OSM from 'ol/source/OSM.js';
import 'ol/ol.css';
import { sabreSource, type Style } from '@sabremaps/ol';

const SCENE = 'https://sabre-bench-tf.nyc3.digitaloceanspaces.com/scenes/S2A_35HLD_20260805_0_L2A_red.tif';

const STYLES: Record<string, Style> = {
  colormap:   { mode: 'colormap', colormap: 'viridis', min: 0, max: 3000 },
  hillshade:  { mode: 'hillshade', max: 3000, z_factor: 0.05, hillshade_colormap: 'greys' },
  contour:    { mode: 'contour', colormap: 'spectral', min: 0, max: 3000, contour_level: 8 },
  classified: { mode: 'classified', stops: [[0, '#2166ac'], [800, '#f7f7f7'], [1600, '#b2182b']] },
};

const status = document.getElementById('status')!;
const state = { loaded: 0, errors: [] as string[], ready: false };
(window as any).__sabre = state;

const source = await sabreSource(SCENE, { style: STYLES.colormap });
source.on('tileloadend', () => { state.loaded++; status.textContent = `${state.loaded} tiles drawn`; });
source.on('tileloaderror', () => { state.errors.push('tile'); status.textContent = `${state.errors.length} tile errors`; });

const map = new Map({
  target: 'map',
  layers: [new TileLayer({ source: new OSM() }), new TileLayer({ source, opacity: 0.85 })],
  view: new View(),
});
map.getView().fit(source.getExtent()!, { padding: [20, 20, 20, 20] });

document.getElementById('mode')!.addEventListener('change', (e) => {
  source.setStyle(STYLES[(e.target as HTMLSelectElement).value]);
});

const info = source.getInfo();
status.textContent = `${info.width}×${info.height}, EPSG:${info.epsg}, native z${info.native_zoom?.toFixed(1)}`;
state.ready = true;
