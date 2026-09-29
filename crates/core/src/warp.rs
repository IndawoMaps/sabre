//! Where each pixel of a Web Mercator tile lands in the raster's own CRS.
//!
//! A tile is square in Web Mercator and almost never square in the raster's
//! CRS. In UTM its footprint is rotated by the grid convergence -- about 0.8°
//! for a farm 1.4° off its zone's central meridian -- and in WGS84 its rows
//! are unevenly spaced in latitude. Mapping the tile linearly onto the
//! bounding box of its reprojected corners gets both wrong: it stretches every
//! tile over a box larger than its footprint, and puts each tile's edges a few
//! pixels from where its neighbours put the same edge. On a map that is a
//! seam at every tile boundary.
//!
//! Reprojecting every pixel is exact and costs tens of milliseconds a tile.
//! This reprojects a grid of control points instead and interpolates between
//! them, refining the grid until interpolation is within [`MAX_ERROR_PX`] of
//! the exact answer -- GDAL's approximate transformer does the same. Control
//! points include the tile's corners and edges, which neighbouring tiles
//! compute identically, so tiles meet exactly where they touch.

use crate::geo::{epsg_to_proj4, Bbox};

/// Largest tolerated interpolation error, in output pixels. GDAL's default
/// for its approximate transformer is 0.125.
pub const MAX_ERROR_PX: f64 = 0.125;

const HALF: f64 = 20037508.342789244;
const RADIUS: f64 = 6378137.0;

/// The tile's pixel grid, reprojected into the raster's CRS.
pub struct TileWarp {
    size: f64,
    step: f64,
    /// Control points per side.
    n: usize,
    /// Row-major, `n × n`, in the raster's CRS. NaN where the projection has
    /// no answer.
    pts: Vec<(f64, f64)>,
}

/// From one tile's pixel coordinates to the raster's CRS, exactly.
struct Exact {
    z: u32,
    x: u32,
    y: u32,
    size: f64,
    to: Target,
}

enum Target {
    Wgs84,
    WebMercator,
    Proj(Box<(proj4rs::Proj, proj4rs::Proj)>),
}

impl Exact {
    fn new(z: u32, x: u32, y: u32, tile_size: u32, epsg: Option<u32>) -> Result<Self, String> {
        let to = match epsg {
            None | Some(4326) | Some(4269) => Target::Wgs84,
            Some(3857) | Some(900913) => Target::WebMercator,
            Some(epsg) => {
                let def = epsg_to_proj4(epsg)
                    .ok_or_else(|| format!("EPSG:{epsg} is not supported"))?;
                let src = proj4rs::Proj::from_proj_string("+proj=longlat +datum=WGS84 +no_defs")
                    .map_err(|e| format!("WGS84 proj error: {e}"))?;
                let dst = proj4rs::Proj::from_proj_string(&def)
                    .map_err(|e| format!("EPSG:{epsg} proj error: {e}"))?;
                Target::Proj(Box::new((src, dst)))
            }
        };
        Ok(Exact { z, x, y, size: tile_size as f64, to })
    }

    /// Pixel coordinates within the tile, `0..=size` on each axis, to the
    /// raster's CRS. `(0, 0)` is the tile's north-west corner.
    fn at(&self, px: f64, py: f64) -> (f64, f64) {
        let tiles = (1u64 << self.z) as f64;
        let mx = -HALF + (self.x as f64 + px / self.size) / tiles * 2.0 * HALF;
        let my = HALF - (self.y as f64 + py / self.size) / tiles * 2.0 * HALF;
        if let Target::WebMercator = self.to {
            return (mx, my);
        }
        let lon = mx / RADIUS;
        let lat = (my / RADIUS).sinh().atan();
        match &self.to {
            Target::Wgs84 => (lon.to_degrees(), lat.to_degrees()),
            Target::Proj(pair) => {
                let mut p = (lon, lat, 0.0);
                match proj4rs::transform::transform(&pair.0, &pair.1, &mut p) {
                    Ok(()) if p.0.is_finite() && p.1.is_finite() => (p.0, p.1),
                    _ => (f64::NAN, f64::NAN),
                }
            }
            Target::WebMercator => unreachable!(),
        }
    }
}

impl TileWarp {
    pub fn new(z: u32, x: u32, y: u32, tile_size: u32, epsg: Option<u32>) -> Result<Self, String> {
        let exact = Exact::new(z, x, y, tile_size, epsg)?;
        let size = tile_size as f64;
        // Start at 32-pixel cells and halve until interpolation is close
        // enough. UTM at farm zooms settles at the first try; Web Mercator's
        // latitude curve at z0-2 needs a few more rounds.
        let mut step = 32.0f64.min(size);
        loop {
            let warp = Self::sample(&exact, size, step);
            if step <= 1.0 || warp.max_error(&exact) <= MAX_ERROR_PX {
                return Ok(warp);
            }
            step /= 2.0;
        }
    }

    fn sample(exact: &Exact, size: f64, step: f64) -> Self {
        let n = (size / step).ceil() as usize + 1;
        let mut pts = Vec::with_capacity(n * n);
        for j in 0..n {
            let py = (j as f64 * step).min(size);
            for i in 0..n {
                let px = (i as f64 * step).min(size);
                pts.push(exact.at(px, py));
            }
        }
        TileWarp { size, step, n, pts }
    }

