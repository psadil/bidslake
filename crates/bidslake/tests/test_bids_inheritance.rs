use anyhow::Result;
use bidslake::{bids::BidsParser, db::BidsDb, fs::LocalFileSystem, schema::Schema};
use tempfile::TempDir;

#[tokio::test]
async fn test_bids_inheritance() -> Result<()> {
    let temp_dir = TempDir::new()?;
    let dataset_path = temp_dir.path().join("test_inheritance");
    std::fs::create_dir(&dataset_path)?;

    // Create dataset_description.json
    let dd_path = dataset_path.join("dataset_description.json");
    std::fs::write(
        &dd_path,
        r#"{"Name": "Inheritance Test", "BIDSVersion": "1.8.0"}"#,
    )?;

    // 1. Top-level sidecar (least specific)
    // task-rest_bold.json
    let top_sidecar = dataset_path.join("task-rest_bold.json");
    std::fs::write(
        &top_sidecar,
        r#"{
        "RepetitionTime": 2.0,
        "TaskName": "rest",
        "Manufacturer": "Siemens"
    }"#,
    )?;

    // 2. Subject directory
    let sub01_func = dataset_path.join("sub-01/func");
    std::fs::create_dir_all(&sub01_func)?;

    // 3. Subject-level imaging file (should inherit from top-level)
    // sub-01_task-rest_bold.nii.gz
    let img_file1 = sub01_func.join("sub-01_task-rest_bold.nii.gz");
    std::fs::write(&img_file1, b"fake nifti")?;

    // 4. Another subject with override
    let sub02_func = dataset_path.join("sub-02/func");
    std::fs::create_dir_all(&sub02_func)?;

    // 5. Subject-level sidecar (more specific - overrides TR)
    // sub-02_task-rest_bold.json
    let sub_sidecar = sub02_func.join("sub-02_task-rest_bold.json");
    std::fs::write(
        &sub_sidecar,
        r#"{
        "RepetitionTime": 1.5
    }"#,
    )?;

    // 6. Subject-level imaging file (should inherit Manufacturer but override TR)
    // sub-02_task-rest_bold.nii.gz
    let img_file2 = sub02_func.join("sub-02_task-rest_bold.nii.gz");
    std::fs::write(&img_file2, b"fake nifti")?;

    // Run parser
    let db_path = temp_dir.path().join("test.duckdb");
    let db = BidsDb::new(db_path.to_str().unwrap())?;
    let schema = Schema::load(None);
    db.create_tables(&schema)?;

    let fs = Box::new(LocalFileSystem::new(dataset_path.clone()));
    let mut parser = BidsParser::new(fs, None, schema);
    parser.parse(&db).await?;

    // Verify sub-01 (Inheritance)
    // Should have TR=2.0, Manufacturer=Siemens
    // Note: RepetitionTime is a standard BIDS field, so it's extracted to its own
    // (verbatim BIDS-named) "RepetitionTime" column and removed from 'other_data'.
    let tr1: f64 = db.conn.query_row(
        "SELECT \"RepetitionTime\" FROM sidecars WHERE file_path LIKE '%sub-01%'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(tr1, 2.0, "sub-01 should inherit TR=2.0");

    let manuf1: String = db.conn.query_row(
        "SELECT \"Manufacturer\" FROM sidecars WHERE file_path LIKE '%sub-01%'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(
        manuf1, "Siemens",
        "sub-01 should inherit Manufacturer=Siemens"
    );

    // Verify sub-02 (Override)
    // Should have TR=1.5 (override), Manufacturer=Siemens (inherited)
    let tr2: f64 = db.conn.query_row(
        "SELECT \"RepetitionTime\" FROM sidecars WHERE file_path LIKE '%sub-02%'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(tr2, 1.5, "sub-02 should override TR=1.5");

    let manuf2: String = db.conn.query_row(
        "SELECT \"Manufacturer\" FROM sidecars WHERE file_path LIKE '%sub-02%'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(
        manuf2, "Siemens",
        "sub-02 should inherit Manufacturer=Siemens"
    );

    // Verify Foreign Key relationship
    // Check that we can join scans and sidecars
    let join_count: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM scans s JOIN sidecars m ON s.dataset_id = m.dataset_id AND s.file_path = m.file_path",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(
        join_count, 2,
        "Should have 2 joined rows (one for each scan)"
    );

    Ok(())
}
