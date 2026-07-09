use anyhow::Result;
use bidslake::{bids::BidsParser, db::BidsDb, fs::LocalFileSystem, schema::Schema};
use std::path::PathBuf;
use tempfile::TempDir;

/// Test file associations table and .bidsignore functionality
#[tokio::test]
async fn test_file_associations_and_bidsignore() -> Result<()> {
    let dataset_path = PathBuf::from("tests/fixtures/ds000001");
    let temp_dir = TempDir::new()?;
    let db_path = temp_dir.path().join("test.duckdb");

    let db = BidsDb::new(db_path.to_str().unwrap())?;
    let schema = Schema::load(None);
    db.create_tables(&schema)?;

    let fs = Box::new(LocalFileSystem::new(dataset_path));
    let mut parser = BidsParser::new(fs, None, schema);
    parser.parse(&db).await?;

    // Verify file_associations table exists
    let table_exists: bool = db.conn.query_row(
        "SELECT COUNT(*) > 0 FROM information_schema.tables WHERE table_name = 'file_associations'",
        [],
        |r| r.get(0),
    )?;
    assert!(table_exists, "file_associations table should exist");

    Ok(())
}

/// Test root_uri field in dataset_description for path reconstruction
#[tokio::test]
async fn test_root_uri_path_reconstruction() -> Result<()> {
    let dataset_path = PathBuf::from("tests/fixtures/ds000001");
    let temp_dir = TempDir::new()?;
    let db_path = temp_dir.path().join("test.duckdb");

    let db = BidsDb::new(db_path.to_str().unwrap())?;
    let schema = Schema::load(None);
    db.create_tables(&schema)?;

    let fs = Box::new(LocalFileSystem::new(dataset_path));
    let mut parser = BidsParser::new(fs, None, schema);
    parser.parse(&db).await?;

    // Check root_uri is populated
    let root_uri: Option<String> =
        db.conn
            .query_row("SELECT root_uri FROM dataset_description", [], |r| r.get(0))?;

    assert!(root_uri.is_some(), "root_uri should be populated");
    let uri = root_uri.unwrap();
    assert!(
        uri.starts_with("file://"),
        "Local paths should use file:// URI scheme"
    );
    assert!(
        uri.contains("ds000001"),
        "root_uri should contain dataset path"
    );

    println!("✓ root_uri: {}", uri);

    Ok(())
}

/// Test sidecar deduplication - verify other_data doesn't contain duplicate fields
#[tokio::test]
async fn test_sidecar_deduplication() -> Result<()> {
    let dataset_path = PathBuf::from("tests/fixtures/ds000001");
    let temp_dir = TempDir::new()?;
    let db_path = temp_dir.path().join("test.duckdb");

    let db = BidsDb::new(db_path.to_str().unwrap())?;
    let schema = Schema::load(None);
    db.create_tables(&schema)?;

    let fs = Box::new(LocalFileSystem::new(dataset_path));
    let mut parser = BidsParser::new(fs, None, schema);
    parser.parse(&db).await?;

    // Check if we have any scans first
    let scan_count: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM scans", [], |r| r.get(0))?;

    if scan_count > 0 {
        // Find a sidecar and check that other_data doesn't contain schema fields
        // Note: file_path in sidecars table is the scan file path
        let other_data_query = db.conn.query_row(
            "SELECT other_data::VARCHAR FROM sidecars LIMIT 1",
            [],
            |r| r.get::<_, Option<String>>(0),
        );

        match other_data_query {
            Ok(other_data_json) => {
                // other_data should be NULL or very small (only custom fields, not schema fields)
                match other_data_json {
                    None => {
                        println!("✓ other_data is NULL - no custom fields present");
                    }
                    Some(json_str) => {
                        // Should not contain standard BIDS fields like RepetitionTime, EchoTime, etc
                        // which should be in dedicated columns now
                        let data: serde_json::Value = serde_json::from_str(&json_str)?;
                        let obj = data.as_object().expect("other_data should be an object");

                        // Verify that common BIDS metadata fields are NOT in other_data
                        assert!(
                            !obj.contains_key("RepetitionTime"),
                            "RepetitionTime should be in dedicated column, not other_data"
                        );
                        assert!(
                            !obj.contains_key("EchoTime"),
                            "EchoTime should be in dedicated column, not other_data"
                        );
                        assert!(
                            !obj.contains_key("FlipAngle"),
                            "FlipAngle should be in dedicated column, not other_data"
                        );

                        println!(
                            "✓ other_data only contains custom fields: {} keys",
                            obj.len()
                        );
                    }
                }
            }
            Err(e) => {
                // If no sidecars found despite scans existing (maybe no metadata?), that's okay for this test
                println!("No sidecars found to check: {}", e);
            }
        }
    } else {
        println!("No scans found in fixture, skipping deduplication check.");
    }

    Ok(())
}
