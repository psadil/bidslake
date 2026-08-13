//! A dataset whose files are symlinks into a content store — the shape DataLad and
//! git-annex produce — must ingest exactly as if the files were real, including when
//! some links are broken (annexed content that was never fetched).
//!
//! This pins the path handling the tabular ingest depends on. `read_csv_source` hands
//! DuckDB `canonical_root.join(rel_path)` rather than resolving each file's own
//! symlink, so these datasets are the case where the two could differ: the resolved
//! path would point into the store, the joined one stays inside the dataset. Reading
//! through a symlink yields the same bytes, and `filename=true` echoes back whatever
//! path was passed — but the batched ingest joins its rows on that echoed path, so a
//! mismatch there would silently drop every row rather than fail loudly.

mod common;

use common::{count, count_data_files, ingest};
use std::fs;
use std::os::unix::fs as unix_fs;

/// Build a dataset of symlinks into a sibling content store, with one broken link.
fn write_annexed_dataset(root: &std::path::Path) -> std::io::Result<()> {
    let store = root.join("store");
    let ds = root.join("ds");
    fs::create_dir_all(&store)?;
    fs::create_dir_all(ds.join("sub-01/func"))?;
    fs::create_dir_all(ds.join("sub-01/anat"))?;

    // dataset_description.json is a real file, as it is under git rather than annex.
    fs::write(
        ds.join("dataset_description.json"),
        r#"{"Name": "annexed", "BIDSVersion": "1.8.0"}"#,
    )?;

    // Annexed content lives in the store; the dataset holds links to it.
    fs::write(
        store.join("events"),
        "onset\tduration\n0.0\t1.0\n2.0\t1.0\n",
    )?;
    fs::write(store.join("sidecar"), r#"{"RepetitionTime": 2.0}"#)?;
    fs::write(store.join("bold"), "not really nifti")?;

    unix_fs::symlink(
        store.join("events"),
        ds.join("sub-01/func/sub-01_task-rest_events.tsv"),
    )?;
    unix_fs::symlink(
        store.join("sidecar"),
        ds.join("sub-01/func/sub-01_task-rest_bold.json"),
    )?;
    unix_fs::symlink(
        store.join("bold"),
        ds.join("sub-01/func/sub-01_task-rest_bold.nii.gz"),
    )?;
    // Annexed but never fetched: the link dangles.
    unix_fs::symlink(
        store.join("absent"),
        ds.join("sub-01/anat/sub-01_T1w.nii.gz"),
    )?;
    Ok(())
}

#[tokio::test]
async fn symlinked_dataset_ingests_like_a_real_one() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    write_annexed_dataset(tmp.path())?;
    let db = ingest(tmp.path().join("ds")).await?;

    // Both data files are registered, the broken link included — the walk classifies
    // from the link itself, so a dangling target is still a file.
    assert_eq!(
        count_data_files(&db)?,
        2,
        "both data files should reach the registry"
    );

    // The TSV was read through its symlink, and the batched ingest's join on the
    // echoed `filename` matched — otherwise the rows would be silently absent.
    assert_eq!(
        count(&db, "events")?,
        2,
        "events rows should survive the symlinked path"
    );

    // Inheritance picked up the sidecar through its symlink too.
    let tr: Option<f64> = db.conn.query_row(
        "SELECT RepetitionTime FROM sidecars JOIN all_files USING (file_id) WHERE file_path LIKE '%_bold.nii.gz'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(tr, Some(2.0), "sidecar metadata should be inherited");

    Ok(())
}
