//! `other_data` must keep every key a table does not declare — including two that differ
//! only in case.
//!
//! `row_values` folds the row's keys to lowercase once, so the per-column lookup is a hash
//! hit rather than a rescan of the row. That index is keyed on the folded spelling, so it
//! cannot also be the source for the overflow: two undeclared keys differing only in case
//! collapse to one entry, and whichever loses is dropped with no warning. The overflow
//! therefore iterates the row itself.
//!
//! BIDS metadata is CamelCase by convention, so this needs a sidecar with custom fields to
//! trigger — which is exactly what a converter or a lab pipeline writes.

use bidslake::db::BidsDb;
use bidslake::schema::Schema;
use serde_json::json;

const FILE: &str = "sub-01/func/sub-01_task-rest_bold.nii.gz";
/// Any stable id will do — these tests never resolve it back to a path.
const FILE_ID: &str = "424242";

/// A catalog with the standard schema and the `file_registry` row `sidecars` keys against —
/// that foreign key is enforced on this insert path, so the parent has to exist first.
fn db() -> anyhow::Result<(BidsDb, Schema)> {
    let db = BidsDb::new(":memory:")?;
    let schema = Schema::load(None)?;
    db.create_tables(&schema)?;
    db.insert(
        &schema,
        "file_registry",
        &json!({
            "file_id": FILE_ID,
            "dataset_id": "d",
            "root_uri": "file:///r",
            "file_path": FILE,
            "kind": "data",
        }),
    )?;
    Ok((db, schema))
}

/// The regression: both spellings survive into `other_data`.
#[test]
fn case_colliding_undeclared_keys_both_survive() -> anyhow::Result<()> {
    let (db, schema) = db()?;
    db.insert(
        &schema,
        "sidecars",
        &json!({
            "file_id": FILE_ID,
            "CustomField": "upper",
            "customfield": "lower",
        }),
    )?;

    let other: String = db.conn.query_row(
        "SELECT other_data FROM sidecars JOIN all_files USING (file_id) WHERE dataset_id = 'd'",
        [],
        |r| r.get(0),
    )?;
    let parsed: serde_json::Value = serde_json::from_str(&other)?;

    assert_eq!(
        parsed.get("CustomField").and_then(|v| v.as_str()),
        Some("upper"),
        "other_data: {other}"
    );
    assert_eq!(
        parsed.get("customfield").and_then(|v| v.as_str()),
        Some("lower"),
        "other_data: {other}"
    );
    Ok(())
}

/// The fold the overflow replaced is still doing its other job: a declared field reaches its
/// own column whichever way it is spelled, and is *not* duplicated into `other_data`.
#[test]
fn a_declared_field_is_matched_case_insensitively_and_not_duplicated() -> anyhow::Result<()> {
    let (db, schema) = db()?;
    db.insert(
        &schema,
        "sidecars",
        &json!({
            "file_id": FILE_ID,
            // The BIDS field is `RepetitionTime`; this is the spelling the DDL dropped.
            "repetitiontime": 2.0,
            "OnlyCustom": 1,
        }),
    )?;

    let (rt, other): (Option<f64>, Option<String>) = db.conn.query_row(
        "SELECT RepetitionTime, other_data FROM sidecars JOIN all_files USING (file_id) WHERE dataset_id = 'd'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!(rt, Some(2.0), "declared column populated");

    let other = other.expect("other_data");
    let parsed: serde_json::Value = serde_json::from_str(&other)?;
    assert!(
        parsed.get("repetitiontime").is_none(),
        "a declared field must not also land in other_data: {other}"
    );
    assert_eq!(parsed.get("OnlyCustom").and_then(|v| v.as_i64()), Some(1));
    Ok(())
}
