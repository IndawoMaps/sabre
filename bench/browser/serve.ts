// A static file server that answers Range requests, as object storage does.
//
// Deliberately small and HTTP/1.1, like S3 and most COG hosts: a browser
// opens at most six connections to it, which both engines live within.
// `latency` holds every response back that long before the first byte, the
// one network property that most changes how a COG reader behaves.

import { createServer, type Server } from 'node:http';
import { createReadStream } from 'node:fs';
import { stat } from 'node:fs/promises';
import { extname, join, normalize, sep } from 'node:path';

const TYPES: Record<string, string> = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript',
  '.mjs': 'text/javascript',
  '.css': 'text/css',
  '.wasm': 'application/wasm',
  '.json': 'application/json',
  '.svg': 'image/svg+xml',
  '.tif': 'image/tiff',
  '.tiff': 'image/tiff',
};

export interface Served {
  url: string;
  /** Every response body started since the last call: path and bytes. */
  take(): Array<{ path: string; bytes: number }>;
  close(): Promise<void>;
}

export async function serve(root: string, { latency = 0 } = {}): Promise<Served> {
  let log: Array<{ path: string; bytes: number }> = [];
  const server: Server = createServer(async (req, res) => {
    res.setHeader('Access-Control-Allow-Origin', '*');
    res.setHeader('Access-Control-Expose-Headers', 'Content-Range, Content-Length, Accept-Ranges');
    if (req.method === 'OPTIONS') {
      res.setHeader('Access-Control-Allow-Headers', 'Range');
      res.writeHead(204).end();
      return;
    }
    const path = decodeURIComponent(new URL(req.url ?? '/', 'http://x').pathname);
    const file = normalize(join(root, path.endsWith('/') ? `${path}index.html` : path));
    if (file !== root && !file.startsWith(root + sep)) {
      res.writeHead(403).end();
      return;
    }
    let size: number;
    try {
      const s = await stat(file);
      if (!s.isFile()) throw new Error('not a file');
      size = s.size;
    } catch {
      res.writeHead(404).end();
      return;
    }
    if (latency) await new Promise((r) => setTimeout(r, latency));

    res.setHeader('Content-Type', TYPES[extname(file)] ?? 'application/octet-stream');
    res.setHeader('Accept-Ranges', 'bytes');
    res.setHeader('Cache-Control', 'no-cache');
    const range = req.headers.range;
    if (!range) {
      res.writeHead(200, { 'Content-Length': size });
      if (req.method === 'HEAD') res.end();
      else {
        log.push({ path, bytes: size });
        createReadStream(file).pipe(res);
      }
      return;
    }
    // One range only. Neither engine asks for several at once, and answering
    // a multi-range request with the whole file would hide it if one did.
    const m = /^bytes=(\d*)-(\d*)$/.exec(range);
    if (!m || (!m[1] && !m[2])) {
      res.writeHead(416, { 'Content-Range': `bytes */${size}` }).end();
      return;
    }
    let start = m[1] ? Number(m[1]) : size - Number(m[2]);
    let end = m[1] && m[2] ? Math.min(Number(m[2]), size - 1) : size - 1;
    start = Math.max(0, start);
    if (start > end) {
      res.writeHead(416, { 'Content-Range': `bytes */${size}` }).end();
      return;
    }
    res.writeHead(206, {
      'Content-Range': `bytes ${start}-${end}/${size}`,
      'Content-Length': end - start + 1,
    });
    if (req.method === 'HEAD') res.end();
    else {
      log.push({ path, bytes: end - start + 1 });
      createReadStream(file, { start, end }).pipe(res);
    }
  });
  server.keepAliveTimeout = 30_000;
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  const { port } = server.address() as { port: number };
  return {
    url: `http://127.0.0.1:${port}`,
    take: () => {
      const t = log;
      log = [];
      return t;
    },
    close: () => new Promise((r) => {
      server.closeAllConnections();
      server.close(() => r());
    }),
  };
}
