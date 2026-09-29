//! Tests that run in a real browser.
//!
//! `cargo check --target wasm32-unknown-unknown` proves almost nothing about
//! this target. `std::time::Instant::now()` compiles there and panics when it
//! runs; the whole render path built cleanly and then trapped with
//! `RuntimeError: unreachable` on the first tile. Anything that matters has to
//! be executed, not compiled.
//!
//!     wasm-pack test --headless --chrome browser

use async_trait::async_trait;
use sabre_core::cog::{decode_tile, fetch_meta, RangeReader};
use sabre_core::mask::parse_wkt_mask;
use sabre_core::render::{render_tile_timed, StyleMode, TileRequest};
use sabre_core::timing::{phase, Timings};
use sabre_core::twkb;
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

/// A real Sentinel-2 red band with `PREDICTOR=2`, the same fixture the native
/// tests use. Everything here reads from memory: what is under test is the
/// decoder and renderer in wasm, not the network.
const FIXTURE: &[u8] = include_bytes!("../../core/tests/fixtures/s2_b04_predictor2.tif");

struct MemReader(&'static [u8]);

#[async_trait(?Send)]
impl RangeReader for MemReader {
    async fn read_range(&self, offset: u64, length: u64) -> Result<Vec<u8>, String> {
        let start = (offset as usize).min(self.0.len());
        let end = (start + length as usize).min(self.0.len());
        Ok(self.0[start..end].to_vec())
    }
}

fn clock() -> f64 {
    web_sys::window().and_then(|w| w.performance()).map(|p| p.now()).unwrap_or(0.0)
}

#[wasm_bindgen_test]
async fn a_cog_header_parses_in_wasm() {
    let meta = fetch_meta(&MemReader(FIXTURE)).await.expect("parse header");
    let ifd = &meta.ifds[0];
    assert_eq!((ifd.image_width, ifd.image_height), (146, 242));
    assert_eq!(ifd.epsg_code, Some(32735), "UTM, so the reprojection path is live");
    assert_eq!(ifd.predictor, 2);
}

#[wasm_bindgen_test]
async fn the_predictor_decodes_to_the_same_values_as_it_does_natively() {
    // The numbers GDAL produced for this fixture, asserted in
    // core/tests/predictor.rs. If wasm arithmetic diverged, it would show here.
    let reader = MemReader(FIXTURE);
    let meta = fetch_meta(&reader).await.unwrap();
    let ifd = &meta.ifds[0];
    let raw = reader.read_range(ifd.tile_offsets[0], ifd.tile_byte_counts[0]).await.unwrap();
    let decoded = decode_tile(
        &raw, ifd.compression, ifd.predictor,
        ifd.tile_width.unwrap(), ifd.tile_height.unwrap(),
        ifd.samples_per_pixel, ifd.bits_per_sample[0], &Default::default(),
    ).expect("decode");

    let tile_w = ifd.tile_width.unwrap() as usize;
    let (mut min, mut max, mut sum) = (u16::MAX, 0u16, 0u64);
    for y in 0..242 {
        for x in 0..146 {
            let b = (y * tile_w + x) * 2;
            let v = u16::from_le_bytes([decoded[b], decoded[b + 1]]);
            min = min.min(v);
            max = max.max(v);
            sum += v as u64;
        }
    }
    assert_eq!((min, max, sum), (47, 528, 6_447_225));
}

#[wasm_bindgen_test]
async fn a_tile_renders_and_reprojects() {
    let reader = MemReader(FIXTURE);
    let meta = fetch_meta(&reader).await.unwrap();
    let req = TileRequest {
        z: 16, x: 37414, y: 39214, tile_size: 256,
        style: StyleMode::Colormap { name: "viridis".into(), min: 0.0, max: 600.0, nodata: None },
        bilinear: false, mask: None,
    };
    let png = render_tile_timed(&req, &reader, &meta, &Timings::off()).await.expect("render");
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    assert!(png.len() > 500, "a tile over the fixture should not be empty: {} bytes", png.len());
}

#[wasm_bindgen_test]
fn the_host_clock_works_and_the_default_one_is_absent() {
    // Instant::now() panics here, so core offers no default clock on wasm and
    // the host supplies performance.now(). Both halves are asserted, because
    // the failure mode of the first is a trap rather than a wrong number.
    assert!(!Timings::new().enabled(), "wasm must have no default clock");

    let t = Timings::with_clock(clock);
    assert!(t.enabled());
    t.time(phase::DECODE, || {
        let mut n = 0u64;
        for i in 0..200_000u64 { n = n.wrapping_add(i); }
        core::hint::black_box(n);
    });
    let spans = t.spans();
    assert_eq!(spans.len(), 1);
    assert!(spans[0].1 >= 0.0, "a duration cannot be negative: {:?}", spans[0]);
    assert!(t.header().is_some());
}

#[wasm_bindgen_test]
fn twkb_round_trips_in_wasm() {
    let wkt = "POLYGON((25.53 -33.35,25.54 -33.35,25.54 -33.36,25.53 -33.36,25.53 -33.35))";
    let mask = parse_wkt_mask(wkt).unwrap();
    let bytes = twkb::encode_mask(&mask, twkb::DEFAULT_PRECISION).unwrap();
    let back = twkb::decode_mask(&bytes).unwrap();
    assert!(back.contains(25.535, -33.355));
    assert!(!back.contains(25.50, -33.355));
    assert!(bytes.len() < wkt.len() / 2, "{} bytes against {} of WKT", bytes.len(), wkt.len());
}

// ── The API JavaScript sees ──────────────────────────────────────────────────

use sabre_browser::Cog;
use wasm_bindgen::JsValue;

/// A `Cog` over the fixture, preloaded from a blob: URL, so the whole public
/// path runs -- options, fetch, caches -- with no network in it.
async fn fixture_cog() -> Cog {
    let parts = js_sys::Array::of1(&js_sys::Uint8Array::from(FIXTURE).into());
    let blob = web_sys::Blob::new_with_u8_array_sequence(&parts).unwrap();
    let url = web_sys::Url::create_object_url_with_blob(&blob).unwrap();
    let cog = Cog::new(url);
    cog.preload().await.expect("preload");
    cog
}

/// Over the fixture, as in `a_tile_renders_and_reprojects`.
const TILE: (u32, u32, u32) = (16, 37414, 39214);

async fn pixels(cog: &Cog, options: &str) -> Result<Vec<u8>, String> {
    let (z, x, y) = TILE;
    let out = cog.pixels(z, x, y, options.into()).await.map_err(|e| e.as_string().unwrap_or_default())?;
    let data = js_sys::Reflect::get(&out, &JsValue::from_str("data")).unwrap();
    Ok(js_sys::Uint8Array::new(&data).to_vec())
}

fn opaque(rgba: &[u8]) -> usize {
    rgba.chunks_exact(4).filter(|p| p[3] > 0).count()
}

#[wasm_bindgen_test]
async fn every_single_band_mode_renders_through_the_options_object() {
    let cog = fixture_cog().await;
    for options in [
        r#"{"mode": "colormap", "colormap": "turbo", "min": 0, "max": 600}"#,
        r#"{"mode": "hillshade", "azimuth": 300, "z_factor": 2, "hillshade_colormap": "viridis", "max": 600}"#,
        r#"{"mode": "contour", "contour_level": 5, "min": 0, "max": 600}"#,
        r##"{"mode": "classified", "stops": [[0, "#2166ac"], [300, [178, 24, 43]]]}"##,
        "",
    ] {
        let rgba = pixels(&cog, options).await.unwrap_or_else(|e| panic!("{options}: {e}"));
        assert_eq!(rgba.len(), 256 * 256 * 4, "{options}");
        assert!(opaque(&rgba) > 0, "{options}: nothing drawn");
    }
}

#[wasm_bindgen_test]
async fn rgb_is_accepted_and_then_refused_for_want_of_bands() {
    // The fixture has one band. Reaching the band check means the options parsed.
    let cog = fixture_cog().await;
    let e = pixels(&cog, r#"{"mode": "rgb", "rgb_max_r": 600}"#).await.unwrap_err();
    assert!(e.contains("3 bands"), "{e}");
}

#[wasm_bindgen_test]
async fn a_style_means_what_it_means_in_a_tile_url() {
    // Numbers as text, as they are in a query string, give the same pixels.
    let cog = fixture_cog().await;
    let a = pixels(&cog, r#"{"min": 0, "max": 600, "tile_size": 128}"#).await.unwrap();
    let b = pixels(&cog, r#"{"min": "0", "max": "600", "tile_size": 128}"#).await.unwrap();
    assert_eq!(a.len(), 128 * 128 * 4);
    assert_eq!(a, b);
}

#[wasm_bindgen_test]
async fn bad_options_say_what_is_wrong() {
    let cog = fixture_cog().await;
    let e = pixels(&cog, r#"{"mode": "hilshade"}"#).await.unwrap_err();
    assert!(e.contains("hilshade"), "{e}");
    let e = pixels(&cog, "{not json").await.unwrap_err();
    assert!(e.contains("bad tile options"), "{e}");
}

#[wasm_bindgen_test]
async fn a_mask_clears_what_is_outside_it() {
    let cog = fixture_cog().await;
    let whole = pixels(&cog, r#"{"max": 600}"#).await.unwrap();
    // A small square at the tile's centre. The raster covers only part of
    // this tile, but the centre is inside it (see info_says_where...).
    let (z, x, y) = TILE;
    let b = sabre_core::geo::tile_to_bbox(z, x, y);
    let (cx, cy) = ((b.west + b.east) / 2.0, (b.south + b.north) / 2.0);
    let (hw, hh) = ((b.east - b.west) / 16.0, (b.north - b.south) / 16.0);
    let (w, e, s, n) = (cx - hw, cx + hw, cy - hh, cy + hh);
    let wkt = format!("POLYGON(({w} {s},{e} {s},{e} {n},{w} {n},{w} {s}))");
    let options = format!(r#"{{"max": 600, "mask": "{wkt}"}}"#);
    let masked = pixels(&cog, &options).await.unwrap();
    assert!(opaque(&masked) > 0 && opaque(&masked) < opaque(&whole),
            "{} opaque masked against {} whole", opaque(&masked), opaque(&whole));
    // Same mask again comes from the cache and draws the same thing.
    assert_eq!(pixels(&cog, &options).await.unwrap(), masked);
}

#[wasm_bindgen_test]
async fn info_says_where_the_raster_is_and_how_fine() {
    let cog = fixture_cog().await;
    let v: serde_json::Value = serde_json::from_str(&cog.info().await.unwrap()).unwrap();
    let ext: Vec<f64> = serde_json::from_value(v["extent"].clone()).unwrap();
    let (x, y) = (ext[0]..ext[2], ext[1]..ext[3]);
    // The tile the other tests render sits inside it.
    let b = sabre_core::geo::tile_to_bbox(TILE.0, TILE.1, TILE.2);
    assert!(x.contains(&((b.west + b.east) / 2.0)) && y.contains(&((b.south + b.north) / 2.0)), "{ext:?}");
    // Sentinel-2 red is 10 m: Web Mercator reaches that near zoom 14 at this latitude.
    let zoom = v["native_zoom"].as_f64().unwrap();
    assert!((13.0..16.0).contains(&zoom), "native_zoom {zoom}");
}
