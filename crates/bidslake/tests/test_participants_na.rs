use bidslake::bids::BidsParser;
use bidslake::db::BidsDb;
use bidslake::fs::LocalFileSystem;
use bidslake::schema::Schema;
use std::fs::File;
use std::io::Write;
use tempfile::TempDir;

#[tokio::test]
async fn test_participants_tsv_na() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let db_path = temp_dir.path().join("test_participants.duckdb");
    let dataset_path = temp_dir.path().join("ds_test");
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

    // Create participants.tsv with "n/a" in age column
    // We assume 'age' is a column that might be treated as numeric
    let part_path = dataset_path.join("participants.tsv");
    let mut part_file = File::create(&part_path)?;
    writeln!(part_file, "participant_id\tage\tsex")?;
    writeln!(part_file, "sub-01\t25\tM")?;
    writeln!(part_file, "sub-02\tn/a\tF")?;

    let db = BidsDb::new(db_path.to_str().unwrap())?;
    let schema = Schema::load(None).unwrap();
    db.create_tables(&schema)?;

    // A precondition, not the claim: `n/a` is only interesting in `age` because the schema
    // types that column numerically. These come from `create_tables`, so they say nothing
    // about the file — which is why every assertion in this test used to run *before* the
    // parse, and nothing after it.
    let columns: Vec<(String, String)> = db
        .conn
        .prepare("PRAGMA table_info(participants)")?
        .query_map([], |row| Ok((row.get(1)?, row.get(2)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    let age_type = columns
        .iter()
        .find(|(name, _)| name == "age")
        .map(|(_, t)| t.as_str())
        .expect("participants should declare an `age` column");
    assert!(
        matches!(age_type, "DOUBLE" | "FLOAT" | "REAL"),
        "fixture assumption: `age` should be numeric, found {age_type}"
    );

    let fs = Box::new(LocalFileSystem::new(dataset_path));
    let mut parser = BidsParser::new(fs, None, schema, None, true, true);

    parser.parse(&db).await?;

    // The claim: `n/a` in the numeric column stores NULL, and does not take the rest of the
    // row (or the other subject) with it.
    let rows: Vec<(String, Option<f64>, Option<String>)> = db
        .conn
        .prepare("SELECT participant_id, age, sex FROM participants ORDER BY participant_id")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(
        rows,
        vec![
            ("sub-01".to_string(), Some(25.0), Some("M".to_string())),
            ("sub-02".to_string(), None, Some("F".to_string())),
        ],
    );

    Ok(())
}
