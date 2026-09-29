//! Drives the native backend through the shared router against the
//! synthetic fixtures in `core/tests/fixtures`.


use std::future::IntoFuture;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::response::IntoResponse;
use http_body_util::BodyExt;
use sabre_server::native::{NativeBackend, NativeConfig};
use tower::ServiceExt;

fn fixtures_dir() -> String {
    format!("{}/../core/tests/fixtures", env!("CARGO_MANIFEST_DIR"))
}

fn app() -> axum::Router {
    let config = NativeConfig { file_root: Some(fixtures_dir().into()), threads: 2, ..NativeConfig::default() };
    sabre_server::router(NativeBackend::new(config).expect("backend"))
}

async fn send(app: axum::Router, req: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
    let resp = app.oneshot(req).await.unwrap();
    let (parts, body) = resp.into_parts();
    let body = body.collect().await.unwrap().to_bytes().to_vec();
    (parts.status, parts.headers, body)
}

fn header(headers: &HeaderMap, name: &str) -> String {
    headers.get(name).map(|v| v.to_str().unwrap().to_string()).unwrap_or_default()
}

async fn get(app: axum::Router, uri: &str) -> (StatusCode, String, Vec<u8>) {
    let (status, headers, body) = send(app, Request::get(uri).body(Body::empty()).unwrap()).await;
    (status, header(&headers, "content-type"), body)
}

/// A `QUERY` request carrying `body`; `content_type` of `None` sends no header.
async fn query(app: axum::Router, uri: &str, content_type: Option<&str>, body: &str) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut req = Request::builder().method("QUERY").uri(uri);
    if let Some(ct) = content_type {
        req = req.header("content-type", ct);
    }
    send(app, req.body(Body::from(body.to_string())).unwrap()).await
}

/// The whole 25×25 fixture: 10°–10.225° E, 0°–0.225° N.
const FULL_EXTENT: &str = "POLYGON((10 0,10.225 0,10.225 0.225,10 0.225,10 0))";

#[tokio::test]
async fn health() {
    let (status, _, body) = get(app(), "/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"ok");
}

#[tokio::test]
async fn info_reads_a_local_file() {
    let (status, ct, body) = get(app(), "/info?url=file://l_shape.tiff").await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(ct, "application/json");
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["width"], 25);
    assert_eq!(v["height"], 25);
    assert_eq!(v["dtype"], "float32");
    assert_eq!(v["nodata"], 0.0);
}

#[tokio::test]
async fn tile_renders_a_png() {
    // Tile 9/270/255 fully contains the 25×25 fixture (see core/tests/snapshot.rs).
    let (status, ct, body) = get(app(), "/tiles/9/270/255?url=file://l_shape.tiff&min=1&max=10").await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(ct, "image/png");
    assert_eq!(&body[..8], b"\x89PNG\r\n\x1a\n");
}

#[tokio::test]
async fn the_old_xyz_order_is_named_rather_than_guessed_at() {
    // /tiles/270/255/9 was the v0.0.1 spelling of today's /tiles/9/270/255.
    // Zoom 270 does not exist, so this can only have been the old order.
    let (status, _, body) = get(app(), "/tiles/270/255/9?url=file://l_shape.tiff&min=1&max=10").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let msg = String::from_utf8_lossy(&body);
    assert!(msg.contains("/tiles/9/270/255"), "should name the request meant: {msg}");
    assert!(msg.contains("x/y/z"), "should say which order it looks like: {msg}");
}

#[tokio::test]
async fn an_ambiguous_tile_is_served_as_zxy_without_complaint() {
    // /tiles/3/5/6 is legal under both orders — z=3 x=5 y=6 now, x=3 y=5 z=6
    // before — and means a different tile each way. That ambiguity is why
    // there is no compatibility shim: the only honest reading is the current
    // one, taken without hedging.
    let (status, ct, body) = get(app(), "/tiles/3/5/6?url=file://l_shape.tiff&min=1&max=10").await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(ct, "image/png");
}

