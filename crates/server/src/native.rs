//! Native shim: a tokio binary for a plain host or a container.
//!
//! Sources are read with reqwest (`https://`) or from the local filesystem
//! (`file://`, only when a file root is configured). Remote sources are read
//! through a [`PagedReader`] backed by [`MemoryPageCache`], a byte-bounded
//! in-process LRU of 64 KB pages in the manner of GDAL's `VSICURL` cache, so
//! the header and the pixel data behind a warm tile never leave memory.
//!
//! `sabre-core` futures are `!Send`, and axum on tokio needs `Send` handler
//! futures, so each request is pinned to one of a small pool of dedicated
//! threads via [`LocalPoolHandle`] and only the finished [`ApiResponse`]
//! crosses back.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use sabre_core::cog::RangeReader;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::task::LocalPoolHandle;

use crate::geometry;
use crate::reader::{range_body, InFlight, Page, PageCache, PagedReader};
use crate::{ApiResponse, Backend};

// ── Readers ───────────────────────────────────────────────────────────────────

pub struct HttpRangeReader {
    client: reqwest::Client,
    url:    String,
}

#[async_trait(?Send)]
impl RangeReader for HttpRangeReader {
    async fn read_range(&self, offset: u64, length: u64) -> Result<Vec<u8>, String> {
        let range = format!("bytes={}-{}", offset, offset + length - 1);
        let resp = self.client.get(&self.url)
            .header(reqwest::header::RANGE, range)
            .send().await
            .map_err(|e| format!("{e} for {}", self.url))?;
        let status = resp.status().as_u16();
        let body = resp.bytes().await.map_err(|e| e.to_string())?.to_vec();
        range_body(status, body, offset, length, &self.url)
    }
}

/// Reads a local file. Ranges past the end of the file are truncated, which
/// matches how object stores answer an over-long range request.
pub struct FileRangeReader {
    path: PathBuf,
}

#[async_trait(?Send)]
impl RangeReader for FileRangeReader {
    async fn read_range(&self, offset: u64, length: u64) -> Result<Vec<u8>, String> {
        let describe = |e: std::io::Error| format!("{e} for {}", self.path.display());
        let mut file = tokio::fs::File::open(&self.path).await.map_err(describe)?;
        file.seek(std::io::SeekFrom::Start(offset)).await.map_err(describe)?;
        let mut buf = Vec::with_capacity(length as usize);
        file.take(length).read_to_end(&mut buf).await.map_err(describe)?;
        Ok(buf)
    }
}

// ── Page cache ────────────────────────────────────────────────────────────────

/// A byte-bounded, least-recently-used cache of source pages, shared by all
/// request threads.
///
/// Entries expire after `ttl`, which is what lets a re-uploaded COG be picked
/// up without a restart: GDAL never revalidates within a process and neither
/// does this, but nothing here outlives the TTL. Reads are served under one
/// short mutex hold per page; the bytes themselves are shared, not copied.
/// Cloneable, and cheap to clone: every clone shares one set of pages. The
/// trait it implements lives in `sabre-core` now, and the orphan rule will not
/// let a foreign trait be implemented for `Arc<T>`, so the sharing is inside
/// the type rather than around it.
#[derive(Clone)]
pub struct MemoryPageCache {
    capacity: usize,
    ttl:      Option<Duration>,
    state:    Arc<Mutex<PageState>>,
}

/// Bookkeeping charged to every entry on top of its bytes, so empty and short
/// pages still count against the capacity.
const ENTRY_OVERHEAD: usize = 128;

#[derive(Default)]
struct PageState {
    entries: HashMap<(String, u64), PageEntry>,
    /// Recency order: the smallest tick is the least recently used entry.
    lru:     BTreeMap<u64, (String, u64)>,
    tick:    u64,
    bytes:   usize,
}

struct PageEntry {
    page:    Page,
    tick:    u64,
    expires: Option<Instant>,
}

impl MemoryPageCache {
    /// `capacity` is in bytes; `ttl` of `None` keeps pages until evicted.
    pub fn new(capacity: usize, ttl: Option<Duration>) -> Self {
        Self { capacity, ttl, state: Arc::new(Mutex::new(PageState::default())) }
    }

    /// Bytes currently held, bookkeeping included.
    pub fn len_bytes(&self) -> usize {
        self.state.lock().unwrap().bytes
    }
}

impl PageState {
    fn remove(&mut self, key: &(String, u64)) -> Option<PageEntry> {
        let entry = self.entries.remove(key)?;
        self.lru.remove(&entry.tick);
        self.bytes -= entry.page.len() + ENTRY_OVERHEAD;
        Some(entry)
    }

    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }
}

