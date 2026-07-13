//! Thin wrapper over the DuckDB connection.
//!
//! [`BidsDb`] owns the `duckdb::Connection` and exposes the write primitives the
//! ingestion pipeline uses. Row shaping and SQL generation for these methods live
//! in [`crate::schema`]; this module just routes calls to it and holds the two
//! hand-written insert paths ([`BidsDb::insert_diffusion`],
//! [`BidsDb::insert_file_association`]) for the static tables.
//!
//! Note that the tabular ingest in [`crate::bids`] (and the driver in `main`) also
//! execute their own hand-built SQL directly against the public [`BidsDb::conn`] —
//! the batched `read_csv` inserts, re-index `DELETE`s, and count-back `SELECT`s — by
//! design; this module deliberately does not gate every statement.

use crate::schema::{self, Schema};
use duckdb::{Connection, Result, params};
use serde_json::Value;

/// Disposition of a tabular file in the `tabular_files` catalog (see
/// [`schema::CREATE_TABULAR_FILES_TABLE`] for the column's documentation). A
/// closed set as a type — not a `&str` — so a typo can't silently corrupt the
/// tabular-coverage invariant this column backs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabularStatus {
    /// Its rows are in `table_name` (`n_rows` of them).
    Ingested,
    /// A compressed continuous recording left on disk; `table_name` names the
    /// table it *would* map to.
    OnDisk,
    /// A tabular file the BIDS schema does not describe (`table_name` NULL).
    Skipped,
    /// A batch `INSERT` execution failure dropped this file's rows for the run.
    Failed,
}

impl TabularStatus {
    /// The literal stored in the `status` column.
    pub fn as_str(self) -> &'static str {
        match self {
            TabularStatus::Ingested => "ingested",
            TabularStatus::OnDisk => "on_disk",
            TabularStatus::Skipped => "skipped",
            TabularStatus::Failed => "failed",
        }
    }
}

/// A cross-reference between two files derived at ingest (e.g. an fmap's
/// `IntendedFor`). Written to the `file_associations` table.
#[derive(Debug, Clone)]
pub struct FileAssociation {
    pub dataset_id: String,
    pub source_file: String,
    pub target_file: String,
    pub assoc_type: String,
}

/// Owns the DuckDB connection bidslake writes to and queries.
pub struct BidsDb {
    pub conn: Connection,
}

impl BidsDb {
    /// Open (or create) the database at `path`. Use `":memory:"` for a transient
    /// in-memory database.
    pub fn new(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        Ok(Self { conn })
    }

    /// Create every table: the schema-generated ones ([`Schema::create_tables_sql`])
    /// plus the static `diffusion` and `file_associations` tables.
    pub fn create_tables(&self, schema: &Schema) -> Result<()> {
        let sqls = schema.create_tables_sql();
        for sql in sqls {
            self.conn.execute(&sql, [])?;
        }
        // Create static tables (diffusion and file associations)
        self.conn.execute(schema::CREATE_DIFFUSION_TABLE, [])?;
        self.conn
            .execute(schema::CREATE_FILE_ASSOCIATIONS_TABLE, [])?;
        self.conn.execute(schema::CREATE_TABULAR_FILES_TABLE, [])?;
        self.stamp_meta(schema)?;
        Ok(())
    }

