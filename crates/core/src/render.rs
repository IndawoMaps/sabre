use crate::cog::{decode_tile, fetch_tile_ranges, CogMeta, RangeReader};
use crate::mask::Mask;
use crate::timing::{phase, Timings};
use crate::geo::{bbox_to_pixel_window, best_overview_for_bbox, PixelWindow};
use crate::style::{apply_classified, apply_colormap, apply_contour, hillshade, stretch_rgb, ClassifiedEntry, Colormap};
use crate::tiff::Ifd;
use crate::warp::TileWarp;

/// Style modes supported by the renderer.
#[derive(Debug, Clone)]
pub enum StyleMode {
    Colormap { name: String, min: f32, max: f32, nodata: Option<f32> },
    Rgb      { min: [f32; 3], max: [f32; 3], nodata: Option<f32> },
    Hillshade {
        colormap: Option<String>,
        min: f32, max: f32,
        z_factor: f64, azimuth: f64, altitude: f64,
        nodata: Option<f32>,
    },
    Contour     { name: String, min: f32, max: f32, contour_level: u32, nodata: Option<f32> },
    Classified  { entries: Vec<ClassifiedEntry>, nodata: Option<f32> },
}

pub struct TileRequest {
    pub z: u32,
    pub x: u32,
    pub y: u32,
    pub tile_size: u32,
    pub style: StyleMode,
    pub bilinear: bool,
    /// Already parsed. The caller resolves the geometry once -- from a `mask`
    /// parameter, or from a geometry provider -- so a tile does not re-parse
    /// text that has not changed since the last one.
    pub mask: Option<std::sync::Arc<Mask>>,
}

/// Render a tile as a PNG-encoded RGBA image.
pub async fn render_tile(req: &TileRequest, reader: &dyn RangeReader, meta: &CogMeta) -> Result<Vec<u8>, String> {
    render_tile_timed(req, reader, meta, &Timings::off()).await
}

/// As [`render_tile`], recording where the time went.
pub async fn render_tile_timed(
    req: &TileRequest,
    reader: &dyn RangeReader,
    meta: &CogMeta,
    t: &Timings,
) -> Result<Vec<u8>, String> {
    let rgba = render_tile_rgba_timed(req, reader, meta, t).await?;
    t.time(phase::ENCODE, || encode_png(&rgba, req.tile_size, req.tile_size))
}

