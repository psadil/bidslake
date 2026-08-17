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
    /// The data table these rows are appended to. It must be a table the *effective* schema
    /// declares, which for every reader bidslake ships means one an adapter overlay added
    /// (`freesurfer_measures`, `freesurfer_aseg`, `freesurfer_aparc`).
    ///
    /// The reader names it from what it parsed, not from the ingestion rule that selected the
    /// reader — that is why one call returns a vector rather than one batch: a single
    /// `?h.aparc.stats` yields both per-structure rows and `# Measure` rows, for two different
    /// tables.
    ///
    /// Nothing validates the name before the write. A table the schema does not declare fails
    /// when the appender is opened, and the caller logs that and drops the batch while still
    /// marking the file `ingested` — so a typo here reads back as an empty table, not as an
    /// error.
    pub table: String,
    /// One JSON object per row, keyed by column name. Values are raw (readers emit strings and
    /// let [`Schema::row_values`](crate::schema::Schema) coerce each to its column's declared
    /// type), and a key `table` does not declare is routed into `other_data` rather than
    /// refused — so a reader may emit more than the overlay describes without failing.
    ///
    /// Every row should carry the `file_id` that `seed_row` puts on it. These tables are
    /// per-row and so have no primary key for an upsert to conflict on; a re-index instead
    /// DELETEs the file's earlier rows by `file_id` before inserting. A row without one is
    /// invisible to that DELETE and gains a duplicate copy on every run.
    pub rows: Vec<Value>,
}

/// Parses a standardized non-BIDS file body into rows for one or more data tables.
pub trait ContentReader: Send + Sync {
    /// Parse one file body into the rows it describes.
    ///
    /// `file_id` is the registry surrogate key of *this* file (ADR 0006) and is the only tie
    /// the emitted rows have back to a path, so it belongs on every row (see `seed_row`).
    /// `content` is the whole decoded body — a reader is handed text, never a handle, so a
    /// `read` disposition is only appropriate for files that fit in memory. `facts` is what
    /// the term map projected onto the path (subject/session entities, datatype, suffix): the
    /// same values the ingestion rule was selected on, and the ones that make a row queryable
    /// by concept.
    ///
    /// One [`ReaderRows`] per table the content turned out to target. An empty vector is a
    /// legitimate answer — "nothing here I recognize" — and the file is still recorded
    /// `ingested`. `Err` is for fatal I/O and nothing else: the caller logs it and moves on,
    /// and unlike a failed tabular batch it records no `failed` status, so such a file reads
    /// back from the registry as though no rule ever routed it.
    fn read(
        &self,
        file_id: u64,
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
pub(crate) fn seed_row(file_id: u64, facts: &FileFacts) -> Map<String, Value> {
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
