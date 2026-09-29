#![allow(dead_code)]
use std::f64::consts::PI;

/// WGS84 bounding box (degrees)
#[derive(Debug, Clone, Copy)]
pub struct Bbox {
    pub west: f64,
    pub south: f64,
    pub east: f64,
    pub north: f64,
}

/// Pixel window within an image
#[derive(Debug, Clone, Copy)]
pub struct PixelWindow {
    pub x_off: i64,
    pub y_off: i64,
    pub x_size: i64,
    pub y_size: i64,
}

/// Convert XYZ tile indices to WGS84 bbox.
pub fn tile_to_bbox(z: u32, x: u32, y: u32) -> Bbox {
    let n = 2u32.pow(z) as f64;
    let west = x as f64 / n * 360.0 - 180.0;
    let east = (x + 1) as f64 / n * 360.0 - 180.0;
    let north_rad = (PI * (1.0 - 2.0 * y as f64 / n)).sinh().atan();
    let south_rad = (PI * (1.0 - 2.0 * (y + 1) as f64 / n)).sinh().atan();
    Bbox { west, south: south_rad.to_degrees(), east, north: north_rad.to_degrees() }
}

/// Reproject a single WGS84 point into the TIFF's native CRS.
pub fn reproject_point_from_wgs84(lon: f64, lat: f64, epsg: u32) -> Result<(f64, f64), String> {
    if epsg == 4326 || epsg == 4269 {
        return Ok((lon, lat));
    }
    let dst_def = epsg_to_proj4(epsg)
        .ok_or_else(|| format!("EPSG:{epsg} not supported for reprojection"))?;
    use proj4rs::{Proj, transform::transform};
    let src = Proj::from_proj_string("+proj=longlat +datum=WGS84 +no_defs")
        .map_err(|e| format!("WGS84 proj error: {e}"))?;
    let dst = Proj::from_proj_string(&dst_def)
        .map_err(|e| format!("EPSG:{epsg} proj error: {e}"))?;
    let mut pt = (lon.to_radians(), lat.to_radians(), 0.0f64);
    transform(&src, &dst, &mut pt).map_err(|e| format!("transform error: {e}"))?;
    Ok((pt.0, pt.1))
}

/// Reproject a WGS84 bbox into the TIFF's native CRS using proj4rs.
/// Returns the bbox in the TIFF's CRS units (metres for projected CRS, degrees for WGS84).
pub fn reproject_bbox_from_wgs84(bbox: &Bbox, epsg: u32) -> Result<Bbox, String> {
    // WGS84 geographic — no reprojection needed
    if epsg == 4326 || epsg == 4269 {
        return Ok(*bbox);
    }

    let dst_def = epsg_to_proj4(epsg)
        .ok_or_else(|| format!("EPSG:{epsg} is not supported; add a `crs_def` query param with a proj4 string"))?;

    use proj4rs::Proj;
    use proj4rs::transform::transform;

    let src = Proj::from_proj_string("+proj=longlat +datum=WGS84 +no_defs")
        .map_err(|e| format!("WGS84 proj error: {e}"))?;
    let dst = Proj::from_proj_string(&dst_def)
        .map_err(|e| format!("EPSG:{epsg} proj error: {e}"))?;

    // Transform all four corners and take the axis-aligned bounding box.
    // (The bbox may be non-rectangular after reprojection for most CRSes.)
    let corners = [
        (bbox.west, bbox.south),
        (bbox.east, bbox.south),
        (bbox.west, bbox.north),
        (bbox.east, bbox.north),
    ];

    let mut min_x = f64::MAX;
    let mut max_x = f64::MIN;
    let mut min_y = f64::MAX;
    let mut max_y = f64::MIN;
    let mut valid = 0u32;

    for (lon, lat) in corners {
        // proj4rs expects geographic input in radians for +proj=longlat
        let mut pt = (lon.to_radians(), lat.to_radians(), 0.0f64);
        if transform(&src, &dst, &mut pt).is_ok() {
            let (x, y, _) = pt;
            min_x = min_x.min(x);
            max_x = max_x.max(x);
            min_y = min_y.min(y);
            max_y = max_y.max(y);
            valid += 1;
        }
    }

    if valid == 0 {
        return Err("all corners outside projection domain".into());
    }
    Ok(Bbox { west: min_x, south: min_y, east: max_x, north: max_y })
}

