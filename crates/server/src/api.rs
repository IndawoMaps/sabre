//! Endpoint logic shared by every runtime.
//!
//! Each function takes already-parsed parameters and a [`RangeReader`] and
//! returns an [`ApiResponse`]. Nothing in here touches the network or the
//! runtime, which is what lets the Worker and the native binary share it.

use std::sync::Arc;

use sabre_core::{
    cog::{fetch_meta, RangeReader},
    mask::Mask,
    geo,
    query::{query_point, query_polygon_timed},
    render::{render_tile_timed, TileRequest},
    timing::{phase, Timings},
    params::StyleParams,
};
use serde::Deserialize;

use crate::ApiResponse;

// ── Parameters ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct TileParams {
    pub url: String,
    #[serde(default = "default_tile_size")]     pub tile_size: u32,
    #[serde(default)]                           pub interpolation: String,
                                                pub mask: Option<String>,
    /// Name a geometry instead of carrying it: `geometry_provider` selects a
    /// registry entry the operator configured, `geometry_id` names one or more
    /// geometries in it. Resolves to `mask`; supplying both is an error.
                                                pub geometry_provider: Option<String>,
                                                pub geometry_id: Option<String>,
    /// Shared with the browser, so a style means the same thing in both.
    #[serde(flatten)]                           pub style: StyleParams,
}

fn default_tile_size()    -> u32    { 256 }

