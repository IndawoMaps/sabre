//! sabre-core rendering COG tiles in a browser, from `fetch` range requests.
//!
//! This is the wasm half of the `@sabremaps/browser` npm package. JavaScript holds
//! a [`Cog`] per raster and asks it for tiles; the package runs it in a Web
//! Worker, so everything here has to work without a `window` -- see [`js`].
//!
//! Range reads go through `fetch`, which means the object store has to allow
//! `Range` in CORS and expose `Content-Range`.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use async_trait::async_trait;
use sabre_core::cog::{fetch_meta, CogMeta, RangeReader};
use std::sync::Arc;

use sabre_core::geo;
use sabre_core::geometry::{parse_ids, to_mask, GeometryStore};
use sabre_core::mask::{parse_wkt_mask, Mask};
use sabre_core::params::StyleParams;
use sabre_core::reader::{range_body, CachingReader, HeaderCache, InFlight, Page, PageCache, PagedReader};
use sabre_core::render::{render_tile_rgba_timed, render_tile_timed, TileRequest};
use sabre_core::timing::Timings;
use wasm_bindgen::prelude::*;
use serde::Deserialize;
use wasm_bindgen::JsCast;

/// Globals that exist in a page and in a Web Worker alike.
///
/// `web_sys::window()` is `None` inside a worker, which is where the npm
/// package runs this. And a `dyn_ref::<Window>()` to tell the two apart is
/// worse than useless there: `instanceof Window` throws, because `Window` is
/// not defined. The bare globals are the same in both, so bind those.
mod js {
    use wasm_bindgen::prelude::*;

    #[wasm_bindgen]
    extern "C" {
        #[wasm_bindgen(js_name = fetch)]
        pub fn fetch_with_init(input: &str, init: &web_sys::RequestInit) -> js_sys::Promise;

        #[wasm_bindgen(js_name = fetch)]
        pub fn fetch(input: &str) -> js_sys::Promise;

        #[wasm_bindgen(js_namespace = performance, js_name = now)]
        pub fn performance_now() -> f64;
    }
}

/// `performance.now()` -- the browser's monotonic clock, in milliseconds.
///
/// `Instant::now()` panics here, which is why `sabre_core::timing` takes a
/// clock rather than reaching for one.
fn now_ms() -> f64 {
    js::performance_now()
}

fn timings() -> Timings {
    Timings::with_clock(now_ms)
}

// ── Reading bytes with fetch ─────────────────────────────────────────────────

struct FetchReader {
    url: String,
    /// Range requests issued, so the spike can report whether the cache is
    /// doing anything rather than assert that it is.
    requests: Rc<RefCell<u32>>,
    bytes: Rc<RefCell<u64>>,
}

#[async_trait(?Send)]
impl RangeReader for FetchReader {
    async fn read_range(&self, offset: u64, length: u64) -> Result<Vec<u8>, String> {
        let headers = web_sys::Headers::new().map_err(|e| format!("{e:?}"))?;
        headers
            .set("Range", &format!("bytes={}-{}", offset, offset + length - 1))
            .map_err(|e| format!("{e:?}"))?;

        let init = web_sys::RequestInit::new();
        init.set_method("GET");
        init.set_headers(&headers);

        let resp: web_sys::Response =
            wasm_bindgen_futures::JsFuture::from(js::fetch_with_init(&self.url, &init))
                .await
                .map_err(|e| format!("fetch failed: {e:?}"))?
                .dyn_into()
                .map_err(|_| "fetch did not return a Response".to_string())?;

        let status = resp.status();
        let buf = wasm_bindgen_futures::JsFuture::from(
            resp.array_buffer().map_err(|e| format!("{e:?}"))?,
        )
        .await
        .map_err(|e| format!("{e:?}"))?;
        let body = js_sys::Uint8Array::new(&buf).to_vec();

        *self.requests.borrow_mut() += 1;
        *self.bytes.borrow_mut() += body.len() as u64;

        // Shared with the server: a 200 means the origin ignored the Range and
        // sent the whole file, which has to be sliced rather than trusted.
        range_body(status, body, offset, length, &self.url)
    }
}

