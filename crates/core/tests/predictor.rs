//! TIFF predictor (tag 317) decoding, checked against GDAL.
//!
//! The fixture is a real Sentinel-2 L2A red band — a 146×242 window over the
//! demo farm in the Eastern Cape, cut straight out of
//! `sentinel-cogs/sentinel-s2-l2a-cogs/35/H/LD/.../B04.tif` with
//! `gdal_translate -of COG -co PREDICTOR=2`. Every Sentinel-2 COG on AWS is
//! written with PREDICTOR=2, so this is the exact byte layout production reads.
//!
//! The reference numbers below come from GDAL decoding the same file. A reader
//! that ignores the predictor does not fail; it returns a plausible-looking
//! raster with the wrong values, which is why the assertions are exact rather
//! than approximate.

use async_trait::async_trait;
use sabre_core::cog::{decode_tile, fetch_meta, RangeReader};

struct MemReader(Vec<u8>);

#[async_trait(?Send)]
impl RangeReader for MemReader {
    async fn read_range(&self, offset: u64, length: u64) -> Result<Vec<u8>, String> {
        let start = offset as usize;
        let end = (start + length as usize).min(self.0.len());
        Ok(if start >= self.0.len() { vec![] } else { self.0[start..end].to_vec() })
    }
}

const FIXTURE: &[u8] = include_bytes!("fixtures/s2_b04_predictor2.tif");
const WIDTH:  usize = 146;
const HEIGHT: usize = 242;

// GDAL's decode of the same file:
//   gdal_translate -of ENVI s2_b04_predictor2.tif ref.img
const REF_MIN:      u16 = 47;
const REF_MAX:      u16 = 528;
const REF_SUM:      u64 = 6_447_225;
// Sum of (index × value), so a raster with the right histogram laid out wrongly
// — a stride slip, a transposed row — still fails.
const REF_WEIGHTED: u64 = 111_629_575_354;

#[test]
fn sentinel2_predictor2_matches_gdal() {
    pollster::block_on(async {
    let reader = MemReader(FIXTURE.to_vec());
    let meta = fetch_meta(&reader).await.expect("parse COG header");
    let ifd = &meta.ifds[0];

    assert_eq!(ifd.predictor, 2, "fixture should carry tag 317 = 2");
    assert_eq!(ifd.epsg_code, Some(32735), "Sentinel-2 scenes are UTM, not WGS84");
    assert_eq!(ifd.compression, 8, "DEFLATE");
    assert_eq!((ifd.image_width, ifd.image_height), (WIDTH as u32, HEIGHT as u32));

    let tile_w = ifd.tile_width.unwrap() as usize;
    let raw = reader
        .read_range(ifd.tile_offsets[0], ifd.tile_byte_counts[0])
        .await
        .unwrap();
    let decoded = decode_tile(
        &raw, ifd.compression, ifd.predictor,
        tile_w as u32, ifd.tile_height.unwrap(),
        ifd.samples_per_pixel, ifd.bits_per_sample[0],
        &Default::default(),
    )
    .expect("decode");

    // The tile is padded out to 256×256; only the top-left image region is real.
    let mut min = u16::MAX;
    let mut max = 0u16;
    let mut sum = 0u64;
    let mut weighted = 0u64;
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let b = (y * tile_w + x) * 2;
            let v = u16::from_le_bytes([decoded[b], decoded[b + 1]]);
            min = min.min(v);
            max = max.max(v);
            sum += v as u64;
            weighted += (y * WIDTH + x) as u64 * v as u64;
        }
    }

    assert_eq!(min, REF_MIN, "minimum reflectance");
    assert_eq!(max, REF_MAX, "maximum reflectance");
    assert_eq!(sum, REF_SUM, "sum over all pixels");
    assert_eq!(weighted, REF_WEIGHTED, "position-weighted sum");
    });
}