#[tokio::test]
async fn a_tile_outside_the_pyramid_is_a_400() {
    // z=2 has 4x4 tiles, so x=9 exists under no reading at all.
    let (status, _, body) = get(app(), "/tiles/2/9/1?url=file://l_shape.tiff&min=1&max=10").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let msg = String::from_utf8_lossy(&body);
    assert!(msg.contains("below 4"), "should say what the bound is: {msg}");
    assert!(!msg.contains("x/y/z"), "this is not the old order; do not suggest it: {msg}");
}

#[tokio::test]
async fn tile_info_rejects_the_old_order_too() {
    let (status, _, body) = get(app(), "/tile-info/270/255/9?url=file://l_shape.tiff").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&body).contains("/tiles/9/270/255"));
}

#[tokio::test]
async fn point_query_samples_a_value() {
    // Row 0, col 1 of the L-shape: inside the vertical stroke, value 1 + 9 * 0/24 = 1.
    let (status, _, body) = get(app(), "/query?url=file://l_shape.tiff&lat=0.22&lng=10.0135").await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["kind"], "point");
    assert_eq!(v["value"], 1.0);
}

#[tokio::test]
async fn tile_info_reports_the_window() {
    let (status, _, body) = get(app(), "/tile-info/9/270/255?url=file://l_shape.tiff").await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["has_data"], true);
    assert_eq!(v["overview_idx"], 0);
}

#[tokio::test]
async fn bad_style_is_a_400() {
    let (status, _, body) = get(app(), "/tiles/9/270/255?url=file://l_shape.tiff&mode=classified").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&body).contains("stops"));
}

#[tokio::test]
async fn every_mode_renders_with_its_parameters_in_a_query_string() {
    // The style fields are flattened in from sabre-core, and a flattened
    // number has to survive arriving as query-string text.
    for q in [
        "mode=colormap&colormap=turbo&min=1&max=10&nodata=-1",
        "mode=hillshade&azimuth=300&altitude=30&z_factor=2.5&hillshade_colormap=viridis",
        "mode=contour&contour_level=4&min=1&max=10",
    ] {
        let (status, ct, body) = get(app(), &format!("/tiles/9/270/255?url=file://l_shape.tiff&{q}")).await;
        assert_eq!(status, StatusCode::OK, "{q}: {}", String::from_utf8_lossy(&body));
        assert_eq!(ct, "image/png", "{q}");
    }
    // The fixture has one band, so rgb cannot render; reaching the band check
    // is what shows its parameters parsed.
    let (_, _, body) = get(app(), "/tiles/9/270/255?url=file://l_shape.tiff&mode=rgb&rgb_min_r=1&rgb_max_r=10").await;
    assert!(String::from_utf8_lossy(&body).contains("3 bands"), "{}", String::from_utf8_lossy(&body));
}

#[tokio::test]
async fn an_unknown_mode_is_a_400_rather_than_a_colormap() {
    let (status, _, body) = get(app(), "/tiles/9/270/255?url=file://l_shape.tiff&mode=hilshade").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&body).contains("hilshade"));
}

#[tokio::test]
async fn a_style_number_that_is_not_one_is_a_400() {
    let (status, _, body) = get(app(), "/tiles/9/270/255?url=file://l_shape.tiff&azimuth=west").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", String::from_utf8_lossy(&body));
}

#[tokio::test]
async fn file_urls_cannot_escape_the_root() {
    let (status, _, body) = get(app(), "/info?url=file://../../Cargo.toml").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&body).contains("outside"), "{}", String::from_utf8_lossy(&body));
}

#[tokio::test]
async fn file_urls_are_disabled_without_a_root() {
    let app = sabre_server::router(NativeBackend::new(NativeConfig { threads: 1, ..NativeConfig::default() }).unwrap());
    let (status, _, body) = get(app, "/info?url=file://l_shape.tiff").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&body).contains("--file-root"));
}

