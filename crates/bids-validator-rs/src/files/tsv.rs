//! TSV file loading and parsing.
//!
//! BIDS TSV files are small and numerous, so we parse them with a simple tab splitter rather
//! than a heavyweight dataframe engine (which incurs large per-file setup cost). Gzip-compressed
//! tables (`.tsv.gz`) are transparently decompressed.

use crate::filetree::BidsFile;
use std::collections::HashMap;
use std::io::Read;

/// Parsed TSV data: maps column name to a list of values as strings.
pub type TsvColumns = HashMap<String, Vec<String>>;

/// Read a TSV file to text, decompressing gzip when needed.
async fn read_tsv_text(file: &BidsFile) -> Result<String, TsvError> {
    let bytes = tokio::fs::read(&file.absolute_path)
        .await
        .map_err(TsvError::Io)?;
    let gzipped = file.name.ends_with(".gz") || bytes.starts_with(&[0x1f, 0x8b]);
    let text = if gzipped {
        let mut decoder = flate2::read::GzDecoder::new(&bytes[..]);
        let mut text = String::new();
        decoder.read_to_string(&mut text).map_err(TsvError::Io)?;
        text
    } else {
        String::from_utf8_lossy(&bytes).into_owned()
    };
    // Strip a leading UTF-8 byte-order mark if present (some TSVs are BOM-prefixed).
    Ok(text
        .strip_prefix('\u{FEFF}')
        .map(str::to_string)
        .unwrap_or(text))
}

/// Split TSV text into a header row and data rows, dropping trailing blank lines (e.g. from a
/// final newline).
fn split_rows(text: &str) -> Option<(Vec<&str>, Vec<&str>)> {
    let mut lines: Vec<&str> = text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    let mut iter = lines.into_iter();
    let header = iter.next()?;
    if header.is_empty() {
        return None;
    }
    Some((header.split('\t').collect(), iter.collect()))
}

/// Load TSV columns, returning a map of column name to a list of values as strings.
pub async fn load_tsv_columns(file: &BidsFile) -> Result<TsvColumns, TsvError> {
    let text = read_tsv_text(file).await?;
    let Some((headers, rows)) = split_rows(&text) else {
        return Ok(HashMap::new());
    };

    let mut cols: Vec<Vec<String>> = vec![Vec::with_capacity(rows.len()); headers.len()];
    for row in rows {
        let mut fields = row.split('\t');
        for col in cols.iter_mut() {
            col.push(fields.next().unwrap_or("").to_string());
        }
    }

    Ok(headers
        .into_iter()
        .map(|h| h.to_string())
        .zip(cols)
        .collect())
}

/// Load a single named TSV column (used e.g. for `participant_id`). Errors if the column is
/// absent.
pub async fn load_tsv_column(file: &BidsFile, column_name: &str) -> Result<Vec<String>, TsvError> {
    let text = read_tsv_text(file).await?;
    let Some((headers, rows)) = split_rows(&text) else {
        return Err(TsvError::Parse {
            path: file.path.clone(),
            message: "empty TSV".to_string(),
        });
    };
    let idx = headers
        .iter()
        .position(|h| *h == column_name)
        .ok_or_else(|| TsvError::Parse {
            path: file.path.clone(),
            message: format!("Column not found: {}", column_name),
        })?;
    Ok(rows
        .into_iter()
        .map(|row| row.split('\t').nth(idx).unwrap_or("").to_string())
        .collect())
}

#[derive(Debug, thiserror::Error)]
pub enum TsvError {
    #[error("Failed to read file: {0}")]
    Io(#[from] std::io::Error),
    #[error("TSV parse error in {path}: {message}")]
    Parse { path: String, message: String },
}
