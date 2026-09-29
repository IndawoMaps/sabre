//! Fetching clip geometry by id instead of carrying it on every request.
//!
//! A farm block is 30–200 vertices. Sent as WKT on every tile request that
//! clips to it, that is a few kilobytes inbound for a few kilobytes of PNG
//! back — and for a small field the request can be larger than the response.
//! The endpoints still take `mask` and `polygon` directly; this adds a second
//! way to say which geometry, by naming one:
//!
//! ```text
//! GET /tiles/16/37414/39214?url=…&geometry_provider=blocks&geometry_id=19519
//! X-Geometry-Provider-Auth: Bearer …
//! ```
//!
//! `geometry_provider` is a key into a registry the operator configures, never
//! a URL. A caller-supplied URL would make sabre an open proxy, and one with
//! the caller's credential attached to it.
//!
//! # Caching
//!
//! Geometry barely changes and is asked for constantly, so it has to be
//! cached; the question is what the key is. Keying it on the credential as
//! well is obviously safe and quietly expensive: ten users looking at the same
//! 86-block farm produce 860 fetches of 86 distinct polygons, and hold all 860
//! in memory.
//!
//! So the two questions are cached separately:
//!
//! * *what is this geometry* — keyed `(provider, id)`, shared by everyone.
//! * *may this caller have it* — keyed `(provider, credential, id)`, a bool.
//!
//! The second needs the provider to answer it, which is what `access=` in the
//! provider config is for. A provider that can answer about several ids at
//! once (`{ids}` rather than `{id}`) turns that ten-user farm view into ten
//! access calls rather than 860.
//!
//! Without `access=` there is nothing to ask, and a shared geometry entry
//! would be a way for one caller to read another's data. Then the credential
//! goes back into the geometry key: correct, unshared, and visibly the
//! operator's choice rather than a silent default.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use sabre_core::mask::{parse_wkt_mask, Mask};
use sabre_core::twkb;
use sha2::{Digest, Sha256};

// ── Limits ────────────────────────────────────────────────────────────────────

/// Ids long enough to be a payload rather than a name are refused outright.
pub const MAX_ID_LEN: usize = 128;

/// How many ids one request may name.
///
/// This bounds the cold-cache cost: a provider without a batch access endpoint
/// is asked about each id, and every id is one geometry fetch. Warm, the whole
/// set is a single cache entry however many ids it names.
///
/// The number has to clear a real farm with room to spare — the block sets
/// this was built for run to 86 — so it is not a limit anyone meets by having
/// an ordinary amount of land.
pub const MAX_IDS: usize = 512;

pub const DEFAULT_TIMEOUT_MS: u64 = 5_000;
pub const DEFAULT_MAX_BYTES: usize = 1 << 20;

/// How long a resolved geometry is held. Field boundaries are redrawn between
/// seasons, not between requests, so this is long on purpose.
pub const DEFAULT_GEOMETRY_TTL_S: u64 = 3_600;

/// How long an access decision is held, and short on purpose: it is the window
/// in which a revoked grant still works. A minute is long enough to collapse
/// the burst of tile requests a single farm view produces into one access call,
/// and short enough that nobody has to think about it as a security boundary.
pub const DEFAULT_ACCESS_TTL_S: u64 = 60;

// ── Registry ──────────────────────────────────────────────────────────────────

/// One configured upstream, identified by the name callers pass as
/// `geometry_provider`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provider {
    pub id: String,
    /// Geometry URL with a single `{id}` placeholder.
    pub geometry: String,
    /// Access-check URL, with `{id}` for one id at a time or `{ids}` for a
    /// comma-separated batch. `None` means the credential is folded into the
    /// geometry cache key instead — see the module docs.
    pub access: Option<String>,
    /// Whether `X-Geometry-Provider-Auth` is passed on as `Authorization`.
    pub forward_auth: bool,
    pub timeout_ms: u64,
    pub max_bytes: usize,
    /// TWKB precision the cached copy is stored at, and what a provider is
    /// asked to serve. Decimal places of lon/lat: 6 is about 11 cm.
    pub precision: i8,
    pub geometry_ttl_s: u64,
    /// Kept separate from `geometry_ttl_s`, and much shorter. The two answers
    /// go stale on completely different clocks.
    pub access_ttl_s: u64,
}

impl Provider {
    pub fn batches(&self) -> bool {
        self.access.as_deref().is_some_and(|a| a.contains("{ids}"))
    }

    fn geometry_url(&self, id: &str) -> String {
        self.geometry.replace("{id}", id)
    }

    fn access_url(&self, ids: &[String]) -> Option<String> {
        let template = self.access.as_deref()?;
        Some(if template.contains("{ids}") {
            template.replace("{ids}", &ids.join(","))
        } else {
            template.replace("{id}", ids.first()?)
        })
    }
}

/// The providers this server will talk to. Empty unless configured, in which
/// case naming one is a 400 rather than a 404 into the void.
#[derive(Debug, Clone, Default)]
pub struct Registry(Arc<HashMap<String, Provider>>);

impl Registry {
    pub fn get(&self, id: &str) -> Option<&Provider> {
        self.0.get(id)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Provider names, sorted, for error messages and startup logging.
    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.0.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
    }

    /// Parse the configuration format, one provider per line:
    ///
    /// ```text
    /// blocks=https://api.example.com/blocks/{id}/geometry
    /// farms=https://internal/geom/{id} access=https://internal/access?ids={ids} timeout=2000
    /// ```
    ///
    /// Blank lines and `#` comments are skipped. Options after the URL are
    /// `key=value`, space-separated: `access`, `auth` (`yes`/`no`),
    /// `timeout` (ms), `max-bytes`.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let mut providers = HashMap::new();
        for (n, raw) in spec.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let at = |msg: String| format!("geometry provider line {}: {msg}", n + 1);

            let mut parts = line.split_whitespace();
            let head = parts.next().ok_or_else(|| at("empty".into()))?;
            let (id, geometry) = head.split_once('=')
                .ok_or_else(|| at(format!("expected <name>=<url>, got {head:?}")))?;
            validate_provider_name(id).map_err(&at)?;
            validate_template(geometry, "{id}").map_err(&at)?;

            let mut p = Provider {
                id: id.to_string(),
                geometry: geometry.to_string(),
                access: None,
                forward_auth: true,
                timeout_ms: DEFAULT_TIMEOUT_MS,
                max_bytes: DEFAULT_MAX_BYTES,
                precision: twkb::DEFAULT_PRECISION,
                geometry_ttl_s: DEFAULT_GEOMETRY_TTL_S,
                access_ttl_s: DEFAULT_ACCESS_TTL_S,
            };