#[tokio::test]
async fn unknown_schemes_are_rejected() {
    // r2:// is in here because it used to be served by the Cloudflare Worker.
    // Anyone still holding such a URL should get the same clear answer as any
    // other scheme sabre does not read, rather than a special case explaining
    // a runtime that no longer exists.
    for url in ["ftp://host/dem.tif", "r2://bucket/dem.tif", "s3://bucket/dem.tif"] {
        let (status, _, body) = get(app(), &format!("/info?url={url}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{url}");
        assert!(String::from_utf8_lossy(&body).contains("scheme"),
                "{url}: {}", String::from_utf8_lossy(&body));
    }
}

// ── QUERY ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn query_method_reads_form_parameters_from_the_body() {
    let body = serde_urlencoded::to_string([("url", "file://l_shape.tiff"), ("polygon", FULL_EXTENT)]).unwrap();
    let (status, headers, body) = query(app(), "/query", Some("application/x-www-form-urlencoded; charset=utf-8"), &body).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(header(&headers, "content-type"), "application/json");
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["kind"], "polygon");
    // Shape pixels run 1.0 (row 0) to 10.0 (row 24); the 0.0 background is nodata.
    assert_eq!(v["min"], 1.0);
    assert_eq!(v["max"], 10.0);
}

#[tokio::test]
async fn query_method_reads_json_parameters_from_the_body() {
    let body = r#"{"url": "file://l_shape.tiff", "lat": 0.22, "lng": 10.0135}"#;
    let (status, _, body) = query(app(), "/query", Some("application/json"), body).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["kind"], "point");
    assert_eq!(v["value"], 1.0);
}

#[tokio::test]
async fn query_method_without_a_content_type_is_a_form() {
    let (status, _, body) = query(app(), "/info", None, "url=file://l_shape.tiff").await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["width"], 25);
}

#[tokio::test]
async fn query_method_ignores_the_query_string() {
    let (status, _, body) = query(app(), "/info?url=file://l_shape.tiff", None, "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&body).contains("url"), "{}", String::from_utf8_lossy(&body));
}

#[tokio::test]
async fn query_method_renders_tiles_with_json_stops() {
    let body = r##"{"url": "file://l_shape.tiff", "mode": "classified", "stops": [[0, "#2166ac"], [5, [178, 24, 43]]]}"##;
    let (status, headers, body) = query(app(), "/tiles/9/270/255", Some("application/json"), body).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(header(&headers, "content-type"), "image/png");
    assert_eq!(&body[..8], b"\x89PNG\r\n\x1a\n");
}

#[tokio::test]
async fn get_renders_tiles_with_string_stops() {
    let stops = "%5B%5B0%2C%22%232166ac%22%5D%2C%5B5%2C%5B178%2C24%2C43%5D%5D%5D";
    let (status, ct, body) = get(app(), &format!("/tiles/9/270/255?url=file://l_shape.tiff&mode=classified&stops={stops}")).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(ct, "image/png");
}

#[tokio::test]
async fn query_method_rejects_other_body_types() {
    let (status, headers, _) = query(app(), "/query", Some("text/plain"), "url=file://l_shape.tiff").await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(header(&headers, "accept-query"), "application/x-www-form-urlencoded, application/json");
}

#[tokio::test]
async fn query_method_with_a_malformed_body_is_a_400() {
    let (status, _, body) = query(app(), "/query", Some("application/json"), "{not json").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&body).contains("JSON"), "{}", String::from_utf8_lossy(&body));
}

