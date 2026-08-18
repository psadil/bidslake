//! File bodies, built from the table a path routes to.
//!
//! The header of a generated table is not a list in this crate. The path is built first, a
//! [`FileContext`] is made from it, and `Schema::tabular().route()` — the very call the ingest
//! makes — names the table and hands back its columns in declared order. So an overlay that adds
//! a column produces a wider generated file with no code change here, which is the whole of what
//! "schema-driven" buys on the contents side.
//!
//! **A test comparing a generated header back to that `TableSpec` proves nothing.** Both sides
//! would be the same call. It is a useful *consistency* check — every declared table got
//! exercised — and it is not a correctness one; correctness lives in the hand-written fixtures
//! under `crates/bidslake/tests/`, where the expected columns are written out by a human.
//!
//! Three shapes, because the three engines read three formats: `csv` wants a header row,
//! `matrix` is positional and headerless, and `fs_stats` carries its column names inside the
//! file on a `# ColHeaders` line.

use std::collections::BTreeMap;

use bidslake::schema::Schema;
use bidslake::schema::tabular::{FileContext, TableSpec};
use serde_json::Value;

use crate::values::{Kind, cell, kind_for_key};

/// A column's header and the kind of value to put under it.
struct Column {
    name: String,
    kind: Kind,
}

fn columns_of(schema: &Schema, spec: &TableSpec) -> Vec<Column> {
    spec.columns
        .iter()
        .map(|c| Column {
            name: c.name.clone(),
            kind: kind_for_key(schema.raw(), &c.key),
        })
        .collect()
}

/// Route `path` the way ingest would, and return the table it lands in.
///
/// `datatype`/`suffix`/`extension` are passed rather than re-derived because the caller already
/// knows them: for a BIDS-named file it built the name, and for a projected one the term map
/// answered.
pub fn route<'a>(
    schema: &'a Schema,
    path: &str,
    datatype: Option<&str>,
    suffix: Option<&str>,
    extension: Option<&str>,
    dataset_type: Option<&str>,
) -> Option<&'a TableSpec> {
    let slashed = format!("/{}", path.trim_start_matches('/'));
    let null = Value::Null;
    schema.tabular().route(&FileContext {
        path: &slashed,
        datatype,
        suffix,
        extension,
        sidecar: &null,
        dataset_type,
    })
}

/// A tab-separated table with a header row: what the batched `csv` engine reads.
///
/// `overrides` supplies whole columns the schema cannot invent — `filename` in a `scans.tsv` has
/// `format: participant_relative` and has to name files that exist, and `participant_id` has to
/// match a subject directory. A column not overridden is filled from its declared kind.
///
/// A [`Kind::NotApplicable`] column with no override is **dropped**, header and all. Emitting one
/// would add a column that can only ever say `n/a`, and for `HED` specifically it would ask the
/// dataset for a `HEDVersion` it does not declare — a warning on every table, for a column
/// carrying nothing. Every such column is optional in the schema, so dropping it is legal.
pub fn tsv(
    schema: &Schema,
    spec: &TableSpec,
    rows: usize,
    overrides: &BTreeMap<String, Vec<String>>,
) -> String {
    let mut columns = columns_of(schema, spec);
    columns.retain(|c| c.kind != Kind::NotApplicable || overrides.contains_key(&c.name));
    let mut out = String::new();
    out.push_str(
        &columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>()
            .join("\t"),
    );
    out.push('\n');
    for row in 0..rows {
        let line: Vec<String> = columns
            .iter()
            .map(|c| match overrides.get(&c.name) {
                Some(values) => values
                    .get(row)
                    .cloned()
                    .unwrap_or_else(|| "n/a".to_string()),
                None => cell(&c.kind, &c.name, row),
            })
            .collect();
        out.push_str(&line.join("\t"));
        out.push('\n');
    }
    out
}

/// A headerless, whitespace-delimited table: what the `matrix` engine reads.
///
/// Column *i* of the table is field *i* of the line, so the width is the declared column count
/// and nothing names anything. Two spaces between fields rather than one, matching FSL's
/// `.par` files and FreeSurfer's `.ctab`, both of which separate by runs.
pub fn matrix(schema: &Schema, spec: &TableSpec, rows: usize) -> String {
    let columns = columns_of(schema, spec);
    let mut out = String::new();
    for row in 0..rows {
        let line: Vec<String> = columns
            .iter()
            .map(|c| cell(&c.kind, &c.name, row))
            .collect();
        out.push_str(&line.join("  "));
        out.push('\n');
    }
    out
}

/// A FreeSurfer `.stats` file: `# Measure` scalars, a `# ColHeaders` line, then the rows.
///
/// The two payloads go to two tables — the per-structure rows to whatever `spec` names, the
/// scalars always to `freesurfer_measures` — so this reads the measures table out of the same
/// schema rather than listing the names. That is the one place a generated file has to agree
/// with a *reader* rather than with a routing rule: `fs_stats` matches its columns **by name**
/// off the `ColHeaders` line, so a header this function invents would be read as an undeclared
/// column and land nowhere.
pub fn fs_stats(schema: &Schema, spec: &TableSpec, rows: usize) -> String {
    let mut out = String::from("# Title Segmentation Statistics\n#\n");

    if let Some(measures) = schema
        .tabular()
        .tables()
        .iter()
        .find(|t| t.table == "freesurfer_measures")
    {
        for (i, column) in columns_of(schema, measures).iter().enumerate() {
            // `Measure <structkey>, <shortname>, <description>, <value>, <units>`; the reader
            // keys on the *second* field and reads the fourth.
            let value = cell(&Kind::Number, &column.name, i);
            out.push_str(&format!(
                "# Measure {n}, {n}, {n}, {value}, unitless\n",
                n = column.name
            ));
        }
    }

    let columns = columns_of(schema, spec);
    out.push_str("# ColHeaders");
    for column in &columns {
        out.push(' ');
        out.push_str(&column.name);
    }
    out.push('\n');

    for row in 0..rows {
        let line: Vec<String> = columns
            .iter()
            .map(|c| cell(&c.kind, &c.name, row))
            .collect();
        out.push_str(&line.join("  "));
        out.push('\n');
    }
    out
}