/// The whole file, fetched once and sliced from memory.
///
/// This looked like the obvious move at farm scale and it is not. A COG
/// covering one property at 1-3 m is single-digit megabytes, so the guess was
/// that a session would read most of it anyway and one request would beat
/// twenty. Measured, a pan across four zooms reads **6% of an 8 MB file** and
/// **32% of a 2 MB one** — the overviews are doing exactly what they are for,
/// and there are only three range requests in the whole session.
///
/// Over a warm transatlantic connection, medians of three:
///
/// ```text
///                     first tile   pan (15 tiles)   requests   bytes read
///   2.25 MB  stream       468 ms         1,079 ms          3    32% of file
///   2.25 MB  whole      1,989 ms            73 ms          1   100%
///   7.93 MB  stream       424 ms           725 ms          3     6% of file
///   7.93 MB  whole      5,013 ms            81 ms          1   100%
/// ```
///
/// Streaming wins the thing a user feels — first tile, by 4-12x — and reads a
/// fraction of the bytes, which is the cost line. Whole-file only wins the
/// pan, having already paid for every byte of it up front.
///
/// So this is kept for the case it is actually good for: taking a property
/// offline, or pre-warming a known extent deliberately. It is not the default
/// and should not be.
struct WholeFileReader {
    bytes: Rc<Vec<u8>>,
}

#[async_trait(?Send)]
impl RangeReader for WholeFileReader {
    async fn read_range(&self, offset: u64, length: u64) -> Result<Vec<u8>, String> {
        let start = (offset as usize).min(self.bytes.len());
        let end = (start + length as usize).min(self.bytes.len());
        Ok(self.bytes[start..end].to_vec())
    }
}

// ── Caches ───────────────────────────────────────────────────────────────────
//
// The same `PagedReader` the server uses, over a map rather than a
// byte-bounded LRU. A tab holding one raster does not need eviction to be
// clever; it needs the pages to still be there when the user pans back.

#[derive(Clone, Default)]
struct TabPageCache(Rc<RefCell<HashMap<(String, u64), Page>>>);

#[async_trait(?Send)]
impl PageCache for TabPageCache {
    async fn get(&self, source: &str, page: u64) -> Option<Page> {
        self.0.borrow().get(&(source.to_string(), page)).cloned()
    }
    async fn put(&self, source: &str, page: u64, bytes: Page) {
        self.0.borrow_mut().insert((source.to_string(), page), bytes);
    }
}

#[derive(Clone, Default)]
struct TabHeaderCache(Rc<RefCell<HashMap<String, Vec<u8>>>>);

#[async_trait(?Send)]
impl HeaderCache for TabHeaderCache {
    async fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.0.borrow().get(key).cloned()
    }
    async fn put(&self, key: &str, bytes: &[u8]) {
        self.0.borrow_mut().insert(key.to_string(), bytes.to_vec());
    }
}

// ── Options ──────────────────────────────────────────────────────────────────

/// What a tile call accepts, as JSON.
///
/// The style fields are [`StyleParams`], the very struct the server reads its
/// query string into, so every name and default is the HTTP API's: a style
/// copied from a working tile URL means the same thing here.
#[derive(Deserialize, Default)]
struct TileOptions {
    #[serde(flatten)]
    style: StyleParams,
    tile_size: Option<u32>,
    #[serde(default)]
    interpolation: String,
    /// WKT. Parsed once per distinct value, not per tile -- see [`Cog::mask`].
    #[serde(default)]
    mask: Option<String>,
    /// Geometry put in with [`set_geometries`], named the way the server's
    /// providers are. The id may also be a number or an array of either,
    /// since that is what JavaScript tends to have to hand.
    #[serde(default)]
    geometry_provider: Option<String>,
    #[serde(default)]
    geometry_id: Option<serde_json::Value>,
}

/// `geometry_id` as the comma-separated list the server takes.
fn id_list(v: &serde_json::Value) -> Result<String, String> {
    let one = |v: &serde_json::Value| match v {
        serde_json::Value::String(s) => Ok(s.clone()),
        serde_json::Value::Number(n) => Ok(n.to_string()),
        other => Err(format!("geometry_id cannot be {other}")),
    };
    match v {
        serde_json::Value::Array(items) => Ok(items.iter().map(one).collect::<Result<Vec<_>, _>>()?.join(",")),
        other => one(other),
    }
}

fn parse_options(json: &str) -> Result<TileOptions, JsValue> {
    if json.trim().is_empty() {
        return Ok(TileOptions::default());
    }
    serde_json::from_str(json).map_err(|e| JsValue::from_str(&format!("bad tile options: {e}")))
}

// ── Geometry ─────────────────────────────────────────────────────────────────
//
// One store for the worker, not one per Cog: a block clips every raster drawn
// over it, and is put in once for all of them. The worker is single-threaded,
// so a thread-local is the whole of the synchronisation.

