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
    let schema = Schema::load(None);
    db.create_tables(&schema)?;

    // Check the schema of participants table
    let columns: Vec<(String, String)> = db
        .conn
        .prepare("PRAGMA table_info(participants)")?
        .query_map([], |row| Ok((row.get(1)?, row.get(2)?)))?
        .collect::<Result<Vec<_>, _>>()?;

    println!("Participants table schema:");
    let mut found_age = false;
    let mut found_sex = false;

    for (name, type_) in &columns {
        println!("  {}: {}", name, type_);
        if name == "age" {
            found_age = true;
            assert!(
                type_ == "DOUBLE" || type_ == "FLOAT" || type_ == "REAL",
                "Age should be numeric, found {}",
                type_
            );
        }
        if name == "sex" {
            found_sex = true;
            assert!(
                type_ == "TEXT" || type_ == "VARCHAR",
                "Sex should be TEXT/VARCHAR, found {}",
                type_
            );
        }
    }

    assert!(found_age, "Age column not found in participants table");
    assert!(found_sex, "Sex column not found in participants table");

    let fs = Box::new(LocalFileSystem::new(dataset_path));
    let mut parser = BidsParser::new(fs, None, schema);

    // This should fail if age is numeric and n/a is not handled
    parser.parse(&db).await?;

    Ok(())
}
