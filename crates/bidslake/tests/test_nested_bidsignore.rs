//! The local walker (via `bids_core::filetree::read_file_tree`) honours **nested**
//! `.bidsignore` files, not just the dataset-root one. This is a deliberate behavior
//! change from bidslake's earlier root-only handling; pin it so it can't regress.

mod common;

use common::{count, ingest};
use std::fs;

/// A `.bidsignore` in a subdirectory excludes matching files in that subtree from the
/// walk, while a non-matching sibling in the same directory is still ingested.
#[tokio::test]
async fn nested_bidsignore_excludes_subtree_files() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path();

    fs::write(
        root.join("dataset_description.json"),
        r#"{"Name": "nested-bidsignore-test", "BIDSVersion": "1.8.0"}"#,
    )?;

    // A derivatives subtree with its own `.bidsignore` that drops `*_ignored.tsv`.
    let deriv = root.join("derivatives/pipe/sub-01");
    fs::create_dir_all(&deriv)?;
    fs::write(root.join("derivatives/pipe/.bidsignore"), "*_ignored.tsv\n")?;
    fs::write(deriv.join("sub-01_desc-a_ignored.tsv"), "col\n1\n")?;
    fs::write(deriv.join("sub-01_desc-b_kept.tsv"), "col\n1\n")?;

    let db = ingest(root).await?;

    // Every tabular file the walk saw is recorded in `tabular_files` (whatever its
    // status). The nested-ignored file must be absent; its sibling must be present.
    let ignored: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM tabular_files WHERE file_path LIKE '%_ignored.tsv'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(
        ignored, 0,
        "a file matched by a nested .bidsignore must not be walked/recorded"
    );

    let kept: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM tabular_files WHERE file_path LIKE '%_kept.tsv'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(
        kept, 1,
        "a sibling not matched by the nested .bidsignore must still be recorded"
    );

    // Sanity: the walk did record something.
    assert!(count(&db, "tabular_files")? >= 1);
    Ok(())
}
