//! A failed bulk write must fail the run.
//!
//! The bulk writes of the flush — `file_registry`, `file_associations`, `bvals`/`bvecs`,
//! `sidecars` — used to print their error and carry on, so the run went on to report
//! "Conversion complete!" and exit 0 over a catalog missing whatever that write held.
//! Nothing downstream could tell: unlike a tabular file, which is recorded with
//! `status = "failed"` when its rows are dropped, these tables keep no such record.
//!
//! Forcing one of those failures needs a catalog whose `sidecars` is narrower than the
//! run writes — which is not contrived. `BidsDb::check_registry_shape` refuses that
//! mismatch for `file_registry`, because tables are created `IF NOT EXISTS` and keep the
//! shape of the run that created them; no equivalent guard covers `sidecars`, so the
//! write is simply attempted and fails.

mod common;

use bidslake::bids::BidsParser;
use bidslake::db::BidsDb;
use bidslake::fs::LocalFileSystem;
use bidslake::schema::Schema;
use common::count;
use std::fs;
use std::path::Path;
use std::process::Command;

/// One subject with a sidecar, so inheritance has a row to write.
fn write_dataset(root: &Path) {
    let func = root.join("sub-01/func");
    fs::create_dir_all(&func).unwrap();
    fs::write(
        root.join("dataset_description.json"),
        r#"{"Name":"fatal","BIDSVersion":"1.9.0"}"#,
    )
    .unwrap();
    fs::write(func.join("sub-01_task-rest_bold.nii.gz"), b"").unwrap();
    fs::write(
        func.join("sub-01_task-rest_bold.json"),
        r#"{"RepetitionTime":2.0,"TaskName":"rest"}"#,
    )
    .unwrap();
}

/// Drop a column the run writes, leaving the catalog narrower than the schema.
fn narrow_the_sidecars_table(db: &BidsDb) -> anyhow::Result<()> {
    db.conn
        .execute("ALTER TABLE sidecars DROP COLUMN \"TaskName\"", [])?;
    Ok(())
}

#[tokio::test]
async fn a_failed_sidecars_write_fails_the_parse() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let ds = tmp.path().join("ds");
    write_dataset(&ds);

    let db = BidsDb::new(":memory:")?;
    let schema = Schema::load(None)?;
    db.create_tables(&schema)?;
    narrow_the_sidecars_table(&db)?;

    let fs = Box::new(LocalFileSystem::new(ds.clone()));
    let mut parser = BidsParser::new(fs, Some("raw".to_string()), schema, None, true);

    // Exactly `main.rs`'s shape: one transaction around the parse, committed only on Ok.
    let txn = db.conn.unchecked_transaction()?;
    let err = parser
        .parse(&db)
        .await
        .expect_err("a failed sidecars write must fail the parse");
    drop(txn);

    let rendered = format!("{err:#}");
    assert!(
        rendered.contains("sidecars"),
        "the error should name what failed to write, got: {rendered}"
    );

    // The transaction was never committed, so the catalog is as it was — the alternative
    // to exiting non-zero is a half-written catalog, and this is the half that matters.
    assert_eq!(count(&db, "file_registry")?, 0, "the ingest rolled back");
    assert_eq!(count(&db, "sidecars")?, 0);
    Ok(())
}

/// The same through the binary, which is where the original report saw it: the run
/// printed the appender's error, printed "Conversion complete!", and exited 0.
#[test]
fn a_failed_write_exits_non_zero() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let ds = tmp.path().join("ds");
    write_dataset(&ds);
    let catalog = tmp.path().join("catalog.duckdb");

    // Build the catalog's tables up front, then narrow one, and close the connection so
    // the CLI can open the file.
    {
        let db = BidsDb::new(catalog.to_str().unwrap())?;
        db.create_tables(&Schema::load(None)?)?;
        narrow_the_sidecars_table(&db)?;
    }

    let out = Command::new(env!("CARGO_BIN_EXE_bidslake"))
        .args([
            "index",
            "-i",
            ds.to_str().unwrap(),
            "-o",
            catalog.to_str().unwrap(),
            "-d",
            "raw",
        ])
        .output()?;

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !out.status.success(),
        "a failed write must exit non-zero, got {:?}\nstdout: {stdout}",
        out.status.code()
    );
    assert!(
        !stdout.contains("Conversion complete!"),
        "and must not report success:\n{stdout}"
    );
    Ok(())
}

/// The counterpart, so this file pins both directions: an ordinary run is unaffected by
/// the change, and still reports success.
#[test]
fn an_ordinary_run_still_reports_success() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let ds = tmp.path().join("ds");
    write_dataset(&ds);
    let catalog = tmp.path().join("catalog.duckdb");

    let out = Command::new(env!("CARGO_BIN_EXE_bidslake"))
        .args([
            "index",
            "-i",
            ds.to_str().unwrap(),
            "-o",
            catalog.to_str().unwrap(),
            "-d",
            "raw",
        ])
        .output()?;

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("Conversion complete!"));

    let db = BidsDb::new(catalog.to_str().unwrap())?;
    assert_eq!(count(&db, "sidecars")?, 1);
    Ok(())
}