#[derive(Debug, Clone, Deserialize)]
pub struct QueryParams {
    pub url:     String,
    pub lat:     Option<f64>,
    pub lng:     Option<f64>,
    pub polygon: Option<String>,
    /// As on a tile, but resolving to `polygon`.
    pub geometry_provider: Option<String>,
    pub geometry_id:       Option<String>,
    #[serde(default)]
    pub band:    usize,
    pub nodata:  Option<f32>,
    /// Polygon queries: add a breakdown by distinct value.
    #[serde(default)]
    pub classes: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UrlParam { pub url: String }

#[derive(Debug, Clone, Deserialize)]
pub struct TileInfoParams {
    pub url: String,
    #[serde(default = "default_tile_size")] pub tile_size: u32,
}

// ── Tiles ─────────────────────────────────────────────────────────────────────

/// Web Mercator stops being meaningful long before this; it is here to bound
/// the shift below, not to express an opinion about zoom.
const MAX_ZOOM: u32 = 30;

/// Reject a tile coordinate that cannot exist, and name the usual reason.
///
/// sabre served tiles as `/tiles/{x}/{y}/{z}` before v0.1.0. The two orders
/// cannot be told apart in general — `/tiles/3/5/10` is a legal request either
/// way and means a different tile each way — so there is no redirect to offer
/// and no compatibility shim that would not sometimes serve the wrong tile
/// silently. What is possible is to notice when the numbers only make sense
/// the old way, and say so, instead of reporting zoom 270 as out of range and
/// leaving the caller to work out why.
fn check_tile(z: u32, x: u32, y: u32) -> Result<(), String> {
    let fits = |z: u32, x: u32, y: u32| z <= MAX_ZOOM && x < (1u32 << z) && y < (1u32 << z);
    if fits(z, x, y) {
        return Ok(());
    }
    // Read the same three path segments the old way round: /tiles/a/b/c was
    // x=a, y=b, z=c, which in today's names is x=z, y=x, z=y.
    if fits(y, z, x) {
        return Err(format!(
            "/tiles/{z}/{x}/{y} is not a tile: zoom {z} does not exist. It is a valid tile \
             under the x/y/z order sabre used before v0.1.0 — the order is now z/x/y, so \
             this request is /tiles/{y}/{z}/{x}."
        ));
    }
    if z > MAX_ZOOM {
        return Err(format!("zoom {z} is out of range; tile URLs are /tiles/{{z}}/{{x}}/{{y}}"));
    }
    Err(format!(
        "tile {x}/{y} does not exist at zoom {z}: both must be below {}",
        1u32 << z
    ))
}

/// `mask` is already resolved and parsed — from the `mask` parameter or from
/// a geometry provider — so a tile never re-reads geometry text.
pub async fn tile(reader: &dyn RangeReader, p: &TileParams, mask: Option<Arc<Mask>>,
                  z: u32, x: u32, y: u32, t: &Timings) -> ApiResponse {
    if let Err(e) = check_tile(z, x, y) {
        return ApiResponse::bad_request(e);
    }
    let meta = match t.time_async(phase::META, fetch_meta(reader)).await {
        Ok(m)  => m,
        Err(e) => return ApiResponse::internal(e),
    };
    let style = match p.style.to_style() {
        Ok(s)  => s,
        Err(e) => return ApiResponse::bad_request(e),
    };
    let req = TileRequest {
        z, x, y,
        tile_size: p.tile_size.clamp(64, 512),
        style,
        bilinear: p.interpolation == "bilinear",
        mask,
    };
    match render_tile_timed(&req, reader, &meta, t).await {
        Ok(png) => ApiResponse::png(png).with_timing(t),
        Err(e)  => ApiResponse::internal(e),
    }
}

// ── Query ─────────────────────────────────────────────────────────────────────

pub async fn query(reader: &dyn RangeReader, p: &QueryParams,
                   mask: Option<Arc<Mask>>, t: &Timings) -> ApiResponse {
    let meta = match t.time_async(phase::META, fetch_meta(reader)).await {
        Ok(m)  => m,
        Err(e) => return ApiResponse::internal(e),
    };
    let result = match (&p.lat, &p.lng, &mask) {
        (Some(lat), Some(lng), _) => query_point(*lng, *lat, p.band, p.nodata, reader, &meta).await,
        (_, _, Some(m))           => query_polygon_timed(m, p.band, p.nodata, p.classes, reader, &meta, t).await,
        _ => Err("provide either lat+lng (point query), polygon=<WKT>, or \
                  geometry_provider with geometry_id".into()),
    };
    match result {
        Ok(r)  => json_or_500(serde_json::to_string(&r)).with_timing(t),
        Err(e) => ApiResponse::bad_request(e),
    }
}

// ── Info ──────────────────────────────────────────────────────────────────────

#[derive(serde::Serialize)]
struct BboxJson { west: f64, south: f64, east: f64, north: f64 }

#[derive(serde::Serialize)]
struct OverviewJson { width: u32, height: u32 }

#[derive(serde::Serialize)]
struct CogInfoResponse {
    width: u32, height: u32, bands: u16, dtype: String,
    tile_width: Option<u32>, tile_height: Option<u32>,
    photometric: String, nodata: Option<f64>, compression: u16,
    overview_count: usize, overviews: Vec<OverviewJson>,
    epsg: Option<u32>, geo_transform: [f64; 6],
    extent: Option<BboxJson>, center: Option<[f64; 2]>,
    stats_min: Option<f64>, stats_max: Option<f64>,
}

#[derive(serde::Serialize)]
struct PixelWindowJson { x_off: i64, y_off: i64, x_size: i64, y_size: i64 }

#[derive(serde::Serialize)]
struct TileInfoResponse {
    overview_idx: usize, overview_width: u32, overview_height: u32,
    has_data: bool, pixel_window: Option<PixelWindowJson>,
}

pub async fn info(reader: &dyn RangeReader) -> ApiResponse {
    match build_cog_info(reader).await {
        Ok(info) => json_or_500(serde_json::to_string(&info)),
        Err(e)   => ApiResponse::internal(e),
    }
}

pub async fn tile_info(reader: &dyn RangeReader, z: u32, x: u32, y: u32, tile_size: u32) -> ApiResponse {
    if let Err(e) = check_tile(z, x, y) {
        return ApiResponse::bad_request(e);
    }
    match build_tile_info(reader, z, x, y, tile_size).await {
        Ok(info) => json_or_500(serde_json::to_string(&info)),
        Err(e)   => ApiResponse::internal(e),
    }
}

fn json_or_500(json: Result<String, serde_json::Error>) -> ApiResponse {
    match json {
        Ok(json) => ApiResponse::json(json),
        Err(e)   => ApiResponse::internal(e.to_string()),
    }
}

async fn build_cog_info(reader: &dyn RangeReader) -> Result<CogInfoResponse, String> {
    let meta = fetch_meta(reader).await?;
    let ifd0 = &meta.ifds[0];
    let gt   = ifd0.geo_transform().ok_or("COG has no geotransform")?;

    let bits  = ifd0.bits_per_sample.first().copied().unwrap_or(8);
    let fmt   = ifd0.sample_format.first().copied().unwrap_or(1);
    let dtype = sample_dtype(bits, fmt);
    let nodata = ifd0.nodata.as_deref().and_then(|s| s.parse::<f64>().ok());
    let (stats_min, stats_max) = ifd0.data_stats();
    let extent = geo::geo_extent_wgs84(&gt, ifd0.image_width, ifd0.image_height, ifd0.epsg_code)
        .map(|b| BboxJson { west: b.west, south: b.south, east: b.east, north: b.north });
    let center = extent.as_ref().map(|b| [(b.west + b.east) / 2.0, (b.south + b.north) / 2.0]);
    let overviews = meta.ifds.iter().map(|i| OverviewJson { width: i.image_width, height: i.image_height }).collect();

    Ok(CogInfoResponse {
        width: ifd0.image_width, height: ifd0.image_height, bands: ifd0.samples_per_pixel,
        dtype, tile_width: ifd0.tile_width, tile_height: ifd0.tile_height,
        photometric: photometric_str(ifd0.photometric), nodata, compression: ifd0.compression,
        overview_count: meta.ifds.len(), overviews, epsg: ifd0.epsg_code,
        geo_transform: gt, extent, center, stats_min, stats_max,
    })
}

async fn build_tile_info(reader: &dyn RangeReader, z: u32, x: u32, y: u32, tile_size: u32) -> Result<TileInfoResponse, String> {
    let meta = fetch_meta(reader).await?;
    if meta.ifds.is_empty() { return Err("no IFDs".into()); }
    let ifd0 = &meta.ifds[0];
    let base_gt = ifd0.geo_transform().ok_or("COG has no geotransform")?;
    let bbox = geo::tile_to_bbox(z, x, y);
    let native_bbox = match ifd0.epsg_code {
        Some(epsg) => geo::reproject_bbox_from_wgs84(&bbox, epsg).unwrap_or(bbox),
        None => bbox,
    };
    let overview_idx = geo::best_overview_for_bbox(&base_gt, &native_bbox, tile_size, meta.ifds.len());
    let ifd = meta.ifd(overview_idx);
    let scale = ifd0.image_width as f64 / ifd.image_width as f64;
    let gt = [base_gt[0], base_gt[1]*scale, base_gt[2]*scale, base_gt[3], base_gt[4]*scale, base_gt[5]*scale];
    let pixel_window = geo::bbox_to_pixel_window(&gt, ifd.image_width, ifd.image_height, &native_bbox)
        .map(|w| PixelWindowJson { x_off: w.x_off, y_off: w.y_off, x_size: w.x_size, y_size: w.y_size });
    Ok(TileInfoResponse { overview_idx, overview_width: ifd.image_width, overview_height: ifd.image_height, has_data: pixel_window.is_some(), pixel_window })
}

fn sample_dtype(bits: u16, fmt: u16) -> String {
    match (fmt, bits) {
        (2, 8) => "int8", (2, 16) => "int16", (2, 32) => "int32",
        (3, 32) => "float32", (3, 64) => "float64",
        (_, 8) => "uint8", (_, 16) => "uint16", (_, 32) => "uint32", _ => "unknown",
    }.into()
}

fn photometric_str(code: u16) -> String {
    match code { 0=>"miniswhite", 1=>"minisblack", 2=>"rgb", 3=>"palette", 4=>"mask", 6=>"ycbcr", _=>"other" }.into()
}
