//! Content readers: parse a matched non-BIDS file's *body* into rows for data tables.
//!
//! A reader is invoked by the ingestion schema (`disposition: read`, keyed by `reader`
//! name); it parses the file body and emits rows for the overlay-declared tables it targets.
//! Which table that is normally comes from the schema — the file's projected concepts route
//! through `rules.tabular_data`, and the caller hands the answer over as a [`DeclaredTable`] —
//! so a reader does not name a table the schema already states. A reader *self-routes* only
//! where the schema cannot know: FreeSurfer's `# Measure` rows are a second payload inside a
//! `.stats` file, and no projection of one path yields two tables. Row values are emitted as
//! raw JSON (typically strings); [`Schema::row_values`](crate::schema::Schema) coerces each to
//! its column's declared type and routes any key the target table doesn't declare into
//! `other_data`.
//!
//! Not every `reader` name reaches this module. `csv`, `diffusion` and `matrix` name **engines**
//! (see [`ENGINE_READERS`]) dispatched in [`crate::bids`]: they hand the file to DuckDB rather
//! than parsing it in Rust, which is what keeps the tabular hot path batched and what lets a
//! headerless whitespace-delimited file be described in JSON alone (docs/adr/0002).
//!
//! Contract: a reader MUST NOT panic or abort the ingest transaction. Fatal I/O is an `Err`
//! (the caller logs and skips); malformed rows are dropped, not propagated.

mod freesurfer_stats;

use std::collections::HashMap;

use bids_schema::term_map::FileFacts;
use serde_json::{Map, Value};

use crate::schema::tabular::ColumnSpec;

/// The ingestion schema's `reader` name for the batched tabular ingest.
pub const CSV_READER: &str = "csv";
/// The ingestion schema's `reader` name for the `.bval`/`.bvec` accumulator.
pub const DIFFUSION_READER: &str = "diffusion";
/// The ingestion schema's `reader` name for the headerless whitespace-delimited read.
pub const MATRIX_READER: &str = "matrix";

/// The `reader` names that name an **engine** rather than a [`ContentReader`].
///
/// Engines are dispatched by name in [`crate::bids`] and never looked up in
/// [`default_readers`], so a name does nothing at all unless it is in this list *or* a key of
/// that map. Nothing else enforces that — an unknown name is a warning on stderr and a silently
/// empty table — which is why the two together are what the bundled fragments are checked
/// against.
pub const ENGINE_READERS: &[&str] = &[CSV_READER, DIFFUSION_READER, MATRIX_READER];

/// What the effective schema declares about the file being read: the table its projected
/// concepts route to, and that table's columns **in declared order** (`initial_columns` first,
/// then the rest by key — see [`ColumnSpec`]).
///
/// `None` at the call site means no `rules.tabular_data` rule matched the file. A reader that
/// needs the answer emits nothing rather than guessing a name, so a missing declaration reads
/// back as an empty table rather than as rows in the wrong one.
pub struct DeclaredTable<'a> {
    /// The DuckDB table the file routes to.
    pub table: &'a str,
    /// Its columns, in the order the schema declares them.
    pub columns: &'a [ColumnSpec],
}

/// Rows a content reader produced for one target table (JSON objects keyed by column name).
pub struct ReaderRows {
    /// The data table these rows are appended to. It must be a table the *effective* schema
    /// declares, which for every reader bidslake ships means one an adapter overlay added
    /// (`freesurfer_aseg`, `freesurfer_aparc`, `freesurfer_measures`).
    ///
    /// Usually it is [`DeclaredTable::table`] — the table the file routed to — copied through.
    /// A reader names one itself only for a payload the routing cannot reach, which is why one
    /// call returns a vector rather than one batch: a single `?h.aparc.stats` yields both
    /// per-structure rows, whose table the projected suffix decides, and `# Measure` rows, whose
    /// table it cannot.
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
    /// by concept. `declared` is the table those same values route to, so a reader takes its
    /// target from the schema rather than restating a name the schema already holds.
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
        declared: Option<DeclaredTable<'_>>,
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
///
/// Short, and meant to stay so: a file whose columns the schema can name positionally belongs
/// to the `matrix` engine, not here. `fs_stats` earns its place because a `.stats` file carries
/// its column names in a comment and holds a second payload besides.
pub fn default_readers() -> HashMap<String, Box<dyn ContentReader>> {
    let mut readers: HashMap<String, Box<dyn ContentReader>> = HashMap::new();
    readers.insert(
        "fs_stats".to_string(),
        Box::new(freesurfer_stats::FreeSurferStats),
    );
    readers
}
