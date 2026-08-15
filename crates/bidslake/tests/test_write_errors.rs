//! What a failed write tells you.
//!
//! The write layer used to report its own bookkeeping failures as
//! `duckdb::Error::ToSqlConversionFailure` wrapping a fabricated `io::Error(NotFound)` — an
//! I/O error, for a table missing from an in-memory map. That existed only so the function
//! could keep returning `duckdb::Result`, and it cost the message: a caller saw a
//! conversion failure and had to guess.
//!
//! These pin the replacements, and the context the call site cannot supply. `bids.rs`
//! knows it is writing `sidecars`; only the write layer knows which row, which column
//! count, and which constraint.

use bidslake::db::BidsDb;
use bidslake::schema::Schema;
use serde_json::json;

fn chain(e: &anyhow::Error) -> String {
    format!("{e:#}")
}

#[test]
fn unknown_table_says_so_plainly() {
    let schema = Schema::load(None).unwrap();
    let err = schema
        .row_values("no_such_table", &json!({"a": 1}))
        .unwrap_err();
    let msg = chain(&err);
    assert!(msg.contains("no_such_table"), "{msg}");
    assert!(msg.contains("not found in schema"), "{msg}");
    // The shape it used to have. An absent key in a HashMap is not a conversion failure.
    assert!(!msg.contains("ToSqlConversionFailure"), "{msg}");
}

#[test]
fn a_bad_row_names_the_table_and_its_keys() {
    let db = BidsDb::new(":memory:").unwrap();
    let schema = Schema::load(None).unwrap();
    db.create_tables(&schema).unwrap();

    // A `file_id` that is not an integer. `row_values` cannot shape it, substitutes NULL,
    // and the registry's NOT NULL constraint rejects the row -- so the failure surfaces
    // from the staged INSERT, several frames from where the bad value came in. That is
    // exactly the case the added context is for.
    let err = db
        .upsert_rows(
            &schema,
            "file_registry",
            &[json!({
                "file_id": "not-an-integer",
                "dataset_id": "ds",
                "root_uri": "file:///tmp/ds",
                "file_path": "sub-01/anat/sub-01_T1w.nii.gz",
                "kind": "data",
            })],
        )
        .unwrap_err();
    let msg = chain(&err);
    assert!(
        msg.contains("file_registry"),
        "should name the table: {msg}"
    );
    assert!(
        msg.contains("upserting 1 rows"),
        "should say what the write layer was doing: {msg}"
    );
    assert!(
        msg.contains("NOT NULL constraint"),
        "should keep DuckDB's own diagnostic: {msg}"
    );
    // And should not trail off into the FFI leaf, which is the same constant every time.
    assert!(!msg.contains("Error code"), "FFI noise leaked: {msg}");
}

#[test]
fn a_good_row_still_writes() {
    // The other half: none of the added context changes what succeeds.
    let db = BidsDb::new(":memory:").unwrap();
    let schema = Schema::load(None).unwrap();
    db.create_tables(&schema).unwrap();
    db.upsert_rows(
        &schema,
        "file_registry",
        &[json!({
            "file_id": "12345678901234567890",
            "dataset_id": "ds",
            "root_uri": "file:///tmp/ds",
            "file_path": "sub-01/anat/sub-01_T1w.nii.gz",
            "kind": "data",
        })],
    )
    .unwrap();
    let n: i64 = db
        .conn
        .query_row("SELECT count(*) FROM file_registry", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
}
