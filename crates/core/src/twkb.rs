//! TWKB (Tiny Well-Known Binary) — the wire format for clip geometry.
//!
//! A farm block written as WKT with full float precision is ~35 bytes per
//! vertex. The same ring as TWKB at precision 6 is under 3. Measured on the
//! real block sets:
//!
//! ```text
//!                     86x1.5 ha   25x19 ha   25x64 ha
//!   median vertices          32         95         68
//!   WKT                  1,120 B    3,643 B    2,373 B
//!   TWKB p6                 96 B      251 B      181 B
//! ```
//!
//! Precision 6 is six decimal places of longitude and latitude, which bounds
//! the error at about 11 cm — roughly a fifth of a Sentinel-2 pixel, and an
//! order of magnitude under the survey uncertainty on a real field boundary.
//! Quantisation happens once, before the deltas are taken, so the error is
//! per-vertex and does not accumulate along a ring.
//!
//! Two things are easy to get wrong and worth stating in any provider
//! contract:
//!
//! * **Precision is decimal places in the source CRS units.** p6 in degrees is
//!   11 cm; p6 in UTM metres is micrometres, whose deltas no longer fit in
//!   short varints and produce a payload *larger* than WKB. Geometry here is
//!   always WGS84 lon/lat.
//! * **Do not compress it.** A varint delta stream has no redundancy left:
//!   gzipping the 25x19 ha set takes it from 8,339 to 8,731 bytes.
//!
//! Only the subset that can be a mask is supported — Polygon and MultiPolygon,
//! two-dimensional, with no optional header blocks. The optional blocks are
//! *rejected* rather than skipped: a reader that ignores a Z dimension it was
//! not expecting silently reads the wrong numbers.

use crate::mask::Mask;

/// Geometry types, from the low nibble of the header byte.
const TYPE_POLYGON: u8 = 3;
const TYPE_MULTIPOLYGON: u8 = 6;

/// Metadata header bits.
const HAS_BBOX: u8 = 0x01;
const HAS_SIZE: u8 = 0x02;
const HAS_IDLIST: u8 = 0x04;
const HAS_EXT_DIMS: u8 = 0x08;
const IS_EMPTY: u8 = 0x10;

/// What a provider should serve, and what this module writes.
pub const DEFAULT_PRECISION: i8 = 6;

/// The media type a provider should label TWKB with, so the format is decided
/// by a header rather than by sniffing the first byte.
pub const CONTENT_TYPE: &str = "application/vnd.twkb";

// ── Reading ───────────────────────────────────────────────────────────────────

struct Reader<'a> {
    bytes: &'a [u8],
    pos:   usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn byte(&mut self) -> Result<u8, String> {
        let b = *self.bytes.get(self.pos).ok_or("TWKB ended mid-geometry")?;
        self.pos += 1;
        Ok(b)
    }

    /// Unsigned LEB128.
    fn uvarint(&mut self) -> Result<u64, String> {
        let mut value = 0u64;
        let mut shift = 0u32;
        loop {
            let b = self.byte()?;
            if shift >= 64 {
                return Err("TWKB varint is too long to be a real value".into());
            }
            value |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
        }
    }

    /// Zigzag-encoded signed LEB128.
    fn varint(&mut self) -> Result<i64, String> {
        let u = self.uvarint()?;
        Ok(((u >> 1) as i64) ^ -((u & 1) as i64))
    }

    /// A count that is about to drive an allocation.
    ///
    /// A varint costs two bytes and can claim four billion rings, so counts
    /// are checked against what is left to read before anything is reserved.
    fn count(&mut self, per_item: usize, what: &str) -> Result<usize, String> {
        let n = self.uvarint()? as usize;
        let remaining = self.bytes.len() - self.pos;
        if per_item > 0 && n > remaining.saturating_mul(2) / per_item.max(1) + 1 {
            return Err(format!("TWKB claims {n} {what} but only {remaining} bytes remain"));
        }
        Ok(n)
    }
}

