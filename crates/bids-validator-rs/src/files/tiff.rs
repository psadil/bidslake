//! TIFF / OME-TIFF header reading.
//!
//! Ports `lib/bids-validator/src/files/tiff.ts`. Reads the TIFF version from the
//! file header and, for OME-TIFF files, extracts the `PhysicalSize*` attributes
//! from the OME-XML stored in the first IFD's `ImageDescription` tag (0x010e).
//! Any parse error is swallowed, leaving the corresponding field `None`.

use crate::filetree::BidsFile;
use serde::Serialize;
use std::io::SeekFrom;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// TIFF header info.
#[derive(Debug, Clone, Serialize)]
pub struct Tiff {
    /// TIFF file format version (the second 2-byte block): 42 = classic, 43 = BigTIFF.
    pub version: u16,
}

/// Physical pixel sizes read from OME-XML (OME-TIFF only).
#[derive(Debug, Clone, Default, Serialize)]
pub struct Ome {
    #[serde(rename = "PhysicalSizeX", skip_serializing_if = "Option::is_none")]
    pub physical_size_x: Option<f64>,
    #[serde(rename = "PhysicalSizeXUnit", skip_serializing_if = "Option::is_none")]
    pub physical_size_x_unit: Option<String>,
    #[serde(rename = "PhysicalSizeY", skip_serializing_if = "Option::is_none")]
    pub physical_size_y: Option<f64>,
    #[serde(rename = "PhysicalSizeYUnit", skip_serializing_if = "Option::is_none")]
    pub physical_size_y_unit: Option<String>,
    #[serde(rename = "PhysicalSizeZ", skip_serializing_if = "Option::is_none")]
    pub physical_size_z: Option<f64>,
    #[serde(rename = "PhysicalSizeZUnit", skip_serializing_if = "Option::is_none")]
    pub physical_size_z_unit: Option<String>,
}

/// The `ImageDescription` TIFF tag.
const TAG_IMAGE_DESCRIPTION: u16 = 0x010e;

/// Parse a TIFF (optionally OME-TIFF) file. Returns `(tiff, ome)`; either field is
/// `None` when unavailable. Never errors — a malformed file just yields `None`s.
pub async fn parse_tiff(file: &BidsFile, ome: bool) -> (Option<Tiff>, Option<Ome>) {
    parse_tiff_inner(file, ome).await.unwrap_or_default()
}

async fn parse_tiff_inner(
    file: &BidsFile,
    ome: bool,
) -> std::io::Result<(Option<Tiff>, Option<Ome>)> {
    let mut f = File::open(&file.absolute_path).await?;

    // Read the header + first IFD region (mirrors the TS 4096-byte read).
    let mut header = vec![0u8; 4096];
    let n = f.read(&mut header).await?;
    header.truncate(n);
    if header.len() < 8 {
        return Ok((None, None));
    }

    // Byte order: "II" (0x4949) little-endian, "MM" (0x4D4D) big-endian.
    let little_endian = match (header[0], header[1]) {
        (0x49, 0x49) => true,
        (0x4d, 0x4d) => false,
        _ => return Ok((None, None)),
    };

    let read_u16 = |buf: &[u8], off: usize| -> Option<u16> {
        let b = buf.get(off..off + 2)?;
        Some(if little_endian {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        })
    };
    let read_u32 = |buf: &[u8], off: usize| -> Option<u32> {
        let b = buf.get(off..off + 4)?;
        Some(if little_endian {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        } else {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]])
        })
    };

    let version = match read_u16(&header, 2) {
        Some(v) => v,
        None => return Ok((None, None)),
    };
    let tiff = Tiff { version };

    // Only classic TIFF (version 42) OME-XML extraction is supported; for anything
    // else (e.g. BigTIFF) return the version only.
    if !ome || version != 42 {
        return Ok((Some(tiff), None));
    }

    // Walk the first IFD looking for the ImageDescription tag (entry size 12).
    let Some(ifd_offset) = read_u32(&header, 4).map(|v| v as usize) else {
        return Ok((Some(tiff), None));
    };
    let Some(entry_count) = read_u16(&header, ifd_offset) else {
        return Ok((Some(tiff), None));
    };

    let mut description: Option<String> = None;
    for i in 0..entry_count as usize {
        let entry = ifd_offset + 2 + i * 12;
        let Some(tag) = read_u16(&header, entry) else {
            break;
        };
        if tag != TAG_IMAGE_DESCRIPTION {
            continue;
        }
        let Some(count) = read_u32(&header, entry + 4).map(|v| v as usize) else {
            break;
        };
        if count == 0 {
            break;
        }
        // ASCII values <= 4 bytes are stored inline; otherwise at a file offset.
        let bytes = if count <= 4 {
            header.get(entry + 8..entry + 8 + count).map(|b| b.to_vec())
        } else {
            let Some(value_offset) = read_u32(&header, entry + 8).map(|v| v as u64) else {
                break;
            };
            f.seek(SeekFrom::Start(value_offset)).await?;
            let mut buf = vec![0u8; count];
            match f.read_exact(&mut buf).await {
                Ok(_) => Some(buf),
                Err(_) => None,
            }
        };
        if let Some(mut b) = bytes {
            // Trim a trailing NUL terminator if present.
            if b.last() == Some(&0) {
                b.pop();
            }
            description = String::from_utf8(b).ok();
        }
        break;
    }

    let ome = description.as_deref().and_then(parse_ome_xml);
    Ok((Some(tiff), ome))
}

/// Extract `PhysicalSize*` attributes from OME-XML by locating the `Pixels` element.
fn parse_ome_xml(xml: &str) -> Option<Ome> {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                // Match the local name (strip any namespace prefix).
                let name = e.name();
                let local = name.as_ref().rsplit(|&c| c == b':').next().unwrap_or(&[]);
                if local != b"Pixels" {
                    continue;
                }
                let mut ome = Ome::default();
                for attr in e.attributes().flatten() {
                    let key = attr.key.as_ref();
                    let val = attr.unescape_value().ok()?.into_owned();
                    match key {
                        b"PhysicalSizeX" => ome.physical_size_x = val.parse().ok(),
                        b"PhysicalSizeXUnit" => ome.physical_size_x_unit = Some(val),
                        b"PhysicalSizeY" => ome.physical_size_y = val.parse().ok(),
                        b"PhysicalSizeYUnit" => ome.physical_size_y_unit = Some(val),
                        b"PhysicalSizeZ" => ome.physical_size_z = val.parse().ok(),
                        b"PhysicalSizeZUnit" => ome.physical_size_z_unit = Some(val),
                        _ => {}
                    }
                }
                return Some(ome);
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}
