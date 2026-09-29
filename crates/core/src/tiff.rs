#![allow(dead_code)]
use byteorder::{BigEndian, LittleEndian, ReadBytesExt};
use std::io::{Cursor, Read, Seek, SeekFrom};

#[derive(Debug, Clone)]
pub struct TiffHeader {
    pub big_endian: bool,
    pub bigtiff: bool,
    pub first_ifd_offset: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Ifd {
    pub image_width: u32,
    pub image_height: u32,
    pub bits_per_sample: Vec<u16>,
    pub compression: u16,
    pub photometric: u16,
    pub samples_per_pixel: u16,
    pub planar_config: u16,
    pub sample_format: Vec<u16>,
    /// Predictor (tag 317). 1 = none, 2 = horizontal differencing, 3 = floating
    /// point. Absent means 1; `Default` leaves it 0, which is read as none too.
    pub predictor: u16,
    pub tile_width: Option<u32>,
    pub tile_height: Option<u32>,
    pub tile_offsets: Vec<u64>,
    pub tile_byte_counts: Vec<u64>,
    pub strip_offsets: Vec<u64>,
    pub strip_byte_counts: Vec<u64>,
    pub rows_per_strip: u32,
    pub pixel_scale: Option<[f64; 3]>,
    pub tiepoints: Vec<f64>,
    pub geo_transform_matrix: Option<[f64; 16]>,
    /// EPSG code read from GeoKeyDirectory (tag 34735).
    /// 3072 = ProjectedCSTypeGeoKey (UTM, state-plane, …)
    /// 2048 = GeographicTypeGeoKey  (WGS84 = 4326, …)
    pub epsg_code: Option<u32>,
    /// GDAL_NODATA (tag 42113) — stored as ASCII string
    pub nodata: Option<String>,
    /// GDAL_METADATA (tag 42112) — XML block containing STATISTICS_MINIMUM/MAXIMUM etc.
    pub gdal_metadata: Option<String>,
    pub next_ifd_offset: u64,
}

impl Ifd {
    pub fn is_tiled(&self) -> bool {
        self.tile_width.is_some()
    }

    /// Affine geotransform [x_origin, pixel_width, 0, y_origin, 0, -pixel_height]
    pub fn geo_transform(&self) -> Option<[f64; 6]> {
        if let Some(mat) = self.geo_transform_matrix {
            return Some([mat[3], mat[0], mat[1], mat[7], mat[4], mat[5]]);
        }
        if let Some(scale) = self.pixel_scale {
            if self.tiepoints.len() >= 6 {
                let tp = &self.tiepoints;
                // tp[0..3] = pixel i,j,k  tp[3..6] = geo x,y,z
                let x_origin = tp[3] - tp[0] * scale[0];
                let y_origin = tp[4] + tp[1] * scale[1];
                return Some([x_origin, scale[0], 0.0, y_origin, 0.0, -scale[1]]);
            }
        }
        None
    }

    /// Returns (min, max) from embedded GDAL statistics, if present.
    pub fn data_stats(&self) -> (Option<f64>, Option<f64>) {
        let xml = match &self.gdal_metadata {
            Some(s) => s.as_str(),
            None => return (None, None),
        };
        (extract_gdal_stat(xml, "STATISTICS_MINIMUM"),
         extract_gdal_stat(xml, "STATISTICS_MAXIMUM"))
    }

    pub fn num_tiles_x(&self) -> u32 {
        let tw = self.tile_width.unwrap_or(self.image_width);
        (self.image_width + tw - 1) / tw
    }