#[async_trait(?Send)]
impl PageCache for MemoryPageCache {
    async fn get(&self, source: &str, page: u64) -> Option<Page> {
        let mut state = self.state.lock().unwrap();
        let key = (source.to_string(), page);
        let expired = matches!(state.entries.get(&key), Some(e) if e.expires.is_some_and(|t| t <= Instant::now()));
        if expired {
            state.remove(&key);
            return None;
        }
        let tick  = state.next_tick();
        let entry = state.entries.get_mut(&key)?;
        let old   = std::mem::replace(&mut entry.tick, tick);
        let page  = entry.page.clone();
        let lru_key = state.lru.remove(&old).expect("every entry has an LRU record");
        state.lru.insert(tick, lru_key);
        Some(page)
    }

    async fn put(&self, source: &str, page: u64, bytes: Page) {
        if self.capacity == 0 { return; }
        let mut state = self.state.lock().unwrap();
        let key = (source.to_string(), page);
        state.remove(&key);
        let tick = state.next_tick();
        state.bytes += bytes.len() + ENTRY_OVERHEAD;
        state.lru.insert(tick, key.clone());
        state.entries.insert(key, PageEntry { page: bytes, tick, expires: self.ttl.map(|ttl| Instant::now() + ttl) });
        while state.bytes > self.capacity {
            match state.lru.iter().next().map(|(_, key)| key.clone()) {
                Some(oldest) => { state.remove(&oldest); }
                None         => break,
            }
        }
    }
}

// ── Backend ───────────────────────────────────────────────────────────────────

/// Configuration for [`NativeBackend`].
#[derive(Debug, Clone)]
pub struct NativeConfig {
    /// Directory that `file://` URLs resolve inside. `None` disables them.
    pub file_root: Option<PathBuf>,
    /// Number of dedicated request threads.
    pub threads: usize,
    /// Bytes of remote source data kept in memory across requests, headers
    /// included. `0` disables caching.
    pub cache_bytes: usize,
    /// How long a cached page stays valid. `None` keeps pages until evicted.
    pub cache_ttl: Option<Duration>,
    /// Geometry providers `geometry_provider` may name. Empty by default.
    pub providers: geometry::Registry,
    /// Bytes of resolved geometry and access decisions to keep. Separate from
    /// `cache_bytes` because it is a separate pool: the two add up, and the
    /// box has to hold both.
    pub geometry_cache_bytes: usize,
}

impl Default for NativeConfig {
    fn default() -> Self {
        Self {
            file_root: None,
            threads: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4),
            cache_bytes: 256 * 1024 * 1024,
            cache_ttl: Some(Duration::from_secs(3600)),
            providers: geometry::Registry::default(),
            geometry_cache_bytes: 64 * 1024 * 1024,
        }
    }
}

#[derive(Clone)]
pub struct NativeBackend {
    pool:           LocalPoolHandle,
    client:         reqwest::Client,
    cache:          MemoryPageCache,
    /// Pages a request is fetching right now, so concurrent requests for
    /// neighbouring tiles fetch the pages they share once.
    in_flight:      InFlight,
    file_root:      Option<Arc<PathBuf>>,
    providers:      geometry::Registry,
    geometry_cache: MemoryTextCache,
}

impl NativeBackend {
    pub fn new(config: NativeConfig) -> Result<Self, String> {
        let file_root = match config.file_root {
            Some(root) => Some(Arc::new(root.canonicalize().map_err(|e| format!("file root {}: {e}", root.display()))?)),
            None       => None,
        };
        let client = reqwest::Client::builder()
            .user_agent(concat!("sabre-server/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            pool: LocalPoolHandle::new(config.threads.max(1)),
            client,
            cache: MemoryPageCache::new(config.cache_bytes, config.cache_ttl),
            in_flight: InFlight::default(),
            file_root,
            providers: config.providers,
            geometry_cache: MemoryTextCache::new(config.geometry_cache_bytes),
        })
    }