/// Decode a TWKB Polygon or MultiPolygon into a [`Mask`].
///
/// Coordinates are read as WGS84 lon/lat. Rings arrive open — TWKB leaves the
/// closing point implicit — and are closed here, because the ray-crossing test
/// walks `ring[i] -> ring[i+1 % n]` and needs the ring to be a loop.
pub fn decode_mask(bytes: &[u8]) -> Result<Mask, String> {
    let mut r = Reader::new(bytes);

    let header = r.byte()?;
    let geom_type = header & 0x0f;
    // The precision nibble is a zigzag-encoded signed 4-bit value: negative
    // precision (rounding to tens, hundreds) is legal and scales the other way.
    let precision = {
        let raw = u64::from(header >> 4);
        ((raw >> 1) as i64) ^ -((raw & 1) as i64)
    };
    if !(-7..=7).contains(&precision) {
        return Err(format!("TWKB precision {precision} is out of range"));
    }
    let scale = 10f64.powi(precision as i32);

    let meta = r.byte()?;
    if meta & IS_EMPTY != 0 {
        return Err("TWKB geometry is empty, so there is nothing to clip to".into());
    }
    // Rejected rather than skipped. Skipping a header block means guessing at
    // its length, and guessing wrong turns the rest of the stream into
    // plausible coordinates.
    for (bit, name) in [
        (HAS_EXT_DIMS, "extended dimensions (Z/M)"),
        (HAS_SIZE,     "a size block"),
        (HAS_BBOX,     "a bounding box"),
        (HAS_IDLIST,   "an id list"),
    ] {
        if meta & bit != 0 {
            return Err(format!(
                "TWKB carries {name}, which this reader does not accept; \
                 encode with ST_AsTWKB(geom, {DEFAULT_PRECISION}) and no optional blocks"));
        }
    }

    // One cursor for the whole geometry: deltas run on from the previous point
    // wherever it was, across ring and polygon boundaries alike.
    let mut cursor = (0i64, 0i64);

    let polygons = match geom_type {
        TYPE_POLYGON => vec![read_polygon(&mut r, &mut cursor, scale)?],
        TYPE_MULTIPOLYGON => {
            let n = r.count(2, "polygons")?;
            if n == 0 {
                return Err("TWKB MultiPolygon has no polygons".into());
            }
            let mut polys = Vec::with_capacity(n);
            for _ in 0..n {
                polys.push(read_polygon(&mut r, &mut cursor, scale)?);
            }
            polys
        }
        other => return Err(format!(
            "TWKB type {other} cannot be a clip geometry; expected Polygon (3) or MultiPolygon (6)")),
    };

    if r.pos != r.bytes.len() {
        return Err(format!("TWKB has {} trailing bytes", r.bytes.len() - r.pos));
    }
    Ok(Mask::from_rings(polygons))
}

fn read_polygon(
    r: &mut Reader,
    cursor: &mut (i64, i64),
    scale: f64,
) -> Result<Vec<Vec<(f64, f64)>>, String> {
    let nrings = r.count(2, "rings")?;
    if nrings == 0 {
        return Err("TWKB polygon has no rings".into());
    }
    let mut rings = Vec::with_capacity(nrings);
    for _ in 0..nrings {
        let npoints = r.count(2, "points")?;
        if npoints < 3 {
            return Err(format!("TWKB ring has {npoints} points; a ring needs at least 3"));
        }
        let mut ring = Vec::with_capacity(npoints + 1);
        for _ in 0..npoints {
            cursor.0 += r.varint()?;
            cursor.1 += r.varint()?;
            ring.push((cursor.0 as f64 / scale, cursor.1 as f64 / scale));
        }
        // TWKB leaves the closing point implicit; the crossing test wants it.
        ring.push(ring[0]);
        rings.push(ring);
    }
    Ok(rings)
}

// ── Writing ───────────────────────────────────────────────────────────────────

fn put_uvarint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn put_varint(out: &mut Vec<u8>, value: i64) {
    put_uvarint(out, ((value << 1) ^ (value >> 63)) as u64);
}

/// Encode a mask as TWKB at `precision` decimal places.
///
/// Always a MultiPolygon, even for one polygon: it costs one byte, and it
/// means every cached geometry has the same shape whatever it came from.
pub fn encode_mask(mask: &Mask, precision: i8) -> Result<Vec<u8>, String> {
    if !(-7..=7).contains(&precision) {
        return Err(format!("TWKB precision {precision} is out of range"));
    }
    let scale = 10f64.powi(precision as i32);
    let mut out = Vec::new();

    let zigzag_precision = ((precision << 1) ^ (precision >> 7)) as u8;
    out.push((zigzag_precision << 4) | TYPE_MULTIPOLYGON);
    out.push(0); // no optional blocks

    let polygons = mask.rings();
    put_uvarint(&mut out, polygons.len() as u64);

    let mut cursor = (0i64, 0i64);
    for rings in polygons {
        put_uvarint(&mut out, rings.len() as u64);
        for ring in rings {
            // Drop the explicit closing point: TWKB implies it. A ring that
            // was not closed to begin with keeps all of its points.
            let points = match ring.len() {
                n if n >= 2 && ring[0] == ring[n - 1] => &ring[..n - 1],
                _ => &ring[..],
            };
            put_uvarint(&mut out, points.len() as u64);
            for &(x, y) in points {
                // Quantise first, then take the delta between the quantised
                // integers, so rounding error stays per-vertex instead of
                // walking along the ring.
                let (qx, qy) = ((x * scale).round() as i64, (y * scale).round() as i64);
                put_varint(&mut out, qx - cursor.0);
                put_varint(&mut out, qy - cursor.1);
                cursor = (qx, qy);
            }
        }
    }
    Ok(out)
}

