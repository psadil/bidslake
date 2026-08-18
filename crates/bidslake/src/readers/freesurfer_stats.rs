//! Reader for FreeSurfer `stats/*.stats` files (the `aseg`/`wmparc` and `aparc` families).
//!
//! Schema-on-read, mirroring FreeSurfer's own format (and a2cps's `freesurfer_wf.py`):
//! `# Measure <key>, <shortname>, <desc>, <value>, <units>` lines become one row in
//! `freesurfer_measures`; the `# ColHeaders …` line names the per-structure columns; each
//! subsequent whitespace-delimited line is one structure row. Values are emitted as raw strings
//! and typed by [`Schema::row_values`](crate::schema::Schema).
//!
//! Two things keep this a reader rather than a use of the `matrix` engine, and they are
//! different things:
//!
//! - **The column names are in the file**, on the `# ColHeaders` line, so they are matched by
//!   name. Positions would also work for one FreeSurfer version and quietly mis-assign under
//!   the next, since `ColHeaders` is what changes when a release adds or reorders a column.
//! - **A `.stats` file holds two payloads.** The per-structure rows go to the table the
//!   projected suffix routes to (`segstats` → `freesurfer_aseg`, `parcstats` →
//!   `freesurfer_aparc`), but the `# Measure` scalars are whole-hemisphere facts that belong in
//!   `freesurfer_measures` — and no projection of one path yields two tables, so that one name
//!   is stated here.

use bids_schema::term_map::FileFacts;
use serde_json::Value;

use super::{ContentReader, DeclaredTable, ReaderRows, seed_row};
use uuid::Uuid;

pub struct FreeSurferStats;

impl ContentReader for FreeSurferStats {
    fn read(
        &self,
        file_id: Uuid,
        content: &str,
        facts: &FileFacts,
        declared: Option<DeclaredTable<'_>>,
    ) -> anyhow::Result<Vec<ReaderRows>> {
        let mut col_headers: Vec<String> = Vec::new();
        let mut measures: Vec<(String, String)> = Vec::new();
        let mut data_lines: Vec<&str> = Vec::new();

        for line in content.lines() {
            let trimmed = line.trim_start();
            if let Some(rest) = trimmed.strip_prefix('#') {
                let rest = rest.trim();
                if let Some(cols) = rest.strip_prefix("ColHeaders") {
                    col_headers = cols.split_whitespace().map(String::from).collect();
                } else if let Some(measure) = rest.strip_prefix("Measure") {
                    // `Measure <structkey>, <shortname>, <description>, <value>, <units>`
                    let parts: Vec<&str> = measure.split(',').map(str::trim).collect();
                    if parts.len() >= 4 && !parts[1].is_empty() {
                        measures.push((parts[1].to_string(), parts[3].to_string()));
                    }
                }
            } else if !trimmed.is_empty() {
                data_lines.push(line);
            }
        }

        // The per-structure table is the schema's answer, not this reader's: `aseg.stats` and
        // `wmparc.stats` project `suffix: segstats`, `?h.*.stats` projects `parcstats`, and the
        // overlay's rules turn those into `freesurfer_aseg` and `freesurfer_aparc`. `None` means
        // no rule claimed the file, in which case there is no table to write to — the
        // `# Measure` rows below still go out, since their table is named here.
        let data_table = declared.map(|d| d.table);

        let mut out = Vec::new();

        if let Some(table) = data_table {
            let mut rows = Vec::with_capacity(data_lines.len());
            for (row_idx, line) in data_lines.iter().enumerate() {
                let tokens: Vec<&str> = line.split_whitespace().collect();
                if col_headers.is_empty() || tokens.len() != col_headers.len() {
                    continue; // schema-on-read: skip malformed/short rows
                }
                let mut obj = seed_row(file_id, facts);
                obj.insert("row_idx".to_string(), Value::from(row_idx as i64));
                for (header, token) in col_headers.iter().zip(&tokens) {
                    obj.insert(header.clone(), Value::String((*token).to_string()));
                }
                rows.push(Value::Object(obj));
            }
            if !rows.is_empty() {
                out.push(ReaderRows {
                    table: table.to_string(),
                    rows,
                });
            }
        }

        // `# Measure` scalars -> one row in freesurfer_measures for this file.
        if !measures.is_empty() {
            let mut obj = seed_row(file_id, facts);
            obj.insert("row_idx".to_string(), Value::from(0i64));
            for (short_name, value) in &measures {
                obj.insert(short_name.clone(), Value::String(value.clone()));
            }
            out.push(ReaderRows {
                table: "freesurfer_measures".to_string(),
                rows: vec![Value::Object(obj)],
            });
        }

        Ok(out)
    }
}