#[tokio::test]
async fn other_methods_are_not_allowed() {
    let req = Request::post("/query").header("content-type", "application/json")
        .body(Body::from(r#"{"url": "file://l_shape.tiff", "lat": 0.22, "lng": 10.0135}"#)).unwrap();
    let (status, headers, _) = send(app(), req).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(header(&headers, "allow"), "GET, HEAD, QUERY, OPTIONS");
    assert_eq!(header(&headers, "accept-query"), "application/x-www-form-urlencoded, application/json");
}

#[tokio::test]
async fn options_answers_the_cors_preflight_for_query() {
    let req = Request::options("/tiles/9/270/255")
        .header("origin", "http://localhost:8080")
        .header("access-control-request-method", "QUERY")
        .body(Body::empty()).unwrap();
    let (status, headers, _) = send(app(), req).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(header(&headers, "access-control-allow-origin"), "*");
    assert_eq!(header(&headers, "access-control-allow-methods"), "GET, HEAD, QUERY");
    // X-Geometry-Provider-Auth has to be listed or a browser will not send it
    // cross-origin, which is the only way a page can name a geometry.
    assert_eq!(header(&headers, "access-control-allow-headers"),
               "Content-Type, X-Geometry-Provider-Auth");
}

// ── Page cache ────────────────────────────────────────────────────────────────

/// An HTTP origin serving the L-shape fixture with range support, counting
/// every request it answers.
struct Origin {
    url:      String,
    requests: Arc<AtomicUsize>,
}

async fn serve_fixture(State((bytes, hits)): State<(Arc<Vec<u8>>, Arc<AtomicUsize>)>, headers: HeaderMap) -> axum::response::Response {
    hits.fetch_add(1, Ordering::SeqCst);
    let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("bytes="));
    match range.and_then(|r| r.split_once('-')) {
        Some((lo, hi)) => {
            let lo: usize = lo.parse().unwrap();
            let hi: usize = hi.parse::<usize>().unwrap().min(bytes.len() - 1);
            let body = bytes[lo..=hi].to_vec();
            let content_range = format!("bytes {lo}-{hi}/{}", bytes.len());
            (StatusCode::PARTIAL_CONTENT, [(header::CONTENT_RANGE, content_range)], body).into_response()
        }
        None => bytes.to_vec().into_response(),
    }
}

async fn origin() -> Origin {
    let bytes = Arc::new(std::fs::read(format!("{}/l_shape.tiff", fixtures_dir())).unwrap());
    let requests = Arc::new(AtomicUsize::new(0));
    let app = axum::Router::new().route("/l_shape.tiff", axum::routing::get(serve_fixture)).with_state((bytes, requests.clone()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/l_shape.tiff", listener.local_addr().unwrap());
    tokio::spawn(axum::serve(listener, app).into_future());
    Origin { url, requests }
}

fn cached_app(cache_bytes: usize) -> axum::Router {
    let config = NativeConfig { threads: 1, cache_bytes, ..NativeConfig::default() };
    sabre_server::router(NativeBackend::new(config).expect("backend"))
}

#[tokio::test]
async fn warm_requests_do_not_touch_the_origin() {
    let origin = origin().await;
    let app = cached_app(256 << 20);
    let tile = format!("/tiles/9/270/255?url={}&min=1&max=10", origin.url);

    let (status, _, first) = get(app.clone(), &tile).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&first));
    // The fixture is a single 64 KB page, and the header fetch brings it in.
    assert_eq!(origin.requests.load(Ordering::SeqCst), 1);

    let (status, _, second) = get(app.clone(), &tile).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(second, first);
    let (status, _, body) = get(app.clone(), &format!("/query?url={}&lat=0.22&lng=10.0135", origin.url)).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let (status, _, _) = get(app, &format!("/info?url={}", origin.url)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(origin.requests.load(Ordering::SeqCst), 1, "warm requests went back to the origin");
}

#[tokio::test]
async fn a_disabled_cache_reads_the_origin_every_time() {
    let origin = origin().await;
    let app = cached_app(0);
    let tile = format!("/tiles/9/270/255?url={}&min=1&max=10", origin.url);

    let (status, _, _) = get(app.clone(), &tile).await;
    assert_eq!(status, StatusCode::OK);
    let after_first = origin.requests.load(Ordering::SeqCst);
    assert!(after_first >= 2, "expected a header and a tile fetch, saw {after_first}");
    let (status, _, _) = get(app, &tile).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(origin.requests.load(Ordering::SeqCst), 2 * after_first);
}

// ── Geometry providers ────────────────────────────────────────────────────────

/// A stand-in geometry service: one polygon per id, an access endpoint that
/// clears whoever presents the right credential, and a count of both.
struct ProviderService {
    base:     String,
    geometry: Arc<AtomicUsize>,
    access:   Arc<AtomicUsize>,
}

/// A square covering the whole L-shape fixture, and one covering its left half.
const WHOLE: &str = r#"{"type":"Feature","properties":{},"geometry":{"type":"Polygon",
    "coordinates":[[[9.9,-0.1],[10.3,-0.1],[10.3,0.3],[9.9,0.3],[9.9,-0.1]]]}}"#;