    /// Resolve an `http(s)://` or `file://` source URL to a boxed reader.
    ///
    /// Remote sources go through the page cache. Local files do not: the
    /// operating system already caches them, and a second copy would only
    /// take memory from the remote ones.
    fn make_reader(&self, source: &str) -> Result<Box<dyn RangeReader>, String> {
        if let Some(rest) = source.strip_prefix("file://") {
            let root = self.file_root.as_deref()
                .ok_or("file:// sources are disabled; start the server with --file-root <dir> to enable them")?;
            return Ok(Box::new(FileRangeReader { path: resolve_file(root, rest)? }));
        }
        if !(source.starts_with("http://") || source.starts_with("https://")) {
            return Err(format!("unsupported source URL scheme: {source}"));
        }
        let http = Box::new(HttpRangeReader { client: self.client.clone(), url: source.to_string() });
        if self.cache.capacity == 0 {
            return Ok(http);
        }
        Ok(Box::new(PagedReader::new(http, Box::new(self.cache.clone()), source)
            .sharing(self.in_flight.clone())))
    }
}

/// Resolve the path part of a `file://` URL against `root`, refusing anything
/// that escapes it. Both `file://dem.tif` and `file:///dem.tif` mean
/// `<root>/dem.tif`.
fn resolve_file(root: &Path, rest: &str) -> Result<PathBuf, String> {
    let relative = rest.trim_start_matches('/');
    let path = root.join(relative);
    let canonical = path.canonicalize().map_err(|e| format!("{e} for file://{rest}"))?;
    if !canonical.starts_with(root) {
        return Err(format!("file://{rest} is outside the configured file root"));
    }
    Ok(canonical)
}

// ── Geometry ──────────────────────────────────────────────────────────────────

/// Fetches geometry with the same reqwest client the readers use.
struct HttpFetcher(reqwest::Client);

#[async_trait(?Send)]
impl geometry::Fetcher for HttpFetcher {
    async fn get(&self, url: &str, auth: Option<&str>, timeout_ms: u64, max_bytes: usize)
        -> Result<(u16, Option<String>, Vec<u8>), String> {
        let mut req = self.0.get(url).timeout(Duration::from_millis(timeout_ms));
        if let Some(value) = auth {
            req = req.header(reqwest::header::AUTHORIZATION, value);
        }
        let resp = req.send().await.map_err(|e| format!("{e} for {url}"))?;
        let status = resp.status().as_u16();
        let content_type = resp.headers().get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()).map(str::to_string);

        // Refuse an over-long body rather than truncating it: half a polygon
        // parses into a different shape, it does not fail.
        if let Some(len) = resp.content_length() {
            if len as usize > max_bytes {
                return Err(format!("{url} returned {len} bytes; the limit is {max_bytes}"));
            }
        }
        let body = resp.bytes().await.map_err(|e| e.to_string())?;
        if body.len() > max_bytes {
            return Err(format!("{url} returned more than {max_bytes} bytes"));
        }
        Ok((status, content_type, body.to_vec()))
    }
}

/// A TTL cache for resolved geometry (canonical TWKB) and access decisions,
/// bounded in bytes.
///
/// Bounded in *bytes*, and that is the whole point. It was bounded by entry
/// count, which is not a bound on anything you can put in a container: the
/// entries are geometry, so they grow with how much land a request names, and
/// the cache grows with the *variety* of what is asked for rather than the
/// rate. Measured, a farm view naming 80 blocks costs 8.9 KB an entry, so the
/// old 10,000-entry ceiling was 87 MB — and roughly 500 MB at the 512-id
/// limit — sitting beside a page cache whose 256 MB was the number anyone
/// would have thought they had configured.
///
/// Keys are charged too. A key carries every id in the set, so a request
/// naming 512 geometries writes a 2 KB key, twice: once for the geometry and
/// once for the access decision.
#[derive(Clone)]
pub struct MemoryTextCache {
    capacity: usize,
    state:    Arc<Mutex<TextState>>,
}

/// Cached bytes and the moment they stop being usable.
type Entry = (Arc<[u8]>, Instant);

#[derive(Default)]
struct TextState {
    entries: HashMap<String, Entry>,
    bytes:   usize,
}

/// Charged per entry on top of key and value: the HashMap slot, the Arc
/// header and the Instant. Approximate, and deliberately not generous.
const TEXT_ENTRY_OVERHEAD: usize = 96;

fn charge(key: &str, value: &[u8]) -> usize {
    key.len() + value.len() + TEXT_ENTRY_OVERHEAD
}

impl TextState {
    fn remove(&mut self, key: &str) {
        if let Some((value, _)) = self.entries.remove(key) {
            self.bytes -= charge(key, &value);
        }
    }
}

impl MemoryTextCache {
    /// `capacity` is in bytes; `0` disables the cache.
    pub fn new(capacity: usize) -> Self {
        Self { capacity, state: Arc::new(Mutex::new(TextState::default())) }
    }

