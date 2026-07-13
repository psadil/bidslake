use bidslake::db::BidsDb;
use bidslake::schema::Schema;
use serde_json::json;
use tempfile::TempDir;

#[test]
fn test_na_value_in_numeric_column() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let db_path = temp_dir.path().join("test_na.duckdb");
    let db = BidsDb::new(db_path.to_str().unwrap())?;

    // Load schema
    let schema = Schema::load(None).unwrap();
    db.create_tables(&schema)?;

    // Try to insert a scan with "n/a" in a numeric field (e.g., RepetitionTime)
    // RepetitionTime is defined as number (DOUBLE) in the schema
    let data = json!({
        "dataset_id": "ds000002",
        "file_path": "sub-01/func/sub-01_task-probabilisticclassification_run-01_bold.json",
        "participant_id": "sub-01",
        "session_id": "ses-01",
        "RepetitionTime": "n/a",
        "EchoTime": 0.03
    });

    // This should currently fail
    let result = schema.insert(&db.conn, "scans", &data);

    match result {
        Ok(_) => println!("Insert succeeded unexpectedly"),
        Err(e) => {
            println!("Insert failed as expected with: {}", e);
            let err_msg = e.to_string();
            assert!(
                err_msg.contains("Could not convert string 'n/a' to FLOAT")
                    || err_msg.contains("Conversion Error")
            );
        }
    }

    Ok(())
}
