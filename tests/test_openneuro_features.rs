//! Feature-focused integration tests against the offline `bids-examples`
//! corpus: diffusion (bval/bvec), single-band reference (sbref) files, and
//! fieldmaps.
//!
//! These previously fetched datasets from OpenNeuro over S3 (network-dependent,
//! hence `#[ignore]`d) and queried a `files` table that no longer exists. They
//! now run offline against the vendored submodule and the current schema.

mod common;

use anyhow::Result;
use common::{bids_example, count, ingest};
use duckdb::Connection;

/// ds000117 carries per-acquisition dwi with bval/bvec next to the niftis, so
/// the diffusion table is populated with parsed arrays.
#[tokio::test]
async fn test_diffusion_data() -> Result<()> {
    let db = ingest(bids_example("ds000117")).await?;

    let diffusion_count = count(&db, "diffusion")?;
    assert!(
        diffusion_count > 0,
        "should have diffusion data with bval/bvec"
    );

    // bval/bvec are stored as real DuckDB arrays.
    verify_diffusion_arrays(&db.conn)?;
    Ok(())
}

/// ieeg_visual_multimodal contains single-band reference (`_sbref`) images.
/// sbref files are imaging data, so each must land in the scans table.
#[tokio::test]
async fn test_sbref_files() -> Result<()> {
    let db = ingest(bids_example("ieeg_visual_multimodal")).await?;

    let sbref_count: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM scans WHERE file_path LIKE '%_sbref.nii%'",
        [],
        |r| r.get(0),
    )?;
    assert!(sbref_count > 0, "should capture sbref files as scans");
    Ok(())
}

/// ds000117 has fmap acquisitions; every fmap imaging file must appear in scans.
#[tokio::test]
async fn test_fieldmaps() -> Result<()> {
    let db = ingest(bids_example("ds000117")).await?;

    let fmap_count: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM scans WHERE file_path LIKE '%/fmap/%'",
        [],
        |r| r.get(0),
    )?;
    assert!(fmap_count > 0, "should have files in the fmap directory");
    Ok(())
}

/// Verify bval/bvec are stored as arrays with matching lengths.
fn verify_diffusion_arrays(conn: &Connection) -> Result<()> {
    let bval: String = conn.query_row("SELECT bval::VARCHAR FROM diffusion LIMIT 1", [], |r| {
        r.get(0)
    })?;
    assert!(bval.starts_with('['), "bval should be an array, got {bval}");

    // Every diffusion row's bvec components must line up with its bval length.
    let mismatched: i64 = conn.query_row(
        "SELECT COUNT(*) FROM diffusion WHERE len(bvec_x) <> len(bval) \
         OR len(bvec_y) <> len(bval) OR len(bvec_z) <> len(bval)",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(mismatched, 0, "bvec arrays must match bval length");
    Ok(())
}
