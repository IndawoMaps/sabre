//! Tiles have to agree with each other about where things are.
//!
//! A 512-pixel tile and its four 256-pixel children cover the same ground at
//! the same resolution, so the children laid side by side must be the parent,
//! pixel for pixel. Every tile boundary is a place two renders meet, and any
//! disagreement there is a visible seam on a map.
//!
//! The fixture is in UTM (EPSG:32735), where a Web Mercator tile's footprint
//! is slightly rotated -- about 0.8° at this spot, 1.4° west of the zone's
//! central meridian. Treating it as axis-aligned stretches each tile over the
//! bounding box of its corners and shifts every edge by a few pixels, which is
//! the seam this guards against.

use async_trait::async_trait;
use sabre_core::cog::{fetch_meta, RangeReader};
use sabre_core::render::{render_tile_rgba_timed, StyleMode, TileRequest};
use sabre_core::timing::Timings;

struct MemReader(&'static [u8]);

#[async_trait(?Send)]
impl RangeReader for MemReader {
    async fn read_range(&self, offset: u64, length: u64) -> Result<Vec<u8>, String> {
        let start = (offset as usize).min(self.0.len());
        let end = (start + length as usize).min(self.0.len());
        Ok(self.0[start..end].to_vec())
    }
}

const FIXTURE: &[u8] = include_bytes!("fixtures/s2_b04_predictor2.tif");

fn render(z: u32, x: u32, y: u32, tile_size: u32) -> Vec<u8> {
    let reader = MemReader(FIXTURE);
    pollster::block_on(async {
        let meta = fetch_meta(&reader).await.unwrap();
        let req = TileRequest {
            z, x, y, tile_size,
            // A fine ramp, so neighbouring source pixels rarely share a colour.
            style: StyleMode::Colormap { name: "turbo".into(), min: 40.0, max: 540.0, nodata: None },
            bilinear: false,
            mask: None,
        };
        render_tile_rgba_timed(&req, &reader, &meta, &Timings::off()).await.unwrap()
    })
}

/// Pixels compared, and how many differ, where both renders drew something.
fn compare_to_children(z: u32, x: u32, y: u32) -> (usize, usize) {
    let parent = render(z, x, y, 512);
    let (mut compared, mut differ) = (0, 0);
    for (cx, cy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
        let child = render(z + 1, 2 * x + cx, 2 * y + cy, 256);
        for row in 0..256 {
            for col in 0..256 {
                let c = &child[(row * 256 + col) * 4..][..4];
                let (pr, pc) = (cy as usize * 256 + row, cx as usize * 256 + col);
                let p = &parent[(pr * 512 + pc) * 4..][..4];
                if c[3] == 0 || p[3] == 0 {
                    continue;
                }
                compared += 1;
                differ += (c != p) as usize;
            }
        }
    }
    (compared, differ)
}

#[test]
fn children_laid_side_by_side_are_the_parent() {
    // Over the fixture at 10 m: z15 is a little coarser than the raster, z16
    // and z17 finer, so this covers downsampling and stretching both.
    for (z, x, y) in [(15, 18707, 19607), (16, 37414, 39214), (17, 74828, 78428)] {
        let (compared, differ) = compare_to_children(z, x, y);
        assert!(compared > 10_000, "z{z}: only {compared} pixels overlap the fixture");
        // A pixel centre that falls within a hair of a source pixel's edge can
        // round either way; anything past that is geometry, not rounding.
        let share = differ as f64 / compared as f64;
        eprintln!("z{z}/{x}/{y}: {differ} of {compared} pixels differ");
        assert!(share < 0.005, "z{z}/{x}/{y}: {differ} of {compared} pixels ({:.1}%) differ", share * 100.0);
    }
}
