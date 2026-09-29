use async_trait::async_trait;
use sabre_core::cog::{fetch_meta, RangeReader};
use sabre_core::render::{render_tile, StyleMode, TileRequest};

// ── Mock reader ───────────────────────────────────────────────────────────────

struct MemReader(Vec<u8>);

#[async_trait(?Send)]
impl RangeReader for MemReader {
    async fn read_range(&self, offset: u64, length: u64) -> Result<Vec<u8>, String> {
        let start = offset as usize;
        let end = (start + length as usize).min(self.0.len());
        Ok(if start >= self.0.len() { vec![] } else { self.0[start..end].to_vec() })
    }
}

// ── GeoTIFF builder ───────────────────────────────────────────────────────────
//
// Minimal single-band float32 GeoTIFF: little-endian, no compression, one strip.
// No EPSG tag → render pipeline treats coordinates as WGS84 passthrough.
//
// Layout:
//   0   header (8)
//   8   IFD count (2) + 14 entries×12 (168) + next IFD=0 (4)  →  total 174
//   182 ModelPixelScaleTag  3×f64=24
//   206 ModelTiepointTag    6×f64=48
//   254 pixel data          width×height×4 bytes (float32 LE)

fn make_geotiff(
    pixels: &[f32],
    width: u32,
    height: u32,
    west: f64,
    north: f64,
    pixel_size: f64,
    nodata: f32,
) -> Vec<u8> {
    assert_eq!(pixels.len(), (width * height) as usize);

    const SCALE_OFFSET: u32 = 182;
    const TIEPOINT_OFFSET: u32 = 206;
    const DATA_OFFSET: u32 = 254;
    let pixel_data_len = width * height * 4;

    // GDAL_NODATA is ASCII; "0\0" fits inline for nodata=0.0
    let nodata_str = format!("{nodata}\0");
    let nd_bytes = nodata_str.as_bytes();
    assert!(nd_bytes.len() <= 4, "nodata string too long for inline storage");
    let nd_count = nd_bytes.len() as u32;
    let mut nd_inline = [0u8; 4];
    for (i, &b) in nd_bytes.iter().enumerate() { nd_inline[i] = b; }

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

    // Header
    buf.extend_from_slice(b"II");
    buf.extend_from_slice(&42u16.to_le_bytes());
    buf.extend_from_slice(&8u32.to_le_bytes());

    // IFD — 14 entries, sorted ascending by tag
    buf.extend_from_slice(&14u16.to_le_bytes());
    buf.extend_from_slice(&entry(256,   4,  1, u32v(width)));             // ImageWidth
    buf.extend_from_slice(&entry(257,   4,  1, u32v(height)));            // ImageHeight
    buf.extend_from_slice(&entry(258,   3,  1, u16v(32)));                // BitsPerSample
    buf.extend_from_slice(&entry(259,   3,  1, u16v(1)));                 // Compression (none)
    buf.extend_from_slice(&entry(262,   3,  1, u16v(1)));                 // PhotometricInterpretation
    buf.extend_from_slice(&entry(273,   4,  1, u32v(DATA_OFFSET)));       // StripOffsets
    buf.extend_from_slice(&entry(277,   3,  1, u16v(1)));                 // SamplesPerPixel
    buf.extend_from_slice(&entry(278,   4,  1, u32v(height)));            // RowsPerStrip
    buf.extend_from_slice(&entry(279,   4,  1, u32v(pixel_data_len)));    // StripByteCounts
    buf.extend_from_slice(&entry(284,   3,  1, u16v(1)));                 // PlanarConfiguration
    buf.extend_from_slice(&entry(339,   3,  1, u16v(3)));                 // SampleFormat (float32)
    buf.extend_from_slice(&entry(33550, 12, 3, u32v(SCALE_OFFSET)));      // ModelPixelScaleTag
    buf.extend_from_slice(&entry(33922, 12, 6, u32v(TIEPOINT_OFFSET)));   // ModelTiepointTag
    buf.extend_from_slice(&entry(42113, 2, nd_count, nd_inline));         // GDAL_NODATA
    buf.extend_from_slice(&0u32.to_le_bytes());                           // next IFD = 0

    assert_eq!(buf.len(), 182);

    // ModelPixelScaleTag: [scale_x, scale_y, 0]
    buf.extend_from_slice(&pixel_size.to_le_bytes());
    buf.extend_from_slice(&pixel_size.to_le_bytes());
    buf.extend_from_slice(&0.0f64.to_le_bytes());

    // ModelTiepointTag: pixel(0,0,0) → geo(west, north, 0)
    for v in [0.0f64, 0.0, 0.0, west, north, 0.0] {
        buf.extend_from_slice(&v.to_le_bytes());
    }

    assert_eq!(buf.len(), 254);

    for &p in pixels { buf.extend_from_slice(&p.to_le_bytes()); }
    buf
}

// ── Shape definitions ─────────────────────────────────────────────────────────
//
// Each shape is a 25×25 grid (25 km × 25 km, 1 km/pixel).
// Tiff is anchored at west=10°, north=0.225°, pixel_size=0.009°/px.
// Background pixels = 0.0 (nodata → transparent).  Shape pixels = 1.0–10.0.

