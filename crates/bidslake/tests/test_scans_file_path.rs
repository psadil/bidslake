//! The `file_path` recorded for rows sourced from `scans.tsv`.

use anyhow::Result;
use bidslake::{bids::BidsParser, db::BidsDb, fs::LocalFileSystem, schema::Schema};
use rstest::rstest;
use std::path::PathBuf;
use tempfile::TempDir;

#[tokio::test]
async fn test_scans_file_path_with_root_uri() -> Result<()> {
    let temp_dir = TempDir::new()?;
    let dataset_path = temp_dir.path().join("test_dataset");
    std::fs::create_dir(&dataset_path)?;

    // Create dataset_description.json
    let dd_path = dataset_path.join("dataset_description.json");
    std::fs::write(
        &dd_path,
        r#"{"Name": "Test Dataset", "BIDSVersion": "1.8.0"}"#,
    )?;

    // Create a temporary imaging file structure
    let sub01_anat = dataset_path.join("sub-01/anat");
    std::fs::create_dir_all(&sub01_anat)?;
    let img_file = sub01_anat.join("sub-01_T1w.nii.gz");
    std::fs::write(&img_file, b"fake nifti data")?;

    let db_path = temp_dir.path().join("test.duckdb");
    let db = BidsDb::new(db_path.to_str().unwrap())?;
    let schema = Schema::load(None).unwrap();
    db.create_tables(&schema)?;

    let fs = Box::new(LocalFileSystem::new(dataset_path.clone()));
    let mut parser = BidsParser::new(fs, None, schema, None, true, true);
    parser.parse(&db).await?;

    // Get root_uri from the dataset's root registry
    let root_uri: String =
        db.conn
            .query_row("SELECT root_uri FROM dataset_roots LIMIT 1", [], |r| {
                r.get(0)
            })?;

    println!("Root URI: {}", root_uri);
    assert!(
        root_uri.starts_with("file://"),
        "root_uri should use file:// scheme"
    );

    // Get some file_path entries from scans table
    let mut stmt = db
        .conn
        .prepare("SELECT file_path FROM all_files WHERE kind = 'data' LIMIT 5")?;
    let file_paths: Vec<String> = stmt
        .query_map([], |row| row.get(0))?
        .collect::<Result<Vec<_>, _>>()?;

    println!(
        "Found {} file_path entries in scans table",
        file_paths.len()
    );
    assert!(
        !file_paths.is_empty(),
        "Should have at least one entry in scans table"
    );

    // Verify each file_path entry:
    // 1. file_path should NOT be just a filename (should contain /)
    // 2. Concatenation of root_uri + file_path should exist
    for file_path in file_paths {
        println!("Checking file_path: {}", file_path);

        // Check that file_path is not just a filename
        assert!(
            file_path.contains('/'),
            "file_path should be a relative path, not just a filename: {}",
            file_path
        );

        // Check that file_path doesn't appear at the start (it's relative, not absolute)
        assert!(
            !file_path.starts_with('/'),
            "file_path should be relative, not absolute: {}",
            file_path
        );

        // Construct full path from root_uri + file_path
        let root_path = root_uri.strip_prefix("file://").unwrap_or(&root_uri);
        let full_path = PathBuf::from(root_path).join(&file_path);

        println!("  -> Full path: {:?}", full_path);
        assert!(full_path.exists(), "File should exist at: {:?}", full_path);
    }

    // This dataset ships no `scans.tsv`, and since docs/adr/0006 `scans` is that file's
    // satellite rather than a row per data file — so it is empty, while the files themselves
    // are all in the registry. `scans` used to be seeded with a stub per data file, whose only
    // purpose was to give the sidecars foreign key a referent; that key points at
    // `file_registry` now, so the stubs were rows describing acquisitions nothing had
    // described.
    let scans: i64 = db
        .conn
        .query_row("SELECT count(*) FROM scans", [], |r| r.get(0))?;
    assert_eq!(scans, 0, "no scans.tsv, so no scans rows");
    let registered: i64 = db.conn.query_row(
        "SELECT count(*) FROM all_files WHERE kind = 'data'",
        [],
        |r| r.get(0),
    )?;
    assert!(registered > 0, "the data files are still registered");

    Ok(())
}

mod common;

