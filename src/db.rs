use crate::schema::{self, Schema};
use duckdb::{params, Connection, Result};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct FileAssociation {
    pub dataset_id: String,
    pub source_file: String,
    pub target_file: String,
    pub assoc_type: String,
}

pub struct BidsDb {
    pub conn: Connection,
}

impl BidsDb {
    pub fn new(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        Ok(Self { conn })
    }

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

    pub fn insert(&self, schema: &Schema, table_name: &str, data: &Value) -> Result<()> {
        schema.insert(&self.conn, table_name, data)?;
        Ok(())
    }

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