    /// Record which BIDS schema version (and bidslake build) produced this
    /// catalog, in a one-row `bidslake_meta` table. Downstream readers (notably
    /// the Python query package) compare this to what they were generated
    /// against, so a version mismatch is *detectable* rather than guessed —
    /// they then fall back to runtime column introspection. Idempotent across
    /// re-indexing: the row is inserted only if the table is empty.
    fn stamp_meta(&self, schema: &Schema) -> Result<()> {
        let raw = schema.raw();
        let schema_version = raw
            .get("schema_version")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let bids_version = raw
            .get("bids_version")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS bidslake_meta (\
             schema_version TEXT, bids_version TEXT, bidslake_version TEXT)",
            [],
        )?;
        self.conn.execute(
            "INSERT INTO bidslake_meta (schema_version, bids_version, bidslake_version) \
             SELECT ?, ?, ? WHERE NOT EXISTS (SELECT 1 FROM bidslake_meta)",
            params![schema_version, bids_version, env!("CARGO_PKG_VERSION")],
        )?;
        Ok(())
    }

    /// Record a tabular file's [`TabularStatus`] disposition. Backs the
    /// tabular-data invariant. `INSERT OR REPLACE` keeps it correct across
    /// re-indexing.
    pub fn record_tabular_file(
        &self,
        dataset_id: &str,
        file_path: &str,
        table_name: Option<&str>,
        n_rows: i64,
        status: TabularStatus,
    ) -> anyhow::Result<()> {
        use anyhow::Context as _;
        let mut stmt = self.conn.prepare_cached(
            "INSERT OR REPLACE INTO tabular_files (dataset_id, file_path, table_name, n_rows, status) VALUES (?, ?, ?, ?, ?)",
        )?;
        stmt.execute(params![
            dataset_id,
            file_path,
            table_name,
            n_rows,
            status.as_str()
        ])
        .with_context(|| format!("recording tabular file {file_path} as {}", status.as_str()))?;
        Ok(())
    }

    /// Insert one row (`data`, a JSON object) into a schema-generated table,
    /// mapping keys to columns via [`Schema::insert`].
    pub fn insert(&self, schema: &Schema, table_name: &str, data: &Value) -> Result<()> {
        schema.insert(&self.conn, table_name, data)?;
        Ok(())
    }

    /// Bulk-insert many rows into a schema-generated table via the DuckDB
    /// **Appender**. This bypasses SQL planning entirely — crucial for the tables
    /// that carry the generated (virtual) BIDS-concept columns (`scans`,
    /// `sidecars`): a row-by-row `INSERT` re-parses all ~40 of those column regexes
    /// per statement (~10 ms/row), whereas the Appender writes the physical columns
    /// directly (measured ~300× faster). Each row's values are shaped exactly like
    /// [`Schema::insert`] via [`Schema::row_values`], so the result is identical to
    /// inserting them one at a time. The caller is responsible for primary-key
    /// dedup (the Appender does not run the insert-if-not-exists guard).
    pub fn append_rows(&self, schema: &Schema, table_name: &str, rows: &[Value]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut appender = self.conn.appender(table_name)?;
        for row in rows {
            let values = schema.row_values(table_name, row)?;
            let refs: Vec<&dyn duckdb::ToSql> =
                values.iter().map(|v| v as &dyn duckdb::ToSql).collect();
            appender.append_row(refs.as_slice())?;
        }
        appender.flush()?;
        Ok(())
    }

    /// Insert one derived file association into `file_associations`.
    pub fn insert_file_association(&self, assoc: &FileAssociation) -> Result<()> {
        // Static SQL — prepare_cached reuses the plan across all associations.
        let mut stmt = self.conn.prepare_cached(
            "INSERT INTO file_associations (dataset_id, source_file_path, target_file_path, association_type) VALUES (?, ?, ?, ?)",
        )?;
        stmt.execute(params![
            &assoc.dataset_id,
            &assoc.source_file,
            &assoc.target_file,
            &assoc.assoc_type
        ])?;
        Ok(())
    }

    /// Insert one diffusion NIfTI's parsed `.bval` / `.bvec` values, **one row per
    /// volume**. The four arrays are aligned by index (BIDS guarantees the same
    /// length); a shorter `.bvec` yields NULL for the missing components. Uses the
    /// bulk Appender path.
    pub fn insert_diffusion(
        &self,
        dataset_id: &str,
        file_path: &str,
        bval: &[f64],
        bvec_x: &[f64],
        bvec_y: &[f64],
        bvec_z: &[f64],
    ) -> Result<()> {
        use duckdb::types::Value;
        let component = |v: &[f64], i: usize| v.get(i).copied().map_or(Value::Null, Value::Double);

        let mut appender = self.conn.appender("diffusion")?;
        for (i, &b) in bval.iter().enumerate() {
            appender.append_row([
                Value::Text(dataset_id.to_string()),
                Value::Text(file_path.to_string()),
                Value::BigInt(i as i64),
                Value::Double(b),
                component(bvec_x, i),
                component(bvec_y, i),
                component(bvec_z, i),
            ])?;
        }
        appender.flush()?;
        Ok(())
    }
}
