// sabre against OpenLayers' own GeoTIFF source, in a real browser.
//
//   node bench/browser/run.ts [--scene 35HLD] [--runs 5] [--latency 0]
//                             [--browser chrome|chromium|firefox|webkit] [--headed]
//                             [--engines sabre,ol]
//
// Each run is a fresh browser context -- cold HTTP cache, new workers, the
// wasm compiled again -- that loads the page, waits for the first complete
// render, then walks a fixed sequence of views. Engines alternate within a
// run so drift in the machine lands on both. One warm-up run is discarded.
//
// Needs the wasm built into @sabremaps/browser (`just npm`) and the scenes in
// bench/data/scenes (bench/fetch_scenes.py).

import { chromium, firefox, webkit, type Browser, type Page } from 'playwright';
import { build } from 'vite';
import { existsSync, readFileSync } from 'node:fs';
import { brotliCompressSync, constants } from 'node:zlib';
import { mkdir, writeFile } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import { serve } from './serve.ts';

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = join(HERE, '../..');
const SCENES_DIR = join(REPO, 'bench/data/scenes');

/**
 * `center` is the middle of the scene's footprint, where the first view
 * looks. `focus` is a point inside its data -- a Sentinel-2 tile at a swath
 * edge is partly empty -- which the steps after it move around.
 */
const SCENES: Record<string, { file: string; center: [number, number]; focus: [number, number] }> = {
  '35HLD': { file: 'S2A_35HLD_20260805_0_L2A_red.tif', center: [25.4431, -33.0217], focus: [25.75, -33.02] },
  '34JDP': { file: 'S2B_34JDP_20260906_0_L2A_red.tif', center: [20.5365, -28.5186], focus: [20.5365, -28.5186] },
  '50HPK': { file: 'S2C_50HPK_20260606_0_L2A_red.tif', center: [118.6451, -32.1185], focus: [118.6451, -32.1185] },
};

/** The first view: the whole 110 km scene in a 1280×800 viewport. */
const START_ZOOM = 10;

/** Offsets are viewport sizes at that step's zoom, from the scene centre. */
const STEPS: Array<{ name: string; zoom?: number; dx?: number; dy?: number; restyle?: number }> = [
  { name: 'zoom to z12', zoom: 12, dx: 0, dy: 0 },
  { name: 'zoom to z14', zoom: 14, dx: 0, dy: 0 },
  { name: 'pan east', zoom: 14, dx: 1, dy: 0 },
  { name: 'pan east', zoom: 14, dx: 2, dy: 0 },
  { name: 'pan south', zoom: 14, dx: 2, dy: 1 },
  { name: 'z16, past native', zoom: 16, dx: 8, dy: 4 },
  { name: 'back out to z10', zoom: START_ZOOM, dx: 0, dy: 0 },
  { name: 'restyle, max 2000', restyle: 2000 },
];

const { values: args } = parseArgs({
  options: {
    scene: { type: 'string', default: '35HLD' },
    runs: { type: 'string', default: '5' },
    warmup: { type: 'string', default: '1' },
    latency: { type: 'string', default: '0' },
    browser: { type: 'string', default: 'chrome' },
    engines: { type: 'string', default: 'sabre,ol' },
    headed: { type: 'boolean', default: false },
  },
});

const scene = SCENES[args.scene!];
if (!scene) throw new Error(`unknown scene ${args.scene}; one of ${Object.keys(SCENES).join(', ')}`);
if (!existsSync(join(SCENES_DIR, scene.file))) {
  throw new Error(`${scene.file} is not in bench/data/scenes -- run bench/fetch_scenes.py`);
}
if (!existsSync(join(REPO, 'packages/browser/wasm/sabre_browser_bg.wasm'))) {
  throw new Error('the wasm is not built into @sabremaps/browser -- run `just npm`');
}
const runs = Number(args.runs);
const warmup = Number(args.warmup);
const latency = Number(args.latency);
const engines = args.engines!.split(',');

