//! sabre-server — the HTTP API in front of `sabre-core`.
//!
//! * [`api`] turns parsed query parameters plus a [`RangeReader`] into a
//!   response. It knows nothing about where the bytes come from.
//! * [`router`] wires the endpoints to an axum [`Router`] over a [`Backend`]
//!   that supplies the reader and runs the request. Every endpoint that takes
//!   parameters answers both `GET`, with the parameters in the query string,
//!   and `QUERY`, with the same parameters in the body (see [`Params`]).
//! * [`native`] resolves sources through reqwest or the local filesystem and
//!   runs on tokio.
//!
//! There used to be a Cloudflare Workers shim beside `native`. It was dropped
//! because sabre's advantage is a long-lived process holding a byte cache of
//! the raster it reads, and an isolate cannot have one. Against a network
//! origin the same repeated tile is 30 ms warm and 2,143 ms with the cache
//! switched off — 71x — and the Workers Cache API holds the COG header, not
//! the pixel pages. An edge deployment would have given up the thing that
//! makes this fast to save a hop it mostly does not take.

use std::future::Future;

use axum::{
    body::Bytes,
    extract::{FromRequest, FromRequestParts, Path, Query, Request, State},
    handler::Handler,
    http::{header, HeaderName, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, get, MethodRouter},
    Router,
};
use sabre_core::cog::RangeReader;
use serde::de::DeserializeOwned;

pub mod api;
pub mod geometry;
pub mod native;

/// Reader plumbing now lives in `sabre-core`, so a browser build can use the
/// same page cache. Re-exported here because that is where it used to be.
pub use sabre_core::reader;

use api::{QueryParams, TileInfoParams, TileParams, UrlParam};

// ── Response type ─────────────────────────────────────────────────────────────

/// A finished HTTP response, independent of any runtime.
///
/// Every runtime produces one of these from [`api`] and hands it to axum.
/// Keeping it a plain struct (rather than an axum `Response`) is what lets the
/// native backend move it out of a `!Send` task and back onto the tokio
/// worker threads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiResponse {
    pub status:       StatusCode,
    pub content_type: &'static str,
    pub body:         Vec<u8>,
    /// `Server-Timing` value, when the handler measured its phases.
    pub timing:       Option<String>,
}

impl ApiResponse {
    pub fn png(body: Vec<u8>) -> Self {
        Self { status: StatusCode::OK, content_type: "image/png", body, timing: None }
    }

    pub fn json(body: String) -> Self {
        Self { status: StatusCode::OK, content_type: "application/json",
               body: body.into_bytes(), timing: None }
    }

    pub fn text(status: StatusCode, body: impl Into<String>) -> Self {
        Self { status, content_type: "text/plain", body: body.into().into_bytes(), timing: None }
    }

    /// Attach a phase breakdown. Kept off error responses: a 400 spends its
    /// time deciding to be a 400, and reporting that as a render breakdown
    /// would put noise into every aggregate.
    pub fn with_timing(mut self, t: &sabre_core::timing::Timings) -> Self {
        if self.status.is_success() {
            self.timing = t.header();
        }
        self
    }

    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::text(StatusCode::BAD_REQUEST, msg)
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self::text(StatusCode::INTERNAL_SERVER_ERROR, msg)
    }
}

impl IntoResponse for ApiResponse {
    fn into_response(self) -> Response {
        let mut resp = (self.status, self.body).into_response();
        let headers = resp.headers_mut();
        headers.insert(header::CONTENT_TYPE,                HeaderValue::from_static(self.content_type));
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
        if let Some(value) = self.timing.as_deref().and_then(|v| HeaderValue::from_str(v).ok()) {
            headers.insert(SERVER_TIMING, value);
            // Browsers only expose Server-Timing cross-origin when it is
            // named here, and a tile server is almost always cross-origin
            // from the page drawing the map.
            headers.insert(header::ACCESS_CONTROL_EXPOSE_HEADERS,
                           HeaderValue::from_static("Server-Timing"));
        }
        if self.status == StatusCode::OK && self.content_type == "image/png" {
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=3600"));
        }
        resp
    }
}

// ── Backend ───────────────────────────────────────────────────────────────────

