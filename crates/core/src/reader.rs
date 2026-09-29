//! Reader plumbing: header caching, page caching and HTTP range response
//! handling. The actual byte fetching lives in whatever is driving this --
//! reqwest in the binary, `fetch` in a browser.
//!
//! This lives in `core` rather than in the server because it is the part worth
//! having anywhere the renderer runs. Panning a map re-reads the same overview
//! tiles over and over, and caching them is what turns a repeated tile from
//! 2,143 ms into 30 ms against a network origin. A browser tab is as good a
//! place for that as a server process.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_channel::oneshot;

use async_trait::async_trait;
use crate::cog::{RangeReader, HEADER_FETCH_BYTES};

// ── Header cache ──────────────────────────────────────────────────────────────

/// Storage for the cached COG header, backed by an in-process map.
#[async_trait(?Send)]
pub trait HeaderCache {
    async fn get(&self, key: &str) -> Option<Vec<u8>>;
    async fn put(&self, key: &str, bytes: &[u8]);
}

/// Wraps any [`RangeReader`] and serves the initial header fetch from a
/// [`HeaderCache`], so repeated tile requests for the same COG skip the
/// round-trip for metadata. Every other range passes straight through.
pub struct CachingReader {
    inner: Box<dyn RangeReader>,
    cache: Box<dyn HeaderCache>,
    key:   String,
}

impl CachingReader {
    pub fn new(inner: Box<dyn RangeReader>, cache: Box<dyn HeaderCache>, source: &str) -> Self {
        Self { inner, cache, key: cache_key(source) }
    }
}

/// Derive a cache key from a source URL.
///
/// `://` and query characters are folded into a plain path, so the key is
/// itself a valid URL: `https://host/path?x=1` becomes
/// `https://sabre-meta-v1/https/host/path_x=1`.
pub fn cache_key(source: &str) -> String {
    let slug = source.replace("://", "/").replace(['?', '#', '&'], "_");
    format!("https://sabre-meta-v1/{slug}")
}

#[async_trait(?Send)]
impl RangeReader for CachingReader {
    async fn read_range(&self, offset: u64, length: u64) -> Result<Vec<u8>, String> {
        if offset == 0 && length == HEADER_FETCH_BYTES {
            if let Some(bytes) = self.cache.get(&self.key).await {
                if !bytes.is_empty() {
                    return Ok(bytes);
                }
            }
            let bytes = self.inner.read_range(0, length).await?;
            self.cache.put(&self.key, &bytes).await;
            return Ok(bytes);
        }
        self.inner.read_range(offset, length).await
    }
}

// ── Page cache ────────────────────────────────────────────────────────────────

/// Size of one cached page. Requests are widened to page boundaries, so this
/// is the most a read over-fetches at each end; it also bounds how many cache
/// lookups a tile costs. Comfortably above sabre's 32 KB range-coalescing gap.
pub const PAGE_BYTES: u64 = 64 * 1024;

/// One cached page of a source: `PAGE_BYTES` long, shorter for the last page
/// of the file, and empty for a page past its end. Shared, so serving a hit
/// never copies the page until it is sliced into the caller's buffer.
pub type Page = Arc<[u8]>;

/// Storage for cached pages, keyed by source and page index. The native
/// binary backs this with a byte-bounded LRU in memory; a runtime without a
/// suitable store simply does not wrap its reader in a [`PagedReader`].
#[async_trait(?Send)]
pub trait PageCache {
    async fn get(&self, source: &str, page: u64) -> Option<Page>;
    async fn put(&self, source: &str, page: u64, bytes: Page);
}

/// Wraps any [`RangeReader`] with a GDAL-style byte cache.
///
/// Every read is widened to whole pages. Pages already in the [`PageCache`]
/// are served from it and each run of consecutive missing pages is fetched
/// with a single range read, so a warm cache turns repeated tiles over the
/// same raster into zero origin requests, and a cold one costs at most one
/// request per run of misses — the same as the uncached reader.
///
/// Pages are fixed and aligned rather than keyed on the exact range asked for
/// because the ranges sabre coalesces differ from tile to tile: two viewports
/// that overlap the same bytes rarely ask for the same slab.
pub struct PagedReader {
    inner:      Box<dyn RangeReader>,
    cache:      Box<dyn PageCache>,
    source:     String,
    page_bytes: u64,
    in_flight:  Option<InFlight>,
}

impl PagedReader {
    pub fn new(inner: Box<dyn RangeReader>, cache: Box<dyn PageCache>, source: &str) -> Self {
        Self::with_page_size(inner, cache, source, PAGE_BYTES)
    }

