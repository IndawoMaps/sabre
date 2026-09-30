//! Clip geometry the app puts in once, and tiles and queries name after that.
//!
//! A `mask` in a tile URL is carried on every tile MapLibre asks for, and a
//! farm's worth of WKT is past what a URL can hold at all. Instead the app
//! puts geometry in, under a provider name and ids:
//!
//! ```text
//! POST   /{token}/geometries/blocks   {"19519": <WKT or GeoJSON>, …}   add or overwrite
//! PUT    /{token}/geometries/blocks   {"19519": …}                     replace them all
//! DELETE /{token}/geometries/blocks   ["19519", …]                     remove
//! ```
//!
//! and a tile or query names it with `geometry_provider=blocks&geometry_id=19519`,
//! the parameters a sabre server's providers take. Each change answers with
//! the provider's new revision, which the JavaScript side puts into tile URLs
//! so a map redraws when its geometry changes.
//!
//! The store belongs to the process, not to the running server: iOS takes a
//! suspended app's socket away, and the server that replaces it has to find
//! the geometry still there.

use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use sabre_core::geometry::{to_mask, GeometryStore};
use sabre_core::mask::Mask;
use sabre_server::native::NativeBackend;
use sabre_server::{geometry, ApiResponse, Backend};

static STORE: LazyLock<Mutex<GeometryStore>> = LazyLock::new(|| Mutex::new(GeometryStore::new()));

fn store() -> MutexGuard<'static, GeometryStore> {
    STORE.lock().unwrap_or_else(|e| e.into_inner())
}

/// How much geometry one change may carry. Thousands of fields as GeoJSON
/// fit easily; this is only here so a mistake cannot take the app's memory.
const MAX_BODY: usize = 32 * 1024 * 1024;

/// The routes that change the store, to sit beside the tile server's.
pub fn routes() -> Router {
    Router::new()
        .route("/geometries/{provider}", post(set).put(replace).delete(delete))
        .layer(DefaultBodyLimit::max(MAX_BODY))
}

/// The tile server's backend, answering geometry names from the store.
#[derive(Clone)]
pub struct MobileBackend(pub NativeBackend);

impl Backend for MobileBackend {
    fn run<F, Fut>(&self, source: String, f: F) -> impl std::future::Future<Output = ApiResponse> + Send
    where
        F: FnOnce(Box<dyn sabre_core::cog::RangeReader>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ApiResponse> + 'static,
    {
        self.0.run(source, f)
    }

    fn resolve_geometry(
        &self,
        provider: geometry::Provider,
        ids: Vec<String>,
        auth: Option<String>,
    ) -> impl std::future::Future<Output = Result<Arc<Mask>, geometry::Error>> + Send {
        self.0.resolve_geometry(provider, ids, auth)
    }

    fn local_geometry(&self, provider: &str, ids: &[String]) -> Option<Result<Arc<Mask>, String>> {
        Some(store().resolve(provider, ids))
    }
}

async fn set(Path(provider): Path<String>, body: Bytes) -> Response {
    change(&provider, &body, false)
}

async fn replace(Path(provider): Path<String>, body: Bytes) -> Response {
    change(&provider, &body, true)
}

async fn delete(Path(provider): Path<String>, body: Bytes) -> Response {
    let ids = match serde_json::from_slice::<Vec<serde_json::Value>>(&body) {
        Ok(ids) => ids.iter().map(id_of).collect::<Result<Vec<_>, _>>(),
        Err(e) => Err(format!("expected a JSON array of ids: {e}")),
    };
    match ids {
        Ok(ids) => revision(store().delete(&provider, &ids)),
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}

fn change(provider: &str, body: &[u8], replace: bool) -> Response {
    // Decoded before the store is locked: tiles resolving geometry wait on
    // that lock, and parsing a farm is the slow part.
    let entries = match decode(body) {
        Ok(e) => e,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };
    let mut store = store();
    let done = if replace { store.replace(provider, entries) } else { store.set(provider, entries) };
    match done {
        Ok(rev) => revision(rev),
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}

/// `{id: geometry}`, each WGS84 WKT or GeoJSON, as text or as an object.
fn decode(body: &[u8]) -> Result<Vec<(String, Mask)>, String> {
    let serde_json::Value::Object(entries) = serde_json::from_slice(body)
        .map_err(|e| format!("expected a JSON object of id to geometry: {e}"))? else {
        return Err("expected a JSON object of id to geometry".into());
    };
    entries.into_iter().map(|(id, g)| {
        let mask = match &g {
            serde_json::Value::String(text) => to_mask(text.as_bytes(), None),
            serde_json::Value::Object(_) => to_mask(g.to_string().as_bytes(), None),
            other => Err(format!("expected WKT or GeoJSON, got {other}")),
        };
        mask.map(|m| (id.clone(), m)).map_err(|e| format!("geometry {id}: {e}"))
    }).collect()
}

fn id_of(v: &serde_json::Value) -> Result<String, String> {
    match v {
        serde_json::Value::String(s) => Ok(s.clone()),
        serde_json::Value::Number(n) => Ok(n.to_string()),
        other => Err(format!("an id cannot be {other}")),
    }
}

fn revision(rev: u64) -> Response {
    ([(header::CONTENT_TYPE, "application/json")], format!(r#"{{"revision":{rev}}}"#)).into_response()
}
