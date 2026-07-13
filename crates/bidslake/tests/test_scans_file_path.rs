use anyhow::Result;
use bidslake::{bids::BidsParser, db::BidsDb, fs::LocalFileSystem, schema::Schema};
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
    let mut parser = BidsParser::new(fs, None, schema, None);
    parser.parse(&db).await?;

    // Get root_uri from dataset_description
    let root_uri: String = db.conn.query_row(
        "SELECT root_uri FROM dataset_description LIMIT 1",
        [],
        |r| r.get(0),
    )?;

    println!("Root URI: {}", root_uri);
    assert!(
        root_uri.starts_with("file://"),
        "root_uri should use file:// scheme"
    );

    // Get some file_path entries from scans table
    let mut stmt = db.conn.prepare("SELECT file_path FROM scans LIMIT 5")?;
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

    // Verify that file_path is NOT in other_data
    let other_data_json: Option<String> =
        db.conn
            .query_row("SELECT other_data::VARCHAR FROM scans LIMIT 1", [], |r| {
                r.get(0)
            })?;

    if let Some(json_str) = other_data_json {
        let json: serde_json::Value = serde_json::from_str(&json_str)?;
        if let Some(obj) = json.as_object() {
            assert!(
                !obj.contains_key("file_path"),
                "file_path should NOT appear in other_data column"
            );
            assert!(
                !obj.contains_key("dataset_id"),
                "dataset_id should NOT appear in other_data column"
            );
            println!("✓ file_path and dataset_id correctly excluded from other_data");
        }
    }

    Ok(())
}