const HALF_WKT: &str = "POLYGON((9.9 -0.1,10.01 -0.1,10.01 0.3,9.9 0.3,9.9 -0.1))";

async fn provider_service() -> ProviderService {
    use axum::extract::Path as AxPath;

    let geometry = Arc::new(AtomicUsize::new(0));
    let access   = Arc::new(AtomicUsize::new(0));

    let g = geometry.clone();
    let geom = move |AxPath(id): AxPath<String>| {
        let g = g.clone();
        async move {
            g.fetch_add(1, Ordering::SeqCst);
            // GeoJSON for "whole", TWKB for "half": both routes have to work,
            // and TWKB is what a provider is actually asked to serve.
            match id.as_str() {
                "whole" => (StatusCode::OK, WHOLE).into_response(),
                "half"  => {
                    let mask = sabre_core::mask::parse_wkt_mask(HALF_WKT).unwrap();
                    let bytes = sabre_core::twkb::encode_mask(
                        &mask, sabre_core::twkb::DEFAULT_PRECISION).unwrap();
                    ([(header::CONTENT_TYPE, sabre_core::twkb::CONTENT_TYPE)], bytes)
                        .into_response()
                }
                "boom"  => (StatusCode::INTERNAL_SERVER_ERROR, "upstream on fire").into_response(),
                _       => (StatusCode::NOT_FOUND, "no such geometry").into_response(),
            }
        }
    };

    let a = access.clone();
    let check = move |headers: HeaderMap| {
        let a = a.clone();
        async move {
            a.fetch_add(1, Ordering::SeqCst);
            // Only "Bearer good" is cleared. Note this is the Authorization
            // header: the caller sent X-Geometry-Provider-Auth and sabre
            // forwarded it under the name a provider expects.
            match headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
                Some("Bearer good") => (StatusCode::OK, "").into_response(),
                _                   => (StatusCode::FORBIDDEN, "").into_response(),
            }
        }
    };

    let app = axum::Router::new()
        .route("/geom/{id}", axum::routing::get(geom))
        .route("/access", axum::routing::get(check));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(axum::serve(listener, app).into_future());
    ProviderService { base, geometry, access }
}

fn app_with_providers(spec: &str) -> axum::Router {
    let config = NativeConfig {
        threads: 1,
        providers: sabre_server::geometry::Registry::parse(spec).expect("provider spec"),
        ..NativeConfig::default()
    };
    sabre_server::router(NativeBackend::new(config).expect("backend"))
}

async fn get_with_auth(app: axum::Router, uri: &str, auth: Option<&str>)
    -> (StatusCode, Vec<u8>) {
    let mut req = Request::get(uri);
    if let Some(value) = auth {
        req = req.header("x-geometry-provider-auth", value);
    }
    let (status, _, body) = send(app, req.body(Body::empty()).unwrap()).await;
    (status, body)
}

#[tokio::test]
async fn a_named_geometry_clips_the_same_as_the_wkt_it_stands_for() {
    let origin = origin().await;
    let svc = provider_service().await;
    let app = app_with_providers(&format!("blocks={}/geom/{{id}}", svc.base));

    // The same polygon the provider serves, sent inline instead.
    let wkt = HALF_WKT;
    let base = format!("/tiles/9/270/255?url={}&min=1&max=10", origin.url);

    let (s1, by_value) = get_with_auth(app.clone(),
        &format!("{base}&mask={}", urlencoding(wkt)), None).await;
    let (s2, by_name) = get_with_auth(app.clone(),
        &format!("{base}&geometry_provider=blocks&geometry_id=half"), None).await;

    assert_eq!(s1, StatusCode::OK, "{}", String::from_utf8_lossy(&by_value));
    assert_eq!(s2, StatusCode::OK, "{}", String::from_utf8_lossy(&by_name));
    assert_eq!(by_name, by_value, "naming a geometry must render the same tile as sending it");

    // And the point of it: what the client had to send. A five-vertex square
    // is the smallest polygon there is and already costs more inline than by
    // name; a real field is 30-200 vertices.
    let by_value_len = format!("{base}&mask={}", urlencoding(wkt)).len();
    let by_name_len  = format!("{base}&geometry_provider=blocks&geometry_id=half").len();
    assert!(by_name_len < by_value_len,
            "by name {by_name_len} bytes, inline {by_value_len} bytes");
}