    pub fn num_tiles_y(&self) -> u32 {
        let th = self.tile_height.unwrap_or(self.image_height);
        (self.image_height + th - 1) / th
    }
}

pub fn parse_header(data: &[u8]) -> Result<TiffHeader, String> {
    let mut c = Cursor::new(data);
    let mut magic = [0u8; 2];
    c.read_exact(&mut magic).map_err(|e| e.to_string())?;
    let big_endian = match &magic {
        b"II" => false,
        b"MM" => true,
        _ => return Err("not a TIFF file".into()),
    };

    let version = if big_endian {
        c.read_u16::<BigEndian>()
    } else {
        c.read_u16::<LittleEndian>()
    }
    .map_err(|e| e.to_string())?;

    let (bigtiff, first_ifd_offset) = match version {
        42 => {
            let offset = read_u32(&mut c, big_endian)? as u64;
            (false, offset)
        }
        43 => {
            // BigTIFF: skip 2-byte offset_bytesize and 2-byte always-zero padding
            read_u16(&mut c, big_endian)?;
            read_u16(&mut c, big_endian)?;
            let off = read_u64(&mut c, big_endian)?;
            (true, off)
        }
        _ => return Err(format!("unknown TIFF version {version}")),
    };

    Ok(TiffHeader { big_endian, bigtiff, first_ifd_offset })
}

/// Parse one IFD from `data` at `offset`. Returns the IFD and next-IFD offset.
pub fn parse_ifd(data: &[u8], offset: u64, header: &TiffHeader) -> Result<Ifd, String> {
    parse_ifd_in(data, 0, offset, header)
}

/// Parse one IFD from a window of the file.
///
/// `data` holds the bytes starting at `base`; `offset` is the IFD's position in
/// the *file*. Offsets inside a TIFF are absolute, so a reader holding
/// anything other than the start of the file has to translate them — which is
/// what lets overviews be read out of a chunk near the end without holding
/// everything in between.
pub fn parse_ifd_in(data: &[u8], base: u64, offset: u64, header: &TiffHeader) -> Result<Ifd, String> {
    let local = |file_offset: u64| -> Option<usize> {
        file_offset.checked_sub(base).map(|v| v as usize).filter(|v| *v <= data.len())
    };
    let start = local(offset).ok_or_else(|| format!("IFD at {offset} is outside the window"))?;
    let mut c = Cursor::new(data);
    c.seek(SeekFrom::Start(start as u64)).map_err(|e| e.to_string())?;

    let be = header.big_endian;
    let bigtiff = header.bigtiff;

    let num_entries: u64 = if bigtiff {
        read_u64(&mut c, be)?
    } else {
        read_u16(&mut c, be)? as u64
    };

    let mut ifd = Ifd { rows_per_strip: u32::MAX, planar_config: 1, ..Default::default() };

    for _ in 0..num_entries {
        let tag = read_u16(&mut c, be)?;
        let typ = read_u16(&mut c, be)?;
        let count = if bigtiff { read_u64(&mut c, be)? } else { read_u32(&mut c, be)? as u64 };

        let value_bytes = if bigtiff { 8usize } else { 4 };
        let type_size = tiff_type_size(typ);
        let data_len = type_size * count as usize;

        // Read value-or-offset field inline
        let mut inline = vec![0u8; value_bytes];
        c.read_exact(&mut inline).map_err(|e| e.to_string())?;

        // Determine where the actual values live
        let values: Vec<u8> = if data_len <= value_bytes {
            inline[..data_len].to_vec()
        } else {
            // It's an offset into the file
            let file_offset = read_offset_from_bytes(&inline, be, bigtiff);
            let Some(from) = local(file_offset) else { continue };
            let end = from.saturating_add(data_len);
            if end > data.len() {
                // Tag data lies beyond the fetched range — skip it rather than
                // aborting the whole parse. Cursor is already past the inline
                // field so the next tag entry is in the right position.
                continue;
            }
            data[from..end].to_vec()
        };

        let save_pos = c.position();

        match tag {
            256 => ifd.image_width = read_any_as_u32(&values, typ, be)?,
            257 => ifd.image_height = read_any_as_u32(&values, typ, be)?,
            258 => ifd.bits_per_sample = read_u16_array(&values, count as usize, be),
            259 => ifd.compression = read_any_as_u32(&values, typ, be)? as u16,
            262 => ifd.photometric = read_any_as_u32(&values, typ, be)? as u16,
            277 => ifd.samples_per_pixel = read_any_as_u32(&values, typ, be)? as u16,
            278 => ifd.rows_per_strip = read_any_as_u32(&values, typ, be)?,
            284 => ifd.planar_config = read_any_as_u32(&values, typ, be)? as u16,
            322 => ifd.tile_width = Some(read_any_as_u32(&values, typ, be)?),
            323 => ifd.tile_height = Some(read_any_as_u32(&values, typ, be)?),
            273 => ifd.strip_offsets = read_u64_array(&values, typ, count as usize, be),
            279 => ifd.strip_byte_counts = read_u64_array(&values, typ, count as usize, be),
            324 => ifd.tile_offsets = read_u64_array(&values, typ, count as usize, be),
            325 => ifd.tile_byte_counts = read_u64_array(&values, typ, count as usize, be),
            317 => ifd.predictor = read_any_as_u32(&values, typ, be)? as u16,
            339 => ifd.sample_format = read_u16_array(&values, count as usize, be),
            33550 => ifd.pixel_scale = parse_f64_triple(&values, be),
            33922 => ifd.tiepoints = read_f64_array(&values, count as usize, be),
            34264 => ifd.geo_transform_matrix = parse_f64_16(&values, be),
            34735 => ifd.epsg_code = parse_geo_key_directory(&values, be),
            42112 => ifd.gdal_metadata = parse_ascii_string(&values),
            42113 => ifd.nodata = parse_ascii_string(&values),
            _ => {}
        }

        c.seek(SeekFrom::Start(save_pos)).map_err(|e| e.to_string())?;
    }

    let next_ifd_offset = if bigtiff {
        read_u64(&mut c, be)?
    } else {
        read_u32(&mut c, be)? as u64
    };
    ifd.next_ifd_offset = next_ifd_offset;

    Ok(ifd)
}

/// Parse all IFDs (full-res + overviews) from data.
/// Stops gracefully if the next IFD offset falls outside the fetched range.
pub fn parse_all_ifds(data: &[u8], header: &TiffHeader) -> Result<Vec<Ifd>, String> {
    let mut ifds = Vec::new();
    let mut offset = header.first_ifd_offset;
    while offset != 0 {
        if offset as usize >= data.len() {
            break;
        }
        let ifd = parse_ifd(data, offset, header)?;
        offset = ifd.next_ifd_offset;
        ifds.push(ifd);
    }
    Ok(ifds)
}

// --- helpers ---

fn read_u16(c: &mut Cursor<&[u8]>, be: bool) -> Result<u16, String> {
    if be { c.read_u16::<BigEndian>() } else { c.read_u16::<LittleEndian>() }
        .map_err(|e| e.to_string())
}

fn read_u32(c: &mut Cursor<&[u8]>, be: bool) -> Result<u32, String> {
    if be { c.read_u32::<BigEndian>() } else { c.read_u32::<LittleEndian>() }
        .map_err(|e| e.to_string())
}

fn read_u64(c: &mut Cursor<&[u8]>, be: bool) -> Result<u64, String> {
    if be { c.read_u64::<BigEndian>() } else { c.read_u64::<LittleEndian>() }
        .map_err(|e| e.to_string())
}

fn tiff_type_size(typ: u16) -> usize {
    match typ {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 => 4,
        5 | 10 | 12 | 16 | 17 | 18 => 8,
        _ => 1,
    }
}

fn read_offset_from_bytes(bytes: &[u8], be: bool, bigtiff: bool) -> u64 {
    let mut c = Cursor::new(bytes);
    if bigtiff {
        if be { c.read_u64::<BigEndian>() } else { c.read_u64::<LittleEndian>() }
            .unwrap_or(0)
    } else {
        if be { c.read_u32::<BigEndian>() } else { c.read_u32::<LittleEndian>() }
            .unwrap_or(0) as u64
    }
}

fn read_any_as_u32(data: &[u8], typ: u16, be: bool) -> Result<u32, String> {
    let mut c = Cursor::new(data);
    match typ {
        1 | 7 => Ok(c.read_u8().unwrap_or(0) as u32),
        3 => Ok(if be { c.read_u16::<BigEndian>() } else { c.read_u16::<LittleEndian>() }
            .unwrap_or(0) as u32),
        4 => Ok(if be { c.read_u32::<BigEndian>() } else { c.read_u32::<LittleEndian>() }
            .unwrap_or(0)),
        _ => Ok(0),
    }
}

fn read_u16_array(data: &[u8], count: usize, be: bool) -> Vec<u16> {
    let mut c = Cursor::new(data);
    (0..count)
        .map(|_| {
            if be { c.read_u16::<BigEndian>() } else { c.read_u16::<LittleEndian>() }
                .unwrap_or(0)
        })
        .collect()
}

fn read_u64_array(data: &[u8], typ: u16, count: usize, be: bool) -> Vec<u64> {
    let mut c = Cursor::new(data);
    (0..count)
        .map(|_| match typ {
            3 => if be { c.read_u16::<BigEndian>() } else { c.read_u16::<LittleEndian>() }
                .unwrap_or(0) as u64,
            4 => if be { c.read_u32::<BigEndian>() } else { c.read_u32::<LittleEndian>() }
                .unwrap_or(0) as u64,
            16 => if be { c.read_u64::<BigEndian>() } else { c.read_u64::<LittleEndian>() }
                .unwrap_or(0),
            _ => 0,
        })
        .collect()
}

fn read_f64_array(data: &[u8], count: usize, be: bool) -> Vec<f64> {
    let mut c = Cursor::new(data);
    (0..count)
        .map(|_| {
            if be { c.read_f64::<BigEndian>() } else { c.read_f64::<LittleEndian>() }
                .unwrap_or(0.0)
        })
        .collect()
}

fn parse_f64_triple(data: &[u8], be: bool) -> Option<[f64; 3]> {
    let v = read_f64_array(data, 3, be);
    if v.len() == 3 { Some([v[0], v[1], v[2]]) } else { None }
}

fn parse_f64_16(data: &[u8], be: bool) -> Option<[f64; 16]> {
    let v = read_f64_array(data, 16, be);
    if v.len() == 16 {
        let mut arr = [0f64; 16];
        arr.copy_from_slice(&v);
        Some(arr)
    } else {
        None
    }
}

fn parse_ascii_string(data: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(data).ok()?;
    let trimmed = s.trim_end_matches('\0').trim();
    if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
}

/// Extract a named item value from a GDAL_METADATA XML block.
/// Handles: `<Item name="STATISTICS_MINIMUM" ...>VALUE</Item>`
fn extract_gdal_stat(xml: &str, stat_name: &str) -> Option<f64> {
    let needle = format!("name=\"{stat_name}\"");
    let start = xml.find(needle.as_str())?;
    let after = &xml[start..];
    let gt = after.find('>')?;
    let rest = &after[gt + 1..];
    let lt = rest.find('<')?;
    rest[..lt].trim().parse().ok()
}

/// Parse the GeoKeyDirectory (tag 34735) and return the EPSG code.
/// Prefers ProjectedCSTypeGeoKey (3072) over GeographicTypeGeoKey (2048).
/// Returns None for user-defined (32767) or missing keys.
fn parse_geo_key_directory(data: &[u8], be: bool) -> Option<u32> {
    let shorts = read_u16_array(data, data.len() / 2, be);
    if shorts.len() < 4 {
        return None;
    }
    // Header: [KeyDirectoryVersion, KeyRevision, MinorRevision, NumberOfKeys]
    let n_keys = shorts[3] as usize;

    let mut projected: Option<u16> = None;
    let mut geographic: Option<u16> = None;

    for i in 0..n_keys {
        let base = 4 + i * 4;
        if base + 3 >= shorts.len() {
            break;
        }
        let key_id       = shorts[base];
        let tag_location = shorts[base + 1]; // 0 = value is inline in offset field
        let value_offset = shorts[base + 3];

        // We only handle SHORT values stored inline (TIFFTagLocation == 0)
        if tag_location != 0 || value_offset == 32767 {
            continue; // user-defined or stored in another tag
        }

        match key_id {
            3072 => projected  = Some(value_offset), // ProjectedCSTypeGeoKey
            2048 => geographic = Some(value_offset), // GeographicTypeGeoKey
            _ => {}
        }
    }

    projected.or(geographic).map(|c| c as u32)
}
