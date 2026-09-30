//! Clip geometry named by id: the parts every runtime shares.
//!
//! The server resolves `geometry_provider` + `geometry_id` by fetching from an
//! upstream the operator configured (`sabre-server`'s `geometry` module). A
//! browser or an app has no upstream to ask -- it already holds the geometry --
//! but it has the same problem the providers solve: a polygon carried with
//! every tile. There it crosses a worker boundary or a loopback socket rather
//! than the internet, and it is parsed again on the other side each time.
//!
//! [`GeometryStore`] is the local answer. The app puts geometry in once, under
//! a provider name and an id; tiles then name it with the same two parameters
//! the server takes, so a style means the same thing against either.
//!
//! What is here besides the store is shared with the server so the two cannot
//! drift: which ids are acceptable, and how TWKB, WKT and GeoJSON become a
//! [`Mask`].

use std::collections::HashMap;
use std::sync::Arc;

use crate::mask::{parse_wkt_mask, Mask};
use crate::twkb;

// ── Limits ────────────────────────────────────────────────────────────────────

/// Ids long enough to be a payload rather than a name are refused outright.
pub const MAX_ID_LEN: usize = 128;

/// How many ids one request may name.
///
/// On the server this bounds the cold-cache cost: a provider without a batch
/// access endpoint is asked about each id, and every id is one geometry fetch.
/// Warm, the whole set is a single cache entry however many ids it names.
///
/// The number has to clear a real farm with room to spare — the block sets
/// this was built for run to 86 — so it is not a limit anyone meets by having
/// an ordinary amount of land.
pub const MAX_IDS: usize = 512;

// ── Caller input ──────────────────────────────────────────────────────────────

/// Provider names: short, and safe in a URL, a log line and a cache key.
pub fn validate_provider_name(id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > 64 {
        return Err(format!("provider name {id:?} must be 1–64 characters"));
    }
    if !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return Err(format!("provider name {id:?} may only contain letters, digits, - and _"));
    }
    Ok(())
}

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
        parse_wkt_mask(trimmed)?;
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

// ── Local store ───────────────────────────────────────────────────────────────

/// How many merged id sets are kept per provider before they are all dropped.
///
/// A map shows a handful at once -- a farm, a block, the blocks under a
/// filter -- so this only has to outlast one view. Each one is a copy of its
/// members' rings, which is why it is bounded at all.
pub const MAX_UNIONS: usize = 32;

/// Geometry the app has handed over, by provider name and id.
///
/// Held parsed, as a [`Mask`], so a tile that names geometry costs a lookup
/// rather than a decode. [`set`](Self::set) is an upsert: the ids it names are
/// replaced and every other id is left alone, because geometry changes a few
/// blocks at a time. [`replace`](Self::replace) swaps a whole provider in one
/// step, so no tile can see it half-empty in between.
///
/// Every change returns the provider's new revision. It is what tells a map
/// that tiles it already drew are stale: the store cannot reach those.
/// Revisions come from one counter for the whole store, so they only ever
/// increase, even for a provider that is emptied and filled again.
#[derive(Debug, Default)]
pub struct GeometryStore {
    providers: HashMap<String, Provider>,
    revision: u64,
}

#[derive(Debug, Default)]
struct Provider {
    revision: u64,
    geometries: HashMap<String, Arc<Mask>>,
    /// Several ids clipped as one shape, keyed by the sorted id list. Built
    /// from `geometries`, so any change to the provider drops all of them.
    unions: HashMap<Vec<String>, Arc<Mask>>,
}