#[tokio::test]
async fn a_field_shaped_polygon_costs_far_more_inline_than_by_name() {
    // A 64-vertex ring, the size of a real farm block, against the reference
    // that stands for it.
    let ring: Vec<String> = (0..64)
        .map(|i| {
            let a = std::f64::consts::TAU * f64::from(i) / 64.0;
            format!("{:.6} {:.6}", 10.1 + 0.05 * a.cos(), 0.1 + 0.05 * a.sin())
        })
        .collect();
    let wkt = format!("POLYGON(({},{}))", ring.join(","), ring[0]);

    let inline = urlencoding(&wkt).len() + "&mask=".len();
    let named  = "&geometry_provider=blocks&geometry_id=19519".len();
    assert!(inline > 10 * named,
            "inline {inline} bytes vs {named} by name; the saving should be an order of magnitude");
}

#[tokio::test]
async fn a_named_geometry_works_for_zonal_statistics_too() {
    let origin = origin().await;
    let svc = provider_service().await;
    let app = app_with_providers(&format!("blocks={}/geom/{{id}}", svc.base));

    let (status, body) = get_with_auth(app,
        &format!("/query?url={}&geometry_provider=blocks&geometry_id=whole", origin.url), None).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["kind"], "polygon");
    assert!(v["min"].as_f64().unwrap() >= 1.0);
    assert!(v["max"].as_f64().unwrap() <= 10.0);
}

#[tokio::test]
async fn the_geometry_is_fetched_once_however_many_tiles_want_it() {
    let origin = origin().await;
    let svc = provider_service().await;
    let app = app_with_providers(&format!("blocks={}/geom/{{id}}", svc.base));

    for _ in 0..4 {
        let (status, body) = get_with_auth(app.clone(), &format!(
            "/tiles/9/270/255?url={}&min=1&max=10&geometry_provider=blocks&geometry_id=whole",
            origin.url), None).await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    }
    assert_eq!(svc.geometry.load(Ordering::SeqCst), 1,
               "four tiles of one field is one geometry fetch");
}

#[tokio::test]
async fn two_callers_share_the_geometry_but_are_cleared_separately() {
    let origin = origin().await;
    let svc = provider_service().await;
    let app = app_with_providers(&format!(
        "blocks={0}/geom/{{id}} access={0}/access?ids={{ids}}", svc.base));
    let uri = format!("/tiles/9/270/255?url={}&min=1&max=10\
                       &geometry_provider=blocks&geometry_id=whole", origin.url);

    let (a, _) = get_with_auth(app.clone(), &uri, Some("Bearer good")).await;
    let (b, _) = get_with_auth(app.clone(), &uri, Some("Bearer also-good")).await;
    assert_eq!(a, StatusCode::OK);
    // The fake only clears "Bearer good", so the second caller is refused —
    // and refused without ever reaching the geometry the first one cached.
    assert_eq!(b, StatusCode::FORBIDDEN);
    assert_eq!(svc.geometry.load(Ordering::SeqCst), 1);
    assert_eq!(svc.access.load(Ordering::SeqCst), 2, "each caller is checked on their own");
}