    pub fn len(&self) -> usize {
        self.state.lock().unwrap().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes currently held, keys and bookkeeping included.
    pub fn len_bytes(&self) -> usize {
        self.state.lock().unwrap().bytes
    }
}

#[async_trait(?Send)]
impl geometry::Cache for MemoryTextCache {
    async fn get(&self, key: &str) -> Option<Arc<[u8]>> {
        let mut state = self.state.lock().unwrap();
        match state.entries.get(key) {
            Some((value, expires)) if *expires > Instant::now() => Some(Arc::clone(value)),
            Some(_) => { state.remove(key); None }
            None    => None,
        }
    }

    async fn put(&self, key: &str, value: Arc<[u8]>, ttl_s: u64) {
        if self.capacity == 0 || ttl_s == 0 {
            return;
        }
        let cost = charge(key, &value);
        // One entry larger than the whole budget is not cached rather than
        // emptying the cache to hold it.
        if cost > self.capacity {
            return;
        }

        let mut state = self.state.lock().unwrap();
        let now = Instant::now();
        state.remove(key);

        if state.bytes + cost > self.capacity {
            let expired: Vec<String> = state.entries.iter()
                .filter(|(_, (_, e))| *e <= now)
                .map(|(k, _)| k.clone())
                .collect();
            for k in expired {
                state.remove(&k);
            }
        }
        // Still over: drop entries closest to expiring, which are the least
        // useful to keep, until it fits.
        while state.bytes + cost > self.capacity {
            let Some(soonest) = state.entries.iter()
                .min_by_key(|(_, (_, e))| *e)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            state.remove(&soonest);
        }

        state.bytes += cost;
        state.entries.insert(key.to_string(), (value, now + Duration::from_secs(ttl_s)));
    }
}

impl Backend for NativeBackend {
    fn run<F, Fut>(&self, source: String, f: F) -> impl Future<Output = ApiResponse> + Send
    where
        F:   FnOnce(Box<dyn RangeReader>) -> Fut + Send + 'static,
        Fut: Future<Output = ApiResponse> + 'static,
    {
        let backend = self.clone();
        let task = self.pool.spawn_pinned(move || async move {
            match backend.make_reader(&source) {
                Ok(reader) => f(reader).await,
                Err(e)     => ApiResponse::bad_request(e),
            }
        });
        async move {
            task.await.unwrap_or_else(|e| ApiResponse::internal(format!("request task failed: {e}")))
        }
    }

    fn geometry(&self) -> geometry::Registry {
        self.providers.clone()
    }