interface StepResult { name: string; ms: number; blocking: number; errors: number; requests: number; bytes: number }
interface RunResult {
  engine: string;
  run: number;
  /** ms from navigation to the first complete render. */
  first: number;
  /** ms from navigation until the source knew the raster (header read). */
  source: number;
  firstBlocking: number;
  firstRequests: number;
  firstBytes: number;
  /** JS and wasm the page downloaded, bytes, and brotli-compressed. */
  appBytes: number;
  appBrotli: number;
  steps: StepResult[];
}

await build({ root: HERE, logLevel: 'warn', build: { outDir: join(HERE, 'dist'), emptyOutDir: true } });
const app = await serve(join(HERE, 'dist'));
const data = await serve(SCENES_DIR, { latency });

const launcher = { chrome: chromium, chromium, firefox, webkit }[args.browser!];
if (!launcher) throw new Error(`unknown browser ${args.browser}`);
const browser: Browser = await launcher.launch({
  headless: !args.headed,
  ...(args.browser === 'chrome' ? { channel: 'chrome' } : {}),
});

const out = join(HERE, 'results', `${new Date().toISOString().replace(/[:.]/g, '-')}-${args.scene}-${args.browser}-${latency}ms`);
await mkdir(out, { recursive: true });

const version = browser.version();
const results: RunResult[] = [];
const brotliSizes = new Map<string, number>();
let renderer: string | null = null;
try {
  for (let i = 0; i < warmup + runs; i++) {
    const order = i % 2 ? [...engines].reverse() : engines;
    for (const engine of order) {
      const r = await once(engine, i, i === warmup);
      if (i >= warmup) results.push(r);
      process.stderr.write(`${i < warmup ? 'warm-up' : `run ${i - warmup + 1}/${runs}`}  ${engine.padEnd(5)}  first ${r.first.toFixed(0)} ms, steps ${sum(r.steps.map((s) => s.ms)).toFixed(0)} ms\n`);
    }
  }
} finally {
  await browser.close();
  await app.close();
  await data.close();
}

const meta = {
  scene: args.scene, file: scene.file, browser: args.browser, version,
  renderer, latency, runs, warmup, date: new Date().toISOString(),
};
await writeFile(join(out, 'results.json'), JSON.stringify({ meta, results }, null, 2));
const report = summarize();
await writeFile(join(out, 'summary.md'), report);
console.log(report);
console.log(`\nresults, screenshots: ${out}`);

async function once(engine: string, run: number, screenshots: boolean): Promise<RunResult> {
  const context = await browser.newContext({ viewport: { width: 1280, height: 800 }, deviceScaleFactor: 1 });
  const page: Page = await context.newPage();
  const errors: string[] = [];
  page.on('pageerror', (e) => errors.push(e.message));

  // Reads are counted by the servers, not the browser: that covers both
  // engines' workers alike, in every browser.
  app.take();
  data.take();

  const u = new URL(app.url);
  u.searchParams.set('engine', engine);
  u.searchParams.set('url', `${data.url}/${scene.file}`);
  u.searchParams.set('center', JSON.stringify(scene.center));
  u.searchParams.set('focus', JSON.stringify(scene.focus));
  u.searchParams.set('zoom', String(START_ZOOM));
  await page.goto(u.href);
  try {
    await page.waitForFunction(() => (window as any).benchReady, null, { timeout: 90_000 });
  } catch (e) {
    throw new Error(`${engine} never became ready: ${errors.join('; ') || e}`);
  }
  const { marks, renderer: r } = await page.evaluate(() => {
    const b = (window as any).bench;
    return { marks: b.marks, renderer: b.renderer };
  });
  renderer ??= r;
  const first = data.take();
  const code = app.take().filter((f) => /\.(m?js|wasm)$/.test(f.path));
  if (screenshots) await page.screenshot({ path: join(out, `${engine}-z${START_ZOOM}.png`) });

  const steps: StepResult[] = [];
  for (const s of STEPS) {
    const res = await page.evaluate((step) => (window as any).bench.step(step), s);
    const reads = data.take();
    steps.push({ name: s.name, ...res, requests: reads.length, bytes: sum(reads.map((q) => q.bytes)) });
    if (screenshots && s.zoom === 14 && s.dx === 0) await page.screenshot({ path: join(out, `${engine}-z14.png`) });
  }
  await context.close();
  if (errors.length) throw new Error(`${engine}: ${errors.join('; ')}`);

  return {
    engine, run,
    first: marks.first,
    source: marks.source,
    firstBlocking: marks.blocking,
    firstRequests: first.length,
    firstBytes: sum(first.map((q) => q.bytes)),
    appBytes: sum(code.map((f) => f.bytes)),
    appBrotli: sum(code.map((f) => brotli(f.path))),
    steps,
  };
}

