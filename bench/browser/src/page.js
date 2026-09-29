// One engine drawing one COG, driven step by step by run.ts.
//
//   ?engine=sabre|ol&url=<COG>&center=[lon,lat]&zoom=<z>&focus=[lon,lat]
//
// Both engines draw the same thing: band 1 through viridis over 0..max,
// nodata transparent, nearest-neighbour, onto a Web Mercator view with no
// basemap. `sabre` is @sabremaps/ol on a canvas TileLayer; `ol` is
// OpenLayers' own GeoTIFF source (geotiff.js, decoding in its worker pool) on
// a WebGLTile layer, reprojected from the COG's UTM zone by OpenLayers.

import Map from 'ol/Map.js';
import View from 'ol/View.js';
import { fromLonLat } from 'ol/proj.js';

const q = new URLSearchParams(location.search);
const engine = q.get('engine');
const url = q.get('url');
const MAX = 3000;

const marks = { module: performance.now() };
const errors = [];

// Main-thread blocking, per step: the part of each long task past 50 ms.
let blocking = 0;
try {
  new PerformanceObserver((list) => {
    for (const e of list.getEntries()) blocking += Math.max(0, e.duration - 50);
  }).observe({ type: 'longtask', buffered: true });
} catch { /* not in this browser */ }

function rendered(map) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('no rendercomplete within 60 s')), 60_000);
    map.once('rendercomplete', () => {
      clearTimeout(timer);
      resolve(performance.now());
    });
    map.render();
  });
}

// Each engine is its own chunk, so a page downloads only the one it uses.
const engines = { sabre: () => import('./sabre.js'), ol: () => import('./ol.js') };
if (!engines[engine]) throw new Error(`unknown engine ${engine}`);
const { layer, restyle } = await (await engines[engine]()).makeLayer(url, MAX, errors);
marks.source = performance.now();

// The first view is the whole scene. Steps after it are placed from `focus`,
// a point well inside the scene's data, in viewport widths and heights at
// that step's zoom, so both engines see exactly the same sequence.
const focus = fromLonLat(JSON.parse(q.get('focus')));
const view = new View({
  center: fromLonLat(JSON.parse(q.get('center'))),
  zoom: Number(q.get('zoom')),
  constrainResolution: false,
});
const map = new Map({ target: 'map', layers: [layer], view, controls: [], interactions: [] });
marks.first = await rendered(map);
await new Promise((r) => requestAnimationFrame(() => setTimeout(r, 0)));
marks.blocking = blocking;

function webgl() {
  try {
    const gl = document.createElement('canvas').getContext('webgl2');
    const ext = gl.getExtension('WEBGL_debug_renderer_info');
    return ext ? gl.getParameter(ext.UNMASKED_RENDERER_WEBGL) : gl.getParameter(gl.RENDERER);
  } catch {
    return null;
  }
}

window.bench = {
  engine,
  marks,
  renderer: webgl(),
  /** Apply one step and resolve when the map has finished drawing it. */
  async step(s) {
    blocking = 0;
    const errorsBefore = errors.length;
    const t0 = performance.now();
    if (s.restyle !== undefined) {
      restyle(s.restyle);
    } else {
      const [w, h] = map.getSize();
      const res = view.getResolutionForZoom(s.zoom);
      view.setCenter([focus[0] + s.dx * w * res, focus[1] + s.dy * h * res]);
      view.setZoom(s.zoom);
    }
    const t1 = await rendered(map);
    // Long tasks are reported after they end; give the last one a frame.
    await new Promise((r) => requestAnimationFrame(() => setTimeout(r, 0)));
    return { ms: t1 - t0, blocking, errors: errors.length - errorsBefore };
  },
};
window.benchReady = true;
