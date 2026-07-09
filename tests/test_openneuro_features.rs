use anyhow::Result;
/// Integration tests for OpenNeuro datasets with specific features:
/// - Diffusion data (bval/bvec)
/// - Fieldmap data (fmap with IntendedFor)
/// - Single-band reference files (sbref)
use bidslake::{bids::BidsParser, db::BidsDb, s3::S3Client, schema::Schema};
use duckdb::Connection;

use tempfile::TempDir;

/// Test ds000206 (THP dataset) - Contains diffusion data with bval/bvec
#[tokio::test]
#[ignore] // Ignore by default - run with --ignored flag
async fn test_ds000206_diffusion_data() -> Result<()> {
    let temp_dir = TempDir::new()?;
    let db_path = temp_dir.path().join("ds000206.duckdb");

    let db = BidsDb::new(db_path.to_str().unwrap())?;
    let schema = Schema::load(None);
    db.create_tables(&schema)?;

    // Use S3 client with anonymous access
    let s3 = S3Client::new("openneuro.org", "ds000206", true).await?;
    let mut parser = BidsParser::new(Box::new(s3), Some("ds000206".to_string()), schema);
    parser.parse(&db).await?;

    // Verify diffusion table has data
    let diffusion_count: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM diffusion", [], |r| r.get(0))?;
    assert!(
        diffusion_count > 0,
        "Should have diffusion data with bval/bvec"
    );
    println!("✓ ds000206: Found {} diffusion files", diffusion_count);

    // Verify bval and bvec arrays are populated
    verify_diffusion_arrays(&db.conn)?;

    Ok(())
}

/// Test ds001734 (NARPS) - Contains sbref files
#[tokio::test]
#[ignore]
async fn test_ds001734_sbref_files() -> Result<()> {
    let temp_dir = TempDir::new()?;
    let db_path = temp_dir.path().join("ds001734.duckdb");

    let db = BidsDb::new(db_path.to_str().unwrap())?;
    let schema = Schema::load(None);
    db.create_tables(&schema)?;

    let s3 = S3Client::new("openneuro.org", "ds001734", true).await?;
    let mut parser = BidsParser::new(Box::new(s3), Some("ds001734".to_string()), schema);
    parser.parse(&db).await?;

    // Verify sbref files exist
    let sbref_count: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM files WHERE suffix = 'sbref'",
        [],
        |r| r.get(0),
    )?;
    assert!(sbref_count > 0, "Should have sbref files");
    println!("✓ ds001734: Found {} sbref files", sbref_count);

    // List some sbref files for verification
    let sbref_files: Vec<String> = db
        .conn
        .prepare("SELECT file_path FROM files WHERE suffix = 'sbref' LIMIT 3")?
        .query_map([], |row| row.get(0))?
        .collect::<Result<Vec<String>, _>>()?;

    for file in &sbref_files {
        println!("  - {}", file);
    }

    Ok(())
}

/// Test ds000244 - Additional fieldmap dataset
#[tokio::test]
#[ignore]
async fn test_ds000244_fieldmaps() -> Result<()> {
    let temp_dir = TempDir::new()?;
    let db_path = temp_dir.path().join("ds000244.duckdb");

    let db = BidsDb::new(db_path.to_str().unwrap())?;
    let schema = Schema::load(None);
    db.create_tables(&schema)?;

    let s3 = S3Client::new("openneuro.org", "ds000244", true).await?;
    let mut parser = BidsParser::new(Box::new(s3), Some("ds000244".to_string()), schema);
    parser.parse(&db).await?;

    // Verify fmap directory exists
    let fmap_count: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM files WHERE file_path LIKE '%/fmap/%'",
        [],
        |r| r.get(0),
    )?;
    assert!(fmap_count > 0, "Should have files in fmap directory");
    println!("✓ ds000244: Found {} fmap files", fmap_count);

    Ok(())
}

// Helper function to verify diffusion arrays
fn verify_diffusion_arrays(conn: &Connection) -> Result<()> {
    // Check that bval/bvec are properly parsed as arrays
    let sample: Option<String> =
        conn.query_row("SELECT bval::VARCHAR FROM diffusion LIMIT 1", [], |r| {
            r.get(0)
        })?;

    if let Some(bval_str) = sample {
        assert!(bval_str.starts_with('['), "bval should be an array");
        println!("  ✓ bval array format verified");
    }

    Ok(())
}
