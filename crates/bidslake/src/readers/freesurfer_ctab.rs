//! Reader for FreeSurfer color lookup tables (`label/*.ctab`, FreeSurferColorLUT format).
//!
//! Each non-comment line is `<id> <name> <r> <g> <b> <a>` (whitespace-delimited; `#` starts
//! a comment). Produces a `freesurfer_labels` dimension, joinable to `freesurfer_aseg` on
//! `seg_id`/`struct_name`. Values are emitted as raw strings and typed by
//! [`Schema::row_values`](crate::schema::Schema).

use bids_schema::term_map::FileFacts;
use serde_json::Value;

use super::{ContentReader, ReaderRows, seed_row};

pub struct FreeSurferCtab;

impl ContentReader for FreeSurferCtab {
    fn read(
        &self,
        dataset_id: &str,
        file_path: &str,
        content: &str,
        facts: &FileFacts,
    ) -> anyhow::Result<Vec<ReaderRows>> {
        let mut rows = Vec::new();
        for (row_idx, raw) in content.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let tokens: Vec<&str> = line.split_whitespace().collect();
            if tokens.len() < 6 {
                continue; // not an `id name r g b a` row
            }
            let mut obj = seed_row(dataset_id, file_path, facts);
            obj.insert("row_idx".to_string(), Value::from(row_idx as i64));
            obj.insert("seg_id".to_string(), Value::String(tokens[0].to_string()));
            obj.insert(
                "struct_name".to_string(),
                Value::String(tokens[1].to_string()),
            );
            obj.insert("r".to_string(), Value::String(tokens[2].to_string()));
            obj.insert("g".to_string(), Value::String(tokens[3].to_string()));
            obj.insert("b".to_string(), Value::String(tokens[4].to_string()));
            obj.insert("a".to_string(), Value::String(tokens[5].to_string()));
            rows.push(Value::Object(obj));
        }

        if rows.is_empty() {
            return Ok(Vec::new());
        }
        Ok(vec![ReaderRows {
            table: "freesurfer_labels".to_string(),
            rows,
        }])
    }
}
