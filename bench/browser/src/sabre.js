// sabre: @sabremaps/ol on a plain canvas TileLayer.

import TileLayer from 'ol/layer/Tile.js';
import { sabreSource } from '@sabremaps/ol';

export async function makeLayer(url, max, errors) {
  const style = (max) => ({ mode: 'colormap', colormap: 'viridis', min: 0, max });
  const source = await sabreSource(url, { style: style(max) });
  source.on('tileloaderror', () => errors.push('tile'));
  return {
    layer: new TileLayer({ source }),
    restyle: (max) => source.setStyle(style(max)),
  };
}
