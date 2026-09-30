use crate::cog::{decode_tile, fetch_tile_ranges, CogMeta, RangeReader};
use crate::geo::{bbox_to_pixel_window, reproject_bbox_from_wgs84, reproject_point_from_wgs84};
use crate::mask::Mask;
use crate::timing::{phase, Timings};
use crate::render::{blit_tile, bytes_to_f32, overlapping_strip_indices, overlapping_tile_indices};
use serde::Serialize;

/// Ceiling on the working buffer one polygon query may allocate.
///
/// The cap used to be 4,000,000 *pixels*, which is not a cap on memory: the
/// buffer is `pixels × bands × 4` bytes, so the same limit meant 16 MB on a
/// one-band DEM and 48 MB on a three-band scene — per concurrent request. At
/// 32 in flight that is 1.5 GB of transient allocation, which is over the
/// 2 GB the benchmark stack gives the container before any cache is counted.
///
/// Expressed in bytes it is the same 4 megapixels for a single band and
/// proportionally fewer for more, which is the behaviour anyone would have
/// assumed the pixel limit had.
pub const MAX_QUERY_BYTES: i64 = 16 * 1024 * 1024;

/// The pixel budget for a raster with `bands` bands.
pub fn max_query_pixels(bands: usize) -> i64 {
    MAX_QUERY_BYTES / (bands.max(1) as i64 * std::mem::size_of::<f32>() as i64)
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum QueryResult {
    Point   { value: f64 },
    Polygon { min: f64, max: f64, avg: f64, stdev: f64 },
}

pub async fn query_point(
    lng: f64,
    lat: f64,
    band: usize,
    nodata: Option<f32>,
    reader: &dyn RangeReader,
    meta: &CogMeta,
) -> Result<QueryResult, String> {
    let ifd  = &meta.ifds[0];
    let gt   = ifd.geo_transform().ok_or("COG has no geotransform")?;
    let cog_nodata: Option<f32> = ifd.nodata.as_deref().and_then(|s| s.parse().ok());
    let nd   = nodata.or(cog_nodata);

    let (nx, ny) = match ifd.epsg_code {
        Some(epsg) => reproject_point_from_wgs84(lng, lat, epsg)?,
        None       => (lng, lat),
    };

    let [x0, pw, xr, y0, yr, ph] = gt;
    let det = pw * ph - xr * yr;
    if det.abs() < 1e-15 { return Err("degenerate geotransform".into()); }
    let inv_pw =  ph / det; let inv_xr = -xr / det;
    let inv_yr = -yr / det; let inv_ph =  pw / det;
    let dx = nx - x0; let dy = ny - y0;
    let col = (inv_pw * dx + inv_xr * dy).floor() as i64;
    let row = (inv_yr * dx + inv_ph * dy).floor() as i64;

    if col < 0 || col >= ifd.image_width as i64 || row < 0 || row >= ifd.image_height as i64 {
        return Err("point is outside the raster extent".into());
    }

    let samples    = ifd.samples_per_pixel.max(1) as usize;
    let bits       = ifd.bits_per_sample.first().copied().unwrap_or(8);
    let sample_fmt = ifd.sample_format.first().copied().unwrap_or(1);
    let band       = band.min(samples - 1);
    let bps        = (bits as usize + 7) / 8;

    let value = if ifd.is_tiled() {
        let tile_w = ifd.tile_width.unwrap()  as i64;
        let tile_h = ifd.tile_height.unwrap() as i64;
        let tiles_x = ifd.num_tiles_x() as i64;
        let tx = col / tile_w; let ty = row / tile_h;
        let idx = (ty * tiles_x + tx) as usize;
        if idx >= ifd.tile_offsets.len() { return Err("tile index out of bounds".into()); }
        let raw = fetch_tile_ranges(reader, &[ifd.tile_offsets[idx]], &[ifd.tile_byte_counts[idx]]).await?;
        let dec = decode_tile(&raw[0], ifd.compression, ifd.predictor, tile_w as u32, tile_h as u32, samples as u16, bits, &meta.lzw_early_change)?;
        let px  = ((row % tile_h) * tile_w + (col % tile_w)) as usize;
        let off = (px * samples + band) * bps;
        bytes_to_f32(&dec[off..off + bps], bits, sample_fmt)
    } else {
        let rps     = if ifd.rows_per_strip == u32::MAX { ifd.image_height } else { ifd.rows_per_strip } as i64;
        let img_w   = ifd.image_width as i64;
        let s_idx   = (row / rps) as usize;
        if s_idx >= ifd.strip_offsets.len() { return Err("strip index out of bounds".into()); }
        let raw = fetch_tile_ranges(reader, &[ifd.strip_offsets[s_idx]], &[ifd.strip_byte_counts[s_idx]]).await?;
        let dec = decode_tile(&raw[0], ifd.compression, ifd.predictor, img_w as u32, rps as u32, samples as u16, bits, &meta.lzw_early_change)?;
        let px  = ((row % rps) * img_w + col) as usize;
        let off = (px * samples + band) * bps;
        bytes_to_f32(&dec[off..off + bps], bits, sample_fmt)
    };

    if value.is_nan() || nd.map(|n| (value - n).abs() < f32::EPSILON * 100.0).unwrap_or(false) {
        return Err("point is nodata".into());
    }

    Ok(QueryResult::Point { value: value as f64 })
}

pub async fn query_polygon(
    mask_wgs84: &Mask,
    band: usize,
    nodata: Option<f32>,
    reader: &dyn RangeReader,
    meta: &CogMeta,
) -> Result<QueryResult, String> {
    query_polygon_timed(mask_wgs84, band, nodata, reader, meta, &Timings::off()).await
}

/// As [`query_polygon`], recording where the time went.
pub async fn query_polygon_timed(
    mask_wgs84: &Mask,
    band: usize,
    nodata: Option<f32>,
    reader: &dyn RangeReader,
    meta: &CogMeta,
    t: &Timings,
) -> Result<QueryResult, String> {
    let plan_start = t.start();
    let ifd  = &meta.ifds[0];
    let gt   = ifd.geo_transform().ok_or("COG has no geotransform")?;
    let cog_nodata: Option<f32> = ifd.nodata.as_deref().and_then(|s| s.parse().ok());
    let nd   = nodata.or(cog_nodata);

    let native_mask = t.time(phase::MASK, || mask_wgs84.to_native_crs(ifd.epsg_code))?;

    let wgs84_bbox   = mask_wgs84.wgs84_bbox().ok_or("polygon is empty")?;
    let native_bbox  = match ifd.epsg_code {
        Some(epsg) => reproject_bbox_from_wgs84(&wgs84_bbox, epsg)?,
        None       => wgs84_bbox,
    };

    let win = bbox_to_pixel_window(&gt, ifd.image_width, ifd.image_height, &native_bbox)
        .ok_or("polygon does not intersect raster")?;

    let samples    = ifd.samples_per_pixel.max(1) as usize;
    let budget = max_query_pixels(samples);
    if win.x_size * win.y_size > budget {
        return Err(format!(
            "polygon window ({} × {} = {} px over {} band(s)) needs {:.0} MB; the limit is {} MB",
            win.x_size, win.y_size, win.x_size * win.y_size, samples,
            (win.x_size * win.y_size * samples as i64 * 4) as f64 / 1e6,
            MAX_QUERY_BYTES / (1024 * 1024)
        ));
    }

    let bits       = ifd.bits_per_sample.first().copied().unwrap_or(8);
    let sample_fmt = ifd.sample_format.first().copied().unwrap_or(1);
    let band       = band.min(samples - 1);

    let mw = win.x_size as usize;
    let mh = win.y_size as usize;
    let mut mosaic = vec![f32::NAN; mw * mh * samples];
    t.since(phase::PLAN, plan_start);

    if ifd.is_tiled() {
        let tile_w  = ifd.tile_width.unwrap()  as usize;
        let tile_h  = ifd.tile_height.unwrap() as usize;
        let tiles_x = ifd.num_tiles_x() as usize;
        let indices = overlapping_tile_indices(ifd, &win);
        let offsets:      Vec<u64> = indices.iter().map(|&i| ifd.tile_offsets[i]).collect();
        let byte_counts:  Vec<u64> = indices.iter().map(|&i| ifd.tile_byte_counts[i]).collect();
        let raw_tiles = t.time_async(phase::FETCH,
            fetch_tile_ranges(reader, &offsets, &byte_counts)).await?;
        for (raw, &idx) in raw_tiles.iter().zip(indices.iter()) {
            let dec = t.time(phase::DECODE, || decode_tile(
                raw, ifd.compression, ifd.predictor, tile_w as u32, tile_h as u32,
                samples as u16, bits, &meta.lzw_early_change))?;
            let (tx, ty) = (idx % tiles_x, idx / tiles_x);
            t.time(phase::BLIT, || blit_tile(&dec, bits, samples, sample_fmt,
                tile_w, tile_h, tx * tile_w, ty * tile_h,
                &mut mosaic, win.x_off as usize, win.y_off as usize, mw, mh));
        }
    } else {
        let rps   = if ifd.rows_per_strip == u32::MAX { ifd.image_height } else { ifd.rows_per_strip } as usize;
        let img_w = ifd.image_width as usize;
        let indices = overlapping_strip_indices(&ifd.strip_offsets, &win, rps);
        let offsets:     Vec<u64> = indices.iter().map(|&i| ifd.strip_offsets[i]).collect();
        let byte_counts: Vec<u64> = indices.iter().map(|&i| ifd.strip_byte_counts[i]).collect();
        let raw_strips = t.time_async(phase::FETCH,
            fetch_tile_ranges(reader, &offsets, &byte_counts)).await?;
        for (raw, &idx) in raw_strips.iter().zip(indices.iter()) {
            let dec = t.time(phase::DECODE, || decode_tile(
                raw, ifd.compression, ifd.predictor, img_w as u32, rps as u32,
                samples as u16, bits, &meta.lzw_early_change))?;
            t.time(phase::BLIT, || blit_tile(&dec, bits, samples, sample_fmt,
                img_w, rps, 0, idx * rps,
                &mut mosaic, win.x_off as usize, win.y_off as usize, mw, mh));
        }
    }

    // Accumulate stats over pixels inside the polygon. Reprojecting the mask
    // is folded in here rather than into `plan`: it scales with the geometry,
    // not with the raster, which is the distinction worth being able to see.
    //
    // Which pixels are inside is settled by filling rows between the
    // polygon's edges, in the window's own pixel grid, rather than by asking
    // `contains` about each pixel centre: for a farm of 86 blocks over a few
    // megapixels that per-pixel walk was nearly all of the query. The native
    // geotransform is affine, so inverting it moves the vertices into the grid
    // with every edge still straight, and inside stays exactly inside.
    let stats_start = t.start();
    let inside = window_coverage(&native_mask, gt, (win.x_off, win.y_off), mw, mh)?;

    let mut count  = 0u64;
    let mut sum    = 0f64;
    let mut sum_sq = 0f64;
    let mut min    = f64::MAX;
    let mut max    = f64::MIN;

    for (i, _) in inside.iter().enumerate().filter(|(_, &keep)| keep) {
        let v = mosaic[i * samples + band];
        if v.is_nan() || nd.map(|n| (v - n).abs() < f32::EPSILON * 100.0).unwrap_or(false) {
            continue;
        }
        let vf = v as f64;
        count  += 1;
        sum    += vf;
        sum_sq += vf * vf;
        if vf < min { min = vf; }
        if vf > max { max = vf; }
    }

    t.since(phase::STATS, stats_start);

    if count == 0 {
        return Err("no valid pixels found within polygon".into());
    }

    let avg   = sum / count as f64;
    let stdev = ((sum_sq / count as f64) - avg * avg).max(0.0).sqrt();

    Ok(QueryResult::Polygon { min, max, avg, stdev })
}

/// Which pixels of the `width`×`height` window at `offset` in the raster have
/// their centre inside `native_mask`, which is in the raster's CRS.
fn window_coverage(
    native_mask: &Mask,
    gt: [f64; 6],
    offset: (i64, i64),
    width: usize,
    height: usize,
) -> Result<Vec<bool>, String> {
    let [x0, pw, xr, y0, yr, ph] = gt;
    let det = pw * ph - xr * yr;
    if det == 0.0 || !det.is_finite() {
        return Err("COG geotransform cannot be inverted".into());
    }
    let (wx, wy) = (offset.0 as f64, offset.1 as f64);
    let to_cell = |(nx, ny): (f64, f64)| {
        let (dx, dy) = (nx - x0, ny - y0);
        ((dx * ph - dy * xr) / det - wx, (dy * pw - dx * yr) / det - wy)
    };
    Ok(crate::mask::fill_grid(width, height, native_mask.rings().iter().map(|rings| {
        rings.iter().map(|r| r.iter().copied().map(to_cell).collect()).collect()
    })))
}

#[cfg(test)]
mod coverage_tests {
    use super::*;
    use crate::mask::parse_wkt_mask;

    /// What the stats loop used to ask: `contains` at each pixel centre,
    /// taken through the geotransform into the raster's CRS.
    fn per_pixel(mask: &Mask, gt: [f64; 6], offset: (i64, i64), w: usize, h: usize) -> Vec<bool> {
        let [x0, pw, xr, y0, yr, ph] = gt;
        (0..w * h).map(|i| {
            let col = offset.0 as f64 + (i % w) as f64 + 0.5;
            let row = offset.1 as f64 + (i / w) as f64 + 0.5;
            mask.contains(x0 + col * pw + row * xr, y0 + col * yr + row * ph)
        }).collect()
    }

    /// A closed star-shaped ring, as WKT: `(x y,…)`.
    fn star(cx: f64, cy: f64, r: f64, points: usize) -> String {
        let pts: Vec<String> = (0..=points * 2).map(|k| {
            let a = (k % (points * 2)) as f64 / (points * 2) as f64 * std::f64::consts::TAU;
            let r = if k % 2 == 0 { r } else { r * 0.45 };
            format!("{} {}", cx + r * a.cos(), cy + r * a.sin())
        }).collect();
        format!("({})", pts.join(","))
    }

    #[test]
    fn filled_rows_match_the_per_pixel_test() {
        // UTM-like metres, 10 m pixels, and a geotransform rotated by a few
        // degrees, which is the case the inverse has to get right.
        let (c, s) = (3f64.to_radians().cos(), 3f64.to_radians().sin());
        let north_up = [500_000.0, 10.0, 0.0, 6_300_000.0, 0.0, -10.0];
        let rotated = [500_000.0, 10.0 * c, 10.0 * s, 6_300_000.0, 10.0 * s, -10.0 * c];
        let (cx, cy) = (501_500.0, 6_298_500.0);
        // A star with a star-shaped hole, and two more overlapping it.
        let mask = parse_wkt_mask(&format!(
            "MULTIPOLYGON(({},{}),({}),({}))",
            star(cx, cy, 900.0, 7),
            star(cx, cy, 300.0, 5),
            star(cx + 700.0, cy + 200.0, 500.0, 4),
            star(cx - 400.0, cy - 600.0, 350.0, 6),
        )).unwrap();
        assert_eq!(mask.size().0, 3);

        for (name, gt) in [("north up", north_up), ("rotated", rotated)] {
            for offset in [(0, 0), (37, 91)] {
                let (w, h) = (300, 280);
                let got = window_coverage(&mask, gt, offset, w, h).unwrap();
                let want = per_pixel(&mask, gt, offset, w, h);
                let differ = got.iter().zip(&want).filter(|(a, b)| a != b).count();
                // Exact but for a centre within rounding of an edge, which
                // the inverse transform can move to the other side.
                assert!(differ <= 2, "{name} {offset:?}: {differ} pixels differ");
                assert!(got.iter().filter(|&&v| v).count() > 1_000, "{name} {offset:?}: too little inside");
            }
        }
    }

    #[test]
    fn a_geotransform_that_cannot_be_inverted_is_an_error() {
        let mask = parse_wkt_mask("POLYGON((0 0,1 0,1 1,0 0))").unwrap();
        assert!(window_coverage(&mask, [0.0, 1.0, 1.0, 0.0, 1.0, 1.0], (0, 0), 4, 4).is_err());
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;

    #[test]
    fn the_budget_is_bytes_so_more_bands_get_fewer_pixels() {
        // One band keeps the 4 megapixels the old pixel cap allowed.
        assert_eq!(max_query_pixels(1), 4_194_304);
        assert_eq!(max_query_pixels(3), 1_398_101);
        assert_eq!(max_query_pixels(4), 1_048_576);
        // Zero bands cannot divide by zero.
        assert_eq!(max_query_pixels(0), max_query_pixels(1));
    }

    #[test]
    fn every_band_count_costs_the_same_memory() {
        for bands in 1..=8 {
            let bytes = max_query_pixels(bands) * bands as i64 * 4;
            assert!(bytes <= MAX_QUERY_BYTES,
                    "{bands} bands would allocate {bytes} against a {MAX_QUERY_BYTES} budget");
            assert!(bytes > MAX_QUERY_BYTES - 64,
                    "{bands} bands wastes the budget: {bytes}");
        }
    }

    #[test]
    fn the_worst_case_under_concurrency_fits_a_small_container() {
        // What made this worth changing: the buffer is per in-flight request.
        let worst = MAX_QUERY_BYTES * 32;
        assert!(worst <= 512 * 1024 * 1024,
                "32 concurrent queries would need {} MB", worst / (1024 * 1024));
    }
}