/// A `scans.tsv` names its data files in a `filename` column relative to its own directory,
/// and the batch composes `file_path` from the two. This is only correct because the batch's
/// source column is `__src` rather than `filename` — a literal `filename` column is exactly
/// what `scans.tsv` has, and the collision is what sent these files down a per-file path
/// before. Nothing covered it: the test above ships no `scans.tsv`, and the corpus datasets
/// that do are only asserted with `LIKE`/`>= 1`, which pass either way.
///
/// A regression is silent rather than loud. `scans` is keyed `(dataset_id, file_path)` and the
/// batch inserts `OR REPLACE`, so a wrong `file_path` adds a *second* row at the bare filename
/// instead of replacing the walk-synthesized one — duplicate rows, a lost `acq_time`, and no
/// primary-key error to notice.
/// A dataset whose `sub-01/sub-01_scans.tsv` names its two data files relative to its own
/// directory, so composing them against anything else is visible.
fn write_scans_dataset(root: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(root.join("sub-01/anat"))?;
    std::fs::create_dir_all(root.join("sub-01/func"))?;
    std::fs::write(
        root.join("dataset_description.json"),
        r#"{"Name": "scans-tsv", "BIDSVersion": "1.8.0"}"#,
    )?;
    std::fs::write(root.join("sub-01/anat/sub-01_T1w.nii.gz"), b"nii")?;
    std::fs::write(
        root.join("sub-01/func/sub-01_task-rest_bold.nii.gz"),
        b"nii",
    )?;
    // The `filename` values are relative to the *scans.tsv's* directory, i.e. `sub-01/`.
    std::fs::write(
        root.join("sub-01/sub-01_scans.tsv"),
        "filename\tacq_time\n\
         anat/sub-01_T1w.nii.gz\t2026-08-12T09:00:00\n\
         func/sub-01_task-rest_bold.nii.gz\t2026-08-12T09:15:00\n",
    )?;
    Ok(())
}

/// Composed against the scans.tsv's own directory, not stored bare.
#[rstest]
#[case("sub-01/anat/sub-01_T1w.nii.gz")]
#[case("sub-01/func/sub-01_task-rest_bold.nii.gz")]
#[tokio::test]
async fn a_scans_row_lands_at_the_composed_path(#[case] expected: &str) -> Result<()> {
    let dir = TempDir::new()?;
    let root = dir.path().join("ds");
    write_scans_dataset(&root)?;
    let db = common::ingest(&root).await?;

    let n: i64 = db.conn.query_row(
        "SELECT count(*) FROM all_files WHERE kind = 'data' AND file_path = ?",
        [expected],
        |r| r.get(0),
    )?;

    assert_eq!(n, 1, "expected exactly one scans row at {expected}");
    Ok(())
}

#[tokio::test]
async fn scans_tsv_file_path_is_composed_from_its_directory() -> Result<()> {
    let dir = TempDir::new()?;
    let root = dir.path().join("ds");
    write_scans_dataset(&root)?;
    let db = common::ingest(&root).await?;

    // The failure mode: a row at the bare `filename` value, alongside the real one.
    let bare: i64 = db.conn.query_row(
        "SELECT count(*) FROM all_files WHERE kind = 'data' AND file_path IN \
         ('anat/sub-01_T1w.nii.gz', 'func/sub-01_task-rest_bold.nii.gz')",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(bare, 0, "no scans row may keep the bare `filename` value");

    // The TSV's own data reached the row.
    let with_time: i64 = db.conn.query_row(
        "SELECT count(*) FROM scans WHERE acq_time IS NOT NULL",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(with_time, 2, "acq_time from scans.tsv must be stored");

    // Exactly the two rows the TSV has, one per data file it names.
    let total: i64 = db
        .conn
        .query_row("SELECT count(*) FROM scans", [], |r| r.get(0))?;
    assert_eq!(total, 2, "no duplicate rows");

    // `filename` is structural — consumed into `file_path`, never stored as data.
    let leaked: i64 = db.conn.query_row(
        "SELECT count(*) FROM scans \
         WHERE other_data IS NOT NULL AND json_contains(json_keys(other_data), '\"filename\"')",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(leaked, 0, "`filename` must not appear in other_data");

    Ok(())
}