            for opt in parts {
                let (key, value) = opt.split_once('=')
                    .ok_or_else(|| at(format!("expected <key>=<value>, got {opt:?}")))?;
                match key {
                    "access" => {
                        if !value.contains("{id}") && !value.contains("{ids}") {
                            return Err(at(format!(
                                "access={value:?} has no {{id}} or {{ids}} placeholder")));
                        }
                        validate_scheme(value).map_err(&at)?;
                        p.access = Some(value.to_string());
                    }
                    "auth" => p.forward_auth = match value {
                        "yes" | "true"  => true,
                        "no"  | "false" => false,
                        other => return Err(at(format!("auth={other:?}: expected yes or no"))),
                    },
                    "timeout" => p.timeout_ms = value.parse()
                        .map_err(|_| at(format!("timeout={value:?}: expected milliseconds")))?,
                    "max-bytes" => p.max_bytes = parse_bytes(value)
                        .ok_or_else(|| at(format!("max-bytes={value:?}: expected a size like 256K")))?,
                    "precision" => {
                        p.precision = value.parse()
                            .map_err(|_| at(format!("precision={value:?}: expected a number")))?;
                        if !(-7..=7).contains(&p.precision) {
                            return Err(at(format!("precision={value:?} is out of range")));
                        }
                    }
                    "geometry-ttl" => p.geometry_ttl_s = value.parse()
                        .map_err(|_| at(format!("geometry-ttl={value:?}: expected seconds")))?,
                    "access-ttl" => p.access_ttl_s = value.parse()
                        .map_err(|_| at(format!("access-ttl={value:?}: expected seconds")))?,
                    other => return Err(at(format!("unknown option {other:?}"))),
                }
            }

            if providers.insert(id.to_string(), p).is_some() {
                return Err(at(format!("provider {id:?} is defined twice")));
            }
        }
        Ok(Self(Arc::new(providers)))
    }
}

fn validate_provider_name(id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > 64 {
        return Err(format!("provider name {id:?} must be 1–64 characters"));
    }
    if !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return Err(format!("provider name {id:?} may only contain letters, digits, - and _"));
    }
    Ok(())
}

fn validate_scheme(url: &str) -> Result<(), String> {
    if url.starts_with("https://") || url.starts_with("http://") {
        Ok(())
    } else {
        Err(format!("{url:?} must be an http:// or https:// URL"))
    }
}

fn validate_template(url: &str, placeholder: &str) -> Result<(), String> {
    validate_scheme(url)?;
    match url.matches(placeholder).count() {
        1 => Ok(()),
        0 => Err(format!("{url:?} has no {placeholder} placeholder")),
        n => Err(format!("{url:?} has {placeholder} {n} times; it must appear once")),
    }
}

fn parse_bytes(text: &str) -> Option<usize> {
    let (digits, unit) = match text.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((i, _)) => text.split_at(i),
        None         => (text, ""),
    };
    let shift = match unit.to_ascii_uppercase().as_str() {
        "" | "B"   => 0,
        "K" | "KB" => 10,
        "M" | "MB" => 20,
        _ => return None,
    };
    digits.parse::<usize>().ok()?.checked_shl(shift)
}

// ── Caller input ──────────────────────────────────────────────────────────────

/// Accept only what is plainly a name.
///
/// The id is substituted into a URL the operator wrote, so anything that could
/// end a path segment, start a query, or climb out of one has to be refused
/// before it gets there — `..`, `/`, `?`, `#`, `%` and whitespace all do. What
/// is left covers the shapes ids actually take: integers, UUIDs, slugs, and
/// `ns:key` pairs.
pub fn validate_id(id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err("geometry_id is empty".into());
    }
    if id.len() > MAX_ID_LEN {
        return Err(format!("geometry_id is {} characters; the limit is {MAX_ID_LEN}", id.len()));
    }
    if let Some(bad) = id.chars().find(|c| {
        !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
    }) {
        return Err(format!(
            "geometry_id contains {bad:?}; only letters, digits, and - _ . : are allowed"));
    }
    if id.contains("..") {
        return Err("geometry_id contains '..'".into());
    }
    Ok(())
}

/// Split and validate the `geometry_id` parameter, which may name several.
pub fn parse_ids(raw: &str) -> Result<Vec<String>, String> {
    let ids: Vec<&str> = raw.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    if ids.is_empty() {
        return Err("geometry_id is empty".into());
    }
    if ids.len() > MAX_IDS {
        return Err(format!(
            "geometry_id names {} geometries; the limit is {MAX_IDS}. Clip to a smaller \
             set, or have the provider expose the group under one id.", ids.len()));
    }
    for id in &ids {
        validate_id(id)?;
    }
    let mut out: Vec<String> = ids.into_iter().map(str::to_string).collect();
    // Sorted and deduplicated so the same set written two ways is one cache
    // entry, and so a caller cannot multiply the work by repeating an id.
    out.sort();
    out.dedup();
    Ok(out)
}

// ── Cache keys ────────────────────────────────────────────────────────────────

/// A stable stand-in for the caller's credential.
///
/// SHA-256 rather than a cheap hash because this separates one caller's cache
/// entries from another's: a hash an attacker can collide is a way to read
/// someone else's geometry. Truncated to 128 bits, which is far past what
/// finding a collision here would be worth. The credential itself is never
/// what is held, so the cache is not a place tokens accumulate.
pub fn credential_fingerprint(auth: Option<&str>) -> String {
    match auth {
        // Distinct from any hash, so "no credential" cannot collide with one.
        None => "anon".to_string(),
        Some(secret) => {
            let digest = Sha256::digest(secret.as_bytes());
            digest[..16].iter().fold(String::with_capacity(32), |mut s, b| {
                use std::fmt::Write;
                let _ = write!(s, "{b:02x}");
                s
            })
        }
    }
}

/// Key for the geometry itself.
///
/// Shared across callers when the provider can answer access questions
/// separately; otherwise the credential is folded in, because then being
/// allowed to fetch it is the only evidence of being allowed to have it.
pub fn geometry_key(p: &Provider, ids: &[String], auth: Option<&str>) -> String {
    let ids = ids.join(",");
    match p.access {
        Some(_) => format!("geom/{}/{ids}", p.id),
        None    => format!("geom/{}/{}/{ids}", p.id, credential_fingerprint(auth)),
    }
}

/// Key for "may this caller have this geometry".
pub fn access_key(p: &Provider, ids: &[String], auth: Option<&str>) -> String {
    format!("access/{}/{}/{}", p.id, credential_fingerprint(auth), ids.join(","))
}

// ── Runtime hooks ─────────────────────────────────────────────────────────────

