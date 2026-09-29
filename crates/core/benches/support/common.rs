#![allow(dead_code)]
/// Shared benchmark helpers.
///
/// Included by render_bench.rs and query_bench.rs via `#[path = "common.rs"] mod common;`.
///
/// ## Env vars
/// - `SABRE_BENCH_FILE`  — path to a GeoTIFF; if unset the synthetic l_shape fixture is used
/// - `SABRE_BENCH_ZOOM`  — tile zoom level for render benchmarks (default 8)
use async_trait::async_trait;
use sabre_core::cog::{fetch_meta, CogMeta, RangeReader};
use sabre_core::geo::{geo_extent_wgs84, tile_to_bbox, Bbox};
use std::io::{Read, Seek, SeekFrom};

// ── In-memory reader ──────────────────────────────────────────────────────────

pub struct MemReader(pub Vec<u8>);

#[async_trait(?Send)]
impl RangeReader for MemReader {
    async fn read_range(&self, offset: u64, length: u64) -> Result<Vec<u8>, String> {
        let start = offset as usize;
        let end = (start + length as usize).min(self.0.len());
        Ok(if start >= self.0.len() { vec![] } else { self.0[start..end].to_vec() })
    }
}

// ── File-backed reader (one open per call; file lives in OS page cache) ───────

pub struct FileReader(pub std::path::PathBuf);

#[async_trait(?Send)]
impl RangeReader for FileReader {
    async fn read_range(&self, offset: u64, length: u64) -> Result<Vec<u8>, String> {
        let mut f = std::fs::File::open(&self.0).map_err(|e| e.to_string())?;
        f.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
        let mut buf = vec![0u8; length as usize];
        f.read_exact(&mut buf).map_err(|e| e.to_string())?;
        Ok(buf)
    }
}

// ── Unified reader enum ───────────────────────────────────────────────────────

pub enum BenchReader {
    Mem(MemReader),
    File(FileReader),
}

#[async_trait(?Send)]
impl RangeReader for BenchReader {
    async fn read_range(&self, offset: u64, length: u64) -> Result<Vec<u8>, String> {
        match self {
            BenchReader::Mem(r) => r.read_range(offset, length).await,
            BenchReader::File(r) => r.read_range(offset, length).await,
        }
    }
}

// ── Benchmark configuration resolved at setup time ───────────────────────────

pub struct BenchConfig {
    pub reader: BenchReader,
    pub meta: CogMeta,
    /// WGS84 extent of the source raster.
    pub extent: Bbox,
    /// Tiles that intersect the raster at the requested zoom level.
    pub tiles: Vec<(u32, u32, u32)>,
    pub nodata: Option<f32>,
    pub vmin: f32,
    pub vmax: f32,
}

/// Build a `BenchConfig` from env vars, falling back to the synthetic l_shape fixture.
pub fn setup(zoom_override: Option<u32>) -> BenchConfig {
    let zoom = zoom_override
        .or_else(|| std::env::var("SABRE_BENCH_ZOOM").ok()?.parse().ok())
        .unwrap_or(8);

    match std::env::var("SABRE_BENCH_FILE") {
        Ok(path) => setup_from_file(&path, zoom),
        Err(_)   => setup_from_fixture(zoom),
    }
}

fn setup_from_file(path: &str, zoom: u32) -> BenchConfig {
    let reader = BenchReader::File(FileReader(path.into()));
    let meta = pollster::block_on(fetch_meta(&reader)).expect("failed to read COG metadata");
    let ifd = meta.ifd(0);

    let gt = ifd.geo_transform().expect("COG has no geotransform");
    let extent = geo_extent_wgs84(&gt, ifd.image_width, ifd.image_height, ifd.epsg_code)
        .expect("failed to compute WGS84 extent");

    let tiles = tiles_in_extent(&extent, zoom);
    assert!(!tiles.is_empty(), "no tiles at zoom {zoom} cover the raster extent {extent:?}");

    let nodata: Option<f32> = ifd.nodata.as_deref().and_then(|s| s.parse().ok());
    let (stat_min, stat_max) = ifd.data_stats();
    let vmin = stat_min.unwrap_or(0.0) as f32;
    let vmax = stat_max.unwrap_or(1.0) as f32;

    eprintln!(
        "[bench] using file: {path}  |  zoom={zoom}  |  {} tiles  |  \
         extent=[{:.4},{:.4},{:.4},{:.4}]  |  vmin={vmin}  vmax={vmax}",
        tiles.len(), extent.west, extent.south, extent.east, extent.north,
    );

    BenchConfig { reader, meta, extent, tiles, nodata, vmin, vmax }
}

fn setup_from_fixture(zoom: u32) -> BenchConfig {
    let tiff = make_geotiff(&l_shape(), 25, 25, 10.0, 0.225, 0.009, 0.0);
    let extent = Bbox { west: 10.0, south: 0.0, east: 10.225, north: 0.225 };
    let tiles = tiles_in_extent(&extent, zoom);
    let reader = BenchReader::Mem(MemReader(tiff));
    let meta = pollster::block_on(fetch_meta(&reader)).expect("failed to read fixture metadata");
    eprintln!(
        "[bench] using synthetic l_shape fixture  |  zoom={zoom}  |  {} tiles",
        tiles.len()
    );
    BenchConfig {
        reader,
        meta,
        extent,
        tiles,
        nodata: Some(0.0),
        vmin: 1.0,
        vmax: 10.0,
    }
}

// ── Tile discovery ────────────────────────────────────────────────────────────

