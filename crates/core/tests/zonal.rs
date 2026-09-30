//! Zonal statistics against exactextract, the reference they are defined by.
//!
//! The expected numbers are exactextract 0.3.0's, for the same polygon over
//! the same fixture: the WKT below reprojected to EPSG:32735 vertex by vertex
//! with PROJ, then
//!
//! ```text
//! exact_extract(fixture, polygon, ["count", "sum", "mean", "stdev", "min", "max",
//!     "unique", "frac", "total=count(default_value=1)",
//!     "data_area=count(coverage_weight=area_cartesian)",
//!     "total_area=count(default_value=1,coverage_weight=area_cartesian)"])
//! ```
//!
//! The polygon is a seven-pointed star with a hole, reaching past the
//! raster's east and west edges, so boundary cells, a hole and the raster's
//! own edge all have to be right. exactextract holds coverage fractions as
//! 32-bit floats and sabre reprojects with proj4rs, so the two agree to
//! about 1e-8, not to the last digit.

use async_trait::async_trait;
use sabre_core::cog::{fetch_meta, RangeReader};
use sabre_core::mask::parse_wkt_mask;
use sabre_core::query::{query_polygon, QueryResult};

struct MemReader(Vec<u8>);

#[async_trait(?Send)]
impl RangeReader for MemReader {
    async fn read_range(&self, offset: u64, length: u64) -> Result<Vec<u8>, String> {
        let start = (offset as usize).min(self.0.len());
        let end = (start + length as usize).min(self.0.len());
        Ok(self.0[start..end].to_vec())
    }
}

/// Sentinel-2 red, UTM 35S, 10 m, 146 × 242, uint16, values 47 to 528.
const FIXTURE: &[u8] = include_bytes!("fixtures/s2_b04_predictor2.tif");

const STAR: &str = "POLYGON((25.540928609 -33.354885088,25.535707315 -33.353738743,\
25.535952232 -33.349214158,25.531628902 -33.351925085,25.527566802 -33.348943153,\
25.527396374 -33.353470134,25.522085538 -33.354276077,25.526196658 -33.357210541,\
25.523636079 -33.361197619,25.528933469 -33.360329784,25.531051957 -33.36449557,\
25.533546051 -33.360478872,25.53874823 -33.361686079,25.536560677 -33.357545517,\
25.540928609 -33.354885088),(25.534534127 -33.356783081,25.532945641 -33.356101688,\
25.532713996 -33.354617262,25.531450905 -33.355674487,25.529697051 -33.355400625,\
25.530504845 -33.356735451,25.529652484 -33.35805062,25.531414893 -33.357818379,\
25.532642032 -33.358905049,25.532923407 -33.357426687,25.534534127 -33.356783081))";

/// exactextract's answers, and how many distinct values it saw.
struct Reference {
    count: f64,
    sum: f64,
    mean: f64,
    stdev: f64,
    min: f64,
    max: f64,
    /// `total`: count with nodata cells included.
    total: f64,
    data_area: f64,
    total_area: f64,
    variety: usize,
    frac_of_300: f64,
}

fn close(got: f64, want: f64, what: &str) {
    let rel = (got - want).abs() / want.abs().max(1e-12);
    assert!(rel < 1e-6, "{what}: sabre {got}, exactextract {want} (relative {rel:.1e})");
}

fn check(nodata: Option<f32>, want: Reference) {
    let reader = MemReader(FIXTURE.to_vec());
    let meta = pollster::block_on(fetch_meta(&reader)).unwrap();
    let mask = parse_wkt_mask(STAR).unwrap();
    let got = pollster::block_on(query_polygon(&mask, 0, nodata, true, &reader, &meta)).unwrap();
    let QueryResult::Polygon { min, max, avg, stdev, count, sum, nodata_count, area, classes } = got else {
        panic!("not a polygon result");
    };
    close(count, want.count, "count");
    close(sum, want.sum, "sum");
    close(avg.unwrap(), want.mean, "mean");
    close(stdev.unwrap(), want.stdev, "stdev");
    assert_eq!((min, max), (Some(want.min), Some(want.max)), "min and max take any covered cell");
    close(count + nodata_count, want.total, "count with nodata");
    close(area.data, want.data_area, "data area");
    close(area.total, want.total_area, "total area");
    if want.total_area > want.data_area {
        close(area.nodata, want.total_area - want.data_area, "nodata area");
    } else {
        assert_eq!(area.nodata, 0.0);
    }
    assert_eq!(area.method, "cartesian");

    let classes = classes.unwrap();
    assert_eq!(classes.len(), want.variety, "distinct values");
    close(classes.iter().find(|c| c.value == 300.0).unwrap().frac, want.frac_of_300, "frac of 300");
    close(classes.iter().map(|c| c.frac).sum(), 1.0, "fracs sum to one");
    close(classes.iter().map(|c| c.area).sum(), area.data, "class areas sum to the data area");
}

#[test]
fn statistics_match_exactextract() {
    check(None, Reference {
        count: 12393.871308824935,
        sum: 2265498.389400461,
        mean: 182.79182774693933,
        stdev: 41.84963758068466,
        min: 47.0,
        max: 466.0,
        total: 12393.871308824935,
        data_area: 1239387.130964547,
        total_area: 1239387.130964547,
        variety: 316,
        frac_of_300: 0.00048411023888296777,
    });
}

#[test]
fn nodata_is_measured_not_dropped() {
    // 164, the raster's commonest value, declared nodata: it leaves count,
    // sum and every frac, and its share of the polygon shows up as nodata.
    check(Some(164.0), Reference {
        count: 12212.866906053343,
        sum: 2235813.66734592,
        mean: 183.07033758287602,
        stdev: 42.09558069624799,
        min: 47.0,
        max: 466.0,
        total: 12393.871308824935,
        data_area: 1221286.6906817253,
        total_area: 1239387.130964547,
        variety: 315,
        frac_of_300: 0.0004912851377284791,
    });
}