/// One HTTP GET, supplied by whichever runtime is compiled in.
#[async_trait(?Send)]
pub trait Fetcher {
    /// Returns the status, the content type if one was sent, and at most
    /// `max_bytes` of body. A body longer than that is an error, not a
    /// truncation: half a polygon parses into a different shape rather than
    /// failing.
    async fn get(&self, url: &str, auth: Option<&str>, timeout_ms: u64, max_bytes: usize)
        -> Result<(u16, Option<String>, Vec<u8>), String>;
}

/// Where resolved geometry and access decisions live between requests.
///
/// Bytes rather than text, because what is stored is canonical TWKB: a whole
/// farm is 7 KB of it against 89 KB of WKT, and the Workers Cache API only
/// deals in bodies anyway. Access decisions are the ASCII `allow`/`deny` in
/// the same store.
#[async_trait(?Send)]
pub trait Cache {
    async fn get(&self, key: &str) -> Option<Arc<[u8]>>;
    async fn put(&self, key: &str, value: Arc<[u8]>, ttl_s: u64);
}

/// A cache that stores nothing, for runtimes with nowhere to put it.
pub struct NoCache;

#[async_trait(?Send)]
impl Cache for NoCache {
    async fn get(&self, _key: &str) -> Option<Arc<[u8]>> { None }
    async fn put(&self, _key: &str, _value: Arc<[u8]>, _ttl_s: u64) {}
}

// ── Errors ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The request is wrong: unknown provider, malformed id, both a literal
    /// geometry and a provider reference.
    BadRequest(String),
    /// The provider said this caller may not have this geometry.
    Forbidden(String),
    /// The provider does not have it.
    NotFound(String),
    /// The provider could not be reached, or answered with something
    /// unusable. Never cached: a provider having a bad minute must not become
    /// a denial that outlives it.
    Upstream(String),
}

impl Error {
    pub fn message(&self) -> &str {
        match self {
            Self::BadRequest(m) | Self::Forbidden(m) | Self::NotFound(m) | Self::Upstream(m) => m,
        }
    }
}

// ── GeoJSON ───────────────────────────────────────────────────────────────────

/// Turn whatever the provider returned into a [`Mask`].
///
/// TWKB is what a provider should serve, and it is checked for first — before
/// any attempt to read the body as text, because a TWKB polygon at precision 6
/// begins `0xC3 0x00`, which is not valid UTF-8 and would otherwise be
/// reported as an encoding problem rather than a format it simply is.
///
/// GeoJSON and WKT are still accepted, since not every provider will emit TWKB
/// and the branch costs one byte comparison. A FeatureCollection becomes one
/// MultiPolygon rather than its first feature: asking for a farm and being
/// silently clipped to one of its blocks is the kind of wrong answer that
/// looks right.
pub fn to_mask(body: &[u8], content_type: Option<&str>) -> Result<Mask, String> {
    let declared_twkb = content_type
        .is_some_and(|ct| ct.split(';').next().unwrap_or("").trim() == twkb::CONTENT_TYPE);
    if declared_twkb || twkb::looks_like_twkb(body) {
        return twkb::decode_mask(body);
    }
    parse_wkt_mask(&to_wkt(body)?)
}

/// Convert a textual provider response into the WKT `sabre-core` parses.
pub fn to_wkt(body: &[u8]) -> Result<String, String> {
    let text = std::str::from_utf8(body).map_err(|_| {
        "response is neither TWKB nor valid UTF-8, so it is not a geometry this reader knows"
            .to_string()
    })?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("response is empty".into());
    }
    // Already WKT: hand it to the same parser the `mask` parameter uses, so
    // the two routes cannot disagree about what is valid.
    let upper = trimmed.get(..13).unwrap_or(trimmed).to_ascii_uppercase();
    if upper.starts_with("POLYGON") || upper.starts_with("MULTIPOLYGON") {
        sabre_core::mask::parse_wkt_mask(trimmed)?;
        return Ok(trimmed.to_string());
    }

    let json: serde_json::Value = serde_json::from_str(trimmed)
        .map_err(|e| format!("response is neither WKT nor JSON: {e}"))?;
    let mut polygons = Vec::new();
    collect(&json, &mut polygons, 0)?;
    if polygons.is_empty() {
        return Err("response contains no Polygon or MultiPolygon".into());
    }
    Ok(if polygons.len() == 1 {
        format!("POLYGON{}", polygons.remove(0))
    } else {
        format!("MULTIPOLYGON({})", polygons.join(","))
    })
}

/// Walk a GeoJSON document, appending each polygon as a parenthesised ring
/// list. Depth-limited: a deeply nested document is a payload, not a geometry.
fn collect(v: &serde_json::Value, out: &mut Vec<String>, depth: usize) -> Result<(), String> {
    if depth > 8 {
        return Err("GeoJSON is nested too deeply".into());
    }
    match v.get("type").and_then(|t| t.as_str()) {
        Some("FeatureCollection") => {
            let features = v.get("features").and_then(|f| f.as_array())
                .ok_or("FeatureCollection has no features array")?;
            for f in features {
                collect(f, out, depth + 1)?;
            }
            Ok(())
        }
        Some("Feature") => {
            let g = v.get("geometry").ok_or("Feature has no geometry")?;
            if g.is_null() {
                return Ok(()); // a null-geometry feature is legal and contributes nothing
            }
            collect(g, out, depth + 1)
        }
        Some("GeometryCollection") => {
            let gs = v.get("geometries").and_then(|g| g.as_array())
                .ok_or("GeometryCollection has no geometries array")?;
            for g in gs {
                collect(g, out, depth + 1)?;
            }
            Ok(())
        }
        Some("Polygon") => {
            out.push(rings(v.get("coordinates").ok_or("Polygon has no coordinates")?)?);
            Ok(())
        }
        Some("MultiPolygon") => {
            let polys = v.get("coordinates").and_then(|c| c.as_array())
                .ok_or("MultiPolygon has no coordinates")?;
            for p in polys {
                out.push(rings(p)?);
            }
            Ok(())
        }
        Some(other) => Err(format!("{other} cannot be used as a clip geometry")),
        None => Err("GeoJSON object has no type".into()),
    }
}

/// `[[[x,y],…],…]` -> `((x y,…),…)`
fn rings(coords: &serde_json::Value) -> Result<String, String> {
    let rings = coords.as_array().ok_or("coordinates is not an array")?;
    if rings.is_empty() {
        return Err("polygon has no rings".into());
    }
    let mut parts = Vec::with_capacity(rings.len());
    for ring in rings {
        let points = ring.as_array().ok_or("ring is not an array")?;
        if points.len() < 4 {
            return Err(format!("ring has {} positions; a closed ring needs at least 4", points.len()));
        }
        let mut wkt = String::with_capacity(points.len() * 24);
        for (i, point) in points.iter().enumerate() {
            let p = point.as_array().ok_or("position is not an array")?;
            let (x, y) = (
                p.first().and_then(serde_json::Value::as_f64).ok_or("position has no x")?,
                p.get(1).and_then(serde_json::Value::as_f64).ok_or("position has no y")?,
            );
            if !x.is_finite() || !y.is_finite() {
                return Err("position is not a finite coordinate".into());
            }
            if i > 0 {
                wkt.push(',');
            }
            use std::fmt::Write;
            let _ = write!(wkt, "{x} {y}");
        }
        parts.push(format!("({wkt})"));
    }
    Ok(format!("({})", parts.join(",")))
}