/// Map a bbox (already in the TIFF's CRS) to pixel coordinates using the
/// full affine geotransform [x0, pw, xr, y0, yr, ph].
///
/// Handles rotated/sheared rasters by inverting the 2×2 linear part.
pub fn bbox_to_pixel_window(
    gt: &[f64; 6],
    image_width: u32,
    image_height: u32,
    bbox: &Bbox,
) -> Option<PixelWindow> {
    let [x0, pw, xr, y0, yr, ph] = *gt;

    // Invert the 2×2 transform: [pw, xr; yr, ph]
    let det = pw * ph - xr * yr;
    if det.abs() < 1e-15 {
        return None; // degenerate transform
    }
    let inv_pw =  ph / det;
    let inv_xr = -xr / det;
    let inv_yr = -yr / det;
    let inv_ph =  pw / det;

    // Project all four corners to pixel space, then take the bounding box.
    let corners = [
        (bbox.west, bbox.south),
        (bbox.east, bbox.south),
        (bbox.west, bbox.north),
        (bbox.east, bbox.north),
    ];

    let mut min_col = f64::MAX;
    let mut max_col = f64::MIN;
    let mut min_row = f64::MAX;
    let mut max_row = f64::MIN;

    for (gx, gy) in corners {
        let dx = gx - x0;
        let dy = gy - y0;
        let col = inv_pw * dx + inv_xr * dy;
        let row = inv_yr * dx + inv_ph * dy;
        min_col = min_col.min(col);
        max_col = max_col.max(col);
        min_row = min_row.min(row);
        max_row = max_row.max(row);
    }

    let x_off = min_col.floor() as i64;
    let y_off = min_row.floor() as i64;
    let x_end = max_col.ceil()  as i64;
    let y_end = max_row.ceil()  as i64;

    // Clamp to image bounds
    let x_off = x_off.max(0).min(image_width  as i64);
    let y_off = y_off.max(0).min(image_height as i64);
    let x_end = x_end.max(0).min(image_width  as i64);
    let y_end = y_end.max(0).min(image_height as i64);

    if x_end <= x_off || y_end <= y_off {
        return None;
    }

    Some(PixelWindow { x_off, y_off, x_size: x_end - x_off, y_size: y_end - y_off })
}

/// Choose the overview level for rendering `bbox` (in the raster's native CRS)
/// into a `tile_size`-square image.
///
/// The target reduction factor is the tile's footprint in full-resolution
/// pixels divided by the tile size, measured against the base geotransform
/// and deliberately *not* clamped to the raster: a z6 tile that covers a
/// small raster many times over still wants the coarsest level, and a z14
/// tile inside it wants full resolution. Clamping to the raster (or, worse,
/// using the raster's own size) would pick the same level at every zoom.
pub fn best_overview_for_bbox(gt: &[f64; 6], bbox: &Bbox, tile_size: u32, overview_count: usize) -> usize {
    let [_, pw, xr, _, yr, ph] = *gt;
    // Native units per source pixel along each axis, rotation included.
    let x_res = (pw * pw + yr * yr).sqrt();
    let y_res = (xr * xr + ph * ph).sqrt();
    if x_res <= 0.0 || y_res <= 0.0 || tile_size == 0 {
        return 0;
    }
    let cols = (bbox.east - bbox.west).abs() / x_res;
    let rows = (bbox.north - bbox.south).abs() / y_res;
    let target = (cols / tile_size as f64).min(rows / tile_size as f64);
    best_overview_for_factor(target, overview_count)
}

/// Choose the best overview level for a given output size.
pub fn best_overview(
    full_width: u32,
    full_height: u32,
    out_width: u32,
    out_height: u32,
    overview_count: usize,
) -> usize {
    let x_factor = full_width  as f64 / out_width  as f64;
    let y_factor = full_height as f64 / out_height as f64;
    best_overview_for_factor(x_factor.min(y_factor), overview_count)
}

/// The finest level whose reduction factor (2^level) does not exceed `target`,
/// so a tile is never rendered from data coarser than its own pixels.
pub fn best_overview_for_factor(target: f64, overview_count: usize) -> usize {
    let mut best = 0usize;
    let mut best_factor = 1.0f64;
    for i in 0..overview_count {
        let factor = 2f64.powi(i as i32);
        if factor <= target && factor > best_factor {
            best = i;
            best_factor = factor;
        }
    }
    best
}

/// Compute the WGS84 bounding box of a raster by forward-projecting its four
/// pixel corners through the affine geotransform and reprojecting to WGS84.
pub fn geo_extent_wgs84(
    geo_transform: &[f64; 6],
    image_width: u32,
    image_height: u32,
    epsg: Option<u32>,
) -> Option<Bbox> {
    let [x0, pw, xr, y0, yr, ph] = *geo_transform;
    let w = image_width as f64;
    let h = image_height as f64;
    // geo(col,row) = (x0 + col*pw + row*xr, y0 + col*yr + row*ph)
    let corners = [
        (x0,               y0),
        (x0 + w * pw,      y0 + w * yr),
        (x0 + h * xr,      y0 + h * ph),
        (x0 + w*pw + h*xr, y0 + w*yr + h*ph),
    ];
    let mut min_lon = f64::MAX;
    let mut max_lon = f64::MIN;
    let mut min_lat = f64::MAX;
    let mut max_lat = f64::MIN;
    if epsg.map(|c| c != 4326 && c != 4269).unwrap_or(false) {
        let src_def = epsg_to_proj4(epsg.unwrap())?;
        use proj4rs::{Proj, transform::transform};
        let src = Proj::from_proj_string(&src_def).ok()?;
        let dst = Proj::from_proj_string("+proj=longlat +datum=WGS84 +no_defs").ok()?;
        for (nx, ny) in corners {
            let mut pt = (nx, ny, 0.0f64);
            transform(&src, &dst, &mut pt).ok()?;
            // proj4rs returns geographic coords in radians for +proj=longlat
            let (lon, lat) = (pt.0.to_degrees(), pt.1.to_degrees());
            min_lon = min_lon.min(lon);
            max_lon = max_lon.max(lon);
            min_lat = min_lat.min(lat);
            max_lat = max_lat.max(lat);
        }
    } else {
        for (nx, ny) in corners {
            min_lon = min_lon.min(nx);
            max_lon = max_lon.max(nx);
            min_lat = min_lat.min(ny);
            max_lat = max_lat.max(ny);
        }
    }
    Some(Bbox { west: min_lon, south: min_lat, east: max_lon, north: max_lat })
}