thread_local! {
    static GEOMETRIES: RefCell<GeometryStore> = RefCell::new(GeometryStore::new());
}

/// Add or overwrite geometry under `provider`, or with `replace`, make it the
/// provider's entire contents. Returns the provider's new revision.
///
/// `geometries[i]` is the shape for `ids[i]`: TWKB bytes (a `Uint8Array` or
/// `ArrayBuffer`), or WKT or GeoJSON text, in WGS84. Everything is decoded
/// before anything changes, so one bad entry leaves the store as it was.
#[wasm_bindgen(js_name = setGeometries)]
pub fn set_geometries(provider: &str, ids: Vec<String>, geometries: Vec<JsValue>, replace: bool)
    -> Result<f64, JsValue>
{
    if ids.len() != geometries.len() {
        return Err(JsValue::from_str(&format!(
            "{} ids for {} geometries", ids.len(), geometries.len())));
    }
    let entries = ids.into_iter().zip(&geometries)
        .map(|(id, g)| decode_geometry(g).map(|m| (id.clone(), m))
            .map_err(|e| format!("geometry {id}: {e}")))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| JsValue::from_str(&e))?;
    GEOMETRIES.with_borrow_mut(|store| if replace {
        store.replace(provider, entries)
    } else {
        store.set(provider, entries)
    })
    .map(|rev| rev as f64)
    .map_err(|e| JsValue::from_str(&e))
}

/// Remove `ids` from `provider`. Returns its revision, changed only if
/// something was actually removed.
#[wasm_bindgen(js_name = deleteGeometries)]
pub fn delete_geometries(provider: &str, ids: Vec<String>) -> f64 {
    GEOMETRIES.with_borrow_mut(|store| store.delete(provider, &ids)) as f64
}

fn decode_geometry(g: &JsValue) -> Result<Mask, String> {
    if let Some(text) = g.as_string() {
        return to_mask(text.as_bytes(), None);
    }
    let bytes = if let Some(a) = g.dyn_ref::<js_sys::Uint8Array>() {
        a.to_vec()
    } else if g.is_instance_of::<js_sys::ArrayBuffer>() {
        js_sys::Uint8Array::new(g).to_vec()
    } else {
        return Err("expected TWKB bytes or WKT or GeoJSON text".into());
    };
    to_mask(&bytes, None)
}

// ── The thing JavaScript holds ───────────────────────────────────────────────

/// One COG, open for the life of the tab.
#[wasm_bindgen]
pub struct Cog {
    url: String,
    pages: TabPageCache,
    /// Pages some tile is fetching right now. The worker renders several
    /// tiles at once, and without this each would fetch the pages they share.
    in_flight: InFlight,
    headers: TabHeaderCache,
    requests: Rc<RefCell<u32>>,
    bytes: Rc<RefCell<u64>>,
    /// Parsed once. Re-parsing the header per tile is what the server's
    /// `meta` phase measures, and there is no reason to pay it here.
    meta: RefCell<Option<Rc<CogMeta>>>,
    /// Set by [`Cog::preload`]. When present every read is served from it.
    whole: RefCell<Option<Rc<Vec<u8>>>>,
    /// Off only to measure a cold read against a warm one.
    cache: Cell<bool>,
    /// The last mask seen, parsed. A map layer sends the same one with every
    /// tile, and parsing a farm's worth of WKT per tile is the cost the
    /// server's geometry providers exist to avoid.
    mask: RefCell<Option<(String, Arc<Mask>)>>,
}

#[wasm_bindgen]
impl Cog {
    #[wasm_bindgen(constructor)]
    pub fn new(url: String) -> Cog {
        Cog {
            url,
            pages: TabPageCache::default(),
            in_flight: InFlight::default(),
            headers: TabHeaderCache::default(),
            requests: Rc::new(RefCell::new(0)),
            bytes: Rc::new(RefCell::new(0)),
            meta: RefCell::new(None),
            whole: RefCell::new(None),
            cache: Cell::new(true),
            mask: RefCell::new(None),
        }
    }