// ── Resolution ────────────────────────────────────────────────────────────────

/// Turn `geometry_provider` + `geometry_id` into WKT.
///
/// `ids` must have come from [`parse_ids`], which sorts and deduplicates them.
/// Both cache keys are built from the list as given, so an unnormalised one
/// would key the same request two ways.
///
/// The order matters. Access is settled before the geometry cache is read, so
/// a shared entry is never handed to a caller who has not been cleared for it
/// in this TTL window — the cheap check gates the shared answer rather than
/// the other way round.
pub async fn resolve(
    provider: &Provider,
    ids: &[String],
    auth: Option<&str>,
    fetcher: &dyn Fetcher,
    cache: &dyn Cache,
) -> Result<Arc<Mask>, Error> {
    debug_assert!(ids.windows(2).all(|w| w[0] < w[1]),
                  "ids must be sorted and deduplicated; use parse_ids");
    let auth = provider.forward_auth.then_some(auth).flatten();

    if provider.access.is_some() {
        check_access(provider, ids, auth, fetcher, cache).await?;
    }

    let key = geometry_key(provider, ids, auth);
    if let Some(hit) = cache.get(&key).await {
        // A cache entry is always canonical TWKB, whatever the provider sent.
        return twkb::decode_mask(&hit)
            .map(Arc::new)
            .map_err(|e| Error::Upstream(format!("cached geometry for {} is unreadable: {e}",
                                                 ids.join(", "))));
    }

    // Several geometries clip as one shape, so their rings are concatenated
    // into one mask. `sabre-core` tests a point against every polygon, which
    // makes this a union and not an intersection.
    let mut rings = Vec::new();
    for id in ids {
        let url = provider.geometry_url(id);
        let (status, content_type, body) = fetcher
            .get(&url, auth, provider.timeout_ms, provider.max_bytes)
            .await
            .map_err(|e| Error::Upstream(format!("{}: {e}", provider.id)))?;
        match status {
            200..=299 => {}
            401 | 403 => return Err(Error::Forbidden(format!(
                "{} refused access to geometry {id}", provider.id))),
            404 => return Err(Error::NotFound(format!(
                "{} has no geometry {id}", provider.id))),
            other => return Err(Error::Upstream(format!(
                "{} answered {other} for geometry {id}", provider.id))),
        }
        let mask = to_mask(&body, content_type.as_deref())
            .map_err(|e| Error::Upstream(format!("{} returned {id}: {e}", provider.id)))?;
        rings.extend_from_slice(mask.rings());
    }
    if rings.is_empty() {
        return Err(Error::Upstream(format!(
            "{} returned nothing clippable for {}", provider.id, ids.join(", "))));
    }

    let mask = Mask::from_rings(rings);
    // Stored as TWKB at the precision providers are asked for, so one farm is
    // 7 KB in the cache rather than 89 KB, and so a GeoJSON provider and a
    // TWKB one leave identical entries behind.
    match twkb::encode_mask(&mask, provider.precision) {
        Ok(bytes) => cache.put(&key, Arc::from(bytes.as_slice()), provider.geometry_ttl_s).await,
        Err(e) => return Err(Error::Upstream(format!("cannot store geometry: {e}"))),
    }
    Ok(Arc::new(mask))
}

async fn check_access(
    provider: &Provider,
    ids: &[String],
    auth: Option<&str>,
    fetcher: &dyn Fetcher,
    cache: &dyn Cache,
) -> Result<(), Error> {
    let key = access_key(provider, ids, auth);
    match cache.get(&key).await.as_deref() {
        Some(b"allow") => return Ok(()),
        Some(_) => return Err(Error::Forbidden(format!(
            "{} refused access to {}", provider.id, ids.join(", ")))),
        None => {}
    }

    // A provider that can only answer about one id at a time is asked about
    // each; one that takes {ids} is asked once, which is the whole reason for
    // supporting the batch form.
    let batches: Vec<Vec<String>> = if provider.batches() {
        vec![ids.to_vec()]
    } else {
        ids.iter().map(|i| vec![i.clone()]).collect()
    };

    for batch in &batches {
        let url = provider.access_url(batch)
            .ok_or_else(|| Error::Upstream(format!("{}: no access URL", provider.id)))?;
        let (status, _, body) = fetcher
            .get(&url, auth, provider.timeout_ms, provider.max_bytes)
            .await
            .map_err(|e| Error::Upstream(format!("{} access check: {e}", provider.id)))?;

        let allowed = match status {
            200..=299 => allows(&body, batch),
            401 | 403 | 404 => false,
            // Fail closed, and do not remember it. A provider having a bad
            // minute must not turn into a denial that outlives the minute.
            other => return Err(Error::Upstream(format!(
                "{} answered {other} to an access check", provider.id))),
        };
        if !allowed {
            cache.put(&key, Arc::from(&b"deny"[..]), provider.access_ttl_s).await;
            return Err(Error::Forbidden(format!(
                "{} refused access to {}", provider.id, batch.join(", "))));
        }
    }

    cache.put(&key, Arc::from(&b"allow"[..]), provider.access_ttl_s).await;
    Ok(())
}