/// What a runtime has to provide for the shared router to work.
///
/// `run` resolves `source` (an `https://` or `file://` URL) to a
/// [`RangeReader`] and drives `f` to completion with it.
///
/// The reader and the future `f` returns are `!Send`, which axum's handlers
/// are not: the backend pins each request to one of a small pool of dedicated
/// threads and sends only the finished response back. That constraint came
/// from the Workers runtime, whose `fetch` futures are not `Send`. With the
/// Worker gone the constraint is inherited rather than required, and dropping
/// it would take the per-request thread handoff with it.
pub trait Backend: Clone + Send + Sync + 'static {
    fn run<F, Fut>(&self, source: String, f: F) -> impl Future<Output = ApiResponse> + Send
    where
        F:   FnOnce(Box<dyn RangeReader>) -> Fut + Send + 'static,
        Fut: Future<Output = ApiResponse> + 'static;

    /// Geometry providers the operator configured. Empty by default, which
    /// makes `geometry_provider` a 400 rather than a route to anywhere.
    fn geometry(&self) -> geometry::Registry {
        geometry::Registry::default()
    }

    /// Resolve a geometry reference to WKT, using the backend's own HTTP
    /// client and caches. `!Send` for the same reason [`run`] is, and handled
    /// the same way.
    fn resolve_geometry(
        &self,
        provider: geometry::Provider,
        ids: Vec<String>,
        auth: Option<String>,
    ) -> impl Future<Output = Result<std::sync::Arc<sabre_core::mask::Mask>, geometry::Error>> + Send;
}

// ── Geometry references ───────────────────────────────────────────────────────

/// The `X-Geometry-Provider-Auth` header, forwarded to the provider as
/// `Authorization` when one is configured to receive it.
///
/// A header rather than a query parameter because a credential in a URL ends
/// up in access logs, browser history and `Referer`.
pub const GEOMETRY_AUTH_HEADER: &str = "x-geometry-provider-auth";

#[derive(Clone, Debug, Default)]
pub struct GeometryAuth(pub Option<String>);

impl<S: Send + Sync> FromRequestParts<S> for GeometryAuth {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut axum::http::request::Parts, _: &S)
        -> Result<Self, Self::Rejection> {
        Ok(Self(parts.headers.get(GEOMETRY_AUTH_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)))
    }
}

/// Turn a provider reference into WKT, or explain why it cannot be one.
///
/// `literal` is whatever the caller passed as `mask` or `polygon`. Naming a
/// geometry and also sending one is refused rather than resolved by
/// precedence: the two say different things, and quietly honouring one of them
/// clips to a shape nobody asked for.
async fn resolve_geometry<B: Backend>(
    backend: &B,
    literal: Option<&String>,
    provider_name: Option<&String>,
    ids: Option<&String>,
    auth: Option<String>,
    param: &str,
) -> Result<Option<std::sync::Arc<sabre_core::mask::Mask>>, ApiResponse> {
    let (provider_name, ids) = match (provider_name, ids) {
        // No reference: parse the literal here, once, rather than in the
        // renderer once per tile.
        (None, None) => return match literal {
            None => Ok(None),
            Some(wkt) => sabre_core::mask::parse_wkt_mask(wkt)
                .map(|m| Some(std::sync::Arc::new(m)))
                .map_err(ApiResponse::bad_request),
        },
        (Some(p), Some(i)) => (p, i),
        (Some(_), None) => return Err(ApiResponse::bad_request(
            "geometry_provider needs a geometry_id")),
        (None, Some(_)) => return Err(ApiResponse::bad_request(
            "geometry_id needs a geometry_provider")),
    };
    if literal.is_some() {
        return Err(ApiResponse::bad_request(format!(
            "{param} and geometry_provider both name a clip geometry; send one or the other")));
    }

    let registry = backend.geometry();
    let Some(provider) = registry.get(provider_name) else {
        return Err(ApiResponse::bad_request(if registry.is_empty() {
            "no geometry providers are configured on this server".to_string()
        } else {
            format!("unknown geometry_provider {provider_name:?}; configured: {}",
                    registry.names().join(", "))
        }));
    };
    let ids = geometry::parse_ids(ids).map_err(ApiResponse::bad_request)?;

    match backend.resolve_geometry(provider.clone(), ids, auth).await {
        Ok(mask) => Ok(Some(mask)),
        Err(e) => Err(match e {
            geometry::Error::BadRequest(m) => ApiResponse::bad_request(m),
            geometry::Error::Forbidden(m)  => ApiResponse::text(StatusCode::FORBIDDEN, m),
            geometry::Error::NotFound(m)   => ApiResponse::text(StatusCode::NOT_FOUND, m),
            geometry::Error::Upstream(m)   => ApiResponse::text(StatusCode::BAD_GATEWAY, m),
        }),
    }
}

