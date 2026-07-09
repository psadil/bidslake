use bidslake::db::BidsDb;
use bidslake::schema::Schema;
use serde_json::json;
use tempfile::TempDir;

#[test]
fn test_insert_without_pk_fails() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let db_path = temp_dir.path().join("test_pk.duckdb");
    let db = BidsDb::new(db_path.to_str().unwrap())?;

    // Load schema first
    let schema = Schema::load(None);

    // Get the official CREATE SQL but remove PRIMARY KEY to simulate the user's broken state
    let create_sql = schema
        .get_create_sql("dataset_description")
        .expect("Table not found");
    let create_sql_no_pk = create_sql.replace("PRIMARY KEY", "");

    db.conn.execute(&create_sql_no_pk, [])?;

    // Try to insert using schema.insert
    // This should now SUCCEED because we use WHERE NOT EXISTS instead of INSERT OR IGNORE
    let data = json!({
        "dataset_id": "ds000001",
        "Name": "Test Dataset"
    });

    let result = schema.insert(&db.conn, "dataset_description", &data);

    match result {
        Ok(_) => println!("Insert succeeded as expected!"),
        Err(e) => {
            panic!("Insert failed with: {}", e);
        }
    }

    Ok(())
}
