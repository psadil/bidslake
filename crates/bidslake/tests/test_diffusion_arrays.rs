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
    let schema = Schema::load(None).unwrap();
    db.create_tables(&schema)?;

    let fs = Box::new(LocalFileSystem::new(dataset_path));
    let mut parser = BidsParser::new(fs, None, schema, None);
    parser.parse(&db).await?;

    // One row per volume: the .bval had 4 values, so 4 diffusion rows.
    let count: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM diffusion", [], |r| r.get(0))?;
    assert_eq!(count, 4, "should have one diffusion row per volume");

    // b-values in volume order.
    let bvals: Vec<f64> = db
        .conn
        .prepare("SELECT bval FROM diffusion ORDER BY volume_idx")?
        .query_map([], |r| r.get::<_, f64>(0))?
        .collect::<Result<_, _>>()?;
    assert_eq!(bvals, vec![0.0, 1000.0, 1000.0, 2000.0]);

    // Max b-value, as a plain scalar aggregate.
    let max_bval: f64 = db
        .conn
        .query_row("SELECT MAX(bval) FROM diffusion", [], |r| r.get(0))?;
    assert_eq!(max_bval, 2000.0, "max b-value should be 2000");

    // Gradient direction of volume 1 (bvec column-major: row 0 = x components).
    let (x, y, z): (f64, f64, f64) = db.conn.query_row(
        "SELECT bvec_x, bvec_y, bvec_z FROM diffusion WHERE volume_idx = 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    assert_eq!(
        (x, y, z),
        (0.707, 0.707, 0.0),
        "volume 1 gradient direction"
    );

    println!("✓ Diffusion row-per-volume verified");
    Ok(())
}