/// Render a tile as unencoded RGBA, `tile_size * tile_size * 4` bytes.
///
/// For a caller that wants pixels, not a file: a browser map library can take
/// these directly, where a PNG would be encoded here only to be decoded again
/// on the other side. A tile with no data is fully transparent.
pub async fn render_tile_rgba_timed(
    req: &TileRequest,
    reader: &dyn RangeReader,
    meta: &CogMeta,
    t: &Timings,
) -> Result<Vec<u8>, String> {
    let plan_start = t.start();

    if meta.ifds.is_empty() {
        return Err("COG has no IFDs".into());
    }

    let base = &meta.ifds[0];
    let base_gt = base.geo_transform()
        .ok_or_else(|| "COG has no GeoTIFF geotransform".to_string())?;

    // Where each output pixel lands in the raster's CRS. Its bounding box is
    // what to read; the warp itself is what places pixels -- see crate::warp.
    let warp = match TileWarp::new(req.z, req.x, req.y, req.tile_size, base.epsg_code) {
        Ok(w) => w,
        Err(_) => return Ok(blank(req.tile_size)),
    };
    let native_bbox = match warp.bbox() {
        Some(b) => b,
        None => return Ok(blank(req.tile_size)),
    };

    // The level depends on how many source pixels the tile spans, not on the
    // raster's size: see best_overview_for_bbox.
    let overview_idx = best_overview_for_bbox(&base_gt, &native_bbox, req.tile_size, meta.ifds.len());
    let ifd = meta.ifd(overview_idx);
    let scale = base.image_width as f64 / ifd.image_width as f64;
    let geo_transform = [
        base_gt[0], base_gt[1] * scale, base_gt[2] * scale,
        base_gt[3], base_gt[4] * scale, base_gt[5] * scale,
    ];

    let win = match bbox_to_pixel_window(&geo_transform, ifd.image_width, ifd.image_height, &native_bbox) {
        Some(w) => w,
        None => return Ok(blank(req.tile_size)),
    };

    let samples    = ifd.samples_per_pixel.max(1) as usize;
    let bits       = ifd.bits_per_sample.first().copied().unwrap_or(8);
    let sample_fmt = ifd.sample_format.first().copied().unwrap_or(1);
    let cog_nodata: Option<f32> = ifd.nodata.as_deref().and_then(|s| s.parse().ok());

    let fetch_win = if req.bilinear {
        let x0 = (win.x_off - 1).max(0);
        let y0 = (win.y_off - 1).max(0);
        let x1 = (win.x_off + win.x_size + 1).min(ifd.image_width  as i64);
        let y1 = (win.y_off + win.y_size + 1).min(ifd.image_height as i64);
        PixelWindow { x_off: x0, y_off: y0, x_size: x1 - x0, y_size: y1 - y0 }
    } else {
        win
    };

    let mosaic_w = fetch_win.x_size as usize;
    let mosaic_h = fetch_win.y_size as usize;
    let mut mosaic = vec![0f32; mosaic_w * mosaic_h * samples];
    t.since(phase::PLAN, plan_start);

    if ifd.is_tiled() {
        let tile_w  = ifd.tile_width.unwrap()  as usize;
        let tile_h  = ifd.tile_height.unwrap() as usize;
        let tiles_x = ifd.num_tiles_x() as usize;

        let indices = overlapping_tile_indices(ifd, &fetch_win);
        if indices.is_empty() { return Ok(blank(req.tile_size)); }

        let offsets:     Vec<u64> = indices.iter().map(|&i| ifd.tile_offsets[i]).collect();
        let byte_counts: Vec<u64> = indices.iter().map(|&i| ifd.tile_byte_counts[i]).collect();
        let raw_tiles = t.time_async(phase::FETCH,
            fetch_tile_ranges(reader, &offsets, &byte_counts)).await?;

        for (raw, &idx) in raw_tiles.iter().zip(indices.iter()) {
            let decoded = t.time(phase::DECODE, || decode_tile(
                raw, ifd.compression, ifd.predictor, tile_w as u32, tile_h as u32,
                samples as u16, bits, &meta.lzw_early_change))?;
            let (tx, ty) = (idx % tiles_x, idx / tiles_x);
            t.time(phase::BLIT, || blit_tile(&decoded, bits, samples, sample_fmt,
                      tile_w, tile_h, tx * tile_w, ty * tile_h,
                      &mut mosaic, fetch_win.x_off as usize, fetch_win.y_off as usize, mosaic_w, mosaic_h));
        }
    } else {
        let rps   = if ifd.rows_per_strip == u32::MAX { ifd.image_height } else { ifd.rows_per_strip } as usize;
        let img_w = ifd.image_width as usize;

        let indices = overlapping_strip_indices(&ifd.strip_offsets, &fetch_win, rps);
        if indices.is_empty() { return Ok(blank(req.tile_size)); }

        let offsets:     Vec<u64> = indices.iter().map(|&i| ifd.strip_offsets[i]).collect();
        let byte_counts: Vec<u64> = indices.iter().map(|&i| ifd.strip_byte_counts[i]).collect();
        let raw_strips = t.time_async(phase::FETCH,
            fetch_tile_ranges(reader, &offsets, &byte_counts)).await?;

        for (raw, &idx) in raw_strips.iter().zip(indices.iter()) {
            let decoded = t.time(phase::DECODE, || decode_tile(
                raw, ifd.compression, ifd.predictor, img_w as u32, rps as u32,
                samples as u16, bits, &meta.lzw_early_change))?;
            t.time(phase::BLIT, || blit_tile(&decoded, bits, samples, sample_fmt,
                      img_w, rps, 0, idx * rps,
                      &mut mosaic, fetch_win.x_off as usize, fetch_win.y_off as usize, mosaic_w, mosaic_h));
        }
    }

    let raster_nodata = match &req.style {
        StyleMode::Colormap   { nodata, .. } => nodata.or(cog_nodata),
        StyleMode::Rgb        { nodata, .. } => nodata.or(cog_nodata),
        StyleMode::Hillshade  { nodata, .. } => nodata.or(cog_nodata),
        StyleMode::Contour    { nodata, .. } => nodata.or(cog_nodata),
        StyleMode::Classified { nodata, .. } => nodata.or(cog_nodata),
    };

    let mut resampled = t.time(phase::RESAMPLE, || rasterize_to_tile(
        &mosaic, mosaic_w, samples,
        &warp, &geo_transform, &fetch_win,
        req.tile_size, req.bilinear, raster_nodata,
    ));

    if let Some(mask) = &req.mask {
        let mask_start = t.start();
        let native_mask = match base.epsg_code {
            Some(epsg) => match mask.reproject_from_wgs84(epsg) {
                Ok(m)  => { log("[mask] reprojection ok"); std::sync::Arc::new(m) }
                Err(e) => { log(&format!("[mask] reprojection error: {e}")); mask.clone() }
            }
            None => mask.clone(),
        };
        crate::mask::apply_mask(&mut resampled, req.tile_size, samples, &warp, &native_mask);
        t.since(phase::MASK, mask_start);
    }

    let style_start = t.start();
    let rgba = match &req.style {
        StyleMode::Colormap { name, min, max, nodata } => {
            let cm = Colormap::from_name(name).ok_or_else(|| format!("unknown colormap '{name}'"))?;
            let band: Vec<f32> = resampled.chunks(samples).map(|c| c[0]).collect();
            apply_colormap(&band, &cm, req.tile_size, req.tile_size, *min, *max, nodata.or(cog_nodata))
        }
        StyleMode::Rgb { min, max, nodata } => {
            if samples < 3 { return Err("RGB style requires at least 3 bands".into()); }
            stretch_rgb(&resampled, req.tile_size, req.tile_size, *min, *max, nodata.or(cog_nodata))
        }
        StyleMode::Contour { name, min, max, contour_level, nodata } => {
            let cm = Colormap::from_name(name).ok_or_else(|| format!("unknown colormap '{name}'"))?;
            let band: Vec<f32> = resampled.chunks(samples).map(|c| c[0]).collect();
            apply_contour(&band, &cm, req.tile_size, req.tile_size, *min, *max, *contour_level, nodata.or(cog_nodata))
        }
        StyleMode::Classified { entries, nodata } => {
            let band: Vec<f32> = resampled.chunks(samples).map(|c| c[0]).collect();
            apply_classified(&band, entries, nodata.or(cog_nodata))
        }
        StyleMode::Hillshade { colormap, min, max, z_factor, azimuth, altitude, nodata } => {
            let elev: Vec<f32> = resampled.chunks(samples).map(|c| c[0]).collect();
            let nd = nodata.or(cog_nodata);
            let pixel_scale = (geo_transform[1].abs() * 111_320.0).max(geo_transform[5].abs() * 111_320.0);
            let hs = hillshade(&elev, req.tile_size, req.tile_size, pixel_scale, *z_factor, *azimuth, *altitude);
            if let Some(cm_name) = colormap {
                let cm = Colormap::from_name(cm_name).ok_or_else(|| format!("unknown colormap '{cm_name}'"))?;
                let colored = apply_colormap(&elev, &cm, req.tile_size, req.tile_size, *min, *max, nd);
                blend_hillshade(colored, &hs, req.tile_size * req.tile_size)
            } else {
                let elev_nd = nd.unwrap_or(f32::NAN);
                hs.iter().zip(elev.iter())
                    .flat_map(|(&v, &e)| {
                        if e == elev_nd || e.is_nan() { [0u8, 0, 0, 0] }
                        else { let b = (v * 255.0).round() as u8; [b, b, b, if v > 0.0 { 255 } else { 0 }] }
                    })
                    .collect()
            }
        }
    };
    t.since(phase::STYLE, style_start);

    Ok(rgba)
}