    pub fn with_page_size(inner: Box<dyn RangeReader>, cache: Box<dyn PageCache>, source: &str, page_bytes: u64) -> Self {
        assert!(page_bytes > 0, "page size must be positive");
        Self { inner, cache, source: source.to_string(), page_bytes, in_flight: None }
    }

    /// Share fetches with every other reader holding the same [`InFlight`]:
    /// a page one of them is already fetching is waited for, not fetched
    /// again. Give it the same `InFlight` as the cache is shared with.
    pub fn sharing(mut self, in_flight: InFlight) -> Self {
        self.in_flight = Some(in_flight);
        self
    }

    /// Fetch pages `first + i .. first + j` in one range read and cache them.
    async fn fetch_run(&self, first: u64, i: usize, j: usize, pages: &mut [Option<Page>]) -> Result<(), String> {
        let page_bytes = self.page_bytes;
        let start = (first + i as u64) * page_bytes;
        let data  = self.inner.read_range(start, (j - i) as u64 * page_bytes).await?;
        for (k, slot) in pages.iter_mut().enumerate().take(j).skip(i) {
            let lo   = (k - i) * page_bytes as usize;
            let hi   = (lo + page_bytes as usize).min(data.len());
            let page: Page = if lo < data.len() { Arc::from(&data[lo..hi]) } else { Arc::from(&[][..]) };
            self.cache.put(&self.source, first + k as u64, page.clone()).await;
            *slot = Some(page);
        }
        Ok(())
    }
}

/// Pages being fetched right now, shared by the readers of one cache.
///
/// A map asks for many tiles at once, and neighbouring tiles need the same
/// pages. With nothing shared, every one of them misses the cache together
/// and fetches those pages itself: in the browser demo, six tiles in flight
/// downloaded 84% more than one at a time did, some ranges four times over.
/// A reader that finds a page here waits for it rather than fetching it.
#[derive(Clone, Default)]
pub struct InFlight(Arc<Mutex<HashMap<(String, u64), Waiters>>>);

/// Readers waiting on one page. Dropping the senders wakes them.
type Waiters = Vec<oneshot::Sender<()>>;

impl InFlight {
    /// Claim `page` to fetch, or get a receiver that resolves once whoever
    /// holds the claim lets it go.
    fn claim(&self, source: &str, page: u64) -> Option<oneshot::Receiver<()>> {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match map.get_mut(&(source.to_string(), page)) {
            Some(waiters) => {
                let (tx, rx) = oneshot::channel();
                waiters.push(tx);
                Some(rx)
            }
            None => {
                map.insert((source.to_string(), page), Vec::new());
                None
            }
        }
    }

    /// Let claims go. Dropping a waiter's sender is what wakes it, so this
    /// wakes everyone waiting on these pages whether the fetch worked or not.
    fn release(&self, source: &str, pages: impl IntoIterator<Item = u64>) {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        for page in pages {
            map.remove(&(source.to_string(), page));
        }
    }
}

/// Claims a reader still holds, let go however it exits -- an error from the
/// origin or a dropped future must not leave others waiting forever.
struct Claims<'a> {
    in_flight: &'a InFlight,
    source:    &'a str,
    pages:     Vec<u64>,
}

impl Claims<'_> {
    fn release(&mut self, pages: std::ops::Range<u64>) {
        self.pages.retain(|p| !pages.contains(p));
        self.in_flight.release(self.source, pages);
    }
}

impl Drop for Claims<'_> {
    fn drop(&mut self) {
        self.in_flight.release(self.source, self.pages.drain(..));
    }
}

