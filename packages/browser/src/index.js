// @sabremaps/browser -- render Cloud-Optimized GeoTIFF tiles in the browser.
//
// A small client for a Web Worker that holds sabre's wasm. Nothing here is
// tied to a map library: `tile()` gives back an ImageBitmap for whatever
// draws it, and `pixels()` the raw RGBA.

let settings = { wasmUrl: undefined, concurrency: 6 };
let client;

/**
 * Change where the wasm comes from, or how many tiles are in flight at once.
 * Only takes effect before the first `open()`.
 */
export function configure(options) {
  if (client) throw new Error('@sabremaps/browser: configure() must come before the first open()');
  settings = { ...settings, ...options };
}

/** Open a raster by URL. Resolves once its header has been read. */
export async function open(url) {
  const c = getClient();
  const href = new URL(url, globalThis.location?.href).href;
  c.retain(href);
  try {
    const info = await c.call({ type: 'info', url: href });
    return new Raster(c, href, info);
  } catch (e) {
    c.release(href);
    throw e;
  }
}

export class Raster {
  #client;
  #closed = false;

  constructor(client, url, info) {
    this.#client = client;
    /** Absolute URL of the COG. */
    this.url = url;
    /** What the COG says about itself: size, bands, EPSG, WGS84 extent, native zoom. */
    this.info = info;
  }

  /** Render a z/x/y tile. Resolves to an ImageBitmap the caller now owns. */
  async tile(z, x, y, style = {}, { signal } = {}) {
    return (await this.#render(z, x, y, style, 'bitmap', signal)).data;
  }

  /** As `tile()`, but the RGBA pixels, `size × size × 4` bytes. */
  async pixels(z, x, y, style = {}, { signal } = {}) {
    const r = await this.#render(z, x, y, style, 'pixels', signal);
    return { data: r.data, size: r.size };
  }

  /** Free this raster's cache once no other `open()` of the same URL holds it. */
  close() {
    if (this.#closed) return;
    this.#closed = true;
    this.#client.release(this.url);
  }

  #render(z, x, y, style, as, signal) {
    if (this.#closed) return Promise.reject(new Error('@sabremaps/browser: raster is closed'));
    const options = typeof style === 'string' ? style : JSON.stringify(style);
    return this.#client.call({ type: 'tile', url: this.url, z, x, y, options, as }, signal);
  }
}

// ── The worker and its queue ─────────────────────────────────────────────────

function getClient() {
  // Written out in full, not built from variables: bundlers (Vite, webpack,
  // Rollup, esbuild) find the worker by matching this exact expression.
  client ??= new Client(
    new Worker(new URL('./worker.js', import.meta.url), { type: 'module' }),
    settings,
  );
  return client;
}

/**
 * Posts requests to the worker, a bounded number at a time.
 *
 * A map asks for every tile in view at once and cancels most of them when the
 * user keeps panning. Holding the rest back here means a cancelled tile is
 * dropped before the worker ever starts on it -- once started it cannot be
 * interrupted, only ignored.
 */
class Client {
  #worker;
  #nextId = 0;
  #pending = new Map();
  #queue = [];
  #inFlight = 0;
  #limit;
  #holds = new Map();
  #ready;

  constructor(worker, { wasmUrl, concurrency }) {
    this.#worker = worker;
    this.#limit = Math.max(1, concurrency | 0);
    worker.onmessage = ({ data }) => this.#settle(data);
    worker.onerror = (e) => this.#fail(new Error(`@sabremaps/browser worker failed: ${e.message ?? e}`));
    this.#ready = this.#post({ type: 'init', wasm: wasmUrl && String(wasmUrl) });
  }

  retain(url) {
    this.#holds.set(url, (this.#holds.get(url) ?? 0) + 1);
  }

  release(url) {
    const n = (this.#holds.get(url) ?? 1) - 1;
    if (n > 0) {
      this.#holds.set(url, n);
      return;
    }
    this.#holds.delete(url);
    this.call({ type: 'close', url }).catch(() => {});
  }

  async call(msg, signal) {
    await this.#ready;
    if (signal?.aborted) throw abortError(signal);
    return new Promise((resolve, reject) => {
      const job = { msg, resolve, reject, signal, onAbort: null };
      if (signal) {
        job.onAbort = () => {
          const i = this.#queue.indexOf(job);
          if (i >= 0) this.#queue.splice(i, 1);
          reject(abortError(signal));
        };
        signal.addEventListener('abort', job.onAbort, { once: true });
      }
      this.#queue.push(job);
      this.#pump();
    });
  }

  #pump() {
    while (this.#inFlight < this.#limit && this.#queue.length) {
      const job = this.#queue.shift();
      this.#inFlight++;
      this.#post(job.msg).then(
        (result) => job.signal?.aborted ? discard(result) : job.resolve(result),
        (error) => job.reject(error),
      ).finally(() => {
        job.signal?.removeEventListener('abort', job.onAbort);
        this.#inFlight--;
        this.#pump();
      });
    }
  }

  #post(msg) {
    const id = this.#nextId++;
    return new Promise((resolve, reject) => {
      this.#pending.set(id, { resolve, reject });
      this.#worker.postMessage({ ...msg, id });
    });
  }

  #settle({ id, result, error }) {
    const p = this.#pending.get(id);
    if (!p) return;
    this.#pending.delete(id);
    if (error !== undefined) p.reject(new Error(error));
    else p.resolve(result);
  }

  #fail(error) {
    for (const p of this.#pending.values()) p.reject(error);
    this.#pending.clear();
  }
}

function abortError(signal) {
  return signal.reason ?? new DOMException('The operation was aborted.', 'AbortError');
}

/** A result nobody wants any more: release what it holds. */
function discard(result) {
  result?.data?.close?.();
}