fn log(msg: &str) { let _ = msg; }

// ── Helpers ───────────────────────────────────────────────────────────────────

fn blank(size: u32) -> Vec<u8> {
    vec![0u8; (size * size * 4) as usize]
}

fn encode_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    use image::{ImageEncoder, ColorType};
    use image::codecs::png::PngEncoder;
    let mut buf = Vec::new();
    PngEncoder::new(&mut buf)
        .write_image(rgba, width, height, ColorType::Rgba8.into())
        .map_err(|e| e.to_string())?;
    Ok(buf)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn blit_tile(
    tile_data: &[u8], bits: u16, samples: usize, sample_format: u16,
    tile_w: usize, tile_h: usize, tile_origin_x: usize, tile_origin_y: usize,
    mosaic: &mut [f32], win_x: usize, win_y: usize, mosaic_w: usize, mosaic_h: usize,
) {
    let bytes_per_sample = (bits as usize + 7) / 8;
    let bytes_per_pixel  = bytes_per_sample * samples;
    for ty in 0..tile_h {
        let img_y = tile_origin_y + ty;
        if img_y < win_y || img_y >= win_y + mosaic_h { continue; }
        let mosaic_row = img_y - win_y;
        for tx in 0..tile_w {
            let img_x = tile_origin_x + tx;
            if img_x < win_x || img_x >= win_x + mosaic_w { continue; }
            let mosaic_col = img_x - win_x;
            let src = (ty * tile_w + tx) * bytes_per_pixel;
            let dst = (mosaic_row * mosaic_w + mosaic_col) * samples;
            if src + bytes_per_pixel > tile_data.len() || dst + samples > mosaic.len() { continue; }
            for s in 0..samples {
                let b = src + s * bytes_per_sample;
                mosaic[dst + s] = bytes_to_f32(&tile_data[b..b + bytes_per_sample], bits, sample_format);
            }
        }
    }
}

