use crate::geo::epsg_to_proj4;

/// A clip geometry: one or more polygons, each with an exterior ring and
/// optional holes.
#[derive(Clone, Debug)]
pub struct Mask {
    /// First ring per polygon = exterior; remaining = holes.
    polygons: Vec<Vec<Vec<(f64, f64)>>>,
    /// Exterior-ring bounds per polygon, in the same order.
    ///
    /// `apply_mask` asks `contains` once per output pixel, and without this a
    /// pixel inside one block still walks every edge of every other. A whole
    /// farm is the case that makes that intolerable: 86 blocks of ~30 vertices
    /// against a 256 px tile is 168 million edge tests, which measured at
    /// 50 ms per tile — 98% of the request, recomputed for every tile.
    bounds: Vec<(f64, f64, f64, f64)>,
}

fn ring_bounds(ring: &[(f64, f64)]) -> (f64, f64, f64, f64) {
    ring.iter().fold(
        (f64::MAX, f64::MAX, f64::MIN, f64::MIN),
        |(x0, y0, x1, y1), &(x, y)| (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
    )
}

fn bounds_of(polygons: &[Vec<Vec<(f64, f64)>>]) -> Vec<(f64, f64, f64, f64)> {
    polygons.iter()
        .map(|rings| rings.first().map_or((f64::MAX, f64::MAX, f64::MIN, f64::MIN), |r| ring_bounds(r)))
        .collect()
}

impl Mask {
    /// Build from rings, first per polygon being the exterior.
    pub fn from_rings(polygons: Vec<Vec<Vec<(f64, f64)>>>) -> Self {
        let bounds = bounds_of(&polygons);
        Self { polygons, bounds }
    }

    /// The rings themselves, for encoding.
    pub fn rings(&self) -> &[Vec<Vec<(f64, f64)>>] {
        &self.polygons
    }

    /// How many polygons and vertices this mask carries, for diagnostics.
    pub fn size(&self) -> (usize, usize) {
        (self.polygons.len(),
         self.polygons.iter().flatten().map(Vec::len).sum())
    }

    /// Reproject all ring coordinates from WGS84 into the given EPSG's native CRS.
    /// Returns a new Mask whose coordinates are in native CRS units (metres for UTM, etc.)
    /// so that `apply_mask` can test against `native_bbox` directly.
    /// For WGS84 COGs (EPSG 4326/4269 or None) this is a no-op clone.
    pub fn reproject_from_wgs84(&self, epsg: u32) -> Result<Mask, String> {
        if epsg == 4326 || epsg == 4269 {
            return Ok(self.clone());
        }
        let dst_def = epsg_to_proj4(epsg)
            .ok_or_else(|| format!("EPSG:{epsg} not supported for mask reprojection"))?;
        use proj4rs::{Proj, transform::transform};
        let src = Proj::from_proj_string("+proj=longlat +datum=WGS84 +no_defs")
            .map_err(|e| format!("WGS84 proj error: {e}"))?;
        let dst = Proj::from_proj_string(&dst_def)
            .map_err(|e| format!("proj error for EPSG:{epsg}: {e}"))?;
        let mut new_polygons = Vec::with_capacity(self.polygons.len());
        for polygon in &self.polygons {
            let mut new_rings = Vec::with_capacity(polygon.len());
            for ring in polygon {
                let mut new_ring = Vec::with_capacity(ring.len());
                for &(lon, lat) in ring {
                    // proj4rs expects geographic input in radians
                    let mut pt = (lon.to_radians(), lat.to_radians(), 0.0f64);
                    transform(&src, &dst, &mut pt)
                        .map_err(|e| format!("coordinate transform error: {e}"))?;
                    new_ring.push((pt.0, pt.1));
                }
                new_rings.push(new_ring);
            }
            new_polygons.push(new_rings);
        }
        Ok(Mask::from_rings(new_polygons))
    }

    /// Return the WGS84 bounding box of all rings. Returns None if the mask is empty.
    pub fn wgs84_bbox(&self) -> Option<crate::geo::Bbox> {
        let mut min_lon = f64::MAX; let mut max_lon = f64::MIN;
        let mut min_lat = f64::MAX; let mut max_lat = f64::MIN;
        let mut found = false;
        for polygon in &self.polygons {
            for ring in polygon {
                for &(lon, lat) in ring {
                    min_lon = min_lon.min(lon); max_lon = max_lon.max(lon);
                    min_lat = min_lat.min(lat); max_lat = max_lat.max(lat);
                    found = true;
                }
            }
        }
        found.then_some(crate::geo::Bbox { west: min_lon, south: min_lat, east: max_lon, north: max_lat })
    }

    /// Reproject from WGS84 to the given EPSG, or clone as-is if epsg is None.
    pub fn to_native_crs(&self, epsg: Option<u32>) -> Result<Mask, String> {
        match epsg {
            Some(e) => self.reproject_from_wgs84(e),
            None    => Ok(self.clone()),
        }
    }

    /// Returns true if (lon, lat) is inside any polygon via the even-odd rule.
    /// Summing ray-crossings across all rings of a polygon gives odd = inside
    /// exterior and outside all holes.
    ///
    /// The bounds check first is what makes a many-polygon mask usable: a
    /// point outside a block's bounding box cannot be inside the block, and
    /// four comparisons settle it instead of walking thirty edges.
    pub fn contains(&self, lon: f64, lat: f64) -> bool {
        self.polygons.iter().zip(&self.bounds).any(|(rings, &(x0, y0, x1, y1))| {
            if lon < x0 || lon > x1 || lat < y0 || lat > y1 {
                return false;
            }
            let crossings: usize = rings.iter().map(|r| ring_crossings(r, lon, lat)).sum();
            crossings % 2 == 1
        })
    }
}

/// Parse a WKT `POLYGON(...)` or `MULTIPOLYGON(...)` string (WGS84 lon/lat) into a Mask.
/// An optional `SRID=nnnn;` prefix is silently stripped.
pub fn parse_wkt_mask(wkt: &str) -> Result<Mask, String> {
    let wkt = match wkt.find(';') {
        Some(pos) => &wkt[pos + 1..],
        None => wkt,
    }
    .trim();

    let upper = wkt.to_uppercase();

    if upper.starts_with("MULTIPOLYGON") {
        // Nesting: MULTIPOLYGON( (  (ring),(ring)  ), (  (ring)  ), ... )
        //   level 1 → all-polygon string:  "((ring)),((ring),(hole)),..."
        //   level 2 → per-polygon entry:   "(ring)" or "(ext),(hole)"
        //   level 3 → ring content:        "lon lat,..."
        let rest = &upper["MULTIPOLYGON".len()..];
        let mut polygons = Vec::new();
        for all_polys in &extract_groups(rest)? {
            for entry in &extract_groups(all_polys)? {
                let mut rings = Vec::new();
                for rc in &extract_groups(entry)? {
                    rings.push(parse_ring(rc)?);
                }
                if !rings.is_empty() {
                    polygons.push(rings);
                }
            }
        }
        Ok(Mask::from_rings(polygons))
    } else if upper.starts_with("POLYGON") {
        // Nesting: POLYGON( (ring),(hole) )
        //   level 1 → polygon content:   "(ring),(hole)"
        //   level 2 → ring content:      "lon lat,..."
        let rest = &upper["POLYGON".len()..];
        let mut rings = Vec::new();
        for poly_content in &extract_groups(rest)? {
            for rc in &extract_groups(poly_content)? {
                rings.push(parse_ring(rc)?);
            }
        }
        Ok(Mask::from_rings(if rings.is_empty() { vec![] } else { vec![rings] }))
    } else {
        Err(format!(
            "unsupported WKT type (expected POLYGON or MULTIPOLYGON); got: {:.40}",
            wkt
        ))
    }
}

/// Set pixels of the Web Mercator tile `z/x/y` whose centres fall outside
/// `mask` to NaN, which every style draws transparent. `mask` is WGS84.
///
/// This used to reproject the mask into the raster's CRS and ask
/// [`Mask::contains`] about every pixel through the tile's warp: for a farm,
/// 65,536 point-in-polygon tests against 86 polygons, plus the reprojection,
/// on every tile -- about 3 ms each, and half again the cost of the rest of
/// the render. The tile's own pixel grid is simpler to work in. Mercator from
/// lon/lat is a closed form, so the vertices go straight into pixel space and
/// [`fill_grid`] fills the rows between their edges.
///
/// Edges are straight in Mercator here where they were straight in the
/// raster's CRS before. Across a field boundary's edges the two differ by far
/// less than a pixel.
pub fn apply_mask(resampled: &mut [f32], tile_size: u32, samples: usize, z: u32, x: u32, y: u32, mask: &Mask) {
    let n = tile_size as usize;
    let inside = coverage(n, z, x, y, mask);
    for (px, &keep) in resampled.chunks_exact_mut(samples).zip(&inside) {
        if !keep {
            px.fill(f32::NAN);
        }
    }
}

/// Which pixels of the `n`×`n` tile `z/x/y` have their centre inside `mask`.
fn coverage(n: usize, z: u32, x: u32, y: u32, mask: &Mask) -> Vec<bool> {
    let to_px = tile_pixels(n, z, x, y);
    let tile = crate::geo::tile_to_bbox(z, x, y);
    let near = mask.polygons.iter().zip(&mask.bounds)
        .filter(|(_, &(w, s, e, nth))| !(e < tile.west || w > tile.east || nth < tile.south || s > tile.north))
        .map(|(rings, _)| rings.iter().map(|r| r.iter().copied().map(&to_px).collect()).collect());
    fill_grid(n, n, near)
}

/// Which cells of a `width`×`height` grid have their centre inside any of
/// `polygons`, whose rings are already in the grid's coordinates: (0, 0) is
/// the top-left corner of the first cell, and cells are one unit square.
///
/// Scanline: each row's centre line crosses some edges; sorted, consecutive
/// pairs of crossings bound the spans inside, and the spans are filled rather
/// than every cell tested. A cell is inside exactly when [`Mask::contains`]
/// would say so of its centre in the same coordinates: the same half-open
/// rule picks which edges a row crosses, and a centre on a span's left end is
/// in and on its right end is out, which is what counting crossings strictly
/// to the right of it gives. Polygons union -- each is filled even-odd over
/// its own rings, as `contains` does.
pub(crate) fn fill_grid(
    width: usize,
    height: usize,
    polygons: impl IntoIterator<Item = Vec<Vec<(f64, f64)>>>,
) -> Vec<bool> {
    let mut inside = vec![false; width * height];
    let mut edges: Vec<Edge> = Vec::new();
    let mut active: Vec<Edge> = Vec::new();
    let mut xs: Vec<f64> = Vec::new();
    for rings in polygons {
        edges.clear();
        for pts in &rings {
            for i in 0..pts.len() {
                // The closing edge too, as `ring_crossings` walks it: a ring
                // written closed adds a zero-length edge, which crosses nothing.
                let (a, b) = (pts[i], pts[(i + 1) % pts.len()]);
                if a.1 != b.1 {
                    edges.push(Edge { a, b, top: a.1.min(b.1), bottom: a.1.max(b.1) });
                }
            }
        }
        edges.sort_by(|p, q| p.top.total_cmp(&q.top));

        // Rows whose centre is on or below the top edge and above the bottom.
        let first = edges.first().map_or(0.0, |e| (e.top - 0.5).ceil().max(0.0)) as usize;
        let mut next = 0;
        active.clear();
        for row in first..height {
            let yc = row as f64 + 0.5;
            while next < edges.len() && edges[next].top <= yc {
                active.push(edges[next]);
                next += 1;
            }
            active.retain(|e| e.bottom > yc);
            if active.is_empty() {
                if next == edges.len() {
                    break;
                }
                continue;
            }
            xs.clear();
            // The same expression as `ring_crossings`, so a centre that lies
            // exactly on an edge is decided the same way.
            xs.extend(active.iter().map(|e| e.a.0 + (yc - e.a.1) / (e.b.1 - e.a.1) * (e.b.0 - e.a.0)));
            xs.sort_by(f64::total_cmp);
            let line = &mut inside[row * width..(row + 1) * width];
            for span in xs.chunks_exact(2) {
                // Centres c = col + 0.5 with span[0] <= c < span[1].
                let from = (span[0] - 0.5).ceil().clamp(0.0, width as f64) as usize;
                let to = (span[1] - 0.5).ceil().clamp(0.0, width as f64) as usize;
                if from < to {
                    line[from..to].fill(true);
                }
            }
        }
    }
    inside
}

/// WGS84 lon/lat to pixel coordinates in the `n`-pixel Web Mercator tile
/// `z/x/y`, with (0, 0) its top-left corner.
fn tile_pixels(n: usize, z: u32, x: u32, y: u32) -> impl Fn((f64, f64)) -> (f64, f64) {
    let world = 2f64.powi(z as i32) * n as f64;
    let (x0, y0) = (x as f64 * n as f64, y as f64 * n as f64);
    move |(lon, lat)| {
        // Mercator stops short of the poles; a vertex past it would be infinite.
        let lat = lat.clamp(-85.051_128_779_806_59, 85.051_128_779_806_59).to_radians();
        ((lon + 180.0) / 360.0 * world - x0,
         (0.5 - lat.tan().asinh() / (2.0 * std::f64::consts::PI)) * world - y0)
    }
}

#[derive(Clone, Copy)]
struct Edge {
    a: (f64, f64),
    b: (f64, f64),
    top: f64,
    bottom: f64,
}

/// Extract the contents of each top-level `(...)` group within `s`.
fn extract_groups(s: &str) -> Result<Vec<String>, String> {
    let mut groups = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'(' {
            let start = i + 1;
            let mut depth = 1usize;
            i += 1;
            while i < bytes.len() {
                match bytes[i] {
                    b'(' => depth += 1,
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            if depth == 0 {
                groups.push(s[start..i].to_string());
                i += 1; // skip closing ')'
            } else {
                return Err("unmatched parenthesis in WKT".into());
            }
        } else {
            i += 1;
        }
    }
    Ok(groups)
}

/// Parse comma-separated `lon lat` pairs from a ring string.
fn parse_ring(s: &str) -> Result<Vec<(f64, f64)>, String> {
    let mut coords = Vec::new();
    for pair in s.split(',') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let mut parts = pair.split_whitespace();
        let lon_str = parts.next().ok_or("missing longitude in ring")?;
        let lon: f64 = lon_str
            .parse()
            .map_err(|e| format!("invalid longitude {lon_str:?}: {e}"))?;
        let lat_str = parts.next().ok_or("missing latitude in ring")?;
        let lat: f64 = lat_str
            .parse()
            .map_err(|e| format!("invalid latitude {lat_str:?}: {e}"))?;
        coords.push((lon, lat));
    }
    if coords.len() < 3 {
        return Err(format!("ring has fewer than 3 points (got {})", coords.len()));
    }
    Ok(coords)
}

/// Count how many times a horizontal ray from (lon, lat) towards +∞ crosses a ring edge.
fn ring_crossings(ring: &[(f64, f64)], lon: f64, lat: f64) -> usize {
    let n = ring.len();
    let mut crossings = 0;
    for i in 0..n {
        let (x1, y1) = ring[i];
        let (x2, y2) = ring[(i + 1) % n];
        if (y1 > lat) != (y2 > lat) {
            let x_cross = x1 + (lat - y1) / (y2 - y1) * (x2 - x1);
            if lon < x_cross {
                crossings += 1;
            }
        }
    }
    crossings
}

#[cfg(test)]
mod tests {
    use super::*;

    const N: usize = 256;
    const TILE: (u32, u32, u32) = (16, 37414, 39214);

    /// Pixel-space point in the tile, as WGS84.
    fn lonlat(px: f64, py: f64) -> (f64, f64) {
        let (z, x, y) = TILE;
        let world = 2f64.powi(z as i32) * N as f64;
        let (mx, my) = ((x as f64 * N as f64 + px) / world, (y as f64 * N as f64 + py) / world);
        (mx * 360.0 - 180.0,
         (std::f64::consts::PI * (1.0 - 2.0 * my)).sinh().atan().to_degrees())
    }

    /// Build a WGS84 mask from polygons written in tile pixels.
    fn mask(polygons: &[Vec<Vec<(f64, f64)>>]) -> Mask {
        Mask::from_rings(polygons.iter().map(|rings| rings.iter()
            .map(|r| r.iter().map(|&(px, py)| lonlat(px, py)).collect()).collect()).collect())
    }

    fn fill(m: &Mask) -> Vec<bool> {
        let (z, x, y) = TILE;
        coverage(N, z, x, y, m)
    }

    /// What `contains` says of every pixel centre, asked in pixel space with
    /// the vertices projected exactly as `coverage` projects them.
    fn reference(m: &Mask) -> Vec<bool> {
        let (z, x, y) = TILE;
        let to_px = tile_pixels(N, z, x, y);
        let px = Mask::from_rings(m.rings().iter().map(|rings| rings.iter()
            .map(|r| r.iter().copied().map(&to_px).collect()).collect()).collect());
        (0..N * N).map(|i| px.contains((i % N) as f64 + 0.5, (i / N) as f64 + 0.5)).collect()
    }

    fn star(cx: f64, cy: f64, r: f64, points: usize, closed: bool) -> Vec<(f64, f64)> {
        let mut ring: Vec<(f64, f64)> = (0..points * 2).map(|k| {
            let a = k as f64 / (points * 2) as f64 * std::f64::consts::TAU;
            let r = if k % 2 == 0 { r } else { r * 0.4 };
            (cx + r * a.cos(), cy + r * a.sin())
        }).collect();
        if closed {
            ring.push(ring[0]);
        }
        ring
    }

    #[test]
    fn spans_agree_with_contains_pixel_for_pixel() {
        let cases: Vec<(&str, Vec<Vec<Vec<(f64, f64)>>>)> = vec![
            ("concave star", vec![vec![star(128.0, 128.0, 100.0, 7, true)]]),
            ("unclosed ring", vec![vec![star(100.0, 90.0, 60.0, 5, false)]]),
            ("hole", vec![vec![star(128.0, 128.0, 120.0, 9, true), star(128.0, 128.0, 50.0, 4, true)]]),
            ("overlapping polygons union", vec![
                vec![star(100.0, 128.0, 80.0, 6, true)],
                vec![star(160.0, 128.0, 80.0, 6, true)],
            ]),
            ("off every edge of the tile", vec![vec![star(128.0, 128.0, 400.0, 5, true)]]),
            ("vertices on pixel centres and edges on rows", vec![vec![vec![
                (10.5, 10.5), (200.5, 10.5), (200.5, 100.0), (120.5, 100.0),
                (120.5, 180.5), (10.5, 180.5), (10.5, 10.5),
            ]]]),
        ];
        for (name, polygons) in cases {
            let m = mask(&polygons);
            let (got, want) = (fill(&m), reference(&m));
            let differ = got.iter().zip(&want).filter(|(a, b)| a != b).count();
            assert_eq!(differ, 0, "{name}: {differ} pixels differ");
            assert!(got.iter().any(|&v| v), "{name}: nothing inside");
        }
    }

    #[test]
    fn spans_agree_with_contains_asked_in_wgs84() {
        // What the renderer asked before: each pixel centre as lon/lat. Edges
        // are straight in a different space there, so a pixel whose centre
        // is within a hair of an edge could go either way -- but only those.
        let m = mask(&[
            vec![star(128.0, 128.0, 110.0, 7, true), star(128.0, 128.0, 30.0, 5, true)],
            vec![star(40.0, 40.0, 35.0, 3, true)],
        ]);
        let got = fill(&m);
        let differ = (0..N * N).filter(|&i| {
            let (lon, lat) = lonlat((i % N) as f64 + 0.5, (i / N) as f64 + 0.5);
            got[i] != m.contains(lon, lat)
        }).count();
        assert!(differ <= 2, "{differ} pixels differ");
    }

    #[test]
    fn a_mask_elsewhere_clears_the_tile_and_one_around_it_keeps_all_of_it() {
        let away = mask(&[vec![star(128.0 + 5_000.0, 128.0, 50.0, 5, true)]]);
        assert!(fill(&away).iter().all(|&v| !v));
        let around = mask(&[vec![vec![(-10.0, -10.0), (300.0, -10.0), (300.0, 300.0), (-10.0, 300.0)]]]);
        assert!(fill(&around).iter().all(|&v| v));
    }

    #[test]
    fn apply_mask_clears_every_sample_outside() {
        let m = mask(&[vec![vec![(0.0, 0.0), (128.0, 0.0), (128.0, 256.0), (0.0, 256.0)]]]);
        let mut data = vec![1.0f32; N * N * 3];
        let (z, x, y) = TILE;
        apply_mask(&mut data, N as u32, 3, z, x, y, &m);
        for (i, px) in data.chunks_exact(3).enumerate() {
            let left = i % N < 128;
            assert!(px.iter().all(|v| v.is_nan() != left), "pixel {i}: {px:?}");
        }
    }
}