// ── proj4 string lookup ──────────────────────────────────────────────────────

/// Generate a proj4 string for common EPSG codes without needing an external
/// lookup service. Falls back to None for unsupported codes.
pub fn epsg_to_proj4(epsg: u32) -> Option<String> {
    match epsg {
        // Geographic
        4326 | 4269 => Some("+proj=longlat +datum=WGS84 +no_defs".into()),
        // Web Mercator
        3857 | 900913 => Some(
            "+proj=merc +a=6378137 +b=6378137 +lat_ts=0 +lon_0=0 \
             +x_0=0 +y_0=0 +k=1 +units=m +no_defs".into(),
        ),
        // WGS84 UTM North zones (EPSG:32601–32660)
        32601..=32660 => {
            let zone = epsg - 32600;
            Some(format!("+proj=utm +zone={zone} +datum=WGS84 +units=m +no_defs"))
        }
        // WGS84 UTM South zones (EPSG:32701–32760)
        32701..=32760 => {
            let zone = epsg - 32700;
            Some(format!("+proj=utm +zone={zone} +south +datum=WGS84 +units=m +no_defs"))
        }
        // NAD83 UTM North zones (EPSG:26901–26923)
        26901..=26923 => {
            let zone = epsg - 26900;
            Some(format!("+proj=utm +zone={zone} +datum=NAD83 +units=m +no_defs"))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The benchmark raster: 4096 px over 2° at 10–12°E, 0–2°N, five levels
    // (4096, 2048, 1024, 512, 256).
    const GT: [f64; 6] = [10.0, 2.0 / 4096.0, 0.0, 2.0, 0.0, -2.0 / 4096.0];
    const LEVELS: usize = 5;

    fn level_for(z: u32, x: u32, y: u32) -> usize {
        best_overview_for_bbox(&GT, &tile_to_bbox(z, x, y), 256, LEVELS)
    }

    #[test]
    fn overview_follows_zoom() {
        // 5.6° per tile: 45 source pixels per output pixel → coarsest level.
        assert_eq!(level_for(6, 33, 31), 4);
        // 1.4° per tile: 11.25 → factor 8.
        assert_eq!(level_for(8, 136, 127), 3);
        // 0.35° per tile: 2.8 → factor 2.
        assert_eq!(level_for(10, 540, 509), 1);
        // 0.04° per tile: 0.35 → full resolution.
        assert_eq!(level_for(13, 4324, 4076), 0);
    }

    #[test]
    fn overview_is_not_capped_by_the_raster_size() {
        // A z0 tile spans ~1360 source pixels per output pixel on this raster.
        // The footprint is what matters, not the raster's 4096 px: with five
        // levels that is the coarsest one, and with twelve it is level 10
        // (factor 1024), the finest whose factor does not exceed 1360.
        assert_eq!(level_for(0, 0, 0), 4);
        assert_eq!(best_overview_for_bbox(&GT, &tile_to_bbox(0, 0, 0), 256, 12), 10);
    }

    #[test]
    fn overview_factor_never_exceeds_target() {
        assert_eq!(best_overview_for_factor(0.5, 5), 0);
        assert_eq!(best_overview_for_factor(1.99, 5), 0);
        assert_eq!(best_overview_for_factor(2.0, 5), 1);
        assert_eq!(best_overview_for_factor(7.9, 5), 2);
        assert_eq!(best_overview_for_factor(1e9, 5), 4);
        assert_eq!(best_overview_for_factor(64.0, 1), 0);
    }

    #[test]
    fn rotated_geotransform_uses_the_pixel_diagonal() {
        // 45° rotation with 1-unit pixels: resolution along each axis is 1.
        let h = std::f64::consts::FRAC_1_SQRT_2;
        let gt = [0.0, h, -h, 0.0, h, h];
        let bbox = Bbox { west: 0.0, south: 0.0, east: 1024.0, north: 1024.0 };
        assert_eq!(best_overview_for_bbox(&gt, &bbox, 256, 5), 2);
    }
}
