use crate::tiff::{parse_all_ifds, parse_header, parse_ifd_in, Ifd, TiffHeader};
use async_trait::async_trait;
use flate2::read::ZlibDecoder;
use std::io::Read;

pub const HEADER_FETCH_BYTES: u64 = 256 * 1024;

// ── Range reader trait ────────────────────────────────────────────────────────

/// Abstraction over byte-range reads from any source (HTTP, local file, …).
/// Use `?Send` so implementations work in both WASM (non-Send) and native contexts.
#[async_trait(?Send)]
pub trait RangeReader {
    async fn read_range(&self, offset: u64, length: u64) -> Result<Vec<u8>, String>;
}

// ── Fetch helpers ─────────────────────────────────────────────────────────────

/// How much is read when the IFD chain leaves the header, and how many times.
///
/// A cloud-optimised file puts every IFD in the first few kilobytes, so this
/// never triggers. A plain GeoTIFF with overviews added afterwards puts them
/// *after* the image data — in one real 3.6 MB sample they start at byte
/// 2,422,976 — and the chain simply ran off the end of the header read.
///
/// That failed quietly, which is the worst way: no error, just an image that
/// claims to have no overviews, so every tile renders from full resolution.
/// On that sample a z=13 tile read 1.77 MB instead of a few KB.
const IFD_CHASE_BYTES: u64 = 128 * 1024;
const IFD_CHASE_LIMIT: usize = 8;

/// Read the TIFF header and parse every IFD, following the chain out of the
/// header read if that is where it goes.
pub async fn fetch_meta(reader: &dyn RangeReader) -> Result<CogMeta, String> {
    let data = reader.read_range(0, HEADER_FETCH_BYTES).await?;
    let header = parse_header(&data)?;
    let mut ifds = parse_all_ifds(&data, &header)?;
    if ifds.is_empty() {
        return Err("no IFDs found in TIFF".into());
    }

    // Where the chain stopped. Zero means it ended properly.
    let mut next = ifds.last().map_or(0, |i| i.next_ifd_offset);
    let mut chased = 0;
    while next != 0 && chased < IFD_CHASE_LIMIT {
        let window = reader.read_range(next, IFD_CHASE_BYTES).await?;
        if window.is_empty() {
            break;
        }
        chased += 1;
        // Each hop parses as far as this window reaches, then loops.
        let mut offset = next;
        let mut progressed = false;
        while offset != 0 {
            let Ok(ifd) = parse_ifd_in(&window, next, offset, &header) else { break };
            let following = ifd.next_ifd_offset;
            ifds.push(ifd);
            progressed = true;
            // Stay in this window while the chain does.
            if following == 0 || !(next..next + window.len() as u64).contains(&following) {
                offset = 0;
                next = following;
            } else {
                offset = following;
            }
        }
        if !progressed {
            break;
        }
    }

    Ok(CogMeta { header, ifds, lzw_early_change: std::cell::Cell::new(None) })
}

/// Fetch a set of tiles by their (offset, byte_count) pairs, coalescing
/// contiguous ranges into fewer reads to minimise round-trips.
pub async fn fetch_tile_ranges(
    reader: &dyn RangeReader,
    offsets: &[u64],
    byte_counts: &[u64],
) -> Result<Vec<Vec<u8>>, String> {
    let slabs = coalesce_ranges(offsets, byte_counts, 32 * 1024);
    let mut fetched: Vec<(u64, Vec<u8>)> = Vec::with_capacity(slabs.len());
    for (start, length) in slabs {
        let data = reader.read_range(start, length).await?;
        fetched.push((start, data));
    }

    let mut tiles = Vec::with_capacity(offsets.len());
    'outer: for (i, (&off, &bc)) in offsets.iter().zip(byte_counts.iter()).enumerate() {
        for (slab_start, slab_data) in &fetched {
            let rel = off.wrapping_sub(*slab_start) as usize;
            let end = rel.saturating_add(bc as usize);
            if rel < slab_data.len() && end <= slab_data.len() {
                tiles.push(slab_data[rel..end].to_vec());
                continue 'outer;
            }
        }
        return Err(format!("tile {i} at offset {off} not found in any fetched slab"));
    }
    Ok(tiles)
}

// ── COG metadata ──────────────────────────────────────────────────────────────

pub struct CogMeta {
    pub header: TiffHeader,
    pub ifds: Vec<Ifd>,
    /// Cached LZW early-change flag: None = not yet detected, Some(bool) = known.
    /// `Cell` allows detection on first decode without requiring `&mut self`.
    pub(crate) lzw_early_change: std::cell::Cell<Option<bool>>,
}

impl CogMeta {
    /// Index 0 = full resolution, higher = coarser overview.
    pub fn ifd(&self, level: usize) -> &Ifd {
        &self.ifds[level.min(self.ifds.len() - 1)]
    }

