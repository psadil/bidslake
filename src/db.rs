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

    /// Insert one diffusion row: the parsed `.bval` / `.bvec` arrays for a
    /// diffusion NIfTI, stored as DuckDB `DOUBLE[]` list columns.
    pub fn insert_diffusion(
        &self,
        dataset_id: &str,
        file_path: &str,
        bval: &[f64],
        bvec_x: &[f64],
        bvec_y: &[f64],
        bvec_z: &[f64],
    ) -> Result<()> {
        // Convert Rust vectors to DuckDB list format
        let bval_list = format!(
            "[{}]",
            bval.iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        let bvec_x_list = format!(
            "[{}]",
            bvec_x
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        let bvec_y_list = format!(
            "[{}]",
            bvec_y
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        let bvec_z_list = format!(
            "[{}]",
            bvec_z
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );

        let sql = format!(
            "INSERT INTO diffusion (dataset_id, file_path, bval, bvec_x, bvec_y, bvec_z) VALUES (?, ?, {}, {}, {}, {})",
            bval_list, bvec_x_list, bvec_y_list, bvec_z_list
        );

        self.conn.execute(&sql, params![dataset_id, file_path])?;
        Ok(())
    }
}
