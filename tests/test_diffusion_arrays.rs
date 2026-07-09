use anyhow::Result;
use bidslake::{bids::BidsParser, db::BidsDb, fs::LocalFileSystem, schema::Schema};
use tempfile::TempDir;

/// Test diffusion data parsing and storage as numeric arrays
#[tokio::test]
async fn test_diffusion_numeric_arrays() -> Result<()> {
    // Create a temporary test dataset with bval/bvec files
    let temp_dir = TempDir::new()?;
    let dataset_path = temp_dir.path().join("test_dwi");
    std::fs::create_dir_all(dataset_path.join("sub-01/dwi"))?;

    // Create dataset_description.json
    std::fs::write(
        dataset_path.join("dataset_description.json"),
        r#"{"Name": "Test DWI Dataset", "BIDSVersion": "1.0.0"}"#,
    )?;

    // Create a mock .nii.gz file
    std::fs::write(dataset_path.join("sub-01/dwi/sub-01_dwi.nii.gz"), b"")?;

    // Create .bval file with numeric b-values
    std::fs::write(
        dataset_path.join("sub-01/dwi/sub-01_dwi.bval"),
        "0 1000 1000 2000",
    )?;

    // Create .bvec file with gradient directions (3 rows: x, y, z)
    std::fs::write(
        dataset_path.join("sub-01/dwi/sub-01_dwi.bvec"),
        "0 0.707 0.707 0.577\n0 0.707 0 0.577\n0 0 0.707 0.577",
    )?;

    // Run bidslake
    let db_path = temp_dir.path().join("test.duckdb");
    let db = BidsDb::new(db_path.to_str().unwrap())?;
    let schema = Schema::load(None);
    db.create_tables(&schema)?;

    let fs = Box::new(LocalFileSystem::new(dataset_path));
    let mut parser = BidsParser::new(fs, None, schema);
    parser.parse(&db).await?;

    // Verify diffusion table has data
    let count: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM diffusion", [], |r| r.get(0))?;
    assert_eq!(count, 1, "Should have 1 diffusion entry");

    // Verify bval is numeric array
    let bval: Option<String> =
        db.conn
            .query_row("SELECT bval::VARCHAR FROM diffusion", [], |r| r.get(0))?;

    if let Some(bval_str) = bval {
        println!("bval: {}", bval_str);
        assert!(
            bval_str.contains("0") && bval_str.contains("1000") && bval_str.contains("2000"),
            "bval should contain numeric values"
        );
    } else {
        panic!("bval should not be NULL");
    }

    // Verify bvec_x is numeric array
    let bvec_x: Option<String> =
        db.conn
            .query_row("SELECT bvec_x::VARCHAR FROM diffusion", [], |r| r.get(0))?;

    if let Some(bvec_x_str) = bvec_x {
        println!("bvec_x: {}", bvec_x_str);
        assert!(
            bvec_x_str.contains("0.707"),
            "bvec_x should contain numeric values"
        );
    } else {
        panic!("bvec_x should not be NULL");
    }

    // Test array length access
    let bval_len: i32 = db
        .conn
        .query_row("SELECT len(bval) FROM diffusion", [], |r| r.get(0))?;
    assert_eq!(bval_len, 4, "bval should have 4 elements");

    // Test numeric operations on arrays
    let max_bval: f64 = db
        .conn
        .query_row("SELECT list_max(bval) FROM diffusion", [], |r| r.get(0))?;
    assert_eq!(max_bval, 2000.0, "Max b-value should be 2000");

    println!("✓ Diffusion numeric arrays verified");
    Ok(())
}