impl GeometryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add or overwrite `geometries` under `provider`. All or nothing: one bad
    /// id or empty shape and nothing changes.
    pub fn set(&mut self, provider: &str, geometries: Vec<(String, Mask)>) -> Result<u64, String> {
        let checked = check(provider, geometries)?;
        let p = self.providers.entry(provider.to_string()).or_default();
        p.geometries.extend(checked);
        Ok(self.changed(provider))
    }

    /// Make `geometries` the whole of `provider`. An empty list clears it.
    pub fn replace(&mut self, provider: &str, geometries: Vec<(String, Mask)>) -> Result<u64, String> {
        let checked = check(provider, geometries)?;
        let p = self.providers.entry(provider.to_string()).or_default();
        p.geometries = checked.into_iter().collect();
        Ok(self.changed(provider))
    }

    /// Remove `ids` from `provider`. Ids it does not have are not an error:
    /// the caller wants them gone, and they are.
    pub fn delete(&mut self, provider: &str, ids: &[String]) -> u64 {
        let Some(p) = self.providers.get_mut(provider) else {
            return 0;
        };
        let before = p.geometries.len();
        for id in ids {
            p.geometries.remove(id);
        }
        if p.geometries.len() == before {
            return p.revision;
        }
        self.changed(provider)
    }

    /// The provider's current revision; 0 if it has never held anything.
    pub fn revision(&self, provider: &str) -> u64 {
        self.providers.get(provider).map_or(0, |p| p.revision)
    }

    /// Geometries held under `provider`.
    pub fn len(&self, provider: &str) -> usize {
        self.providers.get(provider).map_or(0, |p| p.geometries.len())
    }

    /// The mask `ids` clip to together.
    ///
    /// `ids` must have come from [`parse_ids`], sorted and deduplicated, as
    /// the server requires too. An id the store does not have is an error, not
    /// a smaller clip: a tile drawn without a block it was asked to include
    /// shows data outside the field and looks entirely plausible.
    pub fn resolve(&mut self, provider: &str, ids: &[String]) -> Result<Arc<Mask>, String> {
        debug_assert!(ids.windows(2).all(|w| w[0] < w[1]),
                      "ids must be sorted and deduplicated; use parse_ids");
        let p = self.providers.get_mut(provider)
            .filter(|p| !p.geometries.is_empty())
            .ok_or_else(|| format!("no geometry is registered under {provider:?}"))?;
        let find = |id: &String| p.geometries.get(id).ok_or_else(||
            format!("{provider} has no geometry {id}"));

        if let [id] = ids {
            return find(id).map(Arc::clone);
        }
        if let Some(hit) = p.unions.get(ids) {
            return Ok(Arc::clone(hit));
        }
        // Rings concatenated: `sabre-core` keeps a pixel inside any polygon,
        // so this is a union, exactly as the server merges a multi-id request.
        let mut rings = Vec::new();
        for id in ids {
            rings.extend_from_slice(find(id)?.rings());
        }
        let mask = Arc::new(Mask::from_rings(rings));
        if p.unions.len() >= MAX_UNIONS {
            p.unions.clear();
        }
        p.unions.insert(ids.to_vec(), Arc::clone(&mask));
        Ok(mask)
    }

    fn changed(&mut self, provider: &str) -> u64 {
        self.revision += 1;
        let p = self.providers.get_mut(provider).expect("changed() after the provider exists");
        p.revision = self.revision;
        p.unions.clear();
        p.revision
    }
}

