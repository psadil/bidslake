//! Metadata-only records: JSON sidecars whose data file the dataset never ships.
//!
//! MRIQC is the canonical case — it publishes its image-quality metrics as
//! `sub-…_T1w.json` and writes no `.nii.gz` at all. Because `sidecars` rows are keyed by
//! the data file a sidecar describes, such a sidecar used to be collected and then
//! silently dropped, so every IQM disappeared from the catalog. These tests pin the fix
//! (the JSON becomes the record) *and* its limits: an inheritance template must never be
//! promoted, and an ordinary dataset must be untouched.

mod common;

use bidslake::schema::{AppliedOverlay, Schema};
use common::{count, ingest, ingest_with_schema};
use std::fs;
use std::path::Path;

fn mriqc_overlay() -> AppliedOverlay {
    AppliedOverlay {
        source: "mriqc".to_string(),
        content: bids_schema::overlay::bundled_overlay("mriqc").expect("bundled mriqc overlay"),
    }
}

/// An MRIQC-shaped tree: IQM sidecars only, no imaging data anywhere.
fn write_mriqc_tree(root: &Path) {
    let anat = root.join("sub-01/anat");
    let func = root.join("sub-01/func");
    fs::create_dir_all(&anat).unwrap();
    fs::create_dir_all(&func).unwrap();

    fs::write(
        root.join("dataset_description.json"),
        r#"{"Name":"MRIQC test","BIDSVersion":"1.11.1","DatasetType":"derivative","GeneratedBy":[{"Name":"MRIQC","Version":"23.1.0"}]}"#,
    )
    .unwrap();

    // Anatomical IQMs — note: no sub-01_T1w.nii.gz is ever written.
    fs::write(
        anat.join("sub-01_T1w.json"),
        r#"{"cjv":0.273,"cnr":4.42,"efc":0.76,"snr_total":10.81}"#,
    )
    .unwrap();
    // Functional IQMs, including the motion metric.
    fs::write(
        func.join("sub-01_task-rest_bold.json"),
        r#"{"fd_mean":0.114,"tsnr":64.9,"gcor":0.018}"#,
    )
    .unwrap();
}

#[tokio::test]
async fn mriqc_iqm_sidecars_become_records() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_mriqc_tree(dir.path());

    let schema = Schema::load_with_overlays(None, &[mriqc_overlay()])?;
    let db = ingest_with_schema(dir.path(), schema).await?;

    // Each IQM sidecar is now a record in its own right: a scans row (which the
    // sidecars FK requires) and a sidecars row carrying its metrics.
    assert_eq!(count(&db, "scans")?, 2, "both IQM sidecars became records");
    assert_eq!(count(&db, "sidecars")?, 2);

    // The overlay's typed IQM columns are populated — this is what was lost before.
    let (cjv, cnr): (f64, f64) = db.conn.query_row(
        "SELECT cjv, cnr FROM sidecars WHERE file_path LIKE '%T1w.json'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!(cjv, 0.273);
    assert_eq!(cnr, 4.42);

    let fd_mean: f64 = db.conn.query_row(
        "SELECT fd_mean FROM sidecars WHERE file_path LIKE '%bold.json'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(fd_mean, 0.114);

    // The record is queryable by BIDS concept, so a consumer can ask for
    // "the bold IQMs of sub-01" rather than parsing paths.
    let (sub, datatype, suffix, extension): (String, String, String, String) = db.conn.query_row(
        "SELECT sub, datatype, suffix, extension FROM scans WHERE file_path LIKE '%bold.json'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    assert_eq!((sub.as_str(), datatype.as_str()), ("01", "func"));
    assert_eq!((suffix.as_str(), extension.as_str()), ("bold", ".json"));
    Ok(())
}

/// An inheritance template describes files *elsewhere*; promoting it would invent a
/// record for a file that does not exist. Only file-level sidecars (in a datatype
/// directory, naming a subject) qualify.
#[tokio::test]
async fn inheritance_templates_are_not_promoted() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let func = dir.path().join("sub-01/func");
    fs::create_dir_all(&func).unwrap();
    fs::write(
        dir.path().join("dataset_description.json"),
        r#"{"Name":"templates","BIDSVersion":"1.11.1"}"#,
    )
    .unwrap();
    // Dataset-level template: no subject, not in a datatype directory.
    fs::write(
        dir.path().join("task-rest_bold.json"),
        r#"{"RepetitionTime":2.0}"#,
    )
    .unwrap();
    // Subject-level template: names a subject, but sits above the datatype directory.
    fs::write(
        dir.path().join("sub-01/sub-01_task-rest_bold.json"),
        r#"{"EchoTime":0.03}"#,
    )
    .unwrap();

    let db = ingest(dir.path()).await?;
    assert_eq!(
        count(&db, "scans")?,
        0,
        "templates describe files elsewhere and must not become records"
    );
    Ok(())
}

/// The ordinary case must be untouched: when the data file *is* shipped, its sidecar
/// describes it and only the data file is a record.
#[tokio::test]
async fn sidecars_with_their_data_file_are_not_promoted() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let func = dir.path().join("sub-01/func");
    fs::create_dir_all(&func).unwrap();
    fs::write(
        dir.path().join("dataset_description.json"),
        r#"{"Name":"normal","BIDSVersion":"1.11.1"}"#,
    )
    .unwrap();
    fs::write(func.join("sub-01_task-rest_bold.nii.gz"), b"").unwrap();
    fs::write(
        func.join("sub-01_task-rest_bold.json"),
        r#"{"RepetitionTime":2.0}"#,
    )
    .unwrap();

    let db = ingest(dir.path()).await?;
    assert_eq!(count(&db, "scans")?, 1, "only the .nii.gz is a record");
    let path: String = db
        .conn
        .query_row("SELECT file_path FROM scans", [], |r| r.get(0))?;
    assert!(path.ends_with(".nii.gz"), "got {path}");
    Ok(())
}
