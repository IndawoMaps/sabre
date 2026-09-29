//! sabre inside a mobile app.
//!
//! MapLibre Native only reads raster tiles from a URL template, so the way to
//! put sabre behind it is the whole tile server, listening on the loopback
//! interface of the phone. [`start`] runs [`sabre_server::router`] over a
//! [`NativeBackend`] restricted to one directory of `file://` sources, and
//! hands back the port and a token. Tile URLs look like
//!
//! ```text
//! http://127.0.0.1:{port}/{token}/tiles/{z}/{x}/{y}?url=file://dem.tif&colormap=viridis
//! ```
//!
//! The token is there because loopback is not private on Android: any app on
//! the device can connect to the port. Without the token every route is a 404.
//!
//! Tiles are sent `Cache-Control: no-store`. Each style is a different URL,
//! and MapLibre would otherwise keep a copy of every tile of every style in
//! its on-disk ambient cache, for a source that is already on the device.
//!
//! The platforms call in through [`ffi`] (a C ABI, for iOS) and, on Android,
//! the JNI functions in `android`.

use std::path::PathBuf;
use std::sync::Mutex;
use std::thread::JoinHandle;

use axum::http::{header, HeaderValue};
use axum::response::Response;
use axum::Router;
use sabre_server::native::{NativeBackend, NativeConfig};
use tokio::sync::oneshot;

pub mod ffi;
#[cfg(target_os = "android")]
mod android;

/// Where a running server can be reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub port:  u16,
    pub token: String,
}

impl Endpoint {
    /// `http://127.0.0.1:{port}/{token}`: the prefix of every route.
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/{}", self.port, self.token)
    }
}

struct Running {
    endpoint: Endpoint,
    file_root: PathBuf,
    shutdown: oneshot::Sender<()>,
    thread:   JoinHandle<()>,
}

static SERVER: Mutex<Option<Running>> = Mutex::new(None);

/// Start the server, or return the one already running.
///
/// A second call with the same `file_root` returns the running server's
/// endpoint, so a screen can call this on mount without tracking whether
/// another did. A call with a different root restarts it. `cache_bytes` bounds
/// sabre's page cache; local files skip it today, so it only matters for
/// `https://` sources.
pub fn start(file_root: impl Into<PathBuf>, cache_bytes: usize) -> Result<Endpoint, String> {
    let file_root = file_root.into();
    let mut server = SERVER.lock().unwrap_or_else(|e| e.into_inner());

    if let Some(running) = server.as_ref() {
        // A thread that has finished has lost its listener -- on iOS, the
        // system reclaims the socket of a suspended app -- so start afresh.
        if running.file_root == file_root && !running.thread.is_finished() {
            return Ok(running.endpoint.clone());
        }
    }
    if let Some(running) = server.take() {
        shut_down(running);
    }

    let backend = NativeBackend::new(NativeConfig {
        file_root: Some(file_root.clone()),
        // Phones have many cores but few fast ones, and the UI wants some.
        threads: std::thread::available_parallelism().map(|n| n.get().min(4)).unwrap_or(2),
        cache_bytes,
        geometry_cache_bytes: 0,
        ..NativeConfig::default()
    })?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("sabre-http")
        .enable_io()
        .build()
        .map_err(|e| format!("cannot start runtime: {e}"))?;
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind(("127.0.0.1", 0)))
        .map_err(|e| format!("cannot bind 127.0.0.1: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();

    let endpoint = Endpoint { port, token: token()? };
    let app = Router::new()
        .nest(&format!("/{}", endpoint.token), sabre_server::router(backend))
        .layer(axum::middleware::map_response(no_store));

    let (shutdown, stopped) = oneshot::channel::<()>();
    let thread = std::thread::Builder::new()
        .name("sabre-server".into())
        .spawn(move || {
            runtime.block_on(async move {
                let _ = axum::serve(listener, app)
                    .with_graceful_shutdown(async { let _ = stopped.await; })
                    .await;
            });
        })
        .map_err(|e| format!("cannot start server thread: {e}"))?;

    *server = Some(Running { endpoint: endpoint.clone(), file_root, shutdown, thread });
    Ok(endpoint)
}

/// Stop the server if one is running. Waits for it to let go of the port.
pub fn stop() {
    let running = SERVER.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(running) = running {
        shut_down(running);
    }
}

fn shut_down(running: Running) {
    let _ = running.shutdown.send(());
    let _ = running.thread.join();
}

async fn no_store(mut response: Response) -> Response {
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// 128 bits from the OS's secure random source, as hex. This is what keeps
/// other apps out of a loopback port, so it is not left to a hasher's seed.
fn token() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|e| format!("cannot generate a token: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    // One test, because the server is process-wide.
    #[test]
    fn serves_tiles_behind_the_token_without_caching() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../core/tests/fixtures");
        let ep = start(&root, 1 << 20).unwrap();
        assert_eq!(start(&root, 1 << 20).unwrap(), ep, "same root reuses the running server");

        let health = reqwest::blocking::get(format!("{}/health", ep.base_url())).unwrap();
        assert_eq!(health.status(), 200);
        assert_eq!(health.headers()["cache-control"], "no-store");

        assert_eq!(ep.token.len(), 32);
        assert!(ep.token.bytes().all(|b| b.is_ascii_hexdigit()));

        let bare = reqwest::blocking::get(format!("http://127.0.0.1:{}/health", ep.port)).unwrap();
        assert_eq!(bare.status(), 404, "routes need the token");

        stop();
        assert!(reqwest::blocking::get(format!("{}/health", ep.base_url())).is_err());
    }
}