/// L-shape: vertical stroke cols 0–7, horizontal stroke rows 17–24.
/// Value = 1 + 9 × (row / 24)  → blue at top, yellow at bottom (viridis).
fn l_shape() -> Vec<f32> {
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

/// O-shape: ring spanning rows 4–20, cols 4–20, border ≤ 2 px thick.
/// Value = 1 + 9 × ((col − 4) / 16)  → blue on left, yellow on right.
fn o_shape() -> Vec<f32> {
    let mut px = vec![0.0f32; 25 * 25];
    for row in 4..=20usize {
        for col in 4..=20usize {
            if row <= 6 || row >= 18 || col <= 6 || col >= 18 {
                px[row * 25 + col] = 1.0 + 9.0 * (col - 4) as f32 / 16.0;
            }
        }
    }
    px
}

// ── Render helper ─────────────────────────────────────────────────────────────
//
// Tile z=9, x=270, y=255 covers ≈ 9.84°–10.55° E, 0°–0.35° N,
// which fully contains our 25×25 tiff (10°–10.225° E, 0°–0.225° N).

const TILE: (u32, u32, u32) = (9, 270, 255);

async fn render_shape(pixels: &[f32]) -> Vec<u8> {
    let tiff = make_geotiff(pixels, 25, 25, 10.0, 0.225, 0.009, 0.0);
    let reader = MemReader(tiff);
    let meta = fetch_meta(&reader).await.expect("fetch_meta failed");
    let req = TileRequest {
        z: TILE.0, x: TILE.1, y: TILE.2,
        tile_size: 256,
        style: StyleMode::Colormap {
            name: "viridis".to_string(),
            min: 1.0,
            max: 10.0,
            nodata: Some(0.0),
        },
        bilinear: false,
        mask: None,
    };
    render_tile(&req, &reader, &meta).await.expect("render_tile failed")
}

// ── Fixture helpers ───────────────────────────────────────────────────────────

fn fixture_path(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn read_fixture(name: &str) -> Vec<u8> {
    let path = fixture_path(name);
    std::fs::read(&path).unwrap_or_else(|_| {
        panic!("fixture not found: {path}\nRun: cargo test -p sabre-core -- --ignored generate_fixtures")
    })
}

fn decode_png_rgba(data: &[u8]) -> (Vec<u8>, u32, u32) {
    use image::codecs::png::PngDecoder;
    use image::ImageDecoder;
    let dec = PngDecoder::new(std::io::Cursor::new(data)).expect("invalid PNG");
    let (w, h) = dec.dimensions();
    let mut rgba = vec![0u8; dec.total_bytes() as usize];
    dec.read_image(&mut rgba).expect("PNG decode failed");
    (rgba, w, h)
}

// ── generate_fixtures (run once to write golden files) ────────────────────────

/// Write (or overwrite) golden PNG fixtures.
/// Run with: cargo test -p sabre-core -- --ignored generate_fixtures
#[test]
#[ignore]
fn generate_fixtures() {
    pollster::block_on(async {
        let dir = format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR"));
        std::fs::create_dir_all(&dir).unwrap();

        let png = render_shape(&l_shape()).await;
        std::fs::write(format!("{dir}/l_shape_viridis.png"), &png).unwrap();
        std::fs::write(
            format!("{dir}/l_shape.tiff"),
            make_geotiff(&l_shape(), 25, 25, 10.0, 0.225, 0.009, 0.0),
        )
        .unwrap();

        let png = render_shape(&o_shape()).await;
        std::fs::write(format!("{dir}/o_shape_viridis.png"), &png).unwrap();
        std::fs::write(
            format!("{dir}/o_shape.tiff"),
            make_geotiff(&o_shape(), 25, 25, 10.0, 0.225, 0.009, 0.0),
        )
        .unwrap();
    });
}

// ── Sanity checks ────────────────────────────────────────────────────────────

#[test]
fn l_shape_has_opaque_pixels_in_expected_region() {
    // The tiff occupies output cols ~57-141, rows ~174-255 within the 256x256 tile.
    // The L vertical arm (tiff cols 0-7) is roughly output cols 57-82.
    pollster::block_on(async {
        let rendered = render_shape(&l_shape()).await;
        let (pixels, w, h) = decode_png_rgba(&rendered);
        assert_eq!((w, h), (256, 256));
        let opaque: usize = pixels.chunks(4).filter(|p| p[3] == 255).count();
        assert!(opaque > 1000, "expected thousands of opaque pixels, got {opaque}");
        // Verify transparent outside tiff extent (top-left corner should be transparent)
        let top_left = &pixels[0..4]; // row=0, col=0
        assert_eq!(top_left[3], 0, "top-left pixel should be transparent");
    });
}

// ── Snapshot tests ────────────────────────────────────────────────────────────

#[test]
fn snapshot_l_shape_viridis() {
    pollster::block_on(async {
        let rendered = render_shape(&l_shape()).await;
        let expected = read_fixture("l_shape_viridis.png");
        let (r, rw, rh) = decode_png_rgba(&rendered);
        let (e, ew, eh) = decode_png_rgba(&expected);
        assert_eq!((rw, rh), (ew, eh), "tile size mismatch");
        assert_eq!(r, e, "l_shape_viridis pixel mismatch — re-run generate_fixtures if intentional");
    });
}

#[test]
fn snapshot_o_shape_viridis() {
    pollster::block_on(async {
        let rendered = render_shape(&o_shape()).await;
        let expected = read_fixture("o_shape_viridis.png");
        let (r, rw, rh) = decode_png_rgba(&rendered);
        let (e, ew, eh) = decode_png_rgba(&expected);
        assert_eq!((rw, rh), (ew, eh), "tile size mismatch");
        assert_eq!(r, e, "o_shape_viridis pixel mismatch — re-run generate_fixtures if intentional");
    });
}