/// Read an access response.
///
/// An empty body with a 2xx means yes — the cheapest thing a provider can
/// implement is a bare 200/403. A JSON body may instead list which ids are
/// allowed, and then every id asked about has to be in it: a batch answer that
/// covers only some of them is a denial for the rest, not a pass for all.
fn allows(body: &[u8], asked: &[String]) -> bool {
    let text = std::str::from_utf8(body).unwrap_or("").trim();
    if text.is_empty() {
        return true;
    }
    let Ok(json) = serde_json::from_str::<serde_json::Value>(text) else {
        return true; // a 2xx with a body we cannot read is still a 2xx
    };
    if let Some(ok) = json.as_bool() {
        return ok;
    }
    if let Some(ok) = json.get("allowed").and_then(serde_json::Value::as_bool) {
        return ok;
    }
    let listed = json.get("allowed").and_then(serde_json::Value::as_array)
        .or_else(|| json.get("ids").and_then(serde_json::Value::as_array))
        .or_else(|| json.as_array());
    match listed {
        Some(list) => {
            let have: Vec<&str> = list.iter().filter_map(serde_json::Value::as_str).collect();
            asked.iter().all(|id| have.iter().any(|h| *h == id))
        }
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(line: &str) -> Provider {
        Registry::parse(line).unwrap().get(line.split('=').next().unwrap()).unwrap().clone()
    }

    // ── Test doubles ─────────────────────────────────────────────────────

    use std::cell::RefCell;

    /// status, content type, body.
    type Route = (u16, Option<String>, String);

    #[derive(Default)]
    struct FakeUpstream {
        /// url -> (status, content type, body)
        routes: HashMap<String, Route>,
        /// Every (url, credential) asked for, in order.
        calls: RefCell<Vec<(String, Option<String>)>>,
    }

    impl FakeUpstream {
        fn route(mut self, url: &str, status: u16, body: &str) -> Self {
            self.routes.insert(url.to_string(), (status, None, body.to_string()));
            self
        }

        /// A provider serving TWKB, as the contract asks for.
        fn route_twkb(mut self, url: &str, wkt: &str) -> Self {
            let bytes = twkb::encode_mask(&parse_wkt_mask(wkt).unwrap(),
                                          twkb::DEFAULT_PRECISION).unwrap();
            self.routes.insert(url.to_string(), (
                200,
                Some(twkb::CONTENT_TYPE.to_string()),
                // Carried as text only because this double stores strings; the
                // bytes round-trip through latin-1 unchanged.
                bytes.iter().map(|&b| b as char).collect(),
            ));
            self
        }
        fn urls(&self) -> Vec<String> {
            self.calls.borrow().iter().map(|(u, _)| u.clone()).collect()
        }
        fn creds(&self) -> Vec<Option<String>> {
            self.calls.borrow().iter().map(|(_, a)| a.clone()).collect()
        }
    }

    #[async_trait(?Send)]
    impl Fetcher for FakeUpstream {
        async fn get(&self, url: &str, auth: Option<&str>, _t: u64, _m: usize)
            -> Result<(u16, Option<String>, Vec<u8>), String> {
            self.calls.borrow_mut().push((url.to_string(), auth.map(str::to_string)));
            match self.routes.get(url) {
                Some((status, ct, body)) => Ok((
                    *status, ct.clone(),
                    if ct.as_deref() == Some(twkb::CONTENT_TYPE) {
                        body.chars().map(|c| c as u8).collect()
                    } else {
                        body.clone().into_bytes()
                    },
                )),
                None => Ok((404, None, Vec::new())),
            }
        }
    }

    #[derive(Default)]
    struct MapCache(RefCell<HashMap<String, Cached>>);

    /// The stored bytes and the TTL they were put with.
    type Cached = (Arc<[u8]>, u64);

    #[async_trait(?Send)]
    impl Cache for MapCache {
        async fn get(&self, key: &str) -> Option<Arc<[u8]>> {
            self.0.borrow().get(key).map(|(v, _)| Arc::clone(v))
        }
        async fn put(&self, key: &str, value: Arc<[u8]>, ttl_s: u64) {
            self.0.borrow_mut().insert(key.to_string(), (value, ttl_s));
        }
    }

    impl MapCache {
        fn ttl_of(&self, prefix: &str) -> Option<u64> {
            self.0.borrow().iter().find(|(k, _)| k.starts_with(prefix)).map(|(_, (_, t))| *t)
        }
        fn bytes_of(&self, prefix: &str) -> Option<Vec<u8>> {
            self.0.borrow().iter().find(|(k, _)| k.starts_with(prefix))
                .map(|(_, (v, _))| v.to_vec())
        }
    }

    const SQUARE: &str = r#"{"type":"Polygon","coordinates":[[[0,0],[1,0],[1,1],[0,1],[0,0]]]}"#;

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        pollster::block_on(f)
    }

    // ── GeoJSON ──────────────────────────────────────────────────────────

    #[test]
    fn a_bare_geometry_becomes_a_polygon() {
        assert_eq!(to_wkt(SQUARE.as_bytes()).unwrap(),
                   "POLYGON((0 0,1 0,1 1,0 1,0 0))");
    }

    #[test]
    fn a_feature_is_unwrapped() {
        let doc = format!(r#"{{"type":"Feature","properties":{{"name":"x"}},"geometry":{}}}"#, SQUARE);
        assert_eq!(to_wkt(doc.as_bytes()).unwrap(), "POLYGON((0 0,1 0,1 1,0 1,0 0))");
    }

    #[test]
    fn a_collection_becomes_every_feature_not_the_first() {
        let doc = format!(
            r#"{{"type":"FeatureCollection","features":[
                 {{"type":"Feature","geometry":{0}}},
                 {{"type":"Feature","geometry":{0}}}]}}"#, SQUARE);
        let wkt = to_wkt(doc.as_bytes()).unwrap();
        assert!(wkt.starts_with("MULTIPOLYGON("), "{wkt}");
        assert_eq!(wkt.matches("0 0,1 0").count(), 2, "both features must survive: {wkt}");
    }

    #[test]
    fn interior_rings_survive_the_round_trip() {
        let doc = r#"{"type":"Polygon","coordinates":[
            [[0,0],[10,0],[10,10],[0,10],[0,0]],
            [[2,2],[3,2],[3,3],[2,3],[2,2]]]}"#;
        let wkt = to_wkt(doc.as_bytes()).unwrap();
        let mask = sabre_core::mask::parse_wkt_mask(&wkt).unwrap();
        assert!(mask.contains(1.0, 1.0), "inside the exterior: {wkt}");
        assert!(!mask.contains(2.5, 2.5), "inside the hole: {wkt}");
    }

    #[test]
    fn wkt_is_passed_through_but_still_parsed() {
        assert_eq!(to_wkt(b"POLYGON((0 0,1 0,1 1,0 1,0 0))").unwrap(),
                   "POLYGON((0 0,1 0,1 1,0 1,0 0))");
        assert!(to_wkt(b"POLYGON((this is not wkt))").is_err());
    }

    #[test]
    fn unusable_responses_are_errors_rather_than_empty_masks() {
        for bad in [
            "",
            "{}",
            r#"{"type":"Point","coordinates":[0,0]}"#,
            r#"{"type":"Polygon","coordinates":[[[0,0],[1,0],[0,0]]]}"#,   // ring too short
            r#"{"type":"Polygon","coordinates":[[[0,null],[1,0],[1,1],[0,0]]]}"#,
            "<html>not found</html>",
        ] {
            assert!(to_wkt(bad.as_bytes()).is_err(), "{bad:?} should be an error");
        }
    }

    // ── Resolution and caching ───────────────────────────────────────────

    #[test]
    fn the_geometry_is_fetched_once_and_then_served_from_cache() {
        let p = provider("b=https://x/{id}");
        let up = FakeUpstream::default().route("https://x/19519", 200, SQUARE);
        let cache = MapCache::default();
        let ids = vec!["19519".to_string()];
        block_on(async {
            let a = resolve(&p, &ids, Some("t"), &up, &cache).await.unwrap();
            let b = resolve(&p, &ids, Some("t"), &up, &cache).await.unwrap();
            assert_eq!(a.size(), b.size());
            assert!(a.contains(0.5, 0.5) && b.contains(0.5, 0.5));
            assert_eq!(up.urls().len(), 1, "second request must not reach the provider");
        });
    }

    #[test]
    fn two_callers_share_one_fetch_when_access_can_be_checked_separately() {
        let p = provider("b=https://x/{id} access=https://x/{id}/ok");
        let up = FakeUpstream::default()
            .route("https://x/19519", 200, SQUARE)
            .route("https://x/19519/ok", 200, "");
        let cache = MapCache::default();
        let ids = vec!["19519".to_string()];
        block_on(async {
            resolve(&p, &ids, Some("alice"), &up, &cache).await.unwrap();
            resolve(&p, &ids, Some("bob"), &up, &cache).await.unwrap();
            let geometry_fetches = up.urls().iter().filter(|u| *u == "https://x/19519").count();
            let access_checks = up.urls().iter().filter(|u| u.ends_with("/ok")).count();
            assert_eq!(geometry_fetches, 1, "the polygon is the same polygon for both");
            assert_eq!(access_checks, 2, "but each caller is cleared on their own");
        });
    }

    #[test]
    fn without_an_access_endpoint_each_caller_fetches_its_own() {
        let p = provider("b=https://x/{id}");
        let up = FakeUpstream::default().route("https://x/19519", 200, SQUARE);
        let cache = MapCache::default();
        let ids = vec!["19519".to_string()];
        block_on(async {
            resolve(&p, &ids, Some("alice"), &up, &cache).await.unwrap();
            resolve(&p, &ids, Some("bob"), &up, &cache).await.unwrap();
            assert_eq!(up.urls().len(), 2,
                       "with nothing to ask about access, bob must not read alice's entry");
        });
    }

    #[test]
    fn a_cleared_caller_does_not_re_check_within_the_ttl() {
        let p = provider("b=https://x/{id} access=https://x/{id}/ok");
        let up = FakeUpstream::default()
            .route("https://x/19519", 200, SQUARE)
            .route("https://x/19519/ok", 200, "");
        let cache = MapCache::default();
        let ids = vec!["19519".to_string()];
        block_on(async {
            for _ in 0..5 {
                resolve(&p, &ids, Some("alice"), &up, &cache).await.unwrap();
            }
            assert_eq!(up.urls().len(), 2, "one access check and one fetch, then nothing");
            assert_eq!(cache.ttl_of("access/"), Some(DEFAULT_ACCESS_TTL_S));
            assert_eq!(cache.ttl_of("geom/"), Some(DEFAULT_GEOMETRY_TTL_S));
        });
    }

    #[test]
    fn a_refused_caller_never_reaches_the_shared_entry() {
        let p = provider("b=https://x/{id} access=https://x/{id}/ok");
        let up = FakeUpstream::default()
            .route("https://x/19519", 200, SQUARE)
            .route("https://x/19519/ok", 200, "");
        let cache = MapCache::default();
        let ids = vec!["19519".to_string()];
        block_on(async {
            // alice is cleared, so the geometry is now in the shared cache.
            resolve(&p, &ids, Some("alice"), &up, &cache).await.unwrap();
            // mallory is not: /ok is routed only for alice's check URL, and
            // the fake answers 404 for anything else.
            let err = resolve(&p, &ids, Some("mallory"), &FakeUpstream::default(), &cache)
                .await.unwrap_err();
            assert!(matches!(err, Error::Forbidden(_)), "{err:?}");
        });
    }

    #[test]
    fn a_denial_is_remembered_briefly_and_an_outage_is_not_remembered_at_all() {
        let p = provider("b=https://x/{id} access=https://x/{id}/ok");
        let ids = vec!["19519".to_string()];

        let denied = FakeUpstream::default().route("https://x/19519/ok", 403, "");
        let cache = MapCache::default();
        block_on(async {
            assert!(matches!(resolve(&p, &ids, Some("m"), &denied, &cache).await,
                             Err(Error::Forbidden(_))));
            assert!(matches!(resolve(&p, &ids, Some("m"), &denied, &cache).await,
                             Err(Error::Forbidden(_))));
            assert_eq!(denied.urls().len(), 1, "a denial is cached, briefly");
            assert_eq!(cache.ttl_of("access/"), Some(DEFAULT_ACCESS_TTL_S));
        });

        let broken = FakeUpstream::default().route("https://x/19519/ok", 500, "");
        let cache = MapCache::default();
        block_on(async {
            for _ in 0..3 {
                assert!(matches!(resolve(&p, &ids, Some("m"), &broken, &cache).await,
                                 Err(Error::Upstream(_))));
            }
            assert_eq!(broken.urls().len(), 3,
                       "a bad minute at the provider must not become a lasting denial");
            assert!(cache.ttl_of("access/").is_none());
        });
    }

    #[test]
    fn a_batch_endpoint_clears_a_whole_farm_in_one_call() {
        let p = provider("b=https://x/{id} access=https://x/allowed?ids={ids}");
        // Through parse_ids, as a request would be: the ids arrive normalised,
        // which is what makes the cache key independent of how they were written.
        let ids = parse_ids("1,2,3,4,5,6,7,8,9,10").unwrap();
        let mut up = FakeUpstream::default()
            .route("https://x/allowed?ids=1,10,2,3,4,5,6,7,8,9", 200, "");
        for i in 1..=10 {
            up = up.route(&format!("https://x/{i}"), 200, SQUARE);
        }
        let cache = MapCache::default();
        block_on(async {
            let mask = resolve(&p, &ids, Some("alice"), &up, &cache).await.unwrap();
            let checks = up.urls().iter().filter(|u| u.contains("allowed")).count();
            assert_eq!(checks, 1, "one call for ten blocks, which is the point");
            assert_eq!(mask.size().0, 10, "ten blocks clip as one shape");
            // And the cache holds one compact TWKB entry for the set, not ten
            // copies of anything.
            let stored = cache.bytes_of("geom/").expect("geometry cached");
            assert!(twkb::looks_like_twkb(&stored), "cache entry should be TWKB");
            assert_eq!(twkb::decode_mask(&stored).unwrap().size().0, 10);
        });
    }

    #[test]
    fn a_partial_batch_answer_is_a_denial_for_the_rest() {
        let p = provider("b=https://x/{id} access=https://x/allowed?ids={ids}");
        let ids = vec!["1".to_string(), "2".to_string()];
        let up = FakeUpstream::default()
            .route("https://x/allowed?ids=1,2", 200, r#"{"allowed":["1"]}"#)
            .route("https://x/1", 200, SQUARE)
            .route("https://x/2", 200, SQUARE);
        let cache = MapCache::default();
        block_on(async {
            let err = resolve(&p, &ids, Some("a"), &up, &cache).await.unwrap_err();
            assert!(matches!(err, Error::Forbidden(_)),
                    "being cleared for one of two is not being cleared: {err:?}");
        });
    }

    #[test]
    fn the_credential_goes_to_the_provider_only_when_configured_to() {
        let ids = vec!["1".to_string()];
        let forwarded = provider("b=https://x/{id}");
        let withheld  = provider("b=https://x/{id} auth=no");
        for (p, want) in [(forwarded, Some("Bearer t".to_string())), (withheld, None)] {
            let up = FakeUpstream::default().route("https://x/1", 200, SQUARE);
            let cache = MapCache::default();
            block_on(async {
                resolve(&p, &ids, Some("Bearer t"), &up, &cache).await.unwrap();
                assert_eq!(up.creds(), vec![want.clone()]);
            });
        }
    }

    #[test]
    fn provider_statuses_map_to_distinguishable_errors() {
        let p = provider("b=https://x/{id}");
        let ids = vec!["1".to_string()];
        for (status, want) in [
            (403u16, "forbidden"), (401, "forbidden"),
            (404, "notfound"), (500, "upstream"), (503, "upstream"),
        ] {
            let up = FakeUpstream::default().route("https://x/1", status, "");
            let cache = MapCache::default();
            block_on(async {
                let err = resolve(&p, &ids, Some("t"), &up, &cache).await.unwrap_err();
                let got = match err {
                    Error::Forbidden(_) => "forbidden",
                    Error::NotFound(_)  => "notfound",
                    Error::Upstream(_)  => "upstream",
                    Error::BadRequest(_) => "badrequest",
                };
                assert_eq!(got, want, "status {status}");
            });
        }
    }

    // ── TWKB, the format providers are asked for ─────────────────────────

    #[test]
    fn a_twkb_provider_is_read_without_going_near_text() {
        let p = provider("b=https://x/{id}");
        let up = FakeUpstream::default()
            .route_twkb("https://x/1", "POLYGON((0 0,1 0,1 1,0 1,0 0))");
        let cache = MapCache::default();
        block_on(async {
            let mask = resolve(&p, &["1".to_string()], None, &up, &cache).await.unwrap();
            assert!(mask.contains(0.5, 0.5));
            assert!(!mask.contains(2.0, 2.0));
        });
    }

    #[test]
    fn a_twkb_body_is_not_reported_as_an_encoding_problem() {
        // TWKB for a polygon at p6 starts 0xC3 0x00, which is not valid UTF-8.
        // Read as text it fails, and the message has to point at the format
        // rather than sending someone to look at charsets.
        let bytes = twkb::encode_mask(&parse_wkt_mask("POLYGON((0 0,1 0,1 1,0 1,0 0))").unwrap(),
                                      twkb::DEFAULT_PRECISION).unwrap();
        assert!(std::str::from_utf8(&bytes).is_err(), "precondition: not valid UTF-8");
        to_mask(&bytes, None).expect("sniffed as TWKB");
        to_mask(&bytes, Some(twkb::CONTENT_TYPE)).expect("declared as TWKB");

        let err = to_wkt(&bytes).unwrap_err();
        assert!(err.contains("TWKB"), "the text path should name the format: {err}");
    }

    #[test]
    fn whatever_the_provider_sends_is_cached_as_twkb() {
        let square_wkt = "POLYGON((0 0,1 0,1 1,0 1,0 0))";
        let flavours: Vec<(&str, FakeUpstream)> = vec![
            ("geojson", FakeUpstream::default().route("https://x/1", 200, SQUARE)),
            ("wkt",     FakeUpstream::default().route("https://x/1", 200, square_wkt)),
            ("twkb",    FakeUpstream::default().route_twkb("https://x/1", square_wkt)),
        ];
        let p = provider("b=https://x/{id}");
        let mut stored = Vec::new();
        for (name, up) in flavours {
            let cache = MapCache::default();
            block_on(async {
                resolve(&p, &["1".to_string()], None, &up, &cache).await.unwrap();
                let bytes = cache.bytes_of("geom/").expect("cached");
                assert!(twkb::looks_like_twkb(&bytes), "{name} left a non-TWKB entry");
                stored.push(bytes);
            });
        }
        assert_eq!(stored[0], stored[1], "GeoJSON and WKT must canonicalise the same");
        assert_eq!(stored[1], stored[2], "and so must TWKB");
    }

    #[test]
    fn the_cached_form_is_far_smaller_than_the_geojson_it_came_from() {
        // A ring the size of a real block, as a provider would send it.
        let ring: Vec<String> = (0..64)
            .map(|i| {
                let a = std::f64::consts::TAU * f64::from(i) / 64.0;
                format!("[{:.13},{:.13}]", 25.53 + 0.004 * a.cos(), -33.35 + 0.004 * a.sin())
            })
            .collect();
        let geojson = format!(
            r#"{{"type":"Polygon","coordinates":[[{},{}]]}}"#, ring.join(","), ring[0]);
        let p = provider("b=https://x/{id}");
        let up = FakeUpstream::default().route("https://x/1", 200, &geojson);
        let cache = MapCache::default();
        block_on(async {
            resolve(&p, &["1".to_string()], None, &up, &cache).await.unwrap();
            let stored = cache.bytes_of("geom/").unwrap();
            assert!(stored.len() * 8 < geojson.len(),
                    "cached {} B against {} B of GeoJSON", stored.len(), geojson.len());
        });
    }

    #[test]
    fn a_provider_may_ask_for_a_different_precision() {
        let p = provider("b=https://x/{id} precision=2");
        assert_eq!(p.precision, 2);
        let up = FakeUpstream::default().route("https://x/1", 200, SQUARE);
        let cache = MapCache::default();
        block_on(async {
            resolve(&p, &["1".to_string()], None, &up, &cache).await.unwrap();
            let stored = cache.bytes_of("geom/").unwrap();
            // Precision rides in the high nibble of the header byte, zigzagged.
            assert_eq!(stored[0] >> 4, 4, "p2 zigzags to 4");
        });
        assert!(Registry::parse("b=https://x/{id} precision=99").is_err());
    }

    #[test]
    fn a_corrupt_cache_entry_is_an_upstream_error_not_a_panic() {
        let p = provider("b=https://x/{id}");
        let up = FakeUpstream::default().route("https://x/1", 200, SQUARE);
        let cache = MapCache::default();
        block_on(async {
            let ids = vec!["1".to_string()];
            cache.put(&geometry_key(&p, &ids, None), Arc::from(&b"\x06\x00\xff"[..]), 60).await;
            let err = resolve(&p, &ids, None, &up, &cache).await.unwrap_err();
            assert!(matches!(err, Error::Upstream(_)), "{err:?}");
        });
    }

    #[test]
    fn a_minimal_provider_needs_only_a_geometry_url() {
        let p = provider("blocks=https://api.example.com/blocks/{id}/geometry");
        assert_eq!(p.id, "blocks");
        assert_eq!(p.access, None);
        assert!(p.forward_auth);
        assert_eq!(p.timeout_ms, DEFAULT_TIMEOUT_MS);
        assert_eq!(p.geometry_url("19519"), "https://api.example.com/blocks/19519/geometry");
    }

    #[test]
    fn options_follow_the_url() {
        let p = provider(
            "farms=https://internal/geom/{id} access=https://internal/access?ids={ids} \
             auth=no timeout=2000 max-bytes=256K");
        assert_eq!(p.access.as_deref(), Some("https://internal/access?ids={ids}"));
        assert!(!p.forward_auth);
        assert_eq!(p.timeout_ms, 2000);
        assert_eq!(p.max_bytes, 256 << 10);
        assert!(p.batches());
        assert_eq!(p.access_url(&["a".into(), "b".into()]).unwrap(),
                   "https://internal/access?ids=a,b");
    }

    #[test]
    fn a_single_id_access_endpoint_is_asked_one_at_a_time() {
        let p = provider("b=https://x/{id} access=https://x/{id}/allowed");
        assert!(!p.batches());
        assert_eq!(p.access_url(&["a".into()]).unwrap(), "https://x/a/allowed");
    }

    #[test]
    fn the_two_answers_expire_on_different_clocks() {
        let p = provider("b=https://x/{id} access=https://x/{id}/ok");
        assert!(p.access_ttl_s < p.geometry_ttl_s / 10,
                "an access decision must go stale long before the geometry does");
        let tuned = provider("b=https://x/{id} geometry-ttl=86400 access-ttl=5");
        assert_eq!((tuned.geometry_ttl_s, tuned.access_ttl_s), (86_400, 5));
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let r = Registry::parse("
            # the block service
            blocks=https://x/{id}

            farms=https://y/{id}   # inline
        ").unwrap();
        assert_eq!(r.names(), vec!["blocks", "farms"]);
    }

    #[test]
    fn a_template_without_a_placeholder_is_refused() {
        let err = Registry::parse("b=https://x/fixed").unwrap_err();
        assert!(err.contains("{id}"), "{err}");
    }

    #[test]
    fn a_template_with_two_placeholders_is_refused() {
        let err = Registry::parse("b=https://x/{id}/{id}").unwrap_err();
        assert!(err.contains("twice") || err.contains("2 times"), "{err}");
    }

    #[test]
    fn a_provider_url_must_be_http() {
        for bad in ["b=file:///etc/passwd", "b=ftp://x/{id}", "b=/local/{id}"] {
            assert!(Registry::parse(bad).is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn a_duplicate_provider_is_refused_rather_than_shadowed() {
        let err = Registry::parse("b=https://x/{id}\nb=https://y/{id}").unwrap_err();
        assert!(err.contains("twice"), "{err}");
    }

    #[test]
    fn ids_that_could_change_the_url_are_refused() {
        for bad in ["../admin", "a/b", "a?x=1", "a#f", "a%2F", "a b", "a&b", "..", "a..b"] {
            assert!(validate_id(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn ids_that_real_systems_use_are_accepted() {
        for good in ["19519", "a1b2c3", "0f8e1a2b-3c4d-5e6f-7a8b-9c0d1e2f3a4b",
                     "block.1.4b", "tenant:19519", "some_id-2"] {
            validate_id(good).unwrap_or_else(|e| panic!("{good:?} should be accepted: {e}"));
        }
    }

    #[test]
    fn an_over_long_id_is_refused() {
        assert!(validate_id(&"a".repeat(MAX_ID_LEN + 1)).is_err());
        validate_id(&"a".repeat(MAX_ID_LEN)).unwrap();
    }

    #[test]
    fn a_set_of_ids_is_sorted_and_deduplicated() {
        assert_eq!(parse_ids("b, a ,b,c").unwrap(), vec!["a", "b", "c"]);
    }

    #[test]
    fn too_many_ids_is_refused() {
        let many = (0..MAX_IDS + 1).map(|i| i.to_string()).collect::<Vec<_>>().join(",");
        assert!(parse_ids(&many).is_err());
    }

    #[test]
    fn geometry_is_shared_when_the_provider_can_answer_about_access() {
        let p = provider("b=https://x/{id} access=https://x/{id}/ok");
        let ids = vec!["19519".to_string()];
        assert_eq!(geometry_key(&p, &ids, Some("Bearer alice")),
                   geometry_key(&p, &ids, Some("Bearer bob")),
                   "two callers must share one entry for the same geometry");
    }

    #[test]
    fn geometry_is_not_shared_when_it_cannot() {
        let p = provider("b=https://x/{id}");
        let ids = vec!["19519".to_string()];
        assert_ne!(geometry_key(&p, &ids, Some("Bearer alice")),
                   geometry_key(&p, &ids, Some("Bearer bob")),
                   "without an access endpoint the credential is the only evidence of access");
        assert_ne!(geometry_key(&p, &ids, Some("Bearer alice")),
                   geometry_key(&p, &ids, None));
    }

    #[test]
    fn access_is_always_per_caller() {
        let p = provider("b=https://x/{id} access=https://x/{id}/ok");
        let ids = vec!["19519".to_string()];
        assert_ne!(access_key(&p, &ids, Some("Bearer alice")),
                   access_key(&p, &ids, Some("Bearer bob")));
    }

    #[test]
    fn a_credential_never_appears_in_a_cache_key() {
        let p = provider("b=https://x/{id}");
        let ids = vec!["19519".to_string()];
        let secret = "Bearer hunter2-this-is-the-actual-token";
        for key in [geometry_key(&p, &ids, Some(secret)), access_key(&p, &ids, Some(secret))] {
            assert!(!key.contains("hunter2"), "{key}");
            assert!(!key.contains(secret), "{key}");
        }
    }

    #[test]
    fn the_absent_credential_is_not_a_hash() {
        assert_eq!(credential_fingerprint(None), "anon");
        assert_ne!(credential_fingerprint(Some("")), "anon",
                   "an empty credential is a credential, not its absence");
        assert_eq!(credential_fingerprint(Some("x")).len(), 32);
    }
}
