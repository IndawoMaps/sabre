#![allow(dead_code)]

/// RGB triple
pub type Rgb = (u8, u8, u8);

pub enum Colormap {
    Named(&'static [(u8, u8, u8)]),
    Custom(Vec<(u8, u8, u8)>),
}

impl Colormap {
    pub fn from_name(name: &str) -> Option<Self> {
        Some(Self::Named(match name.to_lowercase().as_str() {
            "viridis" => VIRIDIS,
            "plasma" => PLASMA,
            "turbo" => TURBO,
            "greys" | "gray" | "grayscale" => GREYS,
            "rdylbu" => RDYLBU,
            "spectral" => SPECTRAL,
            "reds" => REDS,
            "blues" => BLUES,
            "greens" => GREENS,
            "ylgnbu" => YLGNBU,
            "hot" => HOT,
            _ => return None,
        }))
    }


    pub fn sample(&self, t: f64) -> Rgb {
        let lut = match self {
            Self::Named(lut) => *lut,
            Self::Custom(lut) => lut.as_slice(),
        };
        if lut.is_empty() {
            return (0, 0, 0);
        }
        let t = t.clamp(0.0, 1.0);
        let idx_f = t * (lut.len() - 1) as f64;
        let lo = idx_f.floor() as usize;
        let hi = (lo + 1).min(lut.len() - 1);
        let frac = idx_f - lo as f64;
        let (r0, g0, b0) = lut[lo];
        let (r1, g1, b1) = lut[hi];
        (
            lerp_u8(r0, r1, frac),
            lerp_u8(g0, g1, frac),
            lerp_u8(b0, b1, frac),
        )
    }
}

fn lerp_u8(a: u8, b: u8, t: f64) -> u8 {
    (a as f64 + (b as f64 - a as f64) * t).round() as u8
}

/// Apply a colormap to a normalized [0,1] f32 buffer, producing RGBA pixels.
/// `nodata` pixels become transparent.
pub fn apply_colormap(
    values: &[f32],
    colormap: &Colormap,
    width: u32,
    height: u32,
    min: f32,
    max: f32,
    nodata: Option<f32>,
) -> Vec<u8> {
    let range = max - min;
    let mut rgba = Vec::with_capacity(values.len() * 4);
    for &v in values {
        let is_nodata = nodata.map(|nd| (v - nd).abs() < f32::EPSILON * 100.0).unwrap_or(false)
            || v.is_nan()
            || v.is_infinite();
        if is_nodata {
            rgba.extend_from_slice(&[0, 0, 0, 0]);
        } else {
            let t = if range > 0.0 { ((v - min) / range) as f64 } else { 0.0 };
            let (r, g, b) = colormap.sample(t);
            rgba.extend_from_slice(&[r, g, b, 255]);
        }
    }
    let _ = (width, height); // dimensions validated by caller
    rgba
}

/// Apply a stepped contour colormap: classify values into `contour_level` equal-interval
/// bands and sample the colormap at each band's midpoint, producing discrete filled bands.
pub fn apply_contour(
    values: &[f32],
    colormap: &Colormap,
    width: u32,
    height: u32,
    min: f32,
    max: f32,
    contour_level: u32,
    nodata: Option<f32>,
) -> Vec<u8> {
    let n = contour_level.max(1) as f32;
    let interval = (max - min).max(0.001) / n;
    let mut rgba = Vec::with_capacity(values.len() * 4);
    for &v in values {
        let is_nodata = nodata.map(|nd| (v - nd).abs() < f32::EPSILON * 100.0).unwrap_or(false)
            || v.is_nan()
            || v.is_infinite();
        if is_nodata {
            rgba.extend_from_slice(&[0, 0, 0, 0]);
        } else {
            let band = ((v - min) / interval).floor() as i32;
            let band = band.clamp(0, contour_level as i32 - 1) as f32;
            let t = (band + 0.5) / n;
            let (r, g, b) = colormap.sample(t as f64);
            rgba.extend_from_slice(&[r, g, b, 255]);
        }
    }
    let _ = (width, height);
    rgba
}

/// Apply per-channel contrast stretch to an RGB buffer (chunky interleaved).
/// Each channel stretched independently from its own min/max.
pub fn stretch_rgb(
    data: &[f32],
    width: u32,
    height: u32,
    min: [f32; 3],
    max: [f32; 3],
    nodata: Option<f32>,
) -> Vec<u8> {
    let n = (width * height) as usize;
    let mut rgba = vec![0u8; n * 4];
    for i in 0..n {
        let base = i * 3;
        if base + 2 >= data.len() {
            break;
        }
        let is_nodata = nodata
            .map(|nd| {
                (data[base] - nd).abs() < f32::EPSILON * 100.0
                    || data[base].is_nan()
            })
            .unwrap_or(data[base].is_nan());
        if is_nodata {
            // alpha stays 0
            continue;
        }
        let out_base = i * 4;
        for ch in 0..3 {
            let range = max[ch] - min[ch];
            let t = if range > 0.0 {
                ((data[base + ch] - min[ch]) / range).clamp(0.0, 1.0)
            } else {
                0.0
            };
            rgba[out_base + ch] = (t * 255.0).round() as u8;
        }
        rgba[out_base + 3] = 255;
    }
    rgba
}

/// Compute a hillshade from a single-band elevation buffer.
/// `z_factor` scales elevation; `azimuth`/`altitude` are in degrees.
pub fn hillshade(
    elev: &[f32],
    width: u32,
    height: u32,
    pixel_scale: f64,
    z_factor: f64,
    azimuth: f64,
    altitude: f64,
) -> Vec<f32> {
    let w = width as usize;
    let h = height as usize;
    let mut out = vec![0f32; w * h];
    let az_rad = (360.0 - azimuth + 90.0).to_radians();
    let _alt_rad = altitude.to_radians();
    let zenith = (90.0 - altitude).to_radians();
    let cos_z = zenith.cos();
    let sin_z = zenith.sin();

    for y in 1..(h - 1) {
        for x in 1..(w - 1) {
            let idx = y * w + x;
            let dz_dx = ((elev[idx - 1 + w] + 2.0 * elev[idx - 1] + elev[idx - 1 - w])
                - (elev[idx + 1 + w] + 2.0 * elev[idx + 1] + elev[idx + 1 - w]))
                as f64
                / (8.0 * pixel_scale * z_factor);
            let dz_dy = ((elev[idx + w - 1] + 2.0 * elev[idx + w] + elev[idx + w + 1])
                - (elev[idx - w - 1] + 2.0 * elev[idx - w] + elev[idx - w + 1]))
                as f64
                / (8.0 * pixel_scale * z_factor);
            let slope = (dz_dx * dz_dx + dz_dy * dz_dy).sqrt().atan();
            let aspect = if dz_dx.abs() < 1e-10 {
                if dz_dy > 0.0 { 0.0 } else { std::f64::consts::PI }
            } else {
                (std::f64::consts::PI / 2.0 - (dz_dy / dz_dx).atan())
                    + if dz_dx < 0.0 { std::f64::consts::PI } else { 0.0 }
            };
            let hs = cos_z * slope.cos() + sin_z * slope.sin() * (az_rad - aspect).cos();
            out[idx] = hs.max(0.0) as f32;
        }
    }
    out
}

// ── Classified colormap ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ClassifiedEntry {
    pub threshold: f32,
    pub color: Rgb,
}