/// Whether a body looks like TWKB, for providers that send no content type.
///
/// Unambiguous against the alternatives: GeoJSON starts with `{`, WKT with
/// `P`/`M` (0x50/0x4D), and both are valid UTF-8. A TWKB header byte for a
/// Polygon or MultiPolygon at a sane precision is neither.
pub fn looks_like_twkb(bytes: &[u8]) -> bool {
    match bytes.first() {
        Some(&b) => matches!(b & 0x0f, TYPE_POLYGON | TYPE_MULTIPOLYGON) && b >= 0x20,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mask::parse_wkt_mask;

    /// The worked example from the TWKB specification: POINT(1 2) at
    /// precision 0 is `01 00 02 04`. Not a mask, but it pins the header
    /// layout and the zigzag varints that everything else is built from.
    #[test]
    fn the_specs_own_example_decodes_the_way_it_says() {
        let mut r = Reader::new(&[0x01, 0x00, 0x02, 0x04]);
        let header = r.byte().unwrap();
        assert_eq!(header & 0x0f, 1, "type 1 = Point");
        assert_eq!(header >> 4, 0, "precision 0");
        assert_eq!(r.byte().unwrap(), 0, "no optional blocks");
        assert_eq!(r.varint().unwrap(), 1);
        assert_eq!(r.varint().unwrap(), 2);
    }

    #[test]
    fn zigzag_varints_round_trip_through_the_awkward_values() {
        for v in [0i64, 1, -1, 63, 64, -64, -65, 8191, -8192, i32::MAX as i64, i32::MIN as i64] {
            let mut buf = Vec::new();
            put_varint(&mut buf, v);
            assert_eq!(Reader::new(&buf).varint().unwrap(), v, "{v}");
        }
        for v in [0u64, 1, 127, 128, 16_383, 16_384, u32::MAX as u64] {
            let mut buf = Vec::new();
            put_uvarint(&mut buf, v);
            assert_eq!(Reader::new(&buf).uvarint().unwrap(), v, "{v}");
        }
    }

    #[test]
    fn a_square_encodes_to_the_bytes_it_should() {
        // POLYGON((0 0,1 0,1 1,0 1,0 0)) at precision 0. The closing point is
        // implicit, so four points go on the wire, as deltas from (0,0):
        //   (0,0) (+1,0) (0,+1) (-1,0)
        let mask = parse_wkt_mask("POLYGON((0 0,1 0,1 1,0 1,0 0))").unwrap();
        assert_eq!(encode_mask(&mask, 0).unwrap(), vec![
            0x06, // precision 0, type 6 (MultiPolygon)
            0x00, // no optional blocks
            0x01, // 1 polygon
            0x01, // 1 ring
            0x04, // 4 points
            0x00, 0x00, // (0,0)
            0x02, 0x00, // +1, 0
            0x00, 0x02, // 0, +1
            0x01, 0x00, // -1, 0
        ]);
    }

    #[test]
    fn a_ring_survives_the_round_trip_closed() {
        let wkt = "POLYGON((0 0,1 0,1 1,0 1,0 0))";
        let mask = decode_mask(&encode_mask(&parse_wkt_mask(wkt).unwrap(), 6).unwrap()).unwrap();
        let ring = &mask.rings()[0][0];
        assert_eq!(ring.first(), ring.last(), "the decoder has to close the ring again");
        assert_eq!(ring.len(), 5);
    }

    #[test]
    fn holes_survive_and_still_cut_a_hole() {
        let wkt = "POLYGON((0 0,10 0,10 10,0 10,0 0),(2 2,3 2,3 3,2 3,2 2))";
        let mask = decode_mask(&encode_mask(&parse_wkt_mask(wkt).unwrap(), 6).unwrap()).unwrap();
        assert_eq!(mask.rings()[0].len(), 2, "exterior and hole");
        assert!(mask.contains(1.0, 1.0), "inside the exterior");
        assert!(!mask.contains(2.5, 2.5), "inside the hole");
        assert!(!mask.contains(-1.0, 1.0), "outside altogether");
    }

    #[test]
    fn several_polygons_share_one_delta_cursor() {
        // The second polygon's first point is a delta from the last point of
        // the first, not from the origin. Getting this wrong puts polygon two
        // somewhere else entirely, which a round-trip through the same bug
        // would not notice — so check the geometry, not just the bytes.
        let wkt = "MULTIPOLYGON(((0 0,1 0,1 1,0 1,0 0)),((50 50,51 50,51 51,50 51,50 50)))";
        let mask = decode_mask(&encode_mask(&parse_wkt_mask(wkt).unwrap(), 6).unwrap()).unwrap();
        assert!(mask.contains(0.5, 0.5), "first polygon");
        assert!(mask.contains(50.5, 50.5), "second polygon, offset by a large delta");
        assert!(!mask.contains(25.0, 25.0), "the gap between them");
    }

    #[test]
    fn precision_six_holds_a_farm_boundary_to_a_tenth_of_a_metre() {
        // A ring with coordinates that do not land on a 1e-6 grid.
        let pts: Vec<String> = (0..64)
            .map(|i| {
                let a = std::f64::consts::TAU * f64::from(i) / 64.0;
                format!("{:.9} {:.9}", 25.5335995455592 + 0.004 * a.cos(),
                                       -33.3569980970338 + 0.004 * a.sin())
            })
            .collect();
        let wkt = format!("POLYGON(({},{}))", pts.join(","), pts[0]);

        let original = parse_wkt_mask(&wkt).unwrap();
        let decoded = decode_mask(&encode_mask(&original, DEFAULT_PRECISION).unwrap()).unwrap();

        let mut worst_m = 0f64;
        for (a, b) in original.rings()[0][0].iter().zip(&decoded.rings()[0][0]) {
            // Degrees to metres at this latitude, near enough for a bound.
            let dx = (a.0 - b.0) * 111_320.0 * a.1.to_radians().cos();
            let dy = (a.1 - b.1) * 111_320.0;
            worst_m = worst_m.max(dx.hypot(dy));
        }
        assert!(worst_m < 0.10, "worst vertex shift {worst_m:.4} m should be under 10 cm");
    }

    #[test]
    fn precision_six_is_far_smaller_than_the_wkt_it_replaces() {
        let pts: Vec<String> = (0..32)
            .map(|i| {
                let a = std::f64::consts::TAU * f64::from(i) / 32.0;
                format!("{:.14} {:.14}", 25.53 + 0.004 * a.cos(), -33.35 + 0.004 * a.sin())
            })
            .collect();
        let wkt = format!("POLYGON(({},{}))", pts.join(","), pts[0]);
        let twkb = encode_mask(&parse_wkt_mask(&wkt).unwrap(), DEFAULT_PRECISION).unwrap();
        assert!(twkb.len() * 8 < wkt.len(),
                "TWKB {} B against WKT {} B; the saving should be most of an order of magnitude",
                twkb.len(), wkt.len());
    }

    #[test]
    fn optional_header_blocks_are_refused_rather_than_skipped() {
        // Skipping a block means guessing its length; guessing wrong turns the
        // rest of the stream into plausible-looking coordinates.
        for (bit, expect) in [
            (HAS_BBOX,     "bounding box"),
            (HAS_SIZE,     "size block"),
            (HAS_IDLIST,   "id list"),
            (HAS_EXT_DIMS, "extended dimensions"),
        ] {
            let err = decode_mask(&[0x06, bit, 0x01, 0x01, 0x04, 0, 0, 2, 0, 0, 2, 1, 0])
                .unwrap_err();
            assert!(err.contains(expect), "bit {bit:#04x}: {err}");
        }
        let err = decode_mask(&[0x06, IS_EMPTY]).unwrap_err();
        assert!(err.contains("empty"), "{err}");
    }

    #[test]
    fn malformed_input_is_an_error_rather_than_a_panic() {
        for bad in [
            vec![],                                  // nothing at all
            vec![0x06],                              // header, then truncated
            vec![0x01, 0x00, 0x02, 0x04],            // a Point is not a mask
            vec![0x06, 0x00, 0x01, 0x01, 0x02, 0, 0, 2, 0], // ring of 2 points
            vec![0x06, 0x00, 0xff, 0xff, 0xff, 0x7f], // four billion polygons
            vec![0x06, 0x00, 0x01, 0x01, 0x04, 0, 0, 2, 0, 0, 2, 1, 0, 0x99], // trailing byte
        ] {
            assert!(decode_mask(&bad).is_err(), "{bad:02x?} should be an error");
        }
    }

    #[test]
    fn twkb_is_told_apart_from_the_formats_it_shares_a_field_with() {
        let twkb = encode_mask(&parse_wkt_mask("POLYGON((0 0,1 0,1 1,0 1,0 0))").unwrap(),
                               DEFAULT_PRECISION).unwrap();
        assert!(looks_like_twkb(&twkb));
        assert!(!looks_like_twkb(br#"{"type":"Polygon","coordinates":[]}"#));
        assert!(!looks_like_twkb(b"POLYGON((0 0,1 0,1 1,0 1,0 0))"));
        assert!(!looks_like_twkb(b"MULTIPOLYGON(((0 0)))"));
        assert!(!looks_like_twkb(b""));
    }
}
