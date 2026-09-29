//! Overviews that live past the header read.
//!
//! A cloud-optimised file puts every IFD in the first few kilobytes. A plain
//! GeoTIFF with overviews added afterwards — `gdaladdo` on a stripped file —
//! puts them *after* the image data. In one real 3.6 MB sample the overview
//! chain started at byte 2,422,976, well past the 256 KB header read, and the
//! chain walk simply stopped.
//!
//! It stopped quietly, which is the part worth a test: no error, just an image
//! reporting one level instead of five, so every tile rendered from full
//! resolution. On that file a z=13 tile read 1.77 MB where it should have read
//! half a megabyte.

use async_trait::async_trait;
use sabre_core::cog::{fetch_meta, RangeReader, HEADER_FETCH_BYTES};

struct MemReader {
    bytes: Vec<u8>,
    /// Every range asked for, so the test can show the extra reads are bounded.
    reads: std::cell::RefCell<Vec<(u64, u64)>>,
}

#[async_trait(?Send)]
impl RangeReader for MemReader {
    async fn read_range(&self, offset: u64, length: u64) -> Result<Vec<u8>, String> {
        self.reads.borrow_mut().push((offset, length));
        let start = offset as usize;
        if start >= self.bytes.len() {
            return Ok(vec![]);
        }
        let end = (start + length as usize).min(self.bytes.len());
        Ok(self.bytes[start..end].to_vec())
    }
}

fn le16(v: u16) -> [u8; 2] { v.to_le_bytes() }
fn le32(v: u32) -> [u8; 4] { v.to_le_bytes() }

/// One IFD: width, height, and a strip offset/count so it parses as an image.
fn ifd(width: u32, height: u32, next: u32, reduced: bool) -> Vec<u8> {
    let entries: Vec<(u16, u16, u32, u32)> = vec![
        (254, 4, 1, u32::from(reduced)),  // NewSubfileType
        (256, 4, 1, width),               // ImageWidth
        (257, 4, 1, height),              // ImageLength
        (258, 3, 1, 32),                  // BitsPerSample
        (259, 3, 1, 1),                   // Compression: none
        (273, 4, 1, 100_000),             // StripOffsets
        (277, 3, 1, 1),                   // SamplesPerPixel
        (278, 4, 1, height),              // RowsPerStrip
        (279, 4, 1, width * height * 4),  // StripByteCounts
        (339, 3, 1, 3),                   // SampleFormat: float
    ];
    let mut out = Vec::new();
    out.extend_from_slice(&le16(entries.len() as u16));
    for (tag, typ, count, value) in entries {
        out.extend_from_slice(&le16(tag));
        out.extend_from_slice(&le16(typ));
        out.extend_from_slice(&le32(count));
        out.extend_from_slice(&le32(value));
    }
    out.extend_from_slice(&le32(next));
    out
}

/// A TIFF whose overview IFDs sit at `overviews_at`, deliberately past the
/// header read.
fn tiff_with_far_overviews(overviews_at: u32, levels: usize) -> Vec<u8> {
    let mut buf = vec![0u8; overviews_at as usize];
    buf[0..2].copy_from_slice(b"II");
    buf[2..4].copy_from_slice(&le16(42));
    buf[4..8].copy_from_slice(&le32(8));

    let full = ifd(2235, 2113, overviews_at, false);
    buf[8..8 + full.len()].copy_from_slice(&full);

    // The overview chain, each pointing at the next.
    let mut at = overviews_at;
    for i in 0..levels {
        let (w, h) = (2235u32 >> (i + 1), 2113u32 >> (i + 1));
        let body = ifd(w, h, 0, true);
        let next = if i + 1 < levels { at + body.len() as u32 } else { 0 };
        let body = ifd(w, h, next, true);
        buf.extend_from_slice(&body);
        at += body.len() as u32;
    }
    buf
}

fn read(bytes: Vec<u8>) -> (usize, Vec<(u64, u64)>) {
    let reader = MemReader { bytes, reads: std::cell::RefCell::new(Vec::new()) };
    let meta = pollster::block_on(fetch_meta(&reader)).expect("parse");
    let reads = reader.reads.borrow().clone();
    (meta.ifds.len(), reads)
}

#[test]
fn overviews_past_the_header_read_are_followed() {
    let far = HEADER_FETCH_BYTES as u32 * 9; // 2.25 MB, near the real sample
    let (levels, reads) = read(tiff_with_far_overviews(far, 4));
    assert_eq!(levels, 5, "full resolution plus four overviews");
    assert!(reads.len() >= 2, "the chain has to be chased: {reads:?}");
    assert_eq!(reads[0], (0, HEADER_FETCH_BYTES), "the header read comes first");
    assert_eq!(reads[1].0, u64::from(far), "then the chain is followed to where it went");
}

#[test]
fn a_cloud_optimised_file_still_costs_one_read() {
    // Everything inside the header: the chase must not trigger at all.
    let (levels, reads) = read(tiff_with_far_overviews(2_000, 4));
    assert_eq!(levels, 5);
    assert_eq!(reads.len(), 1, "no extra round trips for a well-ordered file: {reads:?}");
}

#[test]
fn a_chain_that_goes_nowhere_does_not_read_forever() {
    // A chain pointing past the end of the file must stop, not spin.
    let mut bytes = tiff_with_far_overviews(HEADER_FETCH_BYTES as u32 * 2, 1);
    let len = bytes.len();
    // Repoint the last IFD's next pointer far beyond the file.
    bytes[len - 4..].copy_from_slice(&le32(u32::MAX - 16));
    let (_, reads) = read(bytes);
    assert!(reads.len() <= 10, "bounded number of reads, got {}", reads.len());
}