/// Parse a JSON threshold list into classified entries, sorted ascending.
///
/// Each element is `[threshold, color]` where color is `[r,g,b]` or `"#rrggbb"`.
/// Example: `[[0,[0,0,128]],[10,"#3399ff"],[50,[0,200,0]]]`
pub fn parse_classified_stops(json: &str) -> Result<Vec<ClassifiedEntry>, String> {
    let arr: serde_json::Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let arr = arr.as_array().ok_or("stops must be a JSON array")?;
    let mut entries = Vec::with_capacity(arr.len());
    for item in arr {
        let pair = item.as_array().ok_or("each stop must be [threshold, color]")?;
        if pair.len() != 2 {
            return Err("each stop must be [threshold, color]".into());
        }
        let threshold = pair[0].as_f64().ok_or("threshold must be a number")? as f32;
        let color = parse_color_value(&pair[1])?;
        entries.push(ClassifiedEntry { threshold, color });
    }
    entries.sort_by(|a, b| a.threshold.partial_cmp(&b.threshold).unwrap_or(std::cmp::Ordering::Equal));
    Ok(entries)
}

fn parse_color_value(v: &serde_json::Value) -> Result<Rgb, String> {
    match v {
        serde_json::Value::String(s) => parse_hex_color(s),
        serde_json::Value::Array(arr) if arr.len() == 3 => Ok((
            arr[0].as_u64().ok_or("r must be 0-255")? as u8,
            arr[1].as_u64().ok_or("g must be 0-255")? as u8,
            arr[2].as_u64().ok_or("b must be 0-255")? as u8,
        )),
        _ => Err("color must be [r,g,b] or \"#rrggbb\"".into()),
    }
}

fn parse_hex_color(s: &str) -> Result<Rgb, String> {
    let s = s.strip_prefix('#').unwrap_or(s);
    if s.len() != 6 {
        return Err(format!("invalid hex color '#{s}'"));
    }
    Ok((
        u8::from_str_radix(&s[0..2], 16).map_err(|_| format!("invalid hex '#{s}'"))?,
        u8::from_str_radix(&s[2..4], 16).map_err(|_| format!("invalid hex '#{s}'"))?,
        u8::from_str_radix(&s[4..6], 16).map_err(|_| format!("invalid hex '#{s}'"))?,
    ))
}

/// Apply a classified (threshold-based) colormap to a value buffer.
/// Each pixel gets the color of the highest threshold it meets; below all thresholds → transparent.
pub fn apply_classified(values: &[f32], entries: &[ClassifiedEntry], nodata: Option<f32>) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(values.len() * 4);
    for &v in values {
        let is_nodata = nodata.map(|nd| (v - nd).abs() < f32::EPSILON * 100.0).unwrap_or(false)
            || v.is_nan()
            || v.is_infinite();
        if is_nodata {
            rgba.extend_from_slice(&[0, 0, 0, 0]);
            continue;
        }
        match entries.iter().rev().find(|e| v >= e.threshold) {
            Some(e) => rgba.extend_from_slice(&[e.color.0, e.color.1, e.color.2, 255]),
            None    => rgba.extend_from_slice(&[0, 0, 0, 0]),
        }
    }
    rgba
}

// --- Colormap LUTs (256 entries each) ---
// Sampled from matplotlib at 256 points.

include!("colormaps.rs");