#[async_trait(?Send)]
impl RangeReader for PagedReader {
    async fn read_range(&self, offset: u64, length: u64) -> Result<Vec<u8>, String> {
        if length == 0 {
            return Ok(Vec::new());
        }
        let page_bytes = self.page_bytes;
        let first = offset / page_bytes;
        let last  = (offset + length - 1) / page_bytes;
        let count = (last - first + 1) as usize;

        let mut pages: Vec<Option<Page>> = Vec::with_capacity(count);
        for page in first..=last {
            pages.push(self.cache.get(&self.source, page).await);
        }

        // Split the misses: pages this reader fetches, and pages another
        // reader is already fetching, which it waits for instead.
        let mut mine = vec![true; count];
        let mut waits = Vec::new();
        let mut claims = self.in_flight.as_ref().map(|f| Claims { in_flight: f, source: &self.source, pages: Vec::new() });
        if let (Some(f), Some(claims)) = (&self.in_flight, claims.as_mut()) {
            for (k, slot) in pages.iter().enumerate() {
                if slot.is_some() {
                    continue;
                }
                match f.claim(&self.source, first + k as u64) {
                    Some(rx) => { mine[k] = false; waits.push((k, rx)); }
                    None     => claims.pages.push(first + k as u64),
                }
            }
        }

        // Fetch each run of consecutive misses with one range read. A short
        // body means the file ended inside the run: the page it ended in is
        // stored short and the rest empty, so the end of the file is
        // remembered too and never asked for again. Claims go as soon as the
        // pages are cached, before waiting on anyone else's, so two readers
        // each waiting on the other's pages cannot both be holding theirs.
        let mut i = 0;
        while i < count {
            if pages[i].is_some() || !mine[i] {
                i += 1;
                continue;
            }
            let mut j = i;
            while j < count && pages[j].is_none() && mine[j] {
                j += 1;
            }
            self.fetch_run(first, i, j, &mut pages).await?;
            if let Some(claims) = claims.as_mut() {
                claims.release(first + i as u64..first + j as u64);
            }
            i = j;
        }

        // Pages someone else was fetching are in the cache now -- unless
        // their fetch failed, or the cache has already evicted them, in which
        // case fetch them here after all.
        for (k, rx) in waits {
            let _ = rx.await;
            pages[k] = self.cache.get(&self.source, first + k as u64).await;
        }
        let mut i = 0;
        while i < count {
            if pages[i].is_some() {
                i += 1;
                continue;
            }
            let mut j = i;
            while j < count && pages[j].is_none() {
                j += 1;
            }
            self.fetch_run(first, i, j, &mut pages).await?;
            i = j;
        }

        // Slice the requested range out of the pages. Like the origin, stop
        // at the end of the file rather than padding.
        let end = offset + length;
        let mut out = Vec::with_capacity(length as usize);
        let mut pos = first * page_bytes;
        for page in pages.iter().flatten() {
            let page_end = pos + page.len() as u64;
            let (lo, hi) = (offset.max(pos), end.min(page_end));
            if lo < hi {
                out.extend_from_slice(&page[(lo - pos) as usize..(hi - pos) as usize]);
            }
            if (page.len() as u64) < page_bytes {
                break;
            }
            pos += page_bytes;
        }
        Ok(out)
    }
}

// ── HTTP range responses ──────────────────────────────────────────────────────