    /// Fetch the whole file and serve every read from it.
    ///
    /// Returns the bytes fetched. Deliberate pre-warming only — see
    /// [`WholeFileReader`] for why this loses to streaming on everything a
    /// user notices.
    pub async fn preload(&self) -> Result<f64, JsValue> {
        let resp: web_sys::Response = wasm_bindgen_futures::JsFuture::from(js::fetch(&self.url))
            .await?
            .dyn_into()
            .map_err(|_| JsValue::from_str("fetch did not return a Response"))?;
        if !resp.ok() {
            return Err(JsValue::from_str(&format!("{}: HTTP {}", self.url, resp.status())));
        }
        let buf = wasm_bindgen_futures::JsFuture::from(resp.array_buffer()?).await?;
        let bytes = js_sys::Uint8Array::new(&buf).to_vec();
        let len = bytes.len() as f64;
        *self.requests.borrow_mut() += 1;
        *self.bytes.borrow_mut() += bytes.len() as u64;
        *self.whole.borrow_mut() = Some(Rc::new(bytes));
        *self.meta.borrow_mut() = None;
        Ok(len)
    }

    fn reader(&self) -> Box<dyn RangeReader> {
        if let Some(bytes) = self.whole.borrow().as_ref() {
            return Box::new(WholeFileReader { bytes: Rc::clone(bytes) });
        }
        let raw = Box::new(FetchReader {
            url: self.url.clone(),
            requests: Rc::clone(&self.requests),
            bytes: Rc::clone(&self.bytes),
        });
        if !self.cache.get() {
            return raw;
        }
        let paged = PagedReader::new(raw, Box::new(self.pages.clone()), &self.url)
            .sharing(self.in_flight.clone());
        Box::new(CachingReader::new(
            Box::new(paged),
            Box::new(self.headers.clone()),
            &self.url,
        ))
    }

    async fn meta_of(&self, reader: &dyn RangeReader, t: &Timings) -> Result<Rc<CogMeta>, String> {
        if let Some(m) = self.meta.borrow().as_ref() {
            return Ok(Rc::clone(m));
        }
        let m = Rc::new(t.time_async(sabre_core::timing::phase::META, fetch_meta(reader)).await?);
        *self.meta.borrow_mut() = Some(Rc::clone(&m));
        Ok(m)
    }

    /// The clip a tile asked for: a registered geometry by name, or a WKT
    /// `mask`. Both at once is refused, as the server refuses it -- they say
    /// different things, and honouring one clips to a shape nobody asked for.
    fn clip_of(&self, opts: &TileOptions, t: &Timings) -> Result<Option<Arc<Mask>>, String> {
        let (provider, ids) = match (opts.geometry_provider.as_deref(), opts.geometry_id.as_ref()) {
            (None, None) => return self.mask_of(opts.mask.as_deref(), t),
            (Some(p), Some(i)) => (p, i),
            (Some(_), None) => return Err("geometry_provider needs a geometry_id".into()),
            (None, Some(_)) => return Err("geometry_id needs a geometry_provider".into()),
        };
        if opts.mask.as_deref().is_some_and(|m| !m.trim().is_empty()) {
            return Err("mask and geometry_provider both name a clip geometry; send one or the other".into());
        }
        let ids = parse_ids(&id_list(ids)?)?;
        t.time(sabre_core::timing::phase::GEOMETRY, || {
            GEOMETRIES.with_borrow_mut(|store| store.resolve(provider, &ids))
        })
        .map(Some)
    }

    fn mask_of(&self, wkt: Option<&str>, t: &Timings) -> Result<Option<Arc<Mask>>, String> {
        let Some(wkt) = wkt.filter(|w| !w.trim().is_empty()) else {
            return Ok(None);
        };
        if let Some((seen, mask)) = self.mask.borrow().as_ref() {
            if seen == wkt {
                return Ok(Some(Arc::clone(mask)));
            }
        }
        let mask = Arc::new(t.time(sabre_core::timing::phase::GEOMETRY, || parse_wkt_mask(wkt))?);
        *self.mask.borrow_mut() = Some((wkt.to_string(), Arc::clone(&mask)));
        Ok(Some(mask))
    }

    /// Whether reads go through the page cache. On unless measuring.
    #[wasm_bindgen(getter)]
    pub fn cache(&self) -> bool {
        self.cache.get()
    }

    #[wasm_bindgen(setter)]
    pub fn set_cache(&self, on: bool) {
        self.cache.set(on);
    }

    /// Range requests issued since the tab opened, and bytes pulled.
    #[wasm_bindgen(getter)]
    pub fn requests(&self) -> u32 {
        *self.requests.borrow()
    }

    #[wasm_bindgen(getter)]
    pub fn bytes(&self) -> f64 {
        *self.bytes.borrow() as f64
    }

