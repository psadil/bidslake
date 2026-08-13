//! Content readers: parse a matched non-BIDS file's *body* into rows for data tables.
//!
//! A reader is invoked by the ingestion schema (`disposition: read`, keyed by `reader`
//! name); it parses the file body and emits rows for the overlay-declared tables it targets
//! (a reader self-routes — e.g. FreeSurfer stats to `freesurfer_aseg`/`freesurfer_aparc` by
//! column headers — since choosing the table requires parsing the contents). Row values are
//! emitted as raw JSON (typically strings); [`Schema::row_values`](crate::schema::Schema)
//! coerces each to its column's declared type and routes any key the target table doesn't
//! declare into `other_data`.
//!
//! Contract: a reader MUST NOT panic or abort the ingest transaction. Fatal I/O is an `Err`
//! (the caller logs and skips); malformed rows are dropped, not propagated.

mod freesurfer_ctab;
mod freesurfer_stats;

use std::collections::HashMap;

use bids_schema::term_map::FileFacts;
use serde_json::{Map, Value};

/// Rows a content reader produced for one target table (JSON objects keyed by column name).
pub struct ReaderRows {
    pub table: String,
    pub rows: Vec<Value>,
}

/// Parses a standardized non-BIDS file body into rows for one or more data tables.
pub trait ContentReader: Send + Sync {
    fn read(
        &self,
        file_id: &str,
        content: &str,
        facts: &FileFacts,
    ) -> anyhow::Result<Vec<ReaderRows>>;
}

/// Seed a row with its source file's `file_id` and every projected entity. Any entity a
/// target table doesn't declare as a materialized concept falls through to `other_data` via
/// [`Schema::row_values`](crate::schema::Schema).
///
/// `file_id` rather than `(dataset_id, file_path)`: a data table points at the registry, and
/// the path it points to is a column of that (docs/adr/0006).
pub(crate) fn seed_row(file_id: &str, facts: &FileFacts) -> Map<String, Value> {
    let mut obj = Map::new();
    obj.insert("file_id".to_string(), Value::String(file_id.to_string()));
    for (k, v) in &facts.entities {
        obj.insert(k.clone(), Value::String(v.clone()));
    }
    obj
}

/// The content readers bidslake ships, keyed by the `reader` name used in ingestion rules.
pub fn default_readers() -> HashMap<String, Box<dyn ContentReader>> {
    let mut readers: HashMap<String, Box<dyn ContentReader>> = HashMap::new();
    readers.insert(
        "fs_stats".to_string(),
        Box::new(freesurfer_stats::FreeSurferStats),
    );
    readers.insert(
        "fs_ctab".to_string(),
        Box::new(freesurfer_ctab::FreeSurferCtab),
    );
    readers
}
