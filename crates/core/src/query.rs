use crate::cog::{decode_tile, fetch_tile_ranges, CogMeta, RangeReader};
use std::collections::HashMap;

use crate::geo::{bbox_to_pixel_window, reproject_point_from_wgs84};
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

/// Most distinct values `classes` will report. A categorical raster has a
/// handful; a continuous one has a class per pixel, and a response listing
/// four million of them is a mistake, not a summary.
pub const MAX_CLASSES: usize = 1_000;

#[derive(Serialize, Debug)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum QueryResult {
    Point   { value: f64 },
    /// Zonal statistics, each cell weighted by the fraction of it the polygon
    /// covers. The definitions are exactextract's, so the numbers can be
    /// checked against it: `count` is its `count`, `sum` its `sum`, `avg`
    /// its `mean`, `stdev` its `stdev`, `min` and `max` its `min` and `max`.
    /// Those four are `null` when every covered cell is nodata.
    Polygon {
        min:   Option<f64>,
        max:   Option<f64>,
        avg:   Option<f64>,
        stdev: Option<f64>,
        /// Covered cells with data: the sum of their coverage fractions.
        count: f64,
        /// Coverage-weighted sum of their values.
        sum:   f64,
        /// Covered cells without data, measured the same way as `count`.
        nodata_count: f64,
        area:  Areas,
        /// Per distinct value, when asked for.
        #[serde(skip_serializing_if = "Option::is_none")]
        classes: Option<Vec<ClassStats>>,
    },
}

/// How much of the polygon the raster covers, split by whether it has data.
/// `total` is `data + nodata`, and is the polygon's own area wherever the
/// polygon lies inside the raster.
#[derive(Serialize, Debug)]
pub struct Areas {
    pub total:  f64,
    pub data:   f64,
    pub nodata: f64,
    /// `cartesian`: coverage times the pixel's size in the raster's CRS
    /// units -- square metres for UTM, exactextract's `area_cartesian`.
    /// `spherical`: coverage times the pixel's area on a sphere of the WGS84
    /// equatorial radius, for rasters in degrees -- its `area_spherical_m2`.
    pub method: &'static str,
}

/// One distinct raster value under the polygon.
#[derive(Serialize, Debug)]
pub struct ClassStats {
    pub value: f64,
    /// Coverage fractions summed over the cells holding it.
    pub count: f64,
    /// `count` over the polygon's `count`: its share of the area that has
    /// data, as exactextract's `frac`. Its share of the whole polygon,
    /// nodata included, is `area / area.total`.
    pub frac:  f64,
    pub area:  f64,
}

/// Whether `v` is missing: NaN, or exactly the nodata value. Exact, as GDAL
/// and exactextract compare -- a tolerance would swallow real values next
/// to a small nodata like 0.
fn is_nodata(v: f32, nodata: Option<f32>) -> bool {
    v.is_nan() || nodata == Some(v)
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

    if is_nodata(value, nd) {
        return Err("point is nodata".into());
    }

    Ok(QueryResult::Point { value: value as f64 })
}

/// Zonal statistics under `mask_wgs84`, with a breakdown by value when
/// `classes` is set. See [`QueryResult::Polygon`].
pub async fn query_polygon(
    mask_wgs84: &Mask,
    band: usize,
    nodata: Option<f32>,
    classes: bool,
    reader: &dyn RangeReader,
    meta: &CogMeta,
) -> Result<QueryResult, String> {
    query_polygon_timed(mask_wgs84, band, nodata, classes, reader, meta, &Timings::off()).await
}

/// As [`query_polygon`], recording where the time went.
pub async fn query_polygon_timed(
    mask_wgs84: &Mask,
    band: usize,
    nodata: Option<f32>,
    classes: bool,
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

    // The window is every cell the reprojected polygon touches: bounded by its
    // own vertices, since its edges are straight in this CRS. A bbox
    // reprojected from WGS84 corners can fall a hair short of it, and a
    // partly covered cell left outside is area lost.
    // (`wgs84_bbox` is only the rings' extent, whatever CRS they are in.)
    let native_bbox = native_mask.wgs84_bbox().ok_or("polygon is empty")?;

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

    // Every cell is weighted by the fraction of it inside the polygon, so the
    // cells along the boundary count for the part of them that is in it and
    // the weights add up to the polygon's area. Reprojecting the mask is
    // folded in with this rather than into `plan`: it scales with the
    // geometry, not the raster, which is the distinction worth seeing.
    let stats_start = t.start();
    let cells = window_polygons(&native_mask, gt, (win.x_off, win.y_off))?;
    let (cell_area, method) = cell_areas(gt, ifd.epsg_code, win.y_off, mh);

    let mut acc = Accumulator::default();
    let mut by_value: HashMap<u32, (f64, f64)> = HashMap::new();
    crate::mask::coverage_rows(mw, mh, &cells, |row, col0, cover| {
        let area = cell_area[row];
        let values = &mosaic[(row * mw + col0) * samples..];
        // A row's cells all have the same area, so it is applied per run.
        let (mut data, mut missing) = (0.0, 0.0);
        for (i, &c) in cover.iter().enumerate() {
            let v = values[i * samples + band];
            if is_nodata(v, nd) {
                missing += c;
                continue;
            }
            acc.value(v as f64, c);
            data += c;
            if classes && by_value.len() <= MAX_CLASSES {
                // -0.0 and 0.0 are one class.
                let e = by_value.entry((v + 0.0).to_bits()).or_default();
                e.0 += c;
                e.1 += c * area;
            }
        }
        acc.area += data * area;
        acc.nodata_count += missing;
        acc.nodata_area += missing * area;
    });
    t.since(phase::STATS, stats_start);

    if acc.count + acc.nodata_count == 0.0 {
        return Err("polygon covers no part of the raster".into());
    }
    if by_value.len() > MAX_CLASSES {
        return Err(format!(
            "more than {MAX_CLASSES} distinct values under the polygon; classes= is for \
             categorical rasters"));
    }
    let classes = classes.then(|| {
        let mut out: Vec<ClassStats> = by_value.into_iter().map(|(bits, (count, area))| ClassStats {
            value: f32::from_bits(bits) as f64,
            count,
            frac: count / acc.count,
            area,
        }).collect();
        out.sort_by(|a, b| a.value.total_cmp(&b.value));
        out
    });

    Ok(acc.finish(method, classes))
}

