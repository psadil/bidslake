//! Thin wrapper over the DuckDB connection.
//!
//! [`BidsDb`] owns the `duckdb::Connection` and exposes the write primitives the
//! ingestion pipeline uses. Row shaping and SQL generation live in
//! [`crate::schema`]; this module just routes calls to it and holds the two
//! hand-written insert paths ([`BidsDb::insert_diffusion`],
//! [`BidsDb::insert_file_association`]) for the static tables.

use crate::schema::{self, Schema};
use duckdb::{Connection, Result, params};
use serde_json::Value;

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
        // Files table removed - no longer needed
        // self.conn.execute(schema::CREATE_FILES_TABLE, [])?;
        self.conn.execute(schema::CREATE_DIFFUSION_TABLE, [])?;
        self.conn
            .execute(schema::CREATE_FILE_ASSOCIATIONS_TABLE, [])?;
        self.conn.execute(schema::CREATE_TABULAR_FILES_TABLE, [])?;
        Ok(())
    }

    /// Record a tabular file's disposition (`ingested` / `on_disk` / `skipped`).
    /// Backs the tabular-data invariant. `INSERT OR REPLACE` keeps it correct
    /// across re-indexing.
    pub fn record_tabular_file(
        &self,
        dataset_id: &str,
        file_path: &str,
        table_name: Option<&str>,
        n_rows: i64,
        status: &str,
    ) -> Result<()> {
        let mut stmt = self.conn.prepare_cached(
            "INSERT OR REPLACE INTO tabular_files (dataset_id, file_path, table_name, n_rows, status) VALUES (?, ?, ?, ?, ?)",
        )?;
        stmt.execute(params![dataset_id, file_path, table_name, n_rows, status])?;
        Ok(())
    }

    /// Insert one row (`data`, a JSON object) into a schema-generated table,
    /// mapping keys to columns via [`Schema::insert`].
    pub fn insert(&self, schema: &Schema, table_name: &str, data: &Value) -> Result<()> {
        schema.insert(&self.conn, table_name, data)?;
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
