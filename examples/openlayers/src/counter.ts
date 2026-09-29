import TileLayer from 'ol/layer/Tile'
import VectorLayer from 'ol/layer/Vector'
import VectorSource from 'ol/source/Vector'
import { OSM, XYZ } from 'ol/source'
import Map from 'ol/Map'
import 'ol/ol.css'
import { View } from 'ol'
import Feature from 'ol/Feature'
import { Polygon } from 'ol/geom'
import { Style, Stroke, Fill } from 'ol/style'
import { fromLonLat, toLonLat } from 'ol/proj'
import GeoJSON from 'ol/format/GeoJSON'
import blocksGeojson from './blocks.json'

// ── Mask WKT ─────────────────────────────────────────────────────────────────

function ptLineDist(p: number[], a: number[], b: number[]): number {
  const dx = b[0] - a[0], dy = b[1] - a[1]
  const len2 = dx * dx + dy * dy
  if (len2 === 0) return Math.hypot(p[0] - a[0], p[1] - a[1])
  const t = ((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / len2
  return Math.hypot(p[0] - (a[0] + t * dx), p[1] - (a[1] + t * dy))
}

function dpSimplify(ring: number[][], tol: number): number[][] {
  if (ring.length <= 4) return ring
  let maxD = 0, maxI = 0
  for (let i = 1; i < ring.length - 1; i++) {
    const d = ptLineDist(ring[i], ring[0], ring[ring.length - 1])
    if (d > maxD) { maxD = d; maxI = i }
  }
  if (maxD > tol) {
    const l = dpSimplify(ring.slice(0, maxI + 1), tol)
    const r = dpSimplify(ring.slice(maxI), tol)
    return [...l.slice(0, -1), ...r]
  }
  return [ring[0], ring[ring.length - 1]]
}

function geojsonToMultipolygonWkt(geojson: any, tol = 0.0001): string {
  const polygons = (geojson.features as any[]).map(f => {
    const rings = (f.geometry.coordinates as number[][][]).map(ring => {
      const s = dpSimplify(ring, tol)
      const closed = (s[0][0] === s[s.length - 1][0] && s[0][1] === s[s.length - 1][1])
        ? s : [...s, s[0]]
      return '(' + closed.map(([x, y]) => `${x.toFixed(5)} ${y.toFixed(5)}`).join(',') + ')'
    })
    return '(' + rings.join(',') + ')'
  })
  return 'MULTIPOLYGON(' + polygons.join(',') + ')'
}

const MASK_WKT = geojsonToMultipolygonWkt(blocksGeojson)

const COG_URL   = 'http://localhost:8080/ca.cog.tiff';
const WORKER    = '';  // same origin via Vite proxy

const COMPRESSION: Record<number, string> = {
  1: 'uncompressed', 5: 'LZW', 6: 'JPEG', 7: 'JPEG',
  8: 'Deflate', 32773: 'PackBits', 32946: 'Deflate',
};

function lonLatToTile(lon: number, lat: number, z: number): [number, number] {
  const n = 2 ** z;
  const x = Math.floor((lon + 180) / 360 * n);
  const latR = lat * Math.PI / 180;
  const y = Math.floor((1 - Math.log(Math.tan(latR) + 1 / Math.cos(latR)) / Math.PI) / 2 * n);
  return [x, y];
}

function el(id: string): HTMLElement { return document.getElementById(id)!; }

export async function setup() {
  // ── Layers ────────────────────────────────────────────────────────────────
  const osmLayer = new TileLayer({ source: new OSM(), opacity: 1, zIndex: 0 });

  function buildTileUrl(): string {
    const cm     = (el('colormap')      as HTMLSelectElement).value;
    const interp = (el('interpolation') as HTMLSelectElement).value;
    const mode   = (el('mode')          as HTMLSelectElement).value;
    const levels = (el('contour-level') as HTMLInputElement).value;
    const min    = (el('min-val')       as HTMLInputElement).value;
    const max    = (el('max-val')       as HTMLInputElement).value;
    const p = new URLSearchParams({ colormap: cm, mode, url: COG_URL, interpolation: interp, min, max });
    if (mode === 'contour') p.set('contour_level', levels);
    const maskSuffix = (el('mask-toggle') as HTMLInputElement).checked
      ? `&mask=${encodeURIComponent(MASK_WKT)}`
      : '';
    return `${WORKER}/tiles/{z}/{x}/{y}?${p}${maskSuffix}`;
  }

  const tiffSource = new XYZ({ url: buildTileUrl(), transition: 0 });
  const sarLayer = new TileLayer({ source: tiffSource, zIndex: 10 });

  const extentSource = new VectorSource();
  const extentLayer = new VectorLayer({
    source: extentSource,
    style: new Style({
      stroke: new Stroke({ color: '#ffb055', width: 1.5, lineDash: [6, 3] }),
    }),
    zIndex: 20
  });

  const maskSource = new VectorSource({
    features: new GeoJSON().readFeatures(blocksGeojson, {
      dataProjection: 'EPSG:4326',
      featureProjection: 'EPSG:3857',
    }),
  });
  const maskLayer = new VectorLayer({
    source: maskSource,
    style: new Style({
      stroke: new Stroke({ color: '#e040fb', width: 1.5 }),
      fill: new Fill({ color: 'rgba(224, 64, 251, 0.08)' }),
    }),
    zIndex: 15,
  });

  // ── Map ───────────────────────────────────────────────────────────────────
  const map = new Map({
    target: document.getElementById('map')!,
    layers: [osmLayer, sarLayer, extentLayer, maskLayer],
    view: new View({ center: [0, 0], zoom: 3 }),
  });

  // ── Fetch COG info ────────────────────────────────────────────────────────
  let cogExtent: number[] | null = null;  // OL extent [minX, minY, maxX, maxY]

  try {
    const resp = await fetch(`${WORKER}/info?url=${encodeURIComponent(COG_URL)}`);
    if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
    const info = await resp.json();

    // Update metadata tags
    const epsgLabel = info.epsg ? `EPSG:${info.epsg}` : 'unknown CRS';
    const bands     = info.bands;
    const [, pw,, , yr] = info.geo_transform as number[];
    const gsd = Math.sqrt(pw ** 2 + yr ** 2);
    const gsdStr = gsd < 1 ? `${(gsd * 100).toFixed(0)} cm/px` : `${gsd.toFixed(2)} m/px`;
    const compName = COMPRESSION[info.compression] ?? `comp ${info.compression}`;

    el('crs-tag').textContent  = epsgLabel;
    el('dim-tag').textContent  = `${info.width} × ${info.height} · ${bands}b ${info.dtype}`;
    el('gsd-tag').textContent  = `~${gsdStr}`;
    el('comp-tag').textContent = `${compName} · ${info.overview_count} overviews`;

    if (info.stats_min != null && info.stats_max != null) {
      const fmt = (v: number) => Number.isInteger(v) ? String(v) : v.toPrecision(4);
      el('stats-tag').textContent = `min ${fmt(info.stats_min)} / max ${fmt(info.stats_max)}`;
      el('stats-tag').style.display = '';
      (el('min-val') as HTMLInputElement).value = String(info.stats_min);
      (el('max-val') as HTMLInputElement).value = String(info.stats_max);
      tiffSource.setUrl(buildTileUrl());
    }

    // Draw extent bbox
    if (info.extent) {
      const { west, south, east, north } = info.extent;
      const ring = [
        fromLonLat([west, south]),
        fromLonLat([east, south]),
        fromLonLat([east, north]),
        fromLonLat([west, north]),
        fromLonLat([west, south]),
      ];
      extentSource.addFeature(new Feature(new Polygon([ring])));
      cogExtent = extentSource.getExtent();
      map.getView().fit(cogExtent, { padding: [60, 60, 60, 280], maxZoom: 18, duration: 800 });
    }
  } catch (err) {
    el('crs-tag').textContent = `error: ${err}`;
  }

  // ── Tile debug on hover ───────────────────────────────────────────────────
  let lastTileKey = '';
  let debounceTimer: ReturnType<typeof setTimeout> | null = null;

  map.on('pointermove', (evt) => {
    if (evt.dragging) return;
    const [lon, lat] = toLonLat(evt.coordinate);
    const z  = Math.floor(map.getView().getZoom() ?? 0);
    const [x, y] = lonLatToTile(lon, lat, z);
    const tileKey = `${z}/${x}/${y}`;

    el('dbg-tile').textContent = tileKey;

    if (tileKey === lastTileKey) return;
    lastTileKey = tileKey;

    if (debounceTimer) clearTimeout(debounceTimer);
    debounceTimer = setTimeout(async () => {
      try {
        const url = `${WORKER}/tile-info/${z}/${x}/${y}?url=${encodeURIComponent(COG_URL)}`;
        const r = await fetch(url);
        if (!r.ok) return;
        const t = await r.json();

        el('dbg-overview').textContent =
          `#${t.overview_idx}  (${t.overview_width} × ${t.overview_height})`;

        if (t.has_data && t.pixel_window) {
          const w = t.pixel_window;
          el('dbg-window').textContent =
            `${w.x_off}, ${w.y_off}  +  ${w.x_size} × ${w.y_size} px`;
          el('dbg-status').textContent  = 'has data';
          el('dbg-status').className    = 'debug-value status-ok';
        } else {
          el('dbg-window').textContent  = '—';
          el('dbg-status').textContent  = 'outside image';
          el('dbg-status').className    = 'debug-value status-empty';
        }
      } catch {
        el('dbg-status').textContent = 'fetch error';
      }
    }, 200);
  });

  // ── Controls ──────────────────────────────────────────────────────────────
  el('opacity').addEventListener('input', (e) => {
    sarLayer.setOpacity(parseFloat((e.target as HTMLInputElement).value));
  });

  el('mode').addEventListener('change', () => {
    const isContour = (el('mode') as HTMLSelectElement).value === 'contour';
    el('contour-levels-row').style.display = isContour ? '' : 'none';
    tiffSource.setUrl(buildTileUrl());
  });

  el('colormap').addEventListener('change', () => {
    tiffSource.setUrl(buildTileUrl());
  });

  el('contour-level').addEventListener('input', (e) => {
    el('contour-level-val').textContent = (e.target as HTMLInputElement).value;
    tiffSource.setUrl(buildTileUrl());
  });

  el('min-val').addEventListener('change', () => { tiffSource.setUrl(buildTileUrl()); });
  el('max-val').addEventListener('change', () => { tiffSource.setUrl(buildTileUrl()); });
  el('mask-toggle').addEventListener('change', () => { tiffSource.setUrl(buildTileUrl()); });

  el('interpolation').addEventListener('change', () => {
    tiffSource.setUrl(buildTileUrl());
  });

  el('basemap').addEventListener('change', (e) => {
    osmLayer.setVisible((e.target as HTMLSelectElement).value === 'osm');
  });

  el('fly-btn').addEventListener('click', () => {
    if (cogExtent) {
      map.getView().fit(cogExtent, { padding: [60, 60, 60, 280], maxZoom: 18, duration: 600 });
    }
  });
}
