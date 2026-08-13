//! BIDS writes `n/a` for a missing value, including in columns the schema types as numeric.
//! That must become `NULL` rather than aborting the insert — an aborted row would take every
//! other field on it with it, and inside the ingest transaction would poison the whole run.

use bidslake::db::BidsDb;
use bidslake::schema::Schema;
use serde_json::json;
use tempfile::TempDir;

#[test]
fn na_in_a_numeric_column_becomes_null() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let db_path = temp_dir.path().join("test_na.duckdb");
    let db = BidsDb::new(db_path.to_str().unwrap())?;
    let schema = Schema::load(None).unwrap();
    db.create_tables(&schema)?;

    // `sidecars` foreign-keys into the registry, so the file has to exist first.
    let file_id = "987654321";
    db.insert(
        &schema,
        "file_registry",
        &json!({
            "file_id": file_id,
            "dataset_id": "ds000002",
            "root_uri": "file:///r",
            "file_path": "sub-01/func/sub-01_task-x_run-01_bold.nii.gz",
            "kind": "data",
        }),
    )?;

    // `RepetitionTime` is a DOUBLE; `EchoTime` beside it is a real number, so this also pins
    // that one bad value does not cost the rest of the row.
    db.insert(
        &schema,
        "sidecars",
        &json!({ "file_id": file_id, "RepetitionTime": "n/a", "EchoTime": 0.03 }),
    )?;

    let (tr, te): (Option<f64>, Option<f64>) = db.conn.query_row(
        "SELECT RepetitionTime, EchoTime FROM sidecars WHERE file_id = ?",
        [file_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!(tr, None, "`n/a` in a numeric column must store NULL");
    assert_eq!(te, Some(0.03), "and must not cost the rest of the row");
    Ok(())
}
