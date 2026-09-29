//! Style parameters as a caller names them, and the [`StyleMode`] they mean.
//!
//! The server reads these from a query string, a form body or a JSON body; the
//! browser reads them from an options object. They live here so both take the
//! same names with the same defaults: a style that works as a tile URL works
//! unchanged in the browser, and the two cannot drift apart.

use std::borrow::Cow;

use serde::{Deserialize, Deserializer};

use crate::render::StyleMode;
use crate::style::parse_classified_stops;

#[derive(Debug, Clone, Deserialize)]
pub struct StyleParams {
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default = "default_colormap")]
    pub colormap: String,
    #[serde(default, deserialize_with = "lenient::f32")]
    pub min: f32,
    #[serde(default = "default_max", deserialize_with = "lenient::f32")]
    pub max: f32,
    #[serde(default, deserialize_with = "lenient::opt_f32")]
    pub nodata: Option<f32>,
    #[serde(default = "default_contour_level", deserialize_with = "lenient::u32")]
    pub contour_level: u32,
    #[serde(default, deserialize_with = "lenient::f32")]
    pub rgb_min_r: f32,
    #[serde(default, deserialize_with = "lenient::f32")]
    pub rgb_min_g: f32,
    #[serde(default, deserialize_with = "lenient::f32")]
    pub rgb_min_b: f32,
    #[serde(default = "default_max", deserialize_with = "lenient::f32")]
    pub rgb_max_r: f32,
    #[serde(default = "default_max", deserialize_with = "lenient::f32")]
    pub rgb_max_g: f32,
    #[serde(default = "default_max", deserialize_with = "lenient::f32")]
    pub rgb_max_b: f32,
    #[serde(default = "default_z_factor", deserialize_with = "lenient::f64")]
    pub z_factor: f64,
    #[serde(default = "default_azimuth", deserialize_with = "lenient::f64")]
    pub azimuth: f64,
    #[serde(default = "default_altitude", deserialize_with = "lenient::f64")]
    pub altitude: f64,
    pub hillshade_colormap: Option<String>,
    /// Classified-mode stops: a JSON array, or that array as a string.
    pub stops: Option<serde_json::Value>,
}

fn default_mode() -> String { "colormap".into() }
fn default_colormap() -> String { "viridis".into() }
fn default_max() -> f32 { 255.0 }
fn default_contour_level() -> u32 { 10 }
fn default_z_factor() -> f64 { 1.0 }
fn default_azimuth() -> f64 { 315.0 }
fn default_altitude() -> f64 { 45.0 }

impl Default for StyleParams {
    fn default() -> Self {
        serde_json::from_str("{}").expect("every field has a default")
    }
}

impl StyleParams {
    pub fn to_style(&self) -> Result<StyleMode, String> {
        Ok(match self.mode.as_str() {
            "colormap" => StyleMode::Colormap { name: self.colormap.clone(), min: self.min, max: self.max, nodata: self.nodata },
            "rgb" => StyleMode::Rgb {
                min: [self.rgb_min_r, self.rgb_min_g, self.rgb_min_b],
                max: [self.rgb_max_r, self.rgb_max_g, self.rgb_max_b],
                nodata: self.nodata,
            },
            "hillshade" => StyleMode::Hillshade {
                colormap: self.hillshade_colormap.clone(),
                min: self.min, max: self.max,
                z_factor: self.z_factor, azimuth: self.azimuth, altitude: self.altitude,
                nodata: self.nodata,
            },
            "contour" => StyleMode::Contour {
                name: self.colormap.clone(), min: self.min, max: self.max,
                contour_level: self.contour_level.max(1), nodata: self.nodata,
            },
            "classified" => {
                // In a query string or form body `stops` is the JSON text; in a
                // JSON body or a browser options object it may be the array itself.
                let json = match self.stops.as_ref().ok_or("classified mode requires a `stops` parameter")? {
                    serde_json::Value::String(text) => Cow::Borrowed(text.as_str()),
                    value => Cow::Owned(value.to_string()),
                };
                StyleMode::Classified { entries: parse_classified_stops(&json)?, nodata: self.nodata }
            }
            other => return Err(format!(
                "unknown mode `{other}`: use colormap, rgb, hillshade, contour or classified"
            )),
        })
    }
}