/// Ignoring the predictor is not a safe failure mode: it still decodes, and the
/// numbers it produces are wrong but not obviously so. This pins that, so the
/// test above cannot be satisfied by accident.
#[test]
fn ignoring_the_predictor_silently_changes_the_values() {
    pollster::block_on(async {
    let reader = MemReader(FIXTURE.to_vec());
    let meta = fetch_meta(&reader).await.unwrap();
    let ifd = &meta.ifds[0];
    let raw = reader
        .read_range(ifd.tile_offsets[0], ifd.tile_byte_counts[0])
        .await
        .unwrap();

    let ignored = decode_tile(
        &raw, ifd.compression, 1, // pretend tag 317 says "none"
        ifd.tile_width.unwrap(), ifd.tile_height.unwrap(),
        ifd.samples_per_pixel, ifd.bits_per_sample[0],
        &Default::default(),
    )
    .unwrap();

    let applied = decode_tile(
        &raw, ifd.compression, ifd.predictor,
        ifd.tile_width.unwrap(), ifd.tile_height.unwrap(),
        ifd.samples_per_pixel, ifd.bits_per_sample[0],
        &Default::default(),
    )
    .unwrap();

    assert_eq!(ignored.len(), applied.len(), "same byte count either way");
    assert_ne!(ignored, applied, "the predictor has to change something");
    });
}

// ── Synthetic round-trips ────────────────────────────────────────────────────
//
// The fixture covers uint16 with one sample. These cover the shapes it does
// not: multi-band interleaving, 8-bit, and the floating-point predictor.

/// Encode with horizontal differencing, the operation `undo_predictor` reverses.
fn encode_p2_u16(rows: &[Vec<u16>], samples: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for row in rows {
        let mut enc = row.clone();
        for i in (samples..row.len()).rev() {
            enc[i] = row[i].wrapping_sub(row[i - samples]);
        }
        for v in enc {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
    out
}

#[test]
fn predictor2_round_trips_three_band_u16() {
    // 4 px × 3 bands: each band must accumulate along its own track.
    let rows = vec![
        vec![10u16, 200, 3000, 11, 201, 3001, 12, 202, 3002, 13, 203, 3003],
        vec![65_530, 5, 0, 65_531, 6, 1, 65_532, 7, 2, 65_533, 8, 3], // wraps
    ];
    let encoded = encode_p2_u16(&rows, 3);
    let decoded = decode_tile(&encoded, 1, 2, 4, 2, 3, 16, &Default::default()).unwrap();

    let got: Vec<u16> = decoded
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let want: Vec<u16> = rows.concat();
    assert_eq!(got, want);
}

#[test]
fn predictor2_round_trips_single_band_u8() {
    let row: Vec<u8> = (0..8u8).map(|i| i.wrapping_mul(37)).collect();
    let mut enc = row.clone();
    for i in (1..row.len()).rev() {
        enc[i] = row[i].wrapping_sub(row[i - 1]);
    }
    let decoded = decode_tile(&enc, 1, 2, 8, 1, 1, 8, &Default::default()).unwrap();
    assert_eq!(decoded, row);
}

#[test]
fn predictor3_round_trips_f32() {
    // libtiff's float predictor: byte-plane split, then byte differencing.
    let row = [1.0f32, -2.5, 3.25, 1e6];
    let bps = 4usize;
    let n = row.len();
    let flat: Vec<u8> = row.iter().flat_map(|v| v.to_le_bytes()).collect();

    // Split into planes, most-significant byte plane first.
    let mut planes = vec![0u8; flat.len()];
    for s in 0..n {
        for b in 0..bps {
            planes[b * n + s] = flat[bps * s + bps - b - 1];
        }
    }
    // Difference across the whole row at byte granularity, stride = samples.
    let mut enc = planes.clone();
    for i in (1..planes.len()).rev() {
        enc[i] = planes[i].wrapping_sub(planes[i - 1]);
    }

    let decoded = decode_tile(&enc, 1, 3, n as u32, 1, 1, 32, &Default::default()).unwrap();
    let got: Vec<f32> = decoded
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    assert_eq!(got, row);
}

#[test]
fn predictor_zero_and_one_are_both_no_op() {
    let data: Vec<u8> = (0..16u8).collect();
    for p in [0u16, 1] {
        let out = decode_tile(&data, 1, p, 16, 1, 1, 8, &Default::default()).unwrap();
        assert_eq!(out, data, "predictor {p} must leave the buffer alone");
    }
}

#[test]
fn unsupported_predictor_is_an_error_not_garbage() {
    let data: Vec<u8> = (0..16u8).collect();
    let err = decode_tile(&data, 1, 42, 16, 1, 1, 8, &Default::default()).unwrap_err();
    assert!(err.contains("42"), "message should name the predictor: {err}");
}
