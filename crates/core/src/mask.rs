use crate::geo::epsg_to_proj4;
use crate::warp::TileWarp;

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

/// Set pixels outside the mask to NaN (which all style functions render as transparent).
/// The mask must be in the raster's CRS, the one `warp` maps into, so each
/// pixel is tested exactly where `rasterize_to_tile` drew it from.
pub fn apply_mask(
    resampled: &mut [f32],
    tile_size: u32,
    samples: usize,
    warp: &TileWarp,
    mask: &Mask,
) {
    let n = tile_size as usize;
    for oy in 0..n {
        for ox in 0..n {
            let (x, y) = warp.at(ox as f64 + 0.5, oy as f64 + 0.5);
            if !mask.contains(x, y) {
                let px = (oy * n + ox) * samples;
                for s in 0..samples {
                    resampled[px + s] = f32::NAN;
                }
            }
        }
    }
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