// ── Router ────────────────────────────────────────────────────────────────────

/// Build the sabre HTTP API on top of `backend`.
pub fn router<B: Backend>(backend: B) -> Router {
    Router::new()
        .route("/tiles/{z}/{x}/{y}",     get_or_query(tile_handler::<B>))
        .route("/info",                  get_or_query(info_handler::<B>))
        .route("/meta",                  get_or_query(info_handler::<B>))
        .route("/tile-info/{z}/{x}/{y}", get_or_query(tile_info_handler::<B>))
        .route("/query",                 get_or_query(query_handler::<B>))
        .route("/health",                get(health))
        .with_state(backend)
}

async fn health() -> &'static str { "ok" }

/// A recorder seeded with the geometry phase, which happens before the request
/// reaches the thread the rest of it runs on.
///
/// `ms` is None when the request named no geometry at all. The distinction
/// matters: a tile with no clip would otherwise carry `geom;dur=0.000`, which
/// reads as "resolved instantly" rather than "never asked", and would drag
/// every aggregate of that phase towards zero.
fn timings_with_geometry(ms: Option<f64>) -> sabre_core::timing::Timings {
    let t = sabre_core::timing::Timings::new();
    if let Some(ms) = ms {
        t.add(sabre_core::timing::phase::GEOMETRY, ms);
    }
    t
}

async fn tile_handler<B: Backend>(
    Path((z, x, y)): Path<(u32, u32, u32)>,
    State(backend): State<B>,
    GeometryAuth(auth): GeometryAuth,
    Params(p): Params<TileParams>,
) -> ApiResponse {
    // Timed with a bare Instant rather than a Timings: the recorder holds a
    // RefCell, and a borrow of one cannot cross an await in a handler axum
    // needs to be Send. The elapsed time rides along to the request thread,
    // where the rest of the phases are recorded.
    let asked_for_geometry = p.mask.is_some() || p.geometry_provider.is_some();
    let started = std::time::Instant::now();
    let mask = match resolve_geometry(&backend, p.mask.as_ref(),
                                      p.geometry_provider.as_ref(), p.geometry_id.as_ref(),
                                      auth, "mask").await {
        Ok(m)     => m,
        Err(resp) => return resp,
    };
    let geometry_ms = asked_for_geometry.then(|| started.elapsed().as_secs_f64() * 1e3);
    backend.run(p.url.clone(), move |reader| async move {
        let t = timings_with_geometry(geometry_ms);
        api::tile(reader.as_ref(), &p, mask, z, x, y, &t).await
    }).await
}

async fn query_handler<B: Backend>(
    State(backend): State<B>,
    GeometryAuth(auth): GeometryAuth,
    Params(p): Params<QueryParams>,
) -> ApiResponse {
    let asked_for_geometry = p.polygon.is_some() || p.geometry_provider.is_some();
    let started = std::time::Instant::now();
    let mask = match resolve_geometry(&backend, p.polygon.as_ref(),
                                      p.geometry_provider.as_ref(), p.geometry_id.as_ref(),
                                      auth, "polygon").await {
        Ok(m)     => m,
        Err(resp) => return resp,
    };
    let geometry_ms = asked_for_geometry.then(|| started.elapsed().as_secs_f64() * 1e3);
    backend.run(p.url.clone(), move |reader| async move {
        let t = timings_with_geometry(geometry_ms);
        api::query(reader.as_ref(), &p, mask, &t).await
    }).await
}

async fn info_handler<B: Backend>(
    State(backend): State<B>,
    Params(p): Params<UrlParam>,
) -> ApiResponse {
    backend.run(p.url, move |reader| async move {
        api::info(reader.as_ref()).await
    }).await
}

async fn tile_info_handler<B: Backend>(
    Path((z, x, y)): Path<(u32, u32, u32)>,
    State(backend): State<B>,
    Params(p): Params<TileInfoParams>,
) -> ApiResponse {
    backend.run(p.url.clone(), move |reader| async move {
        api::tile_info(reader.as_ref(), z, x, y, p.tile_size).await
    }).await
}

// ── Methods ───────────────────────────────────────────────────────────────────

/// The methods every parameterised endpoint answers.
const ALLOW: &str = "GET, HEAD, QUERY, OPTIONS";

/// The body types a `QUERY` may carry, advertised in the `Accept-Query`
/// response header the QUERY draft defines.
const ACCEPT_QUERY: &str = "application/x-www-form-urlencoded, application/json";
const ACCEPT_QUERY_HEADER: HeaderName = HeaderName::from_static("accept-query");
const SERVER_TIMING: HeaderName = HeaderName::from_static("server-timing");
const FORM: &str = "application/x-www-form-urlencoded";
const JSON: &str = "application/json";