fn check(provider: &str, geometries: Vec<(String, Mask)>) -> Result<Vec<(String, Arc<Mask>)>, String> {
    validate_provider_name(provider)?;
    geometries.into_iter().map(|(id, mask)| {
        // The same ids the server accepts, so a style that names one here
        // works unchanged against a server's provider of the same name.
        validate_id(&id)?;
        if mask.size().0 == 0 {
            return Err(format!("geometry {id} has no polygons; it would clip everything"));
        }
        Ok((id, Arc::new(mask)))
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(x: f64) -> Mask {
        parse_wkt_mask(&format!("POLYGON(({x} 0,{} 0,{} 1,{x} 1,{x} 0))", x + 1.0, x + 1.0)).unwrap()
    }

    fn ids(raw: &str) -> Vec<String> {
        parse_ids(raw).unwrap()
    }

    #[test]
    fn set_is_an_upsert() {
        let mut s = GeometryStore::new();
        s.set("blocks", vec![("a".into(), square(0.0)), ("b".into(), square(2.0))]).unwrap();
        s.set("blocks", vec![("b".into(), square(4.0)), ("c".into(), square(6.0))]).unwrap();

        assert_eq!(s.len("blocks"), 3, "a is kept, b overwritten, c added");
        assert!(s.resolve("blocks", &ids("b")).unwrap().contains(4.5, 0.5));
        assert!(!s.resolve("blocks", &ids("b")).unwrap().contains(2.5, 0.5));
    }

    #[test]
    fn replace_swaps_the_whole_provider() {
        let mut s = GeometryStore::new();
        s.set("blocks", vec![("a".into(), square(0.0)), ("b".into(), square(2.0))]).unwrap();
        s.replace("blocks", vec![("c".into(), square(6.0))]).unwrap();
        assert_eq!(s.len("blocks"), 1);
        assert!(s.resolve("blocks", &ids("a")).is_err());

        s.replace("blocks", vec![]).unwrap();
        assert_eq!(s.len("blocks"), 0);
    }

    #[test]
    fn a_bad_entry_changes_nothing() {
        let mut s = GeometryStore::new();
        s.set("blocks", vec![("a".into(), square(0.0))]).unwrap();
        let rev = s.revision("blocks");

        let empty = Mask::from_rings(vec![]);
        assert!(s.set("blocks", vec![("b".into(), square(2.0)), ("c".into(), empty)]).is_err());
        assert!(s.set("blocks", vec![("b".into(), square(2.0)), ("../x".into(), square(4.0))]).is_err());
        assert!(s.replace("blocks", vec![("b/c".into(), square(2.0))]).is_err());
        assert!(s.set("no spaces", vec![("a".into(), square(0.0))]).is_err());

        assert_eq!(s.len("blocks"), 1);
        assert_eq!(s.revision("blocks"), rev);
    }

    #[test]
    fn several_ids_clip_as_their_union() {
        let mut s = GeometryStore::new();
        s.set("blocks", vec![("a".into(), square(0.0)), ("b".into(), square(2.0))]).unwrap();
        let both = s.resolve("blocks", &ids("b,a")).unwrap();
        assert!(both.contains(0.5, 0.5) && both.contains(2.5, 0.5));
        assert!(!both.contains(1.5, 0.5));
        assert!(Arc::ptr_eq(&both, &s.resolve("blocks", &ids("a,b")).unwrap()),
                "the same set, however written, is built once");
    }

    #[test]
    fn a_change_drops_unions_built_from_the_old_geometry() {
        let mut s = GeometryStore::new();
        s.set("blocks", vec![("a".into(), square(0.0)), ("b".into(), square(2.0))]).unwrap();
        s.resolve("blocks", &ids("a,b")).unwrap();

        s.set("blocks", vec![("b".into(), square(4.0))]).unwrap();
        let both = s.resolve("blocks", &ids("a,b")).unwrap();
        assert!(both.contains(4.5, 0.5) && !both.contains(2.5, 0.5));

        s.delete("blocks", &ids("b"));
        assert!(s.resolve("blocks", &ids("a,b")).is_err(), "not a clip to a alone");
    }

    #[test]
    fn unknown_names_are_errors() {
        let mut s = GeometryStore::new();
        assert!(s.resolve("blocks", &ids("a")).unwrap_err().contains("no geometry is registered"));
        s.set("blocks", vec![("a".into(), square(0.0))]).unwrap();
        assert!(s.resolve("blocks", &ids("z")).unwrap_err().contains("has no geometry z"));
        assert!(s.resolve("farms", &ids("a")).is_err(), "providers are separate namespaces");
    }

    #[test]
    fn revisions_only_move_forward_and_only_on_change() {
        let mut s = GeometryStore::new();
        assert_eq!(s.revision("blocks"), 0);
        let r1 = s.set("blocks", vec![("a".into(), square(0.0))]).unwrap();
        let r2 = s.set("farms", vec![("f".into(), square(0.0))]).unwrap();
        assert!(r2 > r1);
        assert_eq!(s.revision("blocks"), r1, "a change elsewhere leaves this provider alone");

        assert_eq!(s.delete("blocks", &ids("nope")), r1, "deleting nothing is not a change");
        assert_eq!(s.delete("absent", &ids("a")), 0);

        let r3 = s.delete("blocks", &ids("a"));
        let r4 = s.set("blocks", vec![("a".into(), square(0.0))]).unwrap();
        assert!(r1 < r3 && r3 < r4, "emptied and refilled still moves forward");
    }

    #[test]
    fn unions_are_bounded() {
        let mut s = GeometryStore::new();
        let all: Vec<(String, Mask)> = (0..40).map(|i| (format!("{i:02}"), square(i as f64 * 2.0))).collect();
        s.set("blocks", all).unwrap();
        for i in 0..39 {
            s.resolve("blocks", &ids(&format!("{i:02},{:02}", i + 1))).unwrap();
        }
        assert!(s.providers["blocks"].unions.len() <= MAX_UNIONS);
    }
}
