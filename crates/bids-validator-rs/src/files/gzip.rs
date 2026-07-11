//! GZIP header reading.

use crate::filetree::BidsFile;
use serde_json::Value;
use std::io::SeekFrom;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// How many bytes of the header to read. The optional `FNAME`/`FCOMMENT` fields are
/// NUL-terminated strings of unbounded length; the reference validator reads 1024 bytes and
/// accepts truncation, so we do the same.
const HEADER_READ_BYTES: usize = 1024;

/// Flags in the gzip header's FLG byte (RFC 1952 §2.3.1).
const FEXTRA: u8 = 0x04;
const FNAME: u8 = 0x08;
const FCOMMENT: u8 = 0x10;

/// Read the GZIP header metadata from a `.gz` file.
///
/// Parses past the fixed 10-byte header to recover the optional original `filename` and
/// `comment` fields, which `rules.checks.privacy` inspects for information leakage. Both
/// default to the empty string when their flag is unset, so the schema's truthiness selectors
/// (`gzip.filename`) correctly decline to fire.
pub async fn parse_gzip_header(file: &BidsFile) -> Result<Value, GzipError> {
    let mut f = File::open(&file.absolute_path)
        .await
        .map_err(GzipError::Io)?;
    let mut buf = vec![0u8; HEADER_READ_BYTES];
    let n = f.read(&mut buf).await.map_err(GzipError::Io)?;
    buf.truncate(n);
    if buf.len() < 10 {
        return Err(GzipError::TooShort);
    }

    // Check magic number
    if buf[0] != 0x1f || buf[1] != 0x8b {
        return Err(GzipError::NotGzip);
    }

    let compression_method = buf[2] as i64;
    let flags = buf[3];
    // MTIME is bytes 4-7, little-endian
    let mtime = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as i64;
    let extra_flags = buf[8] as i64;
    let os = buf[9] as i64;

    let mut offset = 10usize;
    if flags & FEXTRA != 0 {
        // 2-byte little-endian XLEN, then XLEN bytes of extra data.
        let xlen = match (buf.get(10), buf.get(11)) {
            (Some(&lo), Some(&hi)) => u16::from_le_bytes([lo, hi]) as usize,
            _ => return Err(GzipError::TooShort),
        };
        offset += 2 + xlen;
    }

    // Each of FNAME / FCOMMENT is a NUL-terminated string starting at `offset`.
    let read_cstr = |offset: &mut usize| -> String {
        if *offset >= buf.len() {
            return String::new();
        }
        let end = buf[*offset..]
            .iter()
            .position(|&b| b == 0)
            .map(|i| *offset + i)
            .unwrap_or(buf.len());
        let s = String::from_utf8_lossy(&buf[*offset..end]).into_owned();
        *offset = end + 1;
        s
    };

    let filename = if flags & FNAME != 0 {
        read_cstr(&mut offset)
    } else {
        String::new()
    };
    let comment = if flags & FCOMMENT != 0 {
        read_cstr(&mut offset)
    } else {
        String::new()
    };

    let result = serde_json::json!({
        "compression_method": compression_method,
        "flags": flags as i64,
        "mtime": mtime,
        "extra_flags": extra_flags,
        "os": os,
        // Also report the timestamp as 0 or not, which some checks use
        "timestamp": mtime,
        "filename": filename,
        "comment": comment,
    });

    Ok(result)
}

/// Get the uncompressed size from a gzip file (last 4 bytes).
pub async fn gzip_uncompressed_size(file: &BidsFile) -> Result<u64, GzipError> {
    let mut f = File::open(&file.absolute_path)
        .await
        .map_err(GzipError::Io)?;

    // Check file length
    let metadata = f.metadata().await.map_err(GzipError::Io)?;
    if metadata.len() < 4 {
        return Err(GzipError::TooShort);
    }

    f.seek(SeekFrom::End(-4)).await.map_err(GzipError::Io)?;
    let mut size_bytes = [0u8; 4];
    f.read_exact(&mut size_bytes).await.map_err(GzipError::Io)?;

    Ok(u32::from_le_bytes(size_bytes) as u64)
}

#[derive(Debug, thiserror::Error)]
pub enum GzipError {
    #[error("Failed to read file: {0}")]
    Io(#[from] std::io::Error),
    #[error("File is too short to contain a GZIP header")]
    TooShort,
    #[error("File does not have GZIP magic number")]
    NotGzip,
}