/// Coverage-weighted statistics, defined as exactextract defines them.
///
/// Summing squares and subtracting the squared mean loses everything to
/// cancellation when the values are large and close together, as an
/// elevation raster's are. exactextract avoids it with West's weighted
/// update, which divides for every cell. Summing about a shift -- the first
/// value seen -- avoids it just as well without the division: the shifted
/// values are small, and what is squared is small.
struct Accumulator {
    count: f64,
    sum: f64,
    shift: Option<f64>,
    /// Σ c(x − shift) and Σ c(x − shift)².
    sum_d: f64,
    sum_d2: f64,
    min: f64,
    max: f64,
    area: f64,
    nodata_count: f64,
    nodata_area: f64,
}

impl Default for Accumulator {
    fn default() -> Self {
        Self {
            count: 0.0, sum: 0.0, shift: None, sum_d: 0.0, sum_d2: 0.0,
            min: f64::INFINITY, max: f64::NEG_INFINITY,
            area: 0.0, nodata_count: 0.0, nodata_area: 0.0,
        }
    }
}

impl Accumulator {
    fn value(&mut self, x: f64, c: f64) {
        self.count += c;
        self.sum += x * c;
        let d = x - *self.shift.get_or_insert(x);
        self.sum_d += c * d;
        self.sum_d2 += c * d * d;
        // Any cell the polygon covers at all, however little -- exactextract's
        // min and max do not weigh coverage either.
        self.min = self.min.min(x);
        self.max = self.max.max(x);
    }

    fn finish(self, method: &'static str, classes: Option<Vec<ClassStats>>) -> QueryResult {
        let some = self.count > 0.0;
        QueryResult::Polygon {
            min: some.then_some(self.min),
            max: some.then_some(self.max),
            avg: some.then_some(self.sum / self.count),
            stdev: some.then(|| {
                let mean_d = self.sum_d / self.count;
                (self.sum_d2 / self.count - mean_d * mean_d).max(0.0).sqrt()
            }),
            count: self.count,
            sum: self.sum,
            nodata_count: self.nodata_count,
            area: Areas {
                total: self.area + self.nodata_area,
                data: self.area,
                nodata: self.nodata_area,
                method,
            },
            classes,
        }
    }
}

/// `native_mask`'s rings in the coordinates of the window at `offset`:
/// (0, 0) is the window's top-left corner and a cell is a unit square.
fn window_polygons(
    native_mask: &Mask,
    gt: [f64; 6],
    offset: (i64, i64),
) -> Result<crate::mask::Polygons, String> {
    let [x0, pw, xr, y0, yr, ph] = gt;
    let det = pw * ph - xr * yr;
    if det == 0.0 || !det.is_finite() {
        return Err("COG geotransform cannot be inverted".into());
    }
    let (wx, wy) = (offset.0 as f64, offset.1 as f64);
    // The geotransform is affine, so its inverse keeps edges straight and
    // scales every area by the same 1/|det|: a cell's coverage fraction is
    // the same measured here or in the raster's CRS.
    let to_cell = |(nx, ny): (f64, f64)| {
        let (dx, dy) = (nx - x0, ny - y0);
        ((dx * ph - dy * xr) / det - wx, (dy * pw - dx * yr) / det - wy)
    };
    Ok(native_mask.rings().iter().map(|rings| {
        rings.iter().map(|r| r.iter().copied().map(to_cell).collect()).collect()
    }).collect())
}

