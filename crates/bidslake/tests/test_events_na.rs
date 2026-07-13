use bidslake::bids::BidsParser;
use bidslake::db::BidsDb;
use bidslake::fs::LocalFileSystem;
use bidslake::schema::Schema;
use std::fs::File;
use std::io::Write;
use tempfile::TempDir;

#[tokio::test]
async fn test_events_tsv_na() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let db_path = temp_dir.path().join("test_events.duckdb");
    let dataset_path = temp_dir.path().join("ds_events_test");
    std::fs::create_dir(&dataset_path)?;

    // Create dataset_description.json
    let dd_path = dataset_path.join("dataset_description.json");
    let mut dd_file = File::create(&dd_path)?;
    writeln!(
        dd_file,
        r#"{{
        "Name": "Test Dataset",
        "BIDSVersion": "1.8.0"
    }}"#
    )?;

    // Create events.tsv with "n/a" in onset column
    // onset is defined as FLOAT in the schema
    let events_path = dataset_path.join("sub-01_task-test_events.tsv");
    let mut events_file = File::create(&events_path)?;
    writeln!(events_file, "onset\tduration\ttrial_type")?;
    writeln!(events_file, "1.0\t0.5\tgo")?;
    writeln!(events_file, "n/a\t0.5\tstop")?; // This should cause error if not handled

    let db = BidsDb::new(db_path.to_str().unwrap())?;
    let schema = Schema::load(None).unwrap();
    db.create_tables(&schema)?;

    let fs = Box::new(LocalFileSystem::new(dataset_path));
    let mut parser = BidsParser::new(fs, None, schema, None, true);

    // This should fail if onset is FLOAT and n/a is not handled
    parser.parse(&db).await?;

    Ok(())
}