    /// Worst interpolation error at cell centres, in output pixels.
    fn max_error(&self, exact: &Exact) -> f64 {
        let mut worst = 0.0f64;
        for j in 0..self.n - 1 {
            for i in 0..self.n - 1 {
                let a = self.pts[j * self.n + i];
                let b = self.pts[j * self.n + i + 1];
                let c = self.pts[(j + 1) * self.n + i];
                // How far one output pixel reaches in the raster's CRS here.
                let px_len = ((b.0 - a.0).hypot(b.1 - a.1)).max((c.0 - a.0).hypot(c.1 - a.1))
                    / self.step;
                if !px_len.is_finite() || px_len == 0.0 {
                    continue;
                }
                let (cx, cy) = (((i as f64) + 0.5) * self.step, ((j as f64) + 0.5) * self.step);
                let (ex, ey) = exact.at(cx.min(self.size), cy.min(self.size));
                let (gx, gy) = self.at(cx.min(self.size), cy.min(self.size));
                let err = (ex - gx).hypot(ey - gy) / px_len;
                if err.is_finite() {
                    worst = worst.max(err);
                }
            }
        }
        worst
    }

    /// Pixel coordinates within the tile, `0..=tile_size`, to the raster's
    /// CRS. A pixel's centre is `(col + 0.5, row + 0.5)`.
    #[inline]
    pub fn at(&self, px: f64, py: f64) -> (f64, f64) {
        let fx = (px / self.step).clamp(0.0, (self.n - 1) as f64);
        let fy = (py / self.step).clamp(0.0, (self.n - 1) as f64);
        let i = (fx as usize).min(self.n - 2);
        let j = (fy as usize).min(self.n - 2);
        // The last cell can be narrower than `step` when it does not divide
        // the tile; interpolate over its real width.
        let x0 = i as f64 * self.step;
        let y0 = j as f64 * self.step;
        let tx = (px - x0) / ((x0 + self.step).min(self.size) - x0);
        let ty = (py - y0) / ((y0 + self.step).min(self.size) - y0);
        let p00 = self.pts[j * self.n + i];
        let p10 = self.pts[j * self.n + i + 1];
        let p01 = self.pts[(j + 1) * self.n + i];
        let p11 = self.pts[(j + 1) * self.n + i + 1];
        let lerp = |a: f64, b: f64, t: f64| a + (b - a) * t;
        (
            lerp(lerp(p00.0, p10.0, tx), lerp(p01.0, p11.0, tx), ty),
            lerp(lerp(p00.1, p10.1, tx), lerp(p01.1, p11.1, tx), ty),
        )
    }

    /// The bounding box of the whole footprint, edges included -- what to
    /// read from the raster. `None` if no part of the tile projects.
    pub fn bbox(&self) -> Option<Bbox> {
        let mut b = Bbox { west: f64::MAX, south: f64::MAX, east: f64::MIN, north: f64::MIN };
        let mut any = false;
        for &(x, y) in &self.pts {
            if x.is_finite() && y.is_finite() {
                any = true;
                b.west = b.west.min(x);
                b.east = b.east.max(x);
                b.south = b.south.min(y);
                b.north = b.north.max(y);
            }
        }
        any.then_some(b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Worst error against exact reprojection over every pixel centre.
    fn worst_px(z: u32, x: u32, y: u32, epsg: Option<u32>) -> f64 {
        let warp = TileWarp::new(z, x, y, 256, epsg).unwrap();
        let exact = Exact::new(z, x, y, 256, epsg).unwrap();
        let mut worst = 0.0f64;
        for row in 0..256 {
            for col in 0..256 {
                let (px, py) = (col as f64 + 0.5, row as f64 + 0.5);
                let (ex, ey) = exact.at(px, py);
                let (gx, gy) = warp.at(px, py);
                let (nx, ny) = exact.at(px + 1.0, py);
                let px_len = (nx - ex).hypot(ny - ey);
                worst = worst.max((ex - gx).hypot(ey - gy) / px_len);
            }
        }
        worst
    }

    #[test]
    fn interpolation_stays_within_an_eighth_of_a_pixel() {
        // The demo farm in UTM 35S at farm zooms, a whole-world tile in WGS84
        // where Mercator's latitude curve is strongest, and a high-latitude
        // UTM tile where convergence is large.
        for (z, x, y, epsg) in [
            (16, 37414, 39214, Some(32735)),
            (12, 2338, 2450, Some(32735)),
            (0, 0, 0, Some(4326)),
            (3, 4, 1, Some(4326)),
            (10, 560, 290, Some(32633)),
        ] {
            let e = worst_px(z, x, y, epsg);
            assert!(e <= MAX_ERROR_PX, "z{z}/{x}/{y} EPSG:{epsg:?}: {e:.3} px");
        }
    }

    #[test]
    fn neighbours_agree_on_their_shared_edge() {
        let a = TileWarp::new(16, 37414, 39214, 256, Some(32735)).unwrap();
        let b = TileWarp::new(16, 37415, 39214, 256, Some(32735)).unwrap();
        let c = TileWarp::new(16, 37414, 39215, 256, Some(32735)).unwrap();
        for v in 0..=256 {
            let v = v as f64;
            let (ax, ay) = a.at(256.0, v);
            let (bx, by) = b.at(0.0, v);
            assert!((ax - bx).abs() < 1e-6 && (ay - by).abs() < 1e-6, "east edge at {v}");
            let (ax, ay) = a.at(v, 256.0);
            let (cx, cy) = c.at(v, 0.0);
            assert!((ax - cx).abs() < 1e-6 && (ay - cy).abs() < 1e-6, "south edge at {v}");
        }
    }

    #[test]
    fn web_mercator_needs_no_refinement() {
        let w = TileWarp::new(5, 10, 12, 256, Some(3857)).unwrap();
        assert_eq!(w.n, 9, "a linear mapping is exact at the first grid");
    }
}
