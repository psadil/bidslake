//! Integration tests for the S3 backend against OpenNeuro's public bucket
//! (`s3://openneuro.org`), the formalized successor to the old
//! `ingest_50_datasets.sh` script.
//!
//! These hit the network and the DuckDB httpfs extension (installed on first
//! use), so they are `#[ignore]` — run explicitly:
//!   `cargo test -p bidslake --test s3_ingest -- --ignored`
//! Anonymous/unsigned access is used, so no AWS credentials are required.

use bidslake::{
    bids::{BidsParser, S3Httpfs},
    db::BidsDb,
    fs::BidsFileSystem,
    s3::{self, S3Client},
    schema::Schema,
};
use std::path::Path;

const BUCKET: &str = "openneuro.org";

/// Ingest an OpenNeuro dataset straight from S3 into an in-memory DuckDB, wiring
/// httpfs onto both the write and preflight connections exactly as `main` does.
async fn ingest_s3(dataset: &str) -> anyhow::Result<BidsDb> {
    let db = BidsDb::new(":memory:")?;
    let schema = Schema::load(None).unwrap();
    db.create_tables(&schema)?;

    let client = S3Client::new(BUCKET, dataset, s3::SigningMode::Anonymous).await?;
    let region = client.region().to_string();
    s3::configure_httpfs(&db.conn, &region, true)?;

    let mut parser = BidsParser::new(
        Box::new(client),
        Some(dataset.to_string()),
        schema,
        Some(S3Httpfs {
            region: region.clone(),
            anonymous: true,
        }),
    );

    let txn = db.conn.unchecked_transaction()?;
    parser.parse(&db).await?;
    txn.commit()?;
    Ok(db)
}

fn count(db: &BidsDb, table: &str) -> i64 {
    db.conn
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap_or(-1)
}

/// The low-level S3 methods: `walk` lists dataset-relative keys, and
/// `read_to_string` returns real file contents.
#[tokio::test]
#[ignore = "network: hits s3://openneuro.org"]
async fn s3_methods_walk_and_read() -> anyhow::Result<()> {
    let client = S3Client::new(BUCKET, "ds000001", s3::SigningMode::Anonymous).await?;

    let files = client.walk(&[]).await?;
    assert!(
        files
            .iter()
            .any(|p| p.ends_with("dataset_description.json")),
        "walk must list dataset_description.json"
    );
    assert!(
        files.iter().any(|p| p.ends_with("participants.tsv")),
        "walk must list participants.tsv"
    );
    // Keys are dataset-relative (prefix stripped), not absolute.
    assert!(
        files
            .iter()
            .all(|p| !p.to_string_lossy().starts_with("ds000001/")),
        "walk keys must be dataset-relative"
    );

    let desc = client
        .read_to_string(Path::new("dataset_description.json"))
        .await?;
    let json: serde_json::Value = serde_json::from_str(&desc)?;
    assert!(json.get("Name").is_some(), "dataset_description has a Name");
    Ok(())
}

/// A full ingest from S3 populates the tabular tables (the httpfs path), and no
/// rows are dropped: each events file's DB row count equals its raw line count.
#[tokio::test]
#[ignore = "network: hits s3://openneuro.org"]
async fn s3_full_ingest_ds000001() -> anyhow::Result<()> {
    let db = ingest_s3("ds000001").await?;

    assert_eq!(count(&db, "dataset_description"), 1);
    assert!(count(&db, "participants") > 0, "participants ingested");
    assert!(count(&db, "events") > 0, "events ingested via httpfs");

    // Every tabular file must be recorded as ingested (materialize no longer fails).
    let skipped: i64 = db
        .conn
        .query_row(
            "SELECT count(*) FROM tabular_files WHERE status <> 'ingested' AND status <> 'on_disk'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(-1);
    assert_eq!(skipped, 0, "no tabular file should be skipped over S3");

    // No rows dropped: compare a sample events file's DB count to its raw S3 lines.
    let client = S3Client::new(BUCKET, "ds000001", s3::SigningMode::Anonymous).await?;
    let sample: String = db.conn.query_row(
        "SELECT file_path FROM events GROUP BY file_path ORDER BY file_path LIMIT 1",
        [],
        |r| r.get(0),
    )?;
    let raw = client.read_to_string(Path::new(&sample)).await?;
    let raw_rows = raw.lines().filter(|l| !l.is_empty()).count() as i64 - 1; // minus header
    let db_rows: i64 = db.conn.query_row(
        "SELECT count(*) FROM events WHERE file_path = ?",
        [&sample],
        |r| r.get(0),
    )?;
    assert_eq!(
        db_rows, raw_rows,
        "events row count must match raw for {sample}"
    );
    Ok(())
}
