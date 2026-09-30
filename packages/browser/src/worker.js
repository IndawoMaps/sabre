// Runs sabre's wasm off the main thread.
//
// Decoding is nearly all of a tile's cost -- 3 to 38 ms each -- and on the
// main thread that is a pan that stutters. Here it costs the map nothing.
//
// One Cog per URL, shared by every layer on the page, so two styles of the
// same raster read its bytes once.

import init, { Cog, setGeometries, deleteGeometries } from '../wasm/sabre_browser.js';

let ready;
const cogs = new Map();

function cog(url) {
  let c = cogs.get(url);
  if (!c) {
    c = new Cog(url);
    cogs.set(url, c);
  }
  return c;
}

async function tile({ url, z, x, y, options, as }) {
  const r = await cog(url).pixels(z, x, y, options);
  const size = Math.sqrt(r.data.length / 4);
  const stats = { ms: r.ms, requests: r.requests, timing: r.timing };
  if (as === 'pixels') {
    const data = new Uint8ClampedArray(r.data.buffer);
    return [{ data, size, ...stats }, [data.buffer]];
  }
  const bitmap = await createImageBitmap(new ImageData(new Uint8ClampedArray(r.data.buffer), size, size));
  return [{ data: bitmap, size, ...stats }, [bitmap]];
}

self.onmessage = async ({ data: msg }) => {
  const { id, type } = msg;
  try {
    if (type === 'init') {
      ready ??= init(msg.wasm ? { module_or_path: msg.wasm } : undefined);
      await ready;
      self.postMessage({ id, result: null });
      return;
    }
    await ready;
    if (type === 'info') {
      self.postMessage({ id, result: JSON.parse(await cog(msg.url).info()) });
    } else if (type === 'tile') {
      const [result, transfer] = await tile(msg);
      self.postMessage({ id, result }, transfer);
    } else if (type === 'geometry-set') {
      const geometries = msg.geometries.map(asGeometry);
      self.postMessage({ id, result: setGeometries(msg.provider, msg.ids, geometries, msg.replace) });
    } else if (type === 'geometry-delete') {
      self.postMessage({ id, result: deleteGeometries(msg.provider, msg.ids) });
    } else if (type === 'close') {
      cogs.get(msg.url)?.free();
      cogs.delete(msg.url);
      self.postMessage({ id, result: null });
    } else {
      throw new Error(`unknown message ${type}`);
    }
  } catch (e) {
    self.postMessage({ id, error: e instanceof Error ? e.message : String(e) });
  }
};

/** What wasm reads: TWKB as bytes, or WKT or GeoJSON as text. */
function asGeometry(g) {
  if (typeof g === 'string' || g instanceof ArrayBuffer || g instanceof Uint8Array) return g;
  if (ArrayBuffer.isView(g)) return new Uint8Array(g.buffer, g.byteOffset, g.byteLength);
  return JSON.stringify(g);
}