/// Turn an HTTP response to a `Range: bytes=offset-…` request into the
/// requested slice.
///
/// A `206` is the slice already. A `200` means the server ignored the range
/// header and sent the whole object, so cut the slice out of it; a body that
/// happens to be exactly `length` long is treated as the slice too, which is
/// what some object stores return for a range at the end of the file.
pub fn range_body(status: u16, body: Vec<u8>, offset: u64, length: u64, url: &str) -> Result<Vec<u8>, String> {
    if status != 200 && status != 206 {
        return Err(format!("HTTP {status} for {url}"));
    }
    let (offset, length) = (offset as usize, length as usize);
    if status == 206 || body.len() == length {
        Ok(body)
    } else if body.len() >= offset + length {
        Ok(body[offset..offset + length].to_vec())
    } else if body.len() > offset {
        Ok(body[offset..].to_vec())
    } else {
        Err(format!("HTTP 200 body ({} B) too short for range {offset}+{length}", body.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_content_is_passed_through() {
        assert_eq!(range_body(206, vec![1, 2, 3], 10, 3, "u").unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn full_body_is_sliced() {
        let body: Vec<u8> = (0..10).collect();
        assert_eq!(range_body(200, body.clone(), 2, 3, "u").unwrap(), vec![2, 3, 4]);
        assert_eq!(range_body(200, body.clone(), 8, 5, "u").unwrap(), vec![8, 9]);
        assert!(range_body(200, body, 10, 1, "u").is_err());
    }

    #[test]
    fn other_statuses_are_errors() {
        assert_eq!(range_body(404, vec![], 0, 1, "http://x").unwrap_err(), "HTTP 404 for http://x");
    }

    #[test]
    fn cache_key_is_a_url() {
        assert_eq!(cache_key("r2://bucket/dem.tif"), "https://sabre-meta-v1/r2/bucket/dem.tif");
        assert_eq!(cache_key("https://h/p.tif?a=1&b=2"), "https://sabre-meta-v1/https/h/p.tif_a=1_b=2");
    }

    // ── PagedReader ───────────────────────────────────────────────────────────

    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    /// An origin that serves a byte string and records every range asked of
    /// it. Cloning shares the log, so a test can keep one handle and give
    /// the other to a reader.
    #[derive(Clone)]
    struct Origin {
        bytes:    Rc<Vec<u8>>,
        requests: Rc<RefCell<Vec<(u64, u64)>>>,
    }

    #[async_trait(?Send)]
    impl RangeReader for Origin {
        async fn read_range(&self, offset: u64, length: u64) -> Result<Vec<u8>, String> {
            self.requests.borrow_mut().push((offset, length));
            let lo = (offset as usize).min(self.bytes.len());
            let hi = ((offset + length) as usize).min(self.bytes.len());
            Ok(self.bytes[lo..hi].to_vec())
        }
    }

    #[derive(Clone, Default)]
    struct MapCache(Rc<RefCell<HashMap<(String, u64), Page>>>);

    #[async_trait(?Send)]
    impl PageCache for MapCache {
        async fn get(&self, source: &str, page: u64) -> Option<Page> {
            self.0.borrow().get(&(source.to_string(), page)).cloned()
        }
        async fn put(&self, source: &str, page: u64, bytes: Page) {
            self.0.borrow_mut().insert((source.to_string(), page), bytes);
        }
    }

    fn origin(len: usize) -> Origin {
        Origin { bytes: Rc::new((0..len).map(|i| (i % 251) as u8).collect()), requests: Rc::default() }
    }

    fn paged(o: &Origin, c: &MapCache) -> PagedReader {
        PagedReader::with_page_size(Box::new(o.clone()), Box::new(c.clone()), "src", 10)
    }

    #[test]
    fn misses_are_widened_to_pages_and_hits_cost_nothing() {
        let (o, c) = (origin(100), MapCache::default());
        let r = paged(&o, &c);
        assert_eq!(pollster::block_on(r.read_range(13, 5)).unwrap(), o.bytes[13..18]);
        assert_eq!(o.requests.borrow().as_slice(), &[(10, 10)]);
        // Anything inside the cached page is served without a request.
        assert_eq!(pollster::block_on(r.read_range(10, 10)).unwrap(), o.bytes[10..20]);
        assert_eq!(o.requests.borrow().len(), 1);
    }

    #[test]
    fn only_missing_runs_are_fetched() {
        let (o, c) = (origin(100), MapCache::default());
        let r = paged(&o, &c);
        pollster::block_on(r.read_range(30, 10)).unwrap();          // page 3 cached
        let out = pollster::block_on(r.read_range(15, 40)).unwrap(); // pages 1..=5
        assert_eq!(out, o.bytes[15..55]);
        // Pages 1–2 and 4–5 are two runs either side of the cached page 3.
        assert_eq!(o.requests.borrow().as_slice(), &[(30, 10), (10, 20), (40, 20)]);
    }

    #[test]
    fn end_of_file_is_truncated_and_remembered() {
        let (o, c) = (origin(25), MapCache::default());
        let r = paged(&o, &c);
        assert_eq!(pollster::block_on(r.read_range(20, 30)).unwrap(), o.bytes[20..25]);
        assert_eq!(o.requests.borrow().as_slice(), &[(20, 30)]);
        // The short page and the empty ones past it are cached, so a read
        // past the end is answered without going back to the origin.
        assert_eq!(pollster::block_on(r.read_range(40, 5)).unwrap(), Vec::<u8>::new());
        assert_eq!(pollster::block_on(r.read_range(22, 2)).unwrap(), o.bytes[22..24]);
        assert_eq!(o.requests.borrow().len(), 1);
    }

    #[test]
    fn empty_reads_do_nothing() {
        let (o, c) = (origin(25), MapCache::default());
        let r = paged(&o, &c);
        assert_eq!(pollster::block_on(r.read_range(5, 0)).unwrap(), Vec::<u8>::new());
        assert!(o.requests.borrow().is_empty());
    }

    // ── Sharing fetches between readers ───────────────────────────────────────

    /// An origin that takes a turn before answering, so readers joined
    /// together really are in flight at once, and that can be told to fail.
    #[derive(Clone)]
    struct SlowOrigin {
        inner: Origin,
        fail:  Rc<RefCell<bool>>,
    }

    #[async_trait(?Send)]
    impl RangeReader for SlowOrigin {
        async fn read_range(&self, offset: u64, length: u64) -> Result<Vec<u8>, String> {
            yield_now().await;
            if *self.fail.borrow() {
                self.inner.requests.borrow_mut().push((offset, length));
                return Err("origin down".into());
            }
            self.inner.read_range(offset, length).await
        }
    }

    async fn yield_now() {
        let mut yielded = false;
        std::future::poll_fn(|cx| {
            if yielded {
                return std::task::Poll::Ready(());
            }
            yielded = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }).await
    }

    fn slow() -> (SlowOrigin, MapCache, InFlight) {
        let inner = Origin { bytes: Rc::new((0..100u8).collect()), requests: Rc::default() };
        (SlowOrigin { inner, fail: Rc::default() }, MapCache::default(), InFlight::default())
    }

    fn shared(o: &SlowOrigin, c: &MapCache, f: &InFlight) -> PagedReader {
        PagedReader::with_page_size(Box::new(o.clone()), Box::new(c.clone()), "src", 10).sharing(f.clone())
    }

    #[test]
    fn readers_in_flight_together_fetch_a_page_once() {
        let (o, c, f) = slow();
        let (a, b) = (shared(&o, &c, &f), shared(&o, &c, &f));
        let (ra, rb) = futures::executor::block_on(async {
            futures::join!(a.read_range(12, 5), b.read_range(12, 5))
        });
        assert_eq!(ra.unwrap(), (12..17).collect::<Vec<u8>>());
        assert_eq!(rb.unwrap(), (12..17).collect::<Vec<u8>>());
        assert_eq!(*o.inner.requests.borrow(), vec![(10, 10)]);
    }

    #[test]
    fn overlapping_reads_fetch_only_what_the_other_is_not() {
        let (o, c, f) = slow();
        let (a, b) = (shared(&o, &c, &f), shared(&o, &c, &f));
        let (ra, rb) = futures::executor::block_on(async {
            futures::join!(a.read_range(5, 20), b.read_range(15, 20))
        });
        assert_eq!(ra.unwrap(), (5..25).collect::<Vec<u8>>());
        assert_eq!(rb.unwrap(), (15..35).collect::<Vec<u8>>());
        // a has pages 0-2; b waits for 1-2 and fetches only 3.
        assert_eq!(*o.inner.requests.borrow(), vec![(0, 30), (30, 10)]);
    }

    #[test]
    fn without_sharing_both_fetch() {
        // The behaviour InFlight exists to prevent, pinned so the test above
        // is known to be measuring something.
        let (o, c, _) = slow();
        let a = PagedReader::with_page_size(Box::new(o.clone()), Box::new(c.clone()), "src", 10);
        let b = PagedReader::with_page_size(Box::new(o.clone()), Box::new(c.clone()), "src", 10);
        let _ = futures::executor::block_on(async { futures::join!(a.read_range(12, 5), b.read_range(12, 5)) });
        assert_eq!(o.inner.requests.borrow().len(), 2);
    }

    #[test]
    fn a_failed_fetch_leaves_the_waiter_to_fetch_for_itself() {
        let (o, c, f) = slow();
        let a = shared(&o, &c, &f);
        let b = shared(&o, &c, &f);
        *o.fail.borrow_mut() = true;
        let (ra, rb) = futures::executor::block_on(async {
            let a = async {
                let r = a.read_range(12, 5).await;
                // The origin recovers by the time b wakes up.
                *o.fail.borrow_mut() = false;
                r
            };
            futures::join!(a, b.read_range(12, 5))
        });
        assert!(ra.is_err());
        assert_eq!(rb.unwrap(), (12..17).collect::<Vec<u8>>());
    }

    #[test]
    fn a_dropped_reader_does_not_strand_the_one_waiting_on_it() {
        use futures::FutureExt;
        let (o, c, f) = slow();
        let (a, b) = (shared(&o, &c, &f), shared(&o, &c, &f));
        let rb = futures::executor::block_on(async {
            // a claims the page and starts fetching it; b finds the claim
            // and waits. Then a is abandoned mid-fetch.
            let mut fa = a.read_range(12, 5).boxed_local();
            assert!((&mut fa).now_or_never().is_none());
            let mut fb = b.read_range(12, 5).boxed_local();
            assert!((&mut fb).now_or_never().is_none(), "b should be waiting on a");
            drop(fa);
            fb.await
        });
        assert_eq!(rb.unwrap(), (12..17).collect::<Vec<u8>>());
        // a never got its answer; b fetched the page itself, once.
        assert_eq!(*o.inner.requests.borrow(), vec![(10, 10)]);
    }

}