#[tokio::test]
async fn a_cleared_caller_is_not_re_checked_on_every_tile() {
    let origin = origin().await;
    let svc = provider_service().await;
    let app = app_with_providers(&format!(
        "blocks={0}/geom/{{id}} access={0}/access?ids={{ids}}", svc.base));
    let uri = format!("/tiles/9/270/255?url={}&min=1&max=10\
                       &geometry_provider=blocks&geometry_id=whole", origin.url);

    for _ in 0..5 {
        let (status, _) = get_with_auth(app.clone(), &uri, Some("Bearer good")).await;
        assert_eq!(status, StatusCode::OK);
    }
    assert_eq!(svc.access.load(Ordering::SeqCst), 1, "one check covers the TTL window");
    assert_eq!(svc.geometry.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn several_ids_clip_as_one_shape_in_one_access_call() {
    let origin = origin().await;
    let svc = provider_service().await;
    let app = app_with_providers(&format!(
        "blocks={0}/geom/{{id}} access={0}/access?ids={{ids}}", svc.base));

    let (status, body) = get_with_auth(app, &format!(
        "/query?url={}&geometry_provider=blocks&geometry_id=half,whole", origin.url),
        Some("Bearer good")).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(svc.access.load(Ordering::SeqCst), 1, "two blocks, one access call");
    assert_eq!(svc.geometry.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn provider_failures_are_not_reported_as_the_rasters_fault() {
    let origin = origin().await;
    let svc = provider_service().await;
    let app = app_with_providers(&format!("blocks={}/geom/{{id}}", svc.base));
    let tile = |id: &str| format!(
        "/tiles/9/270/255?url={}&min=1&max=10&geometry_provider=blocks&geometry_id={id}",
        origin.url);

    let (status, body) = get_with_auth(app.clone(), &tile("missing"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{}", String::from_utf8_lossy(&body));

    let (status, body) = get_with_auth(app.clone(), &tile("boom"), None).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY,
               "a broken provider is upstream's problem, not a 500 from us: {}",
               String::from_utf8_lossy(&body));
}

#[tokio::test]
async fn a_bad_geometry_reference_is_refused_before_anything_is_fetched() {
    let origin = origin().await;
    let svc = provider_service().await;
    let app = app_with_providers(&format!("blocks={}/geom/{{id}}", svc.base));
    let base = format!("/tiles/9/270/255?url={}&min=1&max=10", origin.url);

    for (suffix, expect) in [
        ("&geometry_provider=nope&geometry_id=whole", "unknown geometry_provider"),
        ("&geometry_provider=blocks", "needs a geometry_id"),
        ("&geometry_id=whole", "needs a geometry_provider"),
        ("&geometry_provider=blocks&geometry_id=../../etc/passwd", "geometry_id contains"),
        ("&geometry_provider=blocks&geometry_id=", "geometry_id is empty"),
    ] {
        let (status, body) = get_with_auth(app.clone(), &format!("{base}{suffix}"), None).await;
        let msg = String::from_utf8_lossy(&body);
        assert_eq!(status, StatusCode::BAD_REQUEST, "{suffix}: {msg}");
        assert!(msg.contains(expect), "{suffix}: expected {expect:?}, got {msg}");
    }
    assert_eq!(svc.geometry.load(Ordering::SeqCst), 0,
               "none of those should have reached the provider");
}

#[tokio::test]
async fn naming_a_geometry_and_sending_one_is_refused_rather_than_ranked() {
    let origin = origin().await;
    let svc = provider_service().await;
    let app = app_with_providers(&format!("blocks={}/geom/{{id}}", svc.base));

    let (status, body) = get_with_auth(app, &format!(
        "/tiles/9/270/255?url={}&min=1&max=10&mask={}&geometry_provider=blocks&geometry_id=whole",
        origin.url,
        urlencoding("POLYGON((9.9 -0.1,10.3 -0.1,10.3 0.3,9.9 0.3,9.9 -0.1))")), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&body).contains("send one or the other"));
}

#[tokio::test]
async fn naming_a_provider_on_a_server_with_none_says_so() {
    let origin = origin().await;
    let (status, body) = get_with_auth(app(), &format!(
        "/tiles/9/270/255?url={}&min=1&max=10&geometry_provider=blocks&geometry_id=x",
        origin.url), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&body).contains("no geometry providers are configured"));
}

/// Percent-encode a query parameter value. Only used by these tests.
fn urlencoding(value: &str) -> String {
    value.bytes().map(|b| match b {
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
        _ => format!("%{b:02X}"),
    }).collect()
}

// ── Server timing ─────────────────────────────────────────────────────────────

fn server_timing(headers: &HeaderMap) -> String {
    headers.get("server-timing").and_then(|v| v.to_str().ok()).unwrap_or("").to_string()
}

fn phases(headers: &HeaderMap) -> Vec<String> {
    server_timing(headers).split(',')
        .filter_map(|s| s.trim().split(';').next().map(str::to_string))
        .filter(|s| !s.is_empty())
        .collect()
}

#[tokio::test]
async fn a_tile_reports_where_its_time_went() {
    let req = Request::get("/tiles/9/270/255?url=file://l_shape.tiff&min=1&max=10")
        .body(Body::empty()).unwrap();
    let (status, headers, _) = send(app(), req).await;
    assert_eq!(status, StatusCode::OK);

    let got = phases(&headers);
    for want in ["meta", "plan", "fetch", "decode", "resample", "style", "encode"] {
        assert!(got.iter().any(|p| p == want), "missing {want} in {got:?}");
    }
    // Cross-origin is the normal case for a tile server, and a browser will
    // not show the header unless it is named as exposed.
    assert_eq!(header(&headers, "access-control-expose-headers"), "Server-Timing");
}

#[tokio::test]
async fn a_tile_with_no_clip_has_no_geometry_phase() {
    // Not "geom;dur=0.000": a phase that reads zero says it happened
    // instantly, and would pull every aggregate of it towards zero.
    let req = Request::get("/tiles/9/270/255?url=file://l_shape.tiff&min=1&max=10")
        .body(Body::empty()).unwrap();
    let (_, headers, _) = send(app(), req).await;
    assert!(!phases(&headers).contains(&"geom".to_string()), "{}", server_timing(&headers));
    assert!(!phases(&headers).contains(&"mask".to_string()), "{}", server_timing(&headers));
}

#[tokio::test]
async fn clipping_shows_up_as_its_own_phase() {
    let wkt = urlencoding("POLYGON((9.9 -0.1,10.3 -0.1,10.3 0.3,9.9 0.3,9.9 -0.1))");
    let req = Request::get(format!(
        "/tiles/9/270/255?url=file://l_shape.tiff&min=1&max=10&mask={wkt}"))
        .body(Body::empty()).unwrap();
    let (status, headers, _) = send(app(), req).await;
    assert_eq!(status, StatusCode::OK);
    let got = phases(&headers);
    assert!(got.contains(&"geom".to_string()), "parsing the WKT is the geometry phase: {got:?}");
    assert!(got.contains(&"mask".to_string()), "applying it is its own: {got:?}");
}

#[tokio::test]
async fn a_zonal_query_reports_its_own_phases() {
    let wkt = urlencoding("POLYGON((9.9 -0.1,10.3 -0.1,10.3 0.3,9.9 0.3,9.9 -0.1))");
    let req = Request::get(format!("/query?url=file://l_shape.tiff&polygon={wkt}"))
        .body(Body::empty()).unwrap();
    let (status, headers, _) = send(app(), req).await;
    assert_eq!(status, StatusCode::OK);
    let got = phases(&headers);
    for want in ["meta", "plan", "fetch", "decode", "stats"] {
        assert!(got.iter().any(|p| p == want), "missing {want} in {got:?}");
    }
    assert!(!got.contains(&"encode".to_string()), "nothing to encode: {got:?}");
}

#[tokio::test]
async fn an_error_carries_no_breakdown() {
    // A 400 spends its time deciding to be a 400. Reporting that as a render
    // breakdown would put noise into every aggregate.
    let req = Request::get("/tiles/9/270/255?url=file://l_shape.tiff&mode=classified")
        .body(Body::empty()).unwrap();
    let (status, headers, _) = send(app(), req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(server_timing(&headers), "");
}

#[tokio::test]
async fn the_phases_add_up_to_roughly_the_whole_request() {
    let req = Request::get("/tiles/9/270/255?url=file://l_shape.tiff&min=1&max=10")
        .body(Body::empty()).unwrap();
    let started = std::time::Instant::now();
    let (_, headers, _) = send(app(), req).await;
    let wall = started.elapsed().as_secs_f64() * 1e3;

    let total: f64 = server_timing(&headers).split(',')
        .filter_map(|s| s.trim().split("dur=").nth(1))
        .filter_map(|v| v.parse::<f64>().ok())
        .sum();
    assert!(total > 0.0, "nothing was measured");
    // Loose on purpose: the point is that the phases account for the request
    // rather than a slice of it, not that they account for it exactly.
    assert!(total <= wall * 1.5,
            "phases sum to {total:.3} ms but the request took {wall:.3} ms");
}