/// All XYZ tiles at `zoom` whose bbox overlaps `extent`.
pub fn tiles_in_extent(extent: &Bbox, zoom: u32) -> Vec<(u32, u32, u32)> {
    use std::f64::consts::PI;
    let n = 2u32.pow(zoom) as f64;

    let lon_to_x = |lon: f64| ((lon + 180.0) / 360.0 * n).floor() as u32;
    let lat_to_y = |lat: f64| {
        let rad = lat.to_radians();
        ((1.0 - (rad.tan() + 1.0 / rad.cos()).ln() / PI) / 2.0 * n).floor() as u32
    };

    let x0 = lon_to_x(extent.west).saturating_sub(0).min(n as u32 - 1);
    let x1 = lon_to_x(extent.east).min(n as u32 - 1);
    let y0 = lat_to_y(extent.north).min(n as u32 - 1);
    let y1 = lat_to_y(extent.south).min(n as u32 - 1);

    let mut tiles = Vec::new();
    for x in x0..=x1 {
        for y in y0..=y1 {
            let bb = tile_to_bbox(zoom, x, y);
            if bb.east > extent.west && bb.west < extent.east
                && bb.north > extent.south && bb.south < extent.north
            {
                tiles.push((zoom, x, y));
            }
        }
    }
    tiles
}

// ── Polygon generators ────────────────────────────────────────────────────────

/// Generate `n` random bounding-box polygons (WGS84) inside `extent`.
/// Uses a deterministic LCG so results are reproducible.
pub fn make_polygons(extent: &Bbox, n: usize, size_deg: f64) -> Vec<String> {
    let mut state: u64 = 42;
    let mut rng = move || -> f64 {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 33) as f64 / u32::MAX as f64
    };

    let margin = size_deg / 2.0;
    let span_x = (extent.east - extent.west - size_deg).max(0.0);
    let span_y = (extent.north - extent.south - size_deg).max(0.0);

    (0..n)
        .map(|_| {
            let cx = extent.west + margin + rng() * span_x;
            let cy = extent.south + margin + rng() * span_y;
            let (w, s, e, nn) = (cx - margin, cy - margin, cx + margin, cy + margin);
            format!("POLYGON(({w} {s}, {e} {s}, {e} {nn}, {w} {nn}, {w} {s}))")
        })
        .collect()
}

// ── Synthetic fixture builder (mirrors make_geotiff in snapshot.rs) ───────────

pub fn make_geotiff(
    pixels: &[f32],
    width: u32,
    height: u32,
    west: f64,
    north: f64,
    pixel_size: f64,
    nodata: f32,
) -> Vec<u8> {
    const SCALE_OFFSET: u32 = 182;
    const TIEPOINT_OFFSET: u32 = 206;
    const DATA_OFFSET: u32 = 254;
    let pixel_data_len = width * height * 4;

    let nodata_str = format!("{nodata}\0");
    let nd_bytes = nodata_str.as_bytes();
    let nd_count = nd_bytes.len() as u32;
    let mut nd_inline = [0u8; 4];
    for (i, &b) in nd_bytes.iter().enumerate() {
        nd_inline[i] = b;
    }

    fn entry(tag: u16, typ: u16, count: u32, val: [u8; 4]) -> [u8; 12] {
        let mut e = [0u8; 12];
        e[0..2].copy_from_slice(&tag.to_le_bytes());
        e[2..4].copy_from_slice(&typ.to_le_bytes());
        e[4..8].copy_from_slice(&count.to_le_bytes());
        e[8..12].copy_from_slice(&val);
        e
    }
    fn u16v(v: u16) -> [u8; 4] { let b = v.to_le_bytes(); [b[0], b[1], 0, 0] }
    fn u32v(v: u32) -> [u8; 4] { v.to_le_bytes() }

    let mut buf = Vec::with_capacity((DATA_OFFSET + pixel_data_len) as usize);
    buf.extend_from_slice(b"II");
    buf.extend_from_slice(&42u16.to_le_bytes());
    buf.extend_from_slice(&8u32.to_le_bytes());
    buf.extend_from_slice(&14u16.to_le_bytes());
    buf.extend_from_slice(&entry(256,   4,  1, u32v(width)));
    buf.extend_from_slice(&entry(257,   4,  1, u32v(height)));
    buf.extend_from_slice(&entry(258,   3,  1, u16v(32)));
    buf.extend_from_slice(&entry(259,   3,  1, u16v(1)));
    buf.extend_from_slice(&entry(262,   3,  1, u16v(1)));
    buf.extend_from_slice(&entry(273,   4,  1, u32v(DATA_OFFSET)));
    buf.extend_from_slice(&entry(277,   3,  1, u16v(1)));
    buf.extend_from_slice(&entry(278,   4,  1, u32v(height)));
    buf.extend_from_slice(&entry(279,   4,  1, u32v(pixel_data_len)));
    buf.extend_from_slice(&entry(284,   3,  1, u16v(1)));
    buf.extend_from_slice(&entry(339,   3,  1, u16v(3)));
    buf.extend_from_slice(&entry(33550, 12, 3, u32v(SCALE_OFFSET)));
    buf.extend_from_slice(&entry(33922, 12, 6, u32v(TIEPOINT_OFFSET)));
    buf.extend_from_slice(&entry(42113, 2, nd_count, nd_inline));
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&pixel_size.to_le_bytes());
    buf.extend_from_slice(&pixel_size.to_le_bytes());
    buf.extend_from_slice(&0.0f64.to_le_bytes());
    for v in [0.0f64, 0.0, 0.0, west, north, 0.0] {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    for &p in pixels {
        buf.extend_from_slice(&p.to_le_bytes());
    }
    buf
}

pub fn l_shape() -> Vec<f32> {
    let mut px = vec![0.0f32; 25 * 25];
    for row in 0..25usize {
        for col in 0..25usize {
            if col < 8 || row >= 17 {
                px[row * 25 + col] = 1.0 + 9.0 * row as f32 / 24.0;
            }
        }
    }
    px
}