    pub fn level_count(&self) -> usize {
        self.ifds.len()
    }
}

// ── Tile decompression ────────────────────────────────────────────────────────

pub fn decode_tile(
    data: &[u8],
    compression: u16,
    predictor: u16,
    tile_w: u32,
    tile_h: u32,
    samples: u16,
    bits: u16,
    lzw_mode: &std::cell::Cell<Option<bool>>,
) -> Result<Vec<u8>, String> {
    let mut out = match compression {
        1 => data.to_vec(),
        5 => match lzw_mode.get() {
            Some(early) => decode_lzw_inner(data, early)?,
            None => match decode_lzw_inner(data, false) {
                Ok(b) => { lzw_mode.set(Some(false)); b }
                Err(_) => match decode_lzw_inner(data, true) {
                    Ok(b)  => { lzw_mode.set(Some(true)); b }
                    Err(e) => return Err(e),
                }
            }
        },
        // JPEG payloads carry their own predictor-free DCT coding, and are not
        // decoded here anyway.
        6 | 7 => return Ok(data.to_vec()),
        8 | 32946 => decode_deflate(data)?,
        32773 => decode_packbits(data, tile_w, tile_h, samples, bits)?,
        _ => return Err(format!("unsupported compression {compression}")),
    };
    undo_predictor(&mut out, predictor, tile_w, samples, bits)?;
    Ok(out)
}

/// Reverse TIFF tag 317 in place.
///
/// The encoder stores each sample as its difference from the one `samples`
/// positions to its left, which makes smooth imagery compress far better.
/// Every Sentinel-2 L2A COG on AWS is written this way, so a reader without
/// this step does not fail -- it returns plausible-looking garbage.
///
/// Sample values are read back as little-endian (see `bytes_to_f32`), so the
/// accumulation is little-endian too.
fn undo_predictor(buf: &mut [u8], predictor: u16, row_px: u32, samples: u16, bits: u16) -> Result<(), String> {
    // 0 means the tag was absent; Default leaves it there.
    if predictor == 0 || predictor == 1 { return Ok(()); }

    let samples = samples.max(1) as usize;
    if !bits.is_multiple_of(8) {
        return Err(format!("predictor {predictor} with {bits}-bit samples is not supported"));
    }
    let bps       = (bits / 8) as usize;
    let row_bytes = row_px as usize * samples * bps;
    if row_bytes == 0 { return Ok(()); }

    match predictor {
        2 => {
            for row in buf.chunks_mut(row_bytes) {
                // A short final strip is still a whole number of rows; a
                // partial row would be a malformed file, so clamp rather than
                // read past the end.
                let n = row.len() / bps;
                match bps {
                    1 => for i in samples..row.len() {
                        row[i] = row[i].wrapping_add(row[i - samples]);
                    },
                    2 => accumulate::<2>(row, n, samples),
                    4 => accumulate::<4>(row, n, samples),
                    8 => accumulate::<8>(row, n, samples),
                    _ => return Err(format!("predictor 2 with {bits}-bit samples is not supported")),
                }
            }
            Ok(())
        }
        // Floating-point predictor: bytes are first differenced at byte
        // granularity, then split into planes (all the high bytes of the row,
        // then all the second bytes, ...). Undo in that order.
        3 => {
            let mut plane = vec![0u8; row_bytes];
            for row in buf.chunks_mut(row_bytes) {
                for i in samples..row.len() {
                    row[i] = row[i].wrapping_add(row[i - samples]);
                }
                let n = row.len() / bps; // samples in this row
                plane[..row.len()].copy_from_slice(row);
                for s in 0..n {
                    for b in 0..bps {
                        row[bps * s + bps - b - 1] = plane[b * n + s];
                    }
                }
            }
            Ok(())
        }
        _ => Err(format!("unsupported predictor {predictor}")),
    }
}

/// Horizontal differencing for a `BPS`-byte integer sample, little-endian.
fn accumulate<const BPS: usize>(row: &mut [u8], n_samples: usize, stride: usize) {
    for i in stride..n_samples {
        let (prev, cur) = (i - stride, i);
        let mut carry = 0u16;
        for b in 0..BPS {
            let sum = row[cur * BPS + b] as u16 + row[prev * BPS + b] as u16 + carry;
            row[cur * BPS + b] = sum as u8;
            carry = sum >> 8;
        }
    }
}

fn decode_deflate(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut decoder = ZlibDecoder::new(data);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).map_err(|e| e.to_string())?;
    Ok(out)
}