/// Numbers that may arrive as numbers or as text.
///
/// The server embeds [`StyleParams`] with `#[serde(flatten)]`, and a flattened
/// struct is deserialized from serde's buffered content rather than from the
/// format itself. A query string has no numbers, only text, so a plain `f32`
/// field that parses `min=0` fine on its own fails once flattened. These
/// accept either, which also forgives `{"min": "0"}` from JavaScript.
mod lenient {
    use super::*;

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Num<'a> {
        N(f64),
        #[serde(borrow)]
        S(Cow<'a, str>),
    }

    fn parse<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
        match Num::deserialize(d)? {
            Num::N(n) => Ok(n),
            Num::S(s) => s.trim().parse().map_err(|_| serde::de::Error::custom(format!("expected a number, got `{s}`"))),
        }
    }

    pub fn f64<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
        parse(d)
    }

    pub fn f32<'de, D: Deserializer<'de>>(d: D) -> Result<f32, D::Error> {
        parse(d).map(|n| n as f32)
    }

    pub fn opt_f32<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f32>, D::Error> {
        Option::<Num>::deserialize(d)?
            .map(|n| match n {
                Num::N(n) => Ok(n as f32),
                Num::S(s) => s.trim().parse().map_err(|_| serde::de::Error::custom(format!("expected a number, got `{s}`"))),
            })
            .transpose()
    }

    pub fn u32<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
        let n = parse(d)?;
        if n >= 0.0 && n <= u32::MAX as f64 && n.fract() == 0.0 {
            Ok(n as u32)
        } else {
            Err(serde::de::Error::custom(format!("expected a whole number, got {n}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct Outer {
        url: String,
        #[serde(flatten)]
        style: StyleParams,
    }

    #[test]
    fn a_flattened_query_string_parses_its_numbers() {
        let o: Outer = serde_urlencoded::from_str(
            "url=x&mode=hillshade&min=-5&max=12.5&nodata=-9999&azimuth=300&contour_level=4",
        ).unwrap();
        assert_eq!(o.url, "x");
        assert_eq!(o.style.min, -5.0);
        assert_eq!(o.style.max, 12.5);
        assert_eq!(o.style.nodata, Some(-9999.0));
        assert_eq!(o.style.azimuth, 300.0);
        assert_eq!(o.style.contour_level, 4);
    }

    #[test]
    fn json_takes_numbers_or_text() {
        let o: Outer = serde_json::from_str(r#"{"url": "x", "min": 1, "max": "2", "nodata": null}"#).unwrap();
        assert_eq!((o.style.min, o.style.max, o.style.nodata), (1.0, 2.0, None));
    }

    #[test]
    fn defaults_match_the_http_api() {
        let s = StyleParams::default();
        assert_eq!((s.mode.as_str(), s.colormap.as_str()), ("colormap", "viridis"));
        assert_eq!((s.min, s.max, s.contour_level), (0.0, 255.0, 10));
        assert_eq!((s.z_factor, s.azimuth, s.altitude), (1.0, 315.0, 45.0));
    }

    #[test]
    fn stops_may_be_an_array_or_its_text() {
        for body in [r##"{"mode": "classified", "stops": [[0, "#000000"]]}"##,
                     r##"{"mode": "classified", "stops": "[[0, \"#000000\"]]"}"##] {
            let s: StyleParams = serde_json::from_str(body).unwrap();
            assert!(matches!(s.to_style(), Ok(StyleMode::Classified { .. })), "{body}");
        }
    }

    #[test]
    fn junk_is_an_error_not_a_default() {
        assert!(serde_urlencoded::from_str::<Outer>("url=x&min=abc").is_err());
        assert!(serde_urlencoded::from_str::<Outer>("url=x&contour_level=2.5").is_err());
        let s: StyleParams = serde_json::from_str(r#"{"mode": "hilshade"}"#).unwrap();
        assert!(s.to_style().is_err());
    }
}