/** A served file's size once brotli-compressed, as a CDN would send it. */
function brotli(path: string): number {
  let n = brotliSizes.get(path);
  if (n === undefined) {
    n = brotliCompressSync(readFileSync(join(HERE, 'dist', path)), {
      params: { [constants.BROTLI_PARAM_QUALITY]: 11 },
    }).length;
    brotliSizes.set(path, n);
  }
  return n;
}

function summarize(): string {
  const by = (e: string) => results.filter((r) => r.engine === e);
  const cols = engines;
  const lines: string[] = [];
  const row = (label: string, f: (r: RunResult) => number, fmt: (n: number) => string) => {
    const med = cols.map((e) => median(by(e).map(f)));
    const spread = cols.map((e) => {
      const v = by(e).map(f);
      return v.length > 1 ? ` (${fmt(Math.min(...v))}–${fmt(Math.max(...v))})` : '';
    });
    const ratio = cols.length === 2 && med[0] > 0 ? `${(med[1] / med[0]).toFixed(2)}×` : '';
    lines.push(`| ${label} | ${med.map((m, i) => fmt(m) + spread[i]).join(' | ')} | ${ratio} |`);
  };
  const ms = (n: number) => `${n.toFixed(0)}`;
  const kb = (n: number) => `${(n / 1024).toFixed(0)}`;
  const n = (x: number) => `${x.toFixed(0)}`;

  lines.push(`**${meta.file}**, ${meta.browser} ${meta.version}, ${meta.renderer ?? 'unknown GPU'}, `
    + `${latency} ms added latency, median of ${runs} runs (min–max).\n`);
  lines.push(`| | ${cols.join(' | ')} | ${cols.length === 2 ? `${cols[1]} / ${cols[0]}` : ''} |`);
  lines.push(`| --- | ${cols.map(() => '---:').join(' | ')} | ---: |`);
  lines.push(`| **Time, ms** | | | |`);
  row('first render, from navigation', (r) => r.first, ms);
  row('&nbsp; of which: raster header read', (r) => r.source, ms);
  for (const [k, s] of STEPS.entries()) row(s.name, (r) => r.steps[k].ms, ms);
  row('all steps', (r) => sum(r.steps.map((s) => s.ms)), ms);
  lines.push(`| **Main thread blocked (long tasks past 50 ms), ms** | | | |`);
  row('first render', (r) => r.firstBlocking, ms);
  row('all steps', (r) => sum(r.steps.map((s) => s.blocking)), ms);
  lines.push(`| **COG reads** | | | |`);
  row('requests, first render', (r) => r.firstRequests, n);
  row('requests, all steps', (r) => sum(r.steps.map((s) => s.requests)), n);
  row('KB read, first render', (r) => r.firstBytes, kb);
  row('KB read, all steps', (r) => sum(r.steps.map((s) => s.bytes)), kb);
  lines.push(`| **Download** | | | |`);
  row('page JS + wasm, KB', (r) => r.appBytes, kb);
  row('&nbsp; brotli-compressed', (r) => r.appBrotli, kb);
  const failed = results.filter((r) => r.steps.some((s) => s.errors));
  if (failed.length) lines.push(`\n**Tile errors** in ${failed.length} runs: ${failed.map((r) => `${r.engine}#${r.run}`).join(', ')}`);
  return lines.join('\n');
}

function sum(v: number[]) {
  return v.reduce((a, b) => a + b, 0);
}

function median(v: number[]) {
  if (!v.length) return NaN;
  const s = [...v].sort((a, b) => a - b);
  const m = s.length >> 1;
  return s.length % 2 ? s[m] : (s[m - 1] + s[m]) / 2;
}
