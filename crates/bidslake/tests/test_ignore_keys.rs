//! `ignoreKeys` — the key-level counterpart of the `ignore` disposition.
//!
//! Motivated by dcmstack's DcmMeta, which attaches per-slice DICOM dumps to otherwise
//! ordinary sidecars under `global` and `time` — megabytes per file, of no use in a
//! catalog. The existing dials are the wrong shape for that: `undeclared: catalog`
//! would discard every other custom field along with it.

mod common;

use bidslake::bids::BidsParser;
use bidslake::db::BidsDb;
use bidslake::fs::LocalFileSystem;
use bidslake::schema::{Ingestion, Schema};
use std::fs;

/// Ingest `root` with an ingestion fragment layered over the base policy.
async fn ingest_with(root: &std::path::Path, fragment: &str) -> anyhow::Result<BidsDb> {
    let db = BidsDb::new(":memory:")?;
    let base = bids_schema::bundled_ingestion_source("base").expect("base ingestion");
    let ingestion = Ingestion::from_sources(&[base, fragment])?;
    let schema = Schema::load_full(None, &[], ingestion, &[])?;
    db.create_tables(&schema)?;
    let fs_backend = Box::new(LocalFileSystem::new(root.to_path_buf()));
    let mut parser = BidsParser::new(fs_backend, None, schema, None, true);
    let txn = db.conn.unchecked_transaction()?;
    parser.parse(&db).await?;
    txn.commit()?;
    Ok(db)
}

/// A sidecar carrying both a huge non-BIDS blob and ordinary metadata.
fn write_dataset(root: &std::path::Path) -> std::io::Result<()> {
    fs::create_dir_all(root.join("sub-01/func"))?;
    fs::write(
        root.join("dataset_description.json"),
        r#"{"Name": "dcmmeta", "BIDSVersion": "1.8.0"}"#,
    )?;
    fs::write(
        root.join("sub-01/func/sub-01_task-rest_bold.nii.gz"),
        "not really nifti",
    )?;
    fs::write(
        root.join("sub-01/func/sub-01_task-rest_bold.json"),
        r#"{"RepetitionTime": 2.0, "global": {"slices": [1, 2, 3]},
            "time": [0, 1, 2], "dcmmeta_affine": [1, 0, 0]}"#,
    )?;
    Ok(())
}

const FRAGMENT: &str = r#"{
  "IngestionSchemaVersion": "0.1.0",
  "tables": { "sidecars": { "ignoreKeys": ["global", "time"] } }
}"#;

const NO_IGNORES: &str = r#"{ "IngestionSchemaVersion": "0.1.0" }"#;

/// Named keys are dropped; everything else — declared column and undeclared overflow
/// alike — is untouched.
#[tokio::test]
async fn ignored_keys_are_dropped_and_the_rest_kept() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    write_dataset(tmp.path())?;
    let db = ingest_with(tmp.path(), FRAGMENT).await?;

    let (tr, other): (Option<f64>, Option<String>) = db.conn.query_row(
        "SELECT RepetitionTime, other_data FROM sidecars JOIN all_files USING (file_id) WHERE file_path LIKE '%_bold.nii.gz'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;

    // A declared column is unaffected.
    assert_eq!(tr, Some(2.0));

    let other = other.expect("undeclared metadata should still be stored");
    assert!(
        !other.contains("\"global\"") && !other.contains("\"time\""),
        "ignored keys must not reach the catalog; got {other}"
    );
    // Crucially, the *other* custom field survives — which is what distinguishes this
    // from `undeclared: catalog`, which would have dropped it too.
    assert!(
        other.contains("dcmmeta_affine"),
        "unignored custom metadata must be kept; got {other}"
    );
    Ok(())
}

/// Without the policy the same keys are stored, so the test above is measuring the
/// policy rather than some unrelated filtering.
#[tokio::test]
async fn the_same_keys_are_kept_without_the_policy() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    write_dataset(tmp.path())?;
    let db = ingest_with(tmp.path(), NO_IGNORES).await?;

    let other: Option<String> = db.conn.query_row(
        "SELECT other_data FROM sidecars JOIN all_files USING (file_id) WHERE file_path LIKE '%_bold.nii.gz'",
        [],
        |r| r.get(0),
    )?;
    let other = other.expect("undeclared metadata should be stored");
    assert!(
        other.contains("\"global\"") && other.contains("\"time\""),
        "without `ignoreKeys` these keys are ordinary metadata; got {other}"
    );
    Ok(())
}

/// `ignoreKeys` lives on the shared table policy, so a fragment may name it on any table — but
/// it acts as the JSON is parsed, and `sidecars` is the only table fed from parsed JSON. Naming
/// it on a tabular table therefore validates, loads, and does nothing. That is documented in the
/// metaschema; assert it, so the documented behaviour and the real behaviour cannot part company
/// (and so that making it *work* there would fail here loudly rather than pass unnoticed).
#[tokio::test]
async fn ignore_keys_on_a_tabular_table_is_inert() -> anyhow::Result<()> {
    const ON_EVENTS: &str = r#"{
      "IngestionSchemaVersion": "0.1.0",
      "tables": { "events": { "ignoreKeys": ["trial_type"] } }
    }"#;

    let dir = tempfile::tempdir()?;
    let root = dir.path();
    fs::create_dir_all(root.join("sub-01/func"))?;
    fs::write(
        root.join("dataset_description.json"),
        r#"{"Name": "inert", "BIDSVersion": "1.8.0"}"#,
    )?;
    fs::write(
        root.join("sub-01/func/sub-01_task-rest_bold.nii.gz"),
        b"nii",
    )?;
    fs::write(
        root.join("sub-01/func/sub-01_task-rest_events.tsv"),
        "onset\tduration\ttrial_type\n0.0\t1.0\tgo\n2.0\t1.0\tstop\n",
    )?;

    let db = ingest_with(root, ON_EVENTS).await?;

    // The named column is a *declared* events column, and it is stored regardless.
    let kept: i64 = db.conn.query_row(
        "SELECT count(*) FROM events WHERE trial_type IS NOT NULL",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(
        kept, 2,
        "ignoreKeys on a tabular table must not drop its columns — the column-level dial \
         there is `undeclared`"
    );
    Ok(())
}