pub(crate) fn bytes_to_f32(bytes: &[u8], bits: u16, sample_format: u16) -> f32 {
    use byteorder::{LittleEndian, ReadBytesExt};
    let mut c = std::io::Cursor::new(bytes);
    match (bits, sample_format) {
        (8,  1) => c.read_u8().unwrap_or(0) as f32,
        (8,  2) => c.read_i8().unwrap_or(0) as f32,
        (16, 1) => c.read_u16::<LittleEndian>().unwrap_or(0) as f32,
        (16, 2) => c.read_i16::<LittleEndian>().unwrap_or(0) as f32,
        (32, 1) => c.read_u32::<LittleEndian>().unwrap_or(0) as f32,
        (32, 2) => c.read_i32::<LittleEndian>().unwrap_or(0) as f32,
        (32, 3) => c.read_f32::<LittleEndian>().unwrap_or(0.0),
        (64, 3) => c.read_f64::<LittleEndian>().unwrap_or(0.0) as f32,
        _ => 0.0,
    }
}

fn rasterize_to_tile(
    mosaic: &[f32], mosaic_w: usize, samples: usize,
    warp: &TileWarp, geo_transform: &[f64; 6], win: &PixelWindow,
    tile_size: u32, bilinear: bool, nodata: Option<f32>,
) -> Vec<f32> {
    let out_n = tile_size as usize;
    let mut out = vec![f32::NAN; out_n * out_n * samples];

    let [x0, pw, xr, y0, yr, ph] = *geo_transform;
    let det = pw * ph - xr * yr;
    if det.abs() < 1e-15 { return out; }
    let inv_pw =  ph / det;
    let inv_xr = -xr / det;
    let inv_yr = -yr / det;
    let inv_ph =  pw / det;


    let sample = |col: i64, row: i64, s: usize| -> f32 {
        if col < win.x_off || col >= win.x_off + win.x_size
        || row < win.y_off || row >= win.y_off + win.y_size { return f32::NAN; }
        mosaic[((row - win.y_off) as usize * mosaic_w + (col - win.x_off) as usize) * samples + s]
    };

    let is_invalid = |v: f32| v.is_nan()
        || nodata.map(|nd| (v - nd).abs() <= nd.abs() * 1e-5 + 1e-5).unwrap_or(false);

    for oy in 0..out_n {
        for ox in 0..out_n {
            let (cog_x, cog_y) = warp.at(ox as f64 + 0.5, oy as f64 + 0.5);
            let dx = cog_x - x0; let dy = cog_y - y0;
            let col_f = inv_pw * dx + inv_xr * dy;
            let row_f = inv_yr * dx + inv_ph * dy;
            let dst = (oy * out_n + ox) * samples;

            if bilinear {
                let c0 = col_f.floor() as i64; let r0 = row_f.floor() as i64;
                let fx = (col_f - c0 as f64) as f32; let fy = (row_f - r0 as f64) as f32;
                for s in 0..samples {
                    let v00 = sample(c0,   r0,   s); let v10 = sample(c0+1, r0,   s);
                    let v01 = sample(c0,   r0+1, s); let v11 = sample(c0+1, r0+1, s);
                    out[dst + s] = if is_invalid(v00)||is_invalid(v10)||is_invalid(v01)||is_invalid(v11) {
                        if is_invalid(v00) { f32::NAN } else { v00 }
                    } else {
                        let top = v00 + (v10 - v00) * fx;
                        top + (v01 + (v11 - v01) * fx - top) * fy
                    };
                }
            } else {
                let col = col_f.floor() as i64; let row = row_f.floor() as i64;
                for s in 0..samples { out[dst + s] = sample(col, row, s); }
            }
        }
    }
    out
}