    /// Throw the caches away, to measure a cold read without reloading.
    pub fn forget(&self) {
        self.pages.0.borrow_mut().clear();
        self.headers.0.borrow_mut().clear();
        *self.meta.borrow_mut() = None;
        *self.whole.borrow_mut() = None;
    }

    /// The COG's own description of itself, as JSON.
    ///
    /// `extent` is WGS84 `[west, south, east, north]`, for a map to fit to and
    /// to stop asking for tiles outside. `native_zoom` is the Web Mercator zoom
    /// whose pixels are the raster's own: past it a map can stretch tiles it
    /// has instead of rendering new ones that cannot hold more detail.
    pub async fn info(&self) -> Result<String, JsValue> {
        let reader = self.reader();
        let t = timings();
        let meta = self
            .meta_of(reader.as_ref(), &t)
            .await
            .map_err(|e| JsValue::from_str(&e))?;
        let ifd = &meta.ifds[0];
        let extent = ifd
            .geo_transform()
            .and_then(|gt| geo::geo_extent_wgs84(&gt, ifd.image_width, ifd.image_height, ifd.epsg_code));
        let native_zoom = extent.as_ref().map(|b| {
            // Degrees of longitude per pixel, against 360° over 256·2^z.
            let per_px = (b.east - b.west) / ifd.image_width as f64;
            (360.0 / (256.0 * per_px)).log2()
        });
        let out = serde_json::json!({
            "width": ifd.image_width,
            "height": ifd.image_height,
            "bands": ifd.samples_per_pixel,
            "overviews": meta.ifds.len(),
            "epsg": ifd.epsg_code,
            "compression": ifd.compression,
            "predictor": ifd.predictor,
            "nodata": ifd.nodata.as_deref().and_then(|s| s.parse::<f64>().ok()),
            "extent": extent.map(|b| [b.west, b.south, b.east, b.north]),
            "native_zoom": native_zoom,
        });
        Ok(out.to_string())
    }

    /// Render one tile as unencoded RGBA, `tile_size² × 4` bytes.
    ///
    /// `options` is JSON with the HTTP API's names: `mode`, `colormap`, `min`,
    /// `max`, `nodata`, `stops`, `azimuth`, … plus `tile_size`,
    /// `interpolation`, and a clip: `mask` (WKT), or `geometry_provider` and
    /// `geometry_id` naming geometry put in with [`set_geometries`]. The result is `{data, ms, requests,
    /// timing}`, where `data` owns its buffer and so can be transferred.
    pub async fn pixels(&self, z: u32, x: u32, y: u32, options: String) -> Result<JsValue, JsValue> {
        self.render(z, x, y, &options, false).await
    }

    /// As [`Cog::pixels`], but `data` is a PNG.
    pub async fn png(&self, z: u32, x: u32, y: u32, options: String) -> Result<JsValue, JsValue> {
        self.render(z, x, y, &options, true).await
    }

    async fn render(&self, z: u32, x: u32, y: u32, options: &str, png: bool) -> Result<JsValue, JsValue> {
        let err = |e: String| JsValue::from_str(&e);
        let t = timings();
        let started = now_ms();
        let before = *self.requests.borrow();

        let opts = parse_options(options)?;
        let style = opts.style.to_style().map_err(err)?;
        // Before the first await: the tile keeps the geometry it started
        // with, even if the app replaces or deletes it while this one reads.
        let mask = self.clip_of(&opts, &t).map_err(err)?;
        let reader = self.reader();
        let meta = self.meta_of(reader.as_ref(), &t).await.map_err(err)?;

        let req = TileRequest {
            z,
            x,
            y,
            tile_size: opts.tile_size.unwrap_or(256).clamp(64, 512),
            style,
            bilinear: opts.interpolation == "bilinear",
            mask,
        };
        let data = if png {
            render_tile_timed(&req, reader.as_ref(), &meta, &t).await
        } else {
            render_tile_rgba_timed(&req, reader.as_ref(), &meta, &t).await
        }
        .map_err(err)?;

        let out = js_sys::Object::new();
        js_sys::Reflect::set(&out, &"data".into(), &js_sys::Uint8Array::from(&data[..]))?;
        js_sys::Reflect::set(&out, &"ms".into(), &(now_ms() - started).into())?;
        js_sys::Reflect::set(&out, &"requests".into(),
                             &((*self.requests.borrow() - before) as f64).into())?;
        js_sys::Reflect::set(&out, &"timing".into(),
                             &t.header().unwrap_or_default().into())?;
        Ok(out.into())
    }
}