    fn resolve_geometry(
        &self,
        provider: geometry::Provider,
        ids: Vec<String>,
        auth: Option<String>,
    ) -> impl Future<Output = Result<Arc<sabre_core::mask::Mask>, geometry::Error>> + Send {
        let client = self.client.clone();
        let cache  = self.geometry_cache.clone();
        let task = self.pool.spawn_pinned(move || async move {
            let fetcher = HttpFetcher(client);
            geometry::resolve(&provider, &ids, auth.as_deref(), &fetcher, &cache).await
        });
        async move {
            task.await.unwrap_or_else(|e| {
                Err(geometry::Error::Upstream(format!("geometry task failed: {e}")))
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(fill: u8, len: usize) -> Page {
        Arc::from(vec![fill; len].as_slice())
    }

    fn cache(capacity: usize, ttl: Option<Duration>) -> MemoryPageCache {
        MemoryPageCache::new(capacity, ttl)
    }

    // ── Geometry cache ──────────────────────────────────────────────────
    //
    // The bound that matters is bytes. Entry count bounds nothing you can put
    // in a container, because the entries are geometry and grow with how much
    // land a request names.

    use crate::geometry::Cache as _;

    fn text(capacity: usize) -> MemoryTextCache {
        MemoryTextCache::new(capacity)
    }

    #[test]
    fn geometry_cache_holds_to_its_byte_budget() {
        let c = text(10 * 1024);
        pollster::block_on(async {
            // Farm-sized entries: 8 KB each is what 80 blocks measured at.
            for i in 0..50 {
                c.put(&format!("geom/blocks/set-{i}"), Arc::from(vec![0u8; 8 * 1024].as_slice()), 60).await;
            }
            assert!(c.len_bytes() <= 10 * 1024,
                    "held {} bytes against a 10 KB budget", c.len_bytes());
            assert!(c.len() <= 2, "only what fits: {} entries", c.len());
        });
    }

    #[test]
    fn a_long_key_is_charged_for() {
        // A request naming 512 geometries writes a 2 KB key, twice.
        let c = text(4096);
        let long_key = format!("geom/blocks/{}", "1234,".repeat(400));
        pollster::block_on(async {
            c.put(&long_key, Arc::from(&b"x"[..]), 60).await;
            assert!(c.len_bytes() > long_key.len(),
                    "the key has to count: {} bytes for a {} byte key",
                    c.len_bytes(), long_key.len());
        });
    }

    #[test]
    fn an_entry_bigger_than_the_budget_is_skipped_not_swallowed() {
        let c = text(1024);
        pollster::block_on(async {
            c.put("keep", Arc::from(&b"small"[..]), 60).await;
            c.put("huge", Arc::from(vec![0u8; 4096].as_slice()), 60).await;
            assert!(c.get("keep").await.is_some(), "one oversized put must not empty the cache");
            assert!(c.get("huge").await.is_none());
        });
    }

    #[test]
    fn replacing_an_entry_keeps_the_accounting_straight() {
        let c = text(1 << 20);
        pollster::block_on(async {
            c.put("k", Arc::from(vec![0u8; 500].as_slice()), 60).await;
            let once = c.len_bytes();
            c.put("k", Arc::from(vec![0u8; 500].as_slice()), 60).await;
            assert_eq!(c.len_bytes(), once, "the same key twice is one entry");
            assert_eq!(c.len(), 1);
        });
    }

    #[test]
    fn expired_entries_are_reclaimed_before_live_ones_are_dropped() {
        let c = text(3 * (500 + TEXT_ENTRY_OVERHEAD + 8));
        pollster::block_on(async {
            c.put("old-a", Arc::from(vec![0u8; 500].as_slice()), 1).await;
            c.put("old-b", Arc::from(vec![0u8; 500].as_slice()), 1).await;
            std::thread::sleep(Duration::from_millis(1100));
            c.put("fresh", Arc::from(vec![0u8; 500].as_slice()), 60).await;
            assert!(c.get("fresh").await.is_some());
            assert!(c.get("old-a").await.is_none(), "expired entries go first");
            assert!(c.len_bytes() <= c.capacity);
        });
    }

    #[test]
    fn a_zero_budget_stores_nothing() {
        let c = text(0);
        pollster::block_on(async {
            c.put("k", Arc::from(&b"v"[..]), 60).await;
            assert!(c.get("k").await.is_none());
            assert_eq!(c.len_bytes(), 0);
        });
    }

    #[test]
    fn evicts_the_least_recently_used_page_first() {
        let c = cache(3 * (100 + ENTRY_OVERHEAD), None);
        pollster::block_on(async {
            c.put("a", 0, page(0, 100)).await;
            c.put("a", 1, page(1, 100)).await;
            c.put("b", 0, page(2, 100)).await;
            assert_eq!(c.len_bytes(), 3 * (100 + ENTRY_OVERHEAD));
            // Touch the oldest page so the second becomes the eviction candidate.
            assert!(c.get("a", 0).await.is_some());
            c.put("b", 1, page(3, 100)).await;
            assert!(c.get("a", 0).await.is_some());
            assert!(c.get("a", 1).await.is_none());
            assert!(c.get("b", 0).await.is_some());
            assert!(c.get("b", 1).await.is_some());
            assert_eq!(c.len_bytes(), 3 * (100 + ENTRY_OVERHEAD));
        });
    }

    #[test]
    fn replacing_a_page_keeps_the_accounting_straight() {
        let c = cache(usize::MAX, None);
        pollster::block_on(async {
            c.put("a", 0, page(0, 100)).await;
            c.put("a", 0, page(1, 50)).await;
            assert_eq!(c.len_bytes(), 50 + ENTRY_OVERHEAD);
            assert_eq!(c.get("a", 0).await.unwrap().as_ref(), &[1u8; 50]);
        });
    }

    #[test]
    fn zero_capacity_stores_nothing() {
        let c = cache(0, None);
        pollster::block_on(async {
            c.put("a", 0, page(0, 100)).await;
            assert!(c.get("a", 0).await.is_none());
            assert_eq!(c.len_bytes(), 0);
        });
    }

    #[test]
    fn pages_expire_after_the_ttl() {
        let c = cache(usize::MAX, Some(Duration::from_millis(20)));
        pollster::block_on(async {
            c.put("a", 0, page(0, 100)).await;
            assert!(c.get("a", 0).await.is_some());
            std::thread::sleep(Duration::from_millis(40));
            assert!(c.get("a", 0).await.is_none());
            assert_eq!(c.len_bytes(), 0);
        });
    }
}