fn blend_hillshade(mut rgba: Vec<u8>, hs: &[f32], n_pixels: u32) -> Vec<u8> {
    for i in 0..n_pixels as usize {
        let h = hs.get(i).copied().unwrap_or(0.0);
        let b = i * 4;
        rgba[b]   = ((rgba[b]   as f32) * h).round() as u8;
        rgba[b+1] = ((rgba[b+1] as f32) * h).round() as u8;
        rgba[b+2] = ((rgba[b+2] as f32) * h).round() as u8;
    }
    rgba
}

pub(crate) fn overlapping_strip_indices(strip_offsets: &[u64], win: &PixelWindow, rows_per_strip: usize) -> Vec<usize> {
    if rows_per_strip == 0 || strip_offsets.is_empty() { return vec![]; }
    let first = win.y_off as usize / rows_per_strip;
    let last  = ((win.y_off + win.y_size - 1) as usize / rows_per_strip + 1).min(strip_offsets.len());
    (first..last).collect()
}

pub(crate) fn overlapping_tile_indices(ifd: &Ifd, win: &PixelWindow) -> Vec<usize> {
    let tile_w = ifd.tile_width.unwrap_or(256)  as i64;
    let tile_h = ifd.tile_height.unwrap_or(256) as i64;
    let tiles_x = ifd.num_tiles_x() as i64;
    let tiles_y = ifd.num_tiles_y() as i64;
    let tx_start = (win.x_off / tile_w).max(0);
    let tx_end   = ((win.x_off + win.x_size - 1) / tile_w + 1).min(tiles_x);
    let ty_start = (win.y_off / tile_h).max(0);
    let ty_end   = ((win.y_off + win.y_size - 1) / tile_h + 1).min(tiles_y);
    let mut indices = Vec::new();
    for ty in ty_start..ty_end {
        for tx in tx_start..tx_end {
            let idx = (ty * tiles_x + tx) as usize;
            if idx < ifd.tile_offsets.len() { indices.push(idx); }
        }
    }
    indices
}