/// A wide `*_desc-confounds_timeseries.tsv`, the shape fMRIPrep actually writes.
///
/// Declared columns first, in the order the overlay lists them, then `extra` undeclared ones —
/// the ~1,800 CompCor and cosine regressors that make this file what the per-column storage
/// policy of ADR 0004 exists for. Both halves matter to a benchmark: the declared ones exercise
/// the typed insert, the undeclared ones the policy dial.
///
/// The first row of every column is `n/a`, which is not a hazard but the *normal* shape: every
/// `*_derivative1` regressor has no value at volume zero, and fMRIPrep writes the string into an
/// otherwise-float column. A generator that emitted clean floats would never exercise the
/// conversion, which `test_overlay.rs` already asserts.
pub fn confounds(schema: &Schema, spec: &TableSpec, rows: usize, extra: usize) -> String {
    let declared = columns_of(schema, spec);
    let mut headers: Vec<String> = declared.iter().map(|c| c.name.clone()).collect();
    let mut kinds: Vec<Kind> = declared.iter().map(|c| c.kind.clone()).collect();
    for i in 0..extra {
        headers.push(format!("a_comp_cor_{i:04}"));
        kinds.push(Kind::Number);
    }

    let mut out = String::with_capacity(headers.len() * (rows + 1) * 8);
    out.push_str(&headers.join("\t"));
    out.push('\n');
    for row in 0..rows {
        let line: Vec<String> = kinds
            .iter()
            .zip(&headers)
            .map(|(kind, name)| {
                if row == 0 {
                    "n/a".to_string()
                } else {
                    cell(kind, name, row)
                }
            })
            .collect();
        out.push_str(&line.join("\t"));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use bidslake::schema::AppliedOverlay;

    fn fmriprep_schema() -> Schema {
        let overlay = AppliedOverlay {
            source: "fmriprep".to_string(),
            content: bids_schema::overlay::bundled_overlay("fmriprep").expect("bundled overlay"),
        };
        Schema::load_with_overlays(None, &[overlay]).expect("schema loads")
    }

    fn plain_schema() -> Schema {
        Schema::load(None).expect("schema loads")
    }

    /// The routing this module is built on: a path is enough to find the table, using the same
    /// call the ingest makes. Without the overlay this file routes nowhere, which is why the
    /// benchmark has to pass one.
    #[test]
    fn a_confounds_path_routes_to_the_overlay_table() {
        let schema = fmriprep_schema();

        let spec = route(
            &schema,
            "sub-01/func/sub-01_task-rest_desc-confounds_timeseries.tsv",
            Some("func"),
            Some("timeseries"),
            Some(".tsv"),
            Some("derivative"),
        );

        assert_eq!(spec.map(|s| s.table.as_str()), Some("fmriprep_confounds"));
    }

    /// The same path, against a schema with no overlay merged, routes nowhere — the trap the
    /// benchmark comment warns about, pinned so it stays true.
    #[test]
    fn without_the_overlay_a_confounds_path_routes_nowhere() {
        let schema = plain_schema();

        let spec = route(
            &schema,
            "sub-01/func/sub-01_task-rest_desc-confounds_timeseries.tsv",
            Some("func"),
            Some("timeseries"),
            Some(".tsv"),
            Some("derivative"),
        );

        assert_eq!(spec.map(|s| s.table.as_str()), None);
    }

    #[test]
    fn a_confounds_body_is_declared_columns_then_the_undeclared_tail() {
        let schema = fmriprep_schema();
        let spec = route(
            &schema,
            "sub-01/func/sub-01_task-rest_desc-confounds_timeseries.tsv",
            Some("func"),
            Some("timeseries"),
            Some(".tsv"),
            Some("derivative"),
        )
        .expect("routes");

        let body = confounds(&schema, spec, 3, 5);

        let header: Vec<&str> = body.lines().next().expect("a header").split('\t').collect();
        assert_eq!(
            (header.len(), header.last().copied()),
            (spec.columns.len() + 5, Some("a_comp_cor_0004"))
        );
    }

    /// Volume zero has no derivative, so fMRIPrep writes `n/a` into a float column. Pinned here
    /// because a generator emitting clean floats would silently stop exercising the conversion.
    #[test]
    fn the_first_confounds_row_is_all_na() {
        let schema = fmriprep_schema();
        let spec = route(
            &schema,
            "sub-01/func/sub-01_task-rest_desc-confounds_timeseries.tsv",
            Some("func"),
            Some("timeseries"),
            Some(".tsv"),
            Some("derivative"),
        )
        .expect("routes");

        let body = confounds(&schema, spec, 2, 2);

        let first = body.lines().nth(1).expect("a first data row");
        assert!(
            first.split('\t').all(|c| c == "n/a"),
            "first row was {first:?}"
        );
    }
}