fn decode_lzw_inner(data: &[u8], early_change: bool) -> Result<Vec<u8>, String> {
    // Arena-based LZW string table.
    // arena: flat byte storage for all decoded strings.
    // table: (start: u32, len: u16) per code — no per-entry heap allocation.
    // Codes 0-255 are single bytes stored at arena[b] = b (base layout never moves).
    let mut arena: Vec<u8> = (0u8..=255).collect();
    let mut table: Vec<(u32, u16)> = (0u32..256).map(|b| (b, 1)).collect();
    table.push((0, 0)); // 256: clear
    table.push((0, 0)); // 257: EOI

    let arena_base = arena.len(); // 256 — reset point for Clear code
    let table_base = table.len(); // 258

    let mut out = Vec::new();
    let mut bit_pos = 0usize;
    let mut code_size = 9usize;
    let mut prev: Option<(u32, u16)> = None; // (start, len) of previous entry in arena

    loop {
        let code = read_bits(data, bit_pos, code_size)?;
        bit_pos += code_size;
        match code {
            256 => {
                arena.truncate(arena_base);
                table.truncate(table_base);
                code_size = 9;
                prev = None;
            }
            257 => break,
            _ => {
                let code_usize = code as usize;
                let tlen = table.len();

                // Resolve the entry for this code.
                // For look-ahead (code == tlen), entry = prev ++ [prev[0]].
                // We write it to the arena and add to table here; the standard
                // "add to table" block below is skipped for look-ahead.
                let is_lookahead = code_usize == tlen;
                let (es, el): (u32, u16) = if code_usize < tlen {
                    table[code_usize]
                } else if is_lookahead {
                    let (ps, pl) = prev.ok_or_else(|| format!("LZW invalid code {code}"))?;
                    let ns = arena.len() as u32;
                    let first = arena[ps as usize];
                    arena.extend_from_within(ps as usize..ps as usize + pl as usize);
                    arena.push(first);
                    let el = pl + 1;
                    table.push((ns, el));
                    (ns, el)
                } else {
                    return Err(format!("LZW invalid code {code}"));
                };

                out.extend_from_slice(&arena[es as usize..es as usize + el as usize]);

                if let Some((ps, pl)) = prev {
                    if !is_lookahead {
                        // Standard: new entry = prev ++ [entry[0]]
                        let ns = arena.len() as u32;
                        let first = arena[es as usize];
                        arena.extend_from_within(ps as usize..ps as usize + pl as usize);
                        arena.push(first);
                        table.push((ns, pl + 1));
                    }
                    let bump_at = if early_change {
                        (1usize << code_size).saturating_sub(1)
                    } else {
                        1usize << code_size
                    };
                    if table.len() >= bump_at && code_size < 12 { code_size += 1; }
                }
                prev = Some((es, el));
            }
        }
    }
    Ok(out)
}

fn read_bits(data: &[u8], bit_pos: usize, len: usize) -> Result<u16, String> {
    let byte_pos = bit_pos / 8;
    let bit_off  = bit_pos % 8;
    let b0 = *data.get(byte_pos    ).ok_or("LZW out of data")? as u32;
    let b1 = *data.get(byte_pos + 1).unwrap_or(&0) as u32;
    let b2 = *data.get(byte_pos + 2).unwrap_or(&0) as u32;
    let word  = (b0 << 16) | (b1 << 8) | b2;
    let shift = 24 - bit_off - len;
    Ok(((word >> shift) & ((1 << len) - 1)) as u16)
}

fn decode_packbits(data: &[u8], tile_w: u32, tile_h: u32, samples: u16, bits: u16) -> Result<Vec<u8>, String> {
    let expected = (tile_w * tile_h * samples as u32 * bits as u32 / 8) as usize;
    let mut out = Vec::with_capacity(expected);
    let mut i = 0;
    while i < data.len() && out.len() < expected {
        let n = data[i] as i8; i += 1;
        if n >= 0 {
            let count = n as usize + 1;
            let end = (i + count).min(data.len());
            out.extend_from_slice(&data[i..end]); i += count;
        } else if n != -128 {
            let count = (-n) as usize + 1;
            let byte = *data.get(i).ok_or("PackBits out of data")?;
            out.extend(std::iter::repeat(byte).take(count)); i += 1;
        }
    }
    Ok(out)
}

// ── Range coalescing ──────────────────────────────────────────────────────────

pub fn coalesce_ranges(offsets: &[u64], byte_counts: &[u64], max_gap: u64) -> Vec<(u64, u64)> {
    if offsets.is_empty() { return vec![]; }
    let mut pairs: Vec<(u64, u64)> = offsets.iter().zip(byte_counts.iter())
        .map(|(&o, &c)| (o, c)).collect();
    pairs.sort_unstable_by_key(|p| p.0);
    let mut slabs = Vec::new();
    let (mut ss, mut se) = (pairs[0].0, pairs[0].0 + pairs[0].1);
    for &(off, cnt) in pairs.iter().skip(1) {
        if off <= se + max_gap { se = se.max(off + cnt); }
        else { slabs.push((ss, se - ss)); ss = off; se = off + cnt; }
    }
    slabs.push((ss, se - ss));
    slabs
}
