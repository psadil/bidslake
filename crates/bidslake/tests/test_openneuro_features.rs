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
///
/// The image is the data file and the `.json` beside it is a sidecar, and the metadata is keyed
/// by the *image's* `file_id` — which is the `sidecars` contract, and what a `LIKE '%_sbref%'`
/// count over `all_files` cannot see. That count only asked whether the walk found a path.
#[tokio::test]
async fn sbref_images_are_data_files_with_their_metadata_on_the_image() -> Result<()> {
    let db = ingest(bids_example("ieeg_visual_multimodal")).await?;

    let (data, sidecar_files, with_metadata): (i64, i64, i64) = db.conn.query_row(
        "SELECT (SELECT COUNT(*) FROM all_files \
                  WHERE suffix='sbref' AND datatype='func' AND kind='data' \
                    AND extension='.nii.gz'), \
                (SELECT COUNT(*) FROM all_files \
                  WHERE suffix='sbref' AND kind='sidecar' AND extension='.json'), \
                (SELECT COUNT(*) FROM sidecars s JOIN all_files f USING (file_id) \
                  WHERE f.suffix='sbref')",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    assert_eq!(data, 12, "sbref images are data files, typed func/sbref");
    assert_eq!(
        sidecar_files, 12,
        "their .json siblings are cataloged as sidecars"
    );
    assert_eq!(
        with_metadata, data,
        "and sidecar metadata is keyed by the image's file_id, not the json's"
    );
    Ok(())
}

/// ds000117 has fmap acquisitions.
///
/// `datatype` and `suffix` are derived from the path grammar, so pinning them is the claim
/// `all_files` exists to make — where a `LIKE '%/fmap/%'` count would pass on any dataset with
/// a directory of that name holding any file at all. (15 magnitude1 against 16 magnitude2 is
/// real: one subject ships no magnitude1.)
#[tokio::test]
async fn fmap_files_carry_their_derived_datatype_and_suffix() -> Result<()> {
    let db = ingest(bids_example("ds000117")).await?;

    let mut stmt = db.conn.prepare(
        "SELECT kind, suffix, extension, COUNT(*) FROM all_files \
         WHERE datatype = 'fmap' GROUP BY ALL ORDER BY ALL",
    )?;
    let rows: Vec<(String, String, String, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<Result<_, _>>()?;
    assert_eq!(
        rows,
        vec![
            ("data".into(), "magnitude1".into(), ".nii".into(), 15),
            ("data".into(), "magnitude2".into(), ".nii".into(), 16),
            ("data".into(), "phasediff".into(), ".nii".into(), 16),
            ("sidecar".into(), "phasediff".into(), ".json".into(), 16),
        ],
        "datatype/suffix/kind come from the path grammar, not a LIKE on it"
    );
    Ok(())
}

/// Verify bval/bvec are stored one row per volume with numeric values.
fn verify_diffusion_arrays(conn: &Connection) -> Result<()> {
    let rows: i64 = conn.query_row("SELECT COUNT(*) FROM diffusion", [], |r| r.get(0))?;
    assert!(rows > 0, "diffusion table should have volume rows");

    // bval is a scalar per volume, and every volume has a full gradient direction.
    let missing: i64 = conn.query_row(
        "SELECT COUNT(*) FROM diffusion \
         WHERE bval IS NULL OR bvec_x IS NULL OR bvec_y IS NULL OR bvec_z IS NULL",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(missing, 0, "every volume has bval and a gradient direction");
    Ok(())
}