/// Route `handler` for `GET`, `HEAD` and `QUERY`, and answer the CORS
/// preflight a browser sends before a `QUERY` on `OPTIONS`.
///
/// `QUERY` is not one of the methods axum's filter knows, so `handler` is
/// registered for any method and [`Params`] does the filtering, turning
/// everything but `GET`, `HEAD` and `QUERY` into a 405.
fn get_or_query<H, T, S>(handler: H) -> MethodRouter<S>
where
    H: Handler<T, S>,
    T: 'static,
    S: Clone + Send + Sync + 'static,
{
    any(handler).options(preflight)
}

async fn preflight() -> Response {
    let headers = [
        (header::ALLOW,                        ALLOW),
        (header::ACCESS_CONTROL_ALLOW_ORIGIN,  "*"),
        (header::ACCESS_CONTROL_ALLOW_METHODS, "GET, HEAD, QUERY"),
        // Browsers will not send X-Geometry-Provider-Auth cross-origin unless
        // the preflight says it may.
        (header::ACCESS_CONTROL_ALLOW_HEADERS, "Content-Type, X-Geometry-Provider-Auth"),
        (header::ACCESS_CONTROL_MAX_AGE,       "86400"),
        (ACCEPT_QUERY_HEADER,                  ACCEPT_QUERY),
    ];
    (StatusCode::NO_CONTENT, headers).into_response()
}

/// Request parameters: the query string of a `GET` (or `HEAD`), or the body
/// of a `QUERY`.
///
/// A `QUERY` body is `application/x-www-form-urlencoded` — the same
/// `key=value` pairs a `GET` puts after the `?`, and what curl sends when no
/// type is given — or `application/json`. Both carry exactly the parameters
/// the endpoint takes on a `GET`, which is what lets a WKT polygon or mask
/// too long for a URL reach the same handler unchanged.
pub struct Params<T>(pub T);

impl<S, T> FromRequest<S> for Params<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let method = req.method().clone();
        if method == Method::GET || method == Method::HEAD {
            let (mut parts, _) = req.into_parts();
            return Query::<T>::from_request_parts(&mut parts, state).await
                .map(|Query(p)| Params(p))
                .map_err(|e| ApiResponse::text(e.status(), e.body_text()).into_response());
        }
        if method.as_str() != "QUERY" {
            return Err(with_accept_query(ApiResponse::text(StatusCode::METHOD_NOT_ALLOWED,
                "method not allowed: use GET with a query string or QUERY with a body")
                .into_response()
                .with_header(header::ALLOW, ALLOW)));
        }

        let media_type = media_type(req.headers()).unwrap_or_else(|| FORM.into());
        let body = Bytes::from_request(req, state).await
            .map_err(|e| ApiResponse::text(e.status(), e.body_text()).into_response())?;
        let parsed = match media_type.as_str() {
            FORM => serde_urlencoded::from_bytes::<T>(&body).map_err(|e| format!("Failed to deserialize form body: {e}")),
            JSON => serde_json::from_slice::<T>(&body).map_err(|e| format!("Failed to deserialize JSON body: {e}")),
            other => return Err(with_accept_query(ApiResponse::text(StatusCode::UNSUPPORTED_MEDIA_TYPE,
                format!("unsupported QUERY body type {other}: send {FORM} or {JSON}")).into_response())),
        };
        parsed.map(Params).map_err(|e| ApiResponse::bad_request(e).into_response())
    }
}

/// The `Content-Type` without its parameters, lower-cased: `application/json`
/// for `Application/JSON; charset=utf-8`.
fn media_type(headers: &header::HeaderMap) -> Option<String> {
    let value = headers.get(header::CONTENT_TYPE)?.to_str().ok()?;
    Some(value.split(';').next().unwrap_or("").trim().to_ascii_lowercase())
}

fn with_accept_query(resp: Response) -> Response {
    resp.with_header(ACCEPT_QUERY_HEADER, ACCEPT_QUERY)
}

trait WithHeader {
    fn with_header(self, name: HeaderName, value: &'static str) -> Self;
}

impl WithHeader for Response {
    fn with_header(mut self, name: HeaderName, value: &'static str) -> Self {
        self.headers_mut().insert(name, HeaderValue::from_static(value));
        self
    }
}
