import './style.css'
import { setup } from './counter.ts'

document.querySelector<HTMLDivElement>('#app')!.innerHTML = `
<div id="map"></div>

<div class="card">
  <h2>Umbra SAR — Mount Yasur</h2>
  Tanna Island, Vanuatu · 19.527°S 169.449°E
  <div class="row" id="meta-tags">
    <span class="tag" id="crs-tag">loading…</span>
    <span class="tag" id="dim-tag"></span>
    <span class="tag" id="gsd-tag"></span>
    <span class="tag" id="comp-tag"></span>
    <span class="tag" id="stats-tag" style="display:none"></span>
  </div>
</div>

<div class="tile-debug" id="tile-debug">
  <div class="debug-row">
    <span class="debug-label">tile</span>
    <span class="debug-value" id="dbg-tile">hover map to inspect</span>
  </div>
  <div class="debug-row">
    <span class="debug-label">overview</span>
    <span class="debug-value" id="dbg-overview">—</span>
  </div>
  <div class="debug-row">
    <span class="debug-label">window</span>
    <span class="debug-value" id="dbg-window">—</span>
  </div>
  <div class="debug-row">
    <span class="debug-label">status</span>
    <span class="debug-value" id="dbg-status">—</span>
  </div>
</div>

<div class="controls">
  <label>
    Opacity
    <input id="opacity" type="range" min="0" max="1" step="0.05" value="1" />
  </label>
  <label>
    Mode
    <select id="mode">
      <option value="colormap">Colormap</option>
      <option value="contour">Contour</option>
    </select>
  </label>
  <label>
    Colormap
    <select id="colormap">
      <option value="greys">Greys</option>
      <option value="viridis">Viridis</option>
      <option value="plasma">Plasma</option>
      <option value="turbo">Turbo</option>
      <option value="hot">Hot</option>
      <option value="rdylbu">RdYlBu</option>
    </select>
  </label>
  <label id="contour-levels-row" style="display:none">
    Levels <span id="contour-level-val" style="color:#ffb055;font-size:11px;margin-left:3px">10</span>
    <input id="contour-level" type="range" min="2" max="50" step="1" value="10" />
  </label>
  <label>
    Min
    <input id="min-val" type="number" value="0" step="any" />
  </label>
  <label>
    Max
    <input id="max-val" type="number" value="255" step="any" />
  </label>
  <label>
    Basemap
    <select id="basemap">
      <option value="osm">OpenStreetMap</option>
      <option value="none">None (black)</option>
    </select>
  </label>
  <label>
    Interpolation
    <select id="interpolation">
      <option value="nearest">Nearest</option>
      <option value="bilinear">Bilinear</option>
    </select>
  </label>
  <label>
    Mask
    <input id="mask-toggle" type="checkbox" />
  </label>
  <button id="fly-btn">Fly to image</button>
</div>
`

setup();