/// The ground area of one whole cell in each of the window's `rows`, and how
/// it was measured.
fn cell_areas(gt: [f64; 6], epsg: Option<u32>, y_off: i64, rows: usize) -> (Vec<f64>, &'static str) {
    let [_, pw, xr, y0, yr, ph] = gt;
    // No EPSG code is read as WGS84, as it is everywhere else here: the
    // polygon is applied to such a raster unreprojected.
    if matches!(epsg, None | Some(4326) | Some(4269)) {
        // Degrees: a cell's area shrinks toward the poles. The band of a
        // sphere between two latitudes, cut to the cell's width -- the
        // formula exactextract uses, on the same radius.
        const R: f64 = 6_378_137.0;
        let (dlat, dlon) = (ph.abs().to_radians(), pw.abs().to_radians());
        let areas = (0..rows).map(|r| {
            let lat = (y0 + (y_off as f64 + r as f64 + 0.5) * ph).to_radians();
            R * R * ((lat - dlat / 2.0).sin() - (lat + dlat / 2.0).sin()).abs() * dlon
        }).collect();
        (areas, "spherical")
    } else {
        (vec![(pw * ph - xr * yr).abs(); rows], "cartesian")
    }
}

#[cfg(test)]
mod coverage_tests {
    use super::*;
    use crate::mask::parse_wkt_mask;

    /// A closed star-shaped ring, as WKT: `(x y,…)`.
    fn star(cx: f64, cy: f64, r: f64, points: usize) -> String {
        let pts: Vec<String> = (0..=points * 2).map(|k| {
            let a = (k % (points * 2)) as f64 / (points * 2) as f64 * std::f64::consts::TAU;
            let r = if k % 2 == 0 { r } else { r * 0.45 };
            format!("{} {}", cx + r * a.cos(), cy + r * a.sin())
        }).collect();
        format!("({})", pts.join(","))
    }

    fn ring_area(ring: &[(f64, f64)]) -> f64 {
        (0..ring.len()).map(|i| {
            let (a, b) = (ring[i], ring[(i + 1) % ring.len()]);
            a.0 * b.1 - b.0 * a.1
        }).sum::<f64>().abs() / 2.0
    }

    #[test]
    fn coverage_through_the_geotransform_adds_up_to_the_polygon() {
        // UTM-like metres, 10 m pixels, north-up and rotated by 3°.
        let (c, s) = (3f64.to_radians().cos(), 3f64.to_radians().sin());
        let north_up = [500_000.0, 10.0, 0.0, 6_300_000.0, 0.0, -10.0];
        let rotated = [500_000.0, 10.0 * c, 10.0 * s, 6_300_000.0, 10.0 * s, -10.0 * c];
        // Well inside a 700-cell window at either offset, rotated or not.
        let (cx, cy) = (503_000.0, 6_297_000.0);
        let mask = parse_wkt_mask(&format!(
            "MULTIPOLYGON(({},{}),({}))",
            star(cx, cy, 900.0, 7), star(cx, cy, 300.0, 5), star(cx - 1200.0, cy - 900.0, 250.0, 6),
        )).unwrap();
        let rings = mask.rings();
        let want = ring_area(&rings[0][0]) - ring_area(&rings[0][1]) + ring_area(&rings[1][0]);

        for (name, gt) in [("north up", north_up), ("rotated", rotated)] {
            for offset in [(0, 0), (3, 4)] {
                let cells = window_polygons(&mask, gt, offset).unwrap();
                let mut total = 0.0;
                crate::mask::coverage_rows(700, 700, &cells, |_, _, run| total += run.iter().sum::<f64>());
                let got = total * 100.0; // 10 m × 10 m cells
                assert!((got - want).abs() < 1e-6 * want, "{name} {offset:?}: {got} m² against {want}");
            }
        }
    }

    #[test]
    fn a_geotransform_that_cannot_be_inverted_is_an_error() {
        let mask = parse_wkt_mask("POLYGON((0 0,1 0,1 1,0 0))").unwrap();
        assert!(window_polygons(&mask, [0.0, 1.0, 1.0, 0.0, 1.0, 1.0], (0, 0)).is_err());
    }

    #[test]
    fn degree_cells_shrink_toward_the_poles_and_metre_cells_do_not() {
        let (areas, method) = cell_areas([0.0, 0.001, 0.0, 60.0, 0.0, -0.001], Some(4326), 0, 2);
        assert_eq!(method, "spherical");
        // 0.001° at 60°N on that sphere: 111.32 m × 55.66 m.
        assert!((areas[0] - 6_196.0).abs() < 1.0, "{}", areas[0]);
        assert!(areas[1] > areas[0], "further south, larger");

        let (areas, method) = cell_areas([500_000.0, 10.0, 0.0, 6_300_000.0, 0.0, -10.0], Some(32735), 0, 3);
        assert_eq!((areas, method), (vec![100.0; 3], "cartesian"));

        // No EPSG code means WGS84 here, as it does for the polygon itself.
        assert_eq!(cell_areas([0.0, 0.001, 0.0, 60.0, 0.0, -0.001], None, 0, 1).1, "spherical");
    }

    #[test]
    fn nodata_is_nan_or_exactly_the_value() {
        assert!(is_nodata(f32::NAN, None));
        assert!(is_nodata(0.0, Some(0.0)));
        assert!(!is_nodata(1e-6, Some(0.0)), "a small value next to nodata 0 is data");
        assert!(!is_nodata(3.0, None));
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
