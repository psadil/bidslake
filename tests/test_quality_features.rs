//! File-association table presence, root_uri reconstruction, and sidecar
//! deduplication — exercised against a real bids-examples dataset (ds001).

mod common;

use anyhow::Result;
use common::{bids_example, ingest};

#[tokio::test]
async fn test_file_associations_table_exists() -> Result<()> {
    let db = ingest(bids_example("ds001")).await?;

    let table_exists: bool = db.conn.query_row(
        "SELECT COUNT(*) > 0 FROM information_schema.tables WHERE table_name = 'file_associations'",
        [],
        |r| r.get(0),
    )?;
    assert!(table_exists, "file_associations table should exist");
    Ok(())
}

#[tokio::test]
async fn test_root_uri_path_reconstruction() -> Result<()> {
    let db = ingest(bids_example("ds001")).await?;

    let root_uri: Option<String> =
        db.conn
            .query_row("SELECT root_uri FROM dataset_description", [], |r| r.get(0))?;

    let uri = root_uri.expect("root_uri should be populated");
    assert!(
        uri.starts_with("file://"),
        "local paths should use file:// URI scheme, got {uri}"
    );
    assert!(uri.contains("ds001"), "root_uri should contain dataset path: {uri}");
    Ok(())
}

#[tokio::test]
async fn test_sidecar_deduplication() -> Result<()> {
    let db = ingest(bids_example("ds001")).await?;

    // Known BIDS metadata fields have dedicated columns and must not also appear
    // in the other_data overflow column.
    let rows: Vec<Option<String>> = db
        .conn
        .prepare("SELECT other_data::VARCHAR FROM sidecars")?
        .query_map([], |r| r.get::<_, Option<String>>(0))?
        .collect::<Result<_, _>>()?;

    assert!(!rows.is_empty(), "ds001 should produce sidecars");

    for other_data in rows.into_iter().flatten() {
        let data: serde_json::Value = serde_json::from_str(&other_data)?;
        let obj = data.as_object().expect("other_data should be an object");
        for dedicated in ["RepetitionTime", "EchoTime", "FlipAngle"] {
            assert!(
                !obj.contains_key(dedicated),
                "{dedicated} should be in its dedicated column, not other_data"
            );
        }
    }
    Ok(())
}
